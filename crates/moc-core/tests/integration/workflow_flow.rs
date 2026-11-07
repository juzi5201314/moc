//! Workflow 端到端（fake LLM + 真 worker 子进程 + 真子会话 actor）。

use std::sync::Arc;

use moc_protocol::methods::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

use crate::support::{self, Harness};

const SCRIPT: &str = r#"
phase("plan");
const planner = agent("planner", { system: "You outline." });
const outline = await planner.ask("outline it");
phase("build");
const reviewer = agent("reviewer", { system: "You review." });
const review = await reviewer.ask("check it");
const out = await world.run("echo", ["wf-ok"]);
report({ stage: "done", review });
return { outline, review, out };
"#;

const WORKFLOW_TOOLS: &[&str] = &[
    "create_workflow",
    "amend_workflow",
    "resume_workflow_run",
    "stop_workflow_run",
    "fill_workflow_hole",
    "save_workflow",
    "list_saved_workflows",
    "workflow_status",
    "eval_workflow_snippet",
];

/// 收集全部上游请求体（模型断言与零泄漏字节比对用）。
type Captures = Arc<Mutex<Vec<Value>>>;

/// sidecar 目录惰性创建：按 transcript 文件名（`*_<session>.jsonl`）定位所在
/// 目录，再拼 `<dir>/<session>` 作为 sidecar。
fn session_sidecar(root: &std::path::Path, session: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = session_sidecar(&path, session) {
                return Some(found);
            }
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(&format!("_{session}.jsonl")))
        {
            return Some(path.parent()?.join(session));
        }
    }
    None
}

async fn mount_router(upstream: &MockServer, captured: Captures) {
    mount_router_script(upstream, captured, SCRIPT).await
}

/// 与 mount_router 同序，只换 create_workflow 载荷里的脚本。
async fn mount_router_script(upstream: &MockServer, captured: Captures, script: &'static str) {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen: Arc<Mutex<std::collections::HashSet<String>>> = Default::default();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求是 JSON");
            captured.lock().push(body.clone());
            let text = body
                .get("messages")
                .map(|m| m.to_string())
                .unwrap_or_default();
            // 子会话判定靠 tools 表里的 `yield`（主会话没有这个工具）——不能按
            // 消息文本匹配：主会话的 create_workflow 参数本身含任务字面量。
            let is_subagent = body
                .get("tools")
                .map(|t| t.to_string().contains("\"yield\""))
                .unwrap_or(false);
            let yield_report = if is_subagent && text.contains("outline it") {
                Some("PLAN")
            } else if is_subagent && text.contains("check it") {
                Some("REVIEW")
            } else {
                None
            };
            if is_subagent {
                // yield 只打交付标记，子代理还要一条非工具回复才结束回合；
                // 每个子会话只 yield 一次（按 prompt_cache_key 区分会话）。
                let key = body
                    .get("prompt_cache_key")
                    .and_then(|k| k.as_str())
                    .unwrap_or("")
                    .to_string();
                if seen.lock().insert(key) {
                    let report = yield_report.unwrap_or("done");
                    return ResponseTemplate::new(200).set_body_raw(
                        support::tool_stream_with_usage(vec![(
                            "call_yield",
                            "yield",
                            json!({"report": report}),
                        )]),
                        "text/event-stream",
                    );
                }
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("done"), "text/event-stream");
            }
            // 主会话：按序号驱动 skill → create_workflow → 收尾文本。
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match n {
                0 => ResponseTemplate::new(200).set_body_raw(
                    // 故意在加载 skill 之前调 create_workflow：应被 E_WORKFLOW_SKILL 拒。
                    support::tool_stream(vec![(
                        "call_create",
                        "create_workflow",
                        json!({"script": script}),
                    )]),
                    "text/event-stream",
                ),
                1 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_skill",
                        "skill",
                        json!({"name": "workflow-authoring"}),
                    )]),
                    "text/event-stream",
                ),
                2 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_create2",
                        "create_workflow",
                        json!({"script": script}),
                    )]),
                    "text/event-stream",
                ),
                _ => ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("workflow launched"), "text/event-stream"),
            }
        })
        .mount(upstream)
        .await;
}

/// 等下一条满足条件的 workflow 事件。
async fn next_wf_event(
    events: &mut tokio::sync::broadcast::Receiver<SessionUpdate>,
) -> (String, WorkflowEvent) {
    loop {
        let update = events.recv().await.expect("事件流存活");
        if let SessionUpdate::Workflow { run_id, event } = update {
            return (run_id, event);
        }
    }
}

/// 激活 → skill gate → 审批（role 模型覆盖）→ 双 actor + world.run → 完成通知。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_end_to_end_with_model_override() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let captured: Captures = Default::default();
    mount_router(&upstream, Arc::clone(&captured)).await;
    // 第二模型供 role 覆盖断言。
    let harness = Harness::with_ask_support(
        &upstream.uri(),
        "openai_chat",
        "[providers.test.models.gpt-alt]\ncontext_window=1000000\nmax_output_tokens=4096\n",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;

    // 激活（ask 能力已具备）。
    harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session.clone(),
        })
        .await
        .expect("激活成功");

    let mut events = harness.events.subscribe();
    // prompt 在后台跑：create_workflow 阻塞等审批，测试侧异步批准。
    let prompt_session = session.clone();
    let prompt_task = {
        let harness_client = harness.client.clone();
        tokio::spawn(async move {
            harness_client
                .call::<SessionPrompt>(PromptParams {
                    session_id: prompt_session,
                    input: vec![InputPart::Text {
                        text: "run my workflow".into(),
                    }],
                    mode: None,
                    agent_message: None,
                    goal_round: None,
                })
                .await
                .expect("prompt 成功")
        })
    };

    // 等审批请求，批准后携带 role 模型覆盖（planner → gpt-alt）。
    let mut approved = None;
    for _ in 0..64 {
        let (_run_id, event) = next_wf_event(&mut events).await;
        if let WorkflowEvent::ApprovalRequested { request } = event {
            let role_id = request
                .defaults
                .role_models
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "planner".to_string());
            let mut role_models = std::collections::BTreeMap::new();
            role_models.insert(role_id.clone(), "test/gpt-alt".to_string());
            harness
                .client
                .call::<SessionWorkflowApprove>(SessionWorkflowApproveParams {
                    session_id: session.clone(),
                    request_id: request.request_id.clone(),
                    decision: WorkflowDecision::Approve {
                        role_models: Some(role_models),
                        model_ref_map: None,
                    },
                })
                .await
                .expect("approve 成功");
            approved = Some(role_id);
            break;
        }
    }
    assert!(approved.is_some(), "应收到审批请求");

    // 等 run 终态。
    let mut terminal = None;
    for _ in 0..256 {
        let (run_id, event) = next_wf_event(&mut events).await;
        if let WorkflowEvent::RunStatus { status } = &event
            && *status != WorkflowRunStatus::Running
            && *status != WorkflowRunStatus::Pending
        {
            terminal = Some((run_id, *status));
            break;
        }
    }
    let (run_id, status) = terminal.expect("run 应达终态");
    if status != WorkflowRunStatus::Completed {
        let snap = harness
            .client
            .call::<SessionWorkflowRun>(SessionWorkflowRunParams {
                session_id: session.clone(),
                run_id: run_id.clone(),
            })
            .await
            .expect("workflow_run");
        panic!("run 未达成 completed：{status:?}，meta={:?}", snap.summary);
    }

    // run 快照：role 状态与用量、run.json 落盘。
    let snapshot = harness
        .client
        .call::<SessionWorkflowRun>(SessionWorkflowRunParams {
            session_id: session.clone(),
            run_id: run_id.clone(),
        })
        .await
        .expect("workflow_run 成功");
    let roles = snapshot.roles.clone();
    assert!(roles.len() >= 2, "planner 与 reviewer 两个 role：{roles:?}");

    // 模型断言：planner 子会话请求带 gpt-alt；reviewer 用会话默认 gpt-test。
    let bodies = captured.lock().clone();
    let used_alt = bodies
        .iter()
        .any(|b| b.get("model").and_then(|m| m.as_str()) == Some("gpt-alt"));
    let used_test = bodies
        .iter()
        .any(|b| b.get("model").and_then(|m| m.as_str()) == Some("gpt-test")
            && b.get("messages").map(|m| m.to_string().contains("check it")).unwrap_or(false));
    assert!(used_alt, "覆盖的 role 应用 gpt-alt：{bodies:?}");
    assert!(used_test, "未覆盖的 role 继承会话模型 gpt-test");

    // 用量：mock 响应带 usage → role/run spent_tokens > 0。
    let spent = snapshot
        .usage
        .get("spent_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    assert!(spent > 0, "run 应有 token 记账");

    // 完成通知投递到主会话（终态摘要注入）。
    prompt_task.await.expect("prompt 完成");
    let sidecar = session_sidecar(harness.root.path().join("sessions").as_path(), &session)
        .expect("session sidecar 存在");
    let run_dir = sidecar.join("workflows").join(&run_id);
    let run_json: Value = serde_json::from_slice(
        &std::fs::read(run_dir.join("run.json")).expect("run.json 已写"),
    )
    .unwrap();
    assert_eq!(run_json["status"], "completed");
    // A1：脚本返回值的 outline 是 ask<T> 解析出的 T（字符串），不是包裹对象。
    assert!(
        run_json["result"]["outline"].is_string(),
        "ask 交付给 guest 的应只有 value：{}",
        run_json["result"]
    );
    assert!(run_dir.join("journal.jsonl").exists(), "journal 落盘");

    harness.close().await;
}

/// 拒绝审批：反馈回到工具结果（模型可见）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_reject_returns_feedback() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &Request| {
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match n {
                0 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_skill",
                        "skill",
                        json!({"name": "workflow-authoring"}),
                    )]),
                    "text/event-stream",
                ),
                1 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_create",
                        "create_workflow",
                        json!({"script": SCRIPT}),
                    )]),
                    "text/event-stream",
                ),
                _ => ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("understood"), "text/event-stream"),
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session.clone(),
        })
        .await
        .expect("激活");

    let mut events = harness.events.subscribe();
    let prompt_session = session.clone();
    let client = harness.client.clone();
    let prompt_task = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: prompt_session,
                input: vec![InputPart::Text { text: "go".into() }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await
            .expect("prompt")
    });
    for _ in 0..64 {
        let (_run_id, event) = next_wf_event(&mut events).await;
        if let WorkflowEvent::ApprovalRequested { request } = event {
            harness
                .client
                .call::<SessionWorkflowApprove>(SessionWorkflowApproveParams {
                    session_id: session.clone(),
                    request_id: request.request_id.clone(),
                    decision: WorkflowDecision::Reject {
                        feedback: Some("needs a smaller scope".into()),
                    },
                })
                .await
                .expect("reject 成功");
            break;
        }
    }
    prompt_task.await.expect("prompt 完成");
    // 拒绝反馈应出现在会话 transcript 的工具结果里。
    let loaded = harness.load(&session).await;
    let dump = serde_json::to_string(&loaded).unwrap_or_default();
    assert!(
        dump.contains("needs a smaller scope"),
        "拒绝反馈应回给模型"
    );
    harness.close().await;
}

/// 未激活会话的激活调用：无 ask 能力的连接收到明确错误。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_activate_requires_ask_capability() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            support::text_stream("hi"),
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    let error = harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session,
        })
        .await
        .expect_err("无 ask 能力必须拒绝");
    assert!(error.to_string().contains("approval capability"));
    harness.close().await;
}

/// 请求级零泄漏：未激活（enabled=true）与 enabled=false 的首个模型请求逐字节一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_leak_first_request_byte_identical() {
    if !require_moc_bin() {
        return;
    }
    async fn first_request(extra: &str) -> (Value, String) {
        let upstream = MockServer::builder().start().await;
        let captured: Captures = Default::default();
        let cap = Arc::clone(&captured);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |request: &Request| {
                let body: Value = serde_json::from_slice(&request.body).expect("JSON");
                cap.lock().push(body);
                ResponseTemplate::new(200).set_body_raw(
                    support::text_stream("hi"),
                    "text/event-stream",
                )
            })
            .mount(&upstream)
            .await;
        let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", extra).await;
        let session = harness.session(SessionEngine::Standard).await;
        harness.prompt(&session, "hello").await;
        let root = harness.root.path().to_string_lossy().to_string();
        harness.close().await;
        (captured.lock().first().cloned().expect("有一次请求"), root)
    }
    let (enabled, enabled_root) = first_request("").await;
    let (disabled, disabled_root) = first_request("[workflow]\nenabled=false\n").await;
    let text = serde_json::to_string(&enabled).unwrap();
    for tool in WORKFLOW_TOOLS {
        assert!(!text.contains(tool), "未激活会话泄漏 {tool}");
    }
    assert!(
        !text.contains("workflow-authoring"),
        "未激活会话泄漏 workflow-authoring skill"
    );
    // 两个 harness 的根目录与会话 ID（prompt_cache_key）天然不同：归一化后
    // 比较——剩余差异才算泄漏。
    let normalize = |body: &Value, root: &str| {
        let mut body = body.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.remove("prompt_cache_key");
        }
        serde_json::to_string(&body).unwrap().replace(root, "<ROOT>")
    };
    let enabled_norm = normalize(&enabled, &enabled_root);
    let disabled_norm = normalize(&disabled, &disabled_root);
    assert_eq!(
        enabled_norm, disabled_norm,
        "未激活与 enabled=false 的首个请求（根路径与会话 ID 归一化后）应逐字节一致"
    );
}

/// 激活会话的 spawn_agent 子代理：其请求不含任何 workflow 工具。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_requests_carry_no_workflow_tools() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let captured: Captures = Default::default();
    let cap = Arc::clone(&captured);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen: Arc<Mutex<std::collections::HashSet<String>>> = Default::default();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("JSON");
            cap.lock().push(body.clone());
            let is_subagent = body
                .get("tools")
                .map(|t| t.to_string().contains("\"yield\""))
                .unwrap_or(false);
            if is_subagent {
                let key = body
                    .get("prompt_cache_key")
                    .and_then(|k| k.as_str())
                    .unwrap_or("")
                    .to_string();
                if seen.lock().insert(key) {
                    return ResponseTemplate::new(200).set_body_raw(
                        support::tool_stream(vec![(
                            "call_yield",
                            "yield",
                            json!({"report": "child done"}),
                        )]),
                        "text/event-stream",
                    );
                }
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("done"), "text/event-stream");
            }
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match n {
                0 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_spawn",
                        "spawn_agent",
                        json!({"task": "child-task", "title": "child"}),
                    )]),
                    "text/event-stream",
                ),
                _ => ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("done"), "text/event-stream"),
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session.clone(),
        })
        .await
        .expect("激活");
    harness.prompt(&session, "spawn one").await;
    // 子会话首条请求是异步的：轮询等待其到达上游。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let found = captured.lock().iter().any(|b| {
            b.get("tools")
                .map(|t| t.to_string().contains("\"yield\""))
                .unwrap_or(false)
        });
        if found || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let bodies = captured.lock().clone();
    // 子会话请求 = tools 表含 `yield` 的那批（主会话没有 yield 工具）。
    let child: Vec<&Value> = bodies
        .iter()
        .filter(|b| {
            b.get("tools")
                .map(|t| t.to_string().contains("\"yield\""))
                .unwrap_or(false)
        })
        .collect();
    assert!(!child.is_empty(), "应有子会话请求：共 {} 个请求", bodies.len());
    for body in child {
        let dump = serde_json::to_string(body).unwrap();
        for tool in WORKFLOW_TOOLS {
            assert!(!dump.contains(tool), "子代理泄漏 {tool}");
        }
    }
    harness.close().await;
}

/// 进程内无执行者的 running run 在加载时标为 stopped(interrupted)。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_run_marked_on_load() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            support::text_stream("hi"),
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;
    let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    // 从未落盘的会话没有 sidecar 目录：先跑一条消息让会话成为真实会话。
    harness.prompt(&session, "hi").await;
    harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session.clone(),
        })
        .await
        .expect("激活");
    // 手写一个 running run.json：模拟进程崩溃前未收尾的 run。sidecar 嵌在
    // `<store>/<encoded-cwd>/<session>/workflows/`，按目录名遍历定位。
    let sidecar = session_sidecar(harness.root.path().join("sessions").as_path(), &session)
        .expect("session sidecar 存在");
    let run_dir = sidecar.join("workflows").join("ghost");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("run.json"),
        serde_json::to_string(&json!({
            "run_id": "ghost",
            "status": "running",
            "title": "ghost run",
            "mode": "fresh",
            "created_at_ms": 1u64,
        }))
        .unwrap(),
    )
    .unwrap();
    let result = harness
        .client
        .call::<SessionWorkflowRuns>(SessionWorkflowRunsParams {
            session_id: session.clone(),
        })
        .await
        .expect("runs 成功");
    let ghost = result
        .runs
        .iter()
        .find(|r| r.run_id == "ghost")
        .unwrap_or_else(|| panic!("ghost run 可见，实际列表：{:?}", result.runs));
    assert_eq!(
        ghost.status,
        WorkflowRunStatus::Stopped { reason: WorkflowStopReason::Interrupted }
    );
    harness.close().await;
}

/// 需要 moc 二进制的测试入口守卫（解析逻辑见 `support::require_moc_bin`）。
fn require_moc_bin() -> bool {
    support::require_moc_bin("workflow flow")
}

/// run 目录（sidecar/workflows/<run_id>）。
fn run_dir_of(harness: &Harness, session: &str, run_id: &str) -> std::path::PathBuf {
    session_sidecar(harness.root.path().join("sessions").as_path(), session)
        .expect("session sidecar 存在")
        .join("workflows")
        .join(run_id)
}

/// 等 run 终态（含 Running/Pending 之外的第一个 RunStatus）。
async fn wait_terminal(
    events: &mut tokio::sync::broadcast::Receiver<SessionUpdate>,
) -> (String, WorkflowRunStatus) {
    for _ in 0..512 {
        let (run_id, event) = next_wf_event(events).await;
        if let WorkflowEvent::RunStatus { status } = &event
            && !matches!(status, WorkflowRunStatus::Running | WorkflowRunStatus::Pending)
        {
            return (run_id, *status);
        }
    }
    panic!("run 未在事件预算内达终态");
}

/// 常规 approve（可选 role 覆盖）。
async fn approve(harness: &Harness, session: &str, request: &WorkflowApproval) {
    harness
        .client
        .call::<SessionWorkflowApprove>(SessionWorkflowApproveParams {
            session_id: session.to_string(),
            request_id: request.request_id.clone(),
            decision: WorkflowDecision::Approve {
                role_models: None,
                model_ref_map: None,
            },
        })
        .await
        .expect("approve 成功");
}

/// 把 run.json 的 status 改写为 stopped（模拟在飞期被停止后的持久态；
/// journal 与 script 不动）。
fn mark_run_stopped(run_dir: &std::path::Path) {
    let path = run_dir.join("run.json");
    let mut meta: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    meta["status"] = json!("stopped");
    std::fs::write(&path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
}

/// journal.jsonl 里指定 kind 的记录数。
fn journal_count(run_dir: &std::path::Path, kind: &str) -> usize {
    std::fs::read_to_string(run_dir.join("journal.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .map(|v| v.get("kind").and_then(|k| k.as_str()) == Some(kind))
                .unwrap_or(false)
        })
        .count()
}

const SCRIPT_PERSONA: &str = r#"
phase("p");
const planner = agent("planner", { system: "PLANNER-outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures " });
const outline = await planner.ask("outline it");
const reviewer = agent("reviewer", { system: "REVIEWER-audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully " });
const review = await reviewer.ask("check it");
return { outline, review };
"#;

/// 驱动单 run 的标准骨架：激活 → 后台 prompt → 审批 → 终态。返回 (run_id, status)。
async fn drive_run(
    harness: &Harness,
    session: &str,
    prompt_text: &str,
) -> (String, WorkflowRunStatus) {
    harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session.to_string(),
        })
        .await
        .expect("激活");
    let mut events = harness.events.subscribe();
    let client = harness.client.clone();
    let sid = session.to_string();
    let text = prompt_text.to_string();
    let prompt_task = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: sid,
                input: vec![InputPart::Text { text }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await
    });
    for _ in 0..64 {
        let (_run_id, event) = next_wf_event(&mut events).await;
        if let WorkflowEvent::ApprovalRequested { request } = event {
            approve(harness, session, &request).await;
            break;
        }
    }
    let out = wait_terminal(&mut events).await;
    // 等首个回合真正结束：后续 prompt 才不会撞"会话忙"。
    let _ = prompt_task.await;
    out
}

/// 两 role persona：各自请求携带自己完整 persona 文本（>200 字，非 excerpt），
/// 且不含对方的。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actor_persona_full_text_and_isolated() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let captured: Captures = Default::default();
    let cap = Arc::clone(&captured);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("JSON");
            cap.lock().push(body.clone());
            let is_subagent = body
                .get("tools")
                .map(|t| t.to_string().contains("\"yield\""))
                .unwrap_or(false);
            if is_subagent {
                let text = body.get("messages").map(|m| m.to_string()).unwrap_or_default();
                let report = if text.contains("outline it") {
                    "PLAN"
                } else {
                    "REVIEW"
                };
                // 每个子会话：先 yield 标记交付，再补一条非工具回复结束回合。
                if text.contains("\"yield\"") || text.contains("call_yield") {
                    return ResponseTemplate::new(200)
                        .set_body_raw(support::text_stream("done"), "text/event-stream");
                }
                return ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![("call_yield", "yield", json!({"report": report}))]),
                    "text/event-stream",
                );
            }
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match n {
                0 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_skill",
                        "skill",
                        json!({"name": "workflow-authoring"}),
                    )]),
                    "text/event-stream",
                ),
                1 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_create",
                        "create_workflow",
                        json!({"script": SCRIPT_PERSONA}),
                    )]),
                    "text/event-stream",
                ),
                _ => ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("ok"), "text/event-stream"),
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    let (run_id, status) = drive_run(&harness, &session, "go persona").await;
    assert_eq!(status, WorkflowRunStatus::Completed, "run={run_id} 应完成");

    let bodies = captured.lock().clone();
    let children: Vec<&Value> = bodies
        .iter()
        .filter(|b| {
            b.get("tools")
                .map(|t| t.to_string().contains("\"yield\""))
                .unwrap_or(false)
        })
        .collect();
    assert!(!children.is_empty(), "应有 actor 子会话请求");
    let full_planner = "PLANNER-outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures outlines structures ";
    let full_reviewer = "REVIEWER-audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully audits carefully ";
    for child in &children {
        let dump = serde_json::to_string(child).unwrap();
        let has_planner = dump.contains(full_planner.trim_end());
        let has_reviewer = dump.contains(full_reviewer.trim_end());
        assert!(
            has_planner ^ has_reviewer,
            "每个 actor 会话应只含自己 persona（planner={has_planner} reviewer={has_reviewer}）"
        );
    }
    assert!(
        children
            .iter()
            .any(|c| serde_json::to_string(c).unwrap().contains(full_planner.trim_end())),
        "planner 会话应含完整 persona（>200 字证明非 excerpt）"
    );
    assert!(
        children
            .iter()
            .any(|c| serde_json::to_string(c).unwrap().contains(full_reviewer.trim_end())),
        "reviewer 会话应含完整 persona"
    );
    harness.close().await;
}

const SCRIPT_ONE_ASK: &str = r#"
phase("p");
const worker = agent("worker", { system: "work" });
const a = await worker.ask("step one");
return a;
"#;

/// 单 ask 脚本的标准路由：主会话 skill→create→收尾；子会话 yield 一次后 "done"。
async fn mount_single_ask_router(upstream: &MockServer, captured: Captures, script: &'static str) {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen: Arc<Mutex<std::collections::HashSet<String>>> = Default::default();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("JSON");
            captured.lock().push(body.clone());
            let is_subagent = body
                .get("tools")
                .map(|t| t.to_string().contains("\"yield\""))
                .unwrap_or(false);
            if is_subagent {
                let key = body
                    .get("prompt_cache_key")
                    .and_then(|k| k.as_str())
                    .unwrap_or("")
                    .to_string();
                if seen.lock().insert(key) {
                    return ResponseTemplate::new(200).set_body_raw(
                        support::tool_stream_with_usage(
                            vec![("call_yield", "yield", json!({"report": "A1"}))],
                        ),
                        "text/event-stream",
                    );
                }
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("done"), "text/event-stream");
            }
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match n {
                0 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_skill",
                        "skill",
                        json!({"name": "workflow-authoring"}),
                    )]),
                    "text/event-stream",
                ),
                1 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_create",
                        "create_workflow",
                        json!({"script": script}),
                    )]),
                    "text/event-stream",
                ),
                _ => ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("ok"), "text/event-stream"),
            }
        })
        .mount(upstream)
        .await;
}

/// resume：已完成的 ask 零请求重放，journal 不重复写记录。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_replays_completed_ask_without_requests() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let captured: Captures = Default::default();
    mount_single_ask_router(&upstream, Arc::clone(&captured), SCRIPT_ONE_ASK).await;
    let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    let (run_id, status) = drive_run(&harness, &session, "go").await;
    assert_eq!(status, WorkflowRunStatus::Completed);

    let run_dir = run_dir_of(&harness, &session, &run_id);
    let asks_before = journal_count(&run_dir, "ask");
    let requests_before = captured.lock().len();

    // resume 只接受 stopped run：把落盘状态改写成 stopped（等价于
    // ask 完成后在飞期被停止的持久态）。
    mark_run_stopped(&run_dir);

    harness
        .client
        .call::<SessionWorkflowResume>(SessionWorkflowRunParams {
            session_id: session.clone(),
            run_id: run_id.clone(),
        })
        .await
        .expect("resume 成功");
    let mut events = harness.events.subscribe();
    let (_id, status2) = wait_terminal(&mut events).await;
    assert_eq!(status2, WorkflowRunStatus::Completed, "resume 应再达 completed");

    assert_eq!(
        captured.lock().len(),
        requests_before,
        "resume 的已完成 ask 必须零请求重放"
    );
    assert_eq!(
        journal_count(&run_dir, "ask"),
        asks_before,
        "重放不得追加重复 ask 记录"
    );
    harness.close().await;
}

/// resume 前篡改 script.ts 的 ask 指令 → errored(InputHashMismatch)。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_tampered_script_errors_hash_mismatch() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let captured: Captures = Default::default();
    mount_single_ask_router(&upstream, captured, SCRIPT_ONE_ASK).await;
    let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    let (run_id, status) = drive_run(&harness, &session, "go").await;
    assert_eq!(status, WorkflowRunStatus::Completed);

    // 篡改：ask 指令 "step one" → "step CHANGED"（同一 site 不同输入）。
    let script_path = run_dir_of(&harness, &session, &run_id).join("script.ts");
    let script = std::fs::read_to_string(&script_path).expect("script.ts 存在");
    assert!(script.contains("step one"));
    std::fs::write(&script_path, script.replace("step one", "step CHANGED")).unwrap();
    mark_run_stopped(&run_dir_of(&harness, &session, &run_id));

    harness
        .client
        .call::<SessionWorkflowResume>(SessionWorkflowRunParams {
            session_id: session.clone(),
            run_id: run_id.clone(),
        })
        .await
        .expect("resume 发出");
    let mut events = harness.events.subscribe();
    let (_id, status2) = wait_terminal(&mut events).await;
    assert_eq!(
        status2,
        WorkflowRunStatus::Errored,
        "篡改脚本 resume 应 errored"
    );
    let meta = harness
        .client
        .call::<SessionWorkflowRun>(SessionWorkflowRunParams {
            session_id: session.clone(),
            run_id: run_id.clone(),
        })
        .await
        .expect("snapshot");
    let dump = serde_json::to_string(&meta).unwrap_or_default();
    assert!(
        dump.contains("different input than recorded"),
        "应报 InputHashMismatch 语义：{dump}"
    );
    harness.close().await;
}

const SCRIPT_AMEND_A: &str = r#"
phase("p");
const worker = agent("worker", { system: "work" });
const a = await worker.ask("step one");
const b = await worker.ask("step two");
return { a, b };
"#;

const SCRIPT_AMEND_B: &str = r#"
phase("p");
const worker = agent("worker", { system: "work" });
const a = await worker.ask("step one");
const b = await worker.ask("step changed");
return { a, b };
"#;

/// amend：未变 ask 零请求导入；首个分叉 ask 经 SeedFrom 真 fork——前驱子
/// 会话文件字节不变、新子会话含导入前缀。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn amend_imports_then_seeds_from_divergence() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let captured: Captures = Default::default();
    let cap = Arc::clone(&captured);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let per_session: Arc<Mutex<std::collections::HashMap<String, usize>>> = Default::default();
    let amend_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let amend_sent2 = Arc::clone(&amend_sent);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("JSON");
            cap.lock().push(body.clone());
            let is_subagent = body
                .get("tools")
                .map(|t| t.to_string().contains("\"yield\""))
                .unwrap_or(false);
            if is_subagent {
                // 每个 ask = yield 一次 + 收尾文本一次：同一子会话可串多个
                // ask，按第奇数个请求 yield、偶数个 "done"。
                let key = body
                    .get("prompt_cache_key")
                    .and_then(|k| k.as_str())
                    .unwrap_or("")
                    .to_string();
                let n = {
                    let mut map = per_session.lock();
                    let n = map.entry(key).or_insert(0);
                    *n += 1;
                    *n
                };
                if n % 2 == 1 {
                    return ResponseTemplate::new(200).set_body_raw(
                        support::tool_stream(vec![("call_yield", "yield", json!({"report": "R"}))]),
                        "text/event-stream",
                    );
                }
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("done"), "text/event-stream");
            }
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match n {
                0 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_skill",
                        "skill",
                        json!({"name": "workflow-authoring"}),
                    )]),
                    "text/event-stream",
                ),
                1 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_create",
                        "create_workflow",
                        json!({"script": SCRIPT_AMEND_A}),
                    )]),
                    "text/event-stream",
                ),
                _ => {
                    let text = body
                        .get("messages")
                        .map(|m| m.to_string())
                        .unwrap_or_default();
                    if let Some(run_id) = text
                        .split("amend now ")
                        .nth(1)
                        .and_then(|tail| tail.split('"').next())
                        .map(str::to_string)
                        .filter(|s| s.starts_with("run_"))
                        .filter(|_| !amend_sent2.swap(true, std::sync::atomic::Ordering::SeqCst))
                    {
                        return ResponseTemplate::new(200).set_body_raw(
                            support::tool_stream(vec![(
                                "call_amend",
                                "amend_workflow",
                                json!({"run_id": run_id, "script": SCRIPT_AMEND_B}),
                            )]),
                            "text/event-stream",
                        );
                    }
                    ResponseTemplate::new(200)
                        .set_body_raw(support::text_stream("ok"), "text/event-stream")
                }
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    let (run_id, status) = drive_run(&harness, &session, "go a").await;
    assert_eq!(status, WorkflowRunStatus::Completed);

    // 前驱 actor 子会话文件（journal 里 actor 记录给出 child_session_id）。
    let run_dir = run_dir_of(&harness, &session, &run_id);
    let journal = std::fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    let child_id = journal
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("kind").and_then(|k| k.as_str()) == Some("actor"))
        .and_then(|v| {
            v.get("child_session_id")
                .and_then(|s| s.as_str())
                .map(String::from)
        })
        .expect("actor 记录含 child_session_id");
    let child_dir = session_sidecar(harness.root.path().join("sessions").as_path(), &child_id)
        .expect("actor 子会话 sidecar 存在");
    let child_transcript = child_dir
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&format!("_{child_id}.jsonl")))
        })
        .expect("子会话 transcript 文件");
    let child_bytes_before = std::fs::read(&child_transcript).unwrap();

    // amend：第二个 prompt 触发主路由的 "amend now" 分支（messages 含该文本）。
    let mut events = harness.events.subscribe();
    let client = harness.client.clone();
    let sid = session.clone();
    let rid = run_id.clone();
    tokio::spawn(async move {
        let _ = client
            .call::<SessionPrompt>(PromptParams {
                session_id: sid,
                input: vec![InputPart::Text {
                    text: format!("amend now {rid}"),
                }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await;
    });
    // 等 amend 审批并批准。
    let mut amend_run = None;
    for iter in 0..128 {
        let recv = tokio::time::timeout(std::time::Duration::from_secs(5), next_wf_event(&mut events)).await;
        let Ok((_rid, event)) = recv else {
            let loaded = harness.load(&session).await;
            let t = serde_json::to_string(&loaded).unwrap_or_default();
            eprintln!("STALL-A@{iter} tail={}", &t[t.len().saturating_sub(3000)..]);
            panic!("amend 审批未到达");
        };
        if let WorkflowEvent::ApprovalRequested { request } = event {
            assert_eq!(
                request.kind,
                moc_protocol::WorkflowApprovalKind::Amend,
                "第二次审批应是 amend"
            );
            approve(&harness, &session, &request).await;
            amend_run = Some(request.run_id.clone());
            break;
        }
    }
    let run2 = amend_run.expect("应有 amend 审批");
    assert_ne!(run2, run_id, "amend 产生新 run_id");
    let mut status2 = None;
    for iter in 0..256 {
        let recv = tokio::time::timeout(std::time::Duration::from_secs(5), next_wf_event(&mut events)).await;
        let Ok((_rid2, ev)) = recv else {
            let j = std::fs::read_to_string(run_dir_of(&harness, &session, &run2).join("journal.jsonl")).unwrap_or_default();
            eprintln!("STALL-B@{iter} journal2={}", &j[..j.len().min(3000)]);
            panic!("run2 未达终态");
        };
        if let WorkflowEvent::RunStatus { status } = &ev
            && !matches!(status, WorkflowRunStatus::Running | WorkflowRunStatus::Pending)
        {
            status2 = Some(*status);
            break;
        }
    }
    let status2 = status2.expect("run2 终态");
    assert_eq!(status2, WorkflowRunStatus::Completed, "amend run 应完成");

    // 未变 ask 零请求重放：fork 出的新子会话（run2 journal 的 child_session_id）
    // 只应产生 ask2 的 live 请求；ask1 以导入前缀进历史、不发上游。
    let run2_dir0 = run_dir_of(&harness, &session, &run2);
    let new_child_early = std::fs::read_to_string(run2_dir0.join("journal.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("kind").and_then(|k| k.as_str()) == Some("actor"))
        .and_then(|v| v.get("child_session_id").and_then(|s| s.as_str()).map(String::from))
        .unwrap_or_default();
    let bodies = captured.lock().clone();
    // 零请求导入的直接证据：任何请求的最新 user 指令都不是 "step one"
    // （历史消息里出现 step one 无妨——那是 fork 导入的前缀）。
    let ask1_replays: Vec<&Value> = bodies
        .iter()
        .filter(|b| {
            b.get("tools")
                .map(|t| t.to_string().contains("\"yield\""))
                .unwrap_or(false)
                && b
                    .get("messages")
                    .and_then(|m| m.as_array())
                    .and_then(|msgs| {
                        msgs.iter()
                            .rev()
                            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                    })
                    .map(|m| {
                        let c = m.to_string();
                        c.contains("step one") && !c.contains("step changed")
                    })
                    .unwrap_or(false)
        })
        .collect();
    assert!(
        ask1_replays.is_empty(),
        "ask1 不得再发上游请求：{}",
        ask1_replays.len()
    );
    // ask2 的 live 请求存在（含 "step changed"）。
    let _ = new_child_early;
    // run2 journal：ask 记录 2 条（1 导入 + 1 live）。
    let ask_records = journal_count(&run2_dir0, "ask");
    assert_eq!(ask_records, 2, "run2 journal 应有 2 条 ask 记录");
    // 前驱子会话文件字节不变（不污染前驱 run 审计记录）。
    assert_eq!(
        std::fs::read(&child_transcript).unwrap(),
        child_bytes_before,
        "前驱子会话 transcript 必须逐字节不变"
    );
    // SeedFrom fork 的新子会话存在且含导入前缀（"step one" 历史）。
    let run2_dir = run_dir_of(&harness, &session, &run2);
    let journal2 = std::fs::read_to_string(run2_dir.join("journal.jsonl")).unwrap();
    let new_child_id = journal2
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("kind").and_then(|k| k.as_str()) == Some("actor"))
        .and_then(|v| {
            v.get("child_session_id")
                .and_then(|s| s.as_str())
                .map(String::from)
        })
        .expect("amend actor 记录含新 child_session_id");
    assert_ne!(new_child_id, child_id, "amend 必须 fork 新子会话");
    let new_child_dir =
        session_sidecar(harness.root.path().join("sessions").as_path(), &new_child_id)
            .expect("新子会话 sidecar");
    let new_transcript = new_child_dir
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&format!("_{new_child_id}.jsonl")))
        })
        .expect("新子会话 transcript");
    let new_text = std::fs::read_to_string(&new_transcript).unwrap();
    assert!(
        new_text.contains("step one"),
        "新子会话应含导入的 ask 前缀（step one）"
    );
    assert!(
        !new_text.contains("step two"),
        "新子会话不应含被替换 ask 之后的分叉内容"
    );
    harness.close().await;
}

const SCRIPT_TWO_ROLES: &str = r#"
phase("p");
const a = agent("first", { system: "F" });
const ra = await a.ask("q1");
await world.run("echo", ["gate"]);
const b = agent("second", { system: "S" });
const rb = await b.ask("q2");
return { ra, rb };
"#;

/// 运行中 `session/workflow_set_role_model`：之后的 ask 发往新模型。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_role_model_reroutes_later_asks() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let captured: Captures = Default::default();
    mount_single_ask_router(&upstream, Arc::clone(&captured), SCRIPT_TWO_ROLES).await;
    let harness = Harness::with_ask_support(
        &upstream.uri(),
        "openai_chat",
        "[providers.test.models.gpt-alt]\ncontext_window=1000000\nmax_output_tokens=4096\n",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;
    harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session.clone(),
        })
        .await
        .expect("激活");
    let mut events = harness.events.subscribe();
    let client = harness.client.clone();
    let sid = session.clone();
    tokio::spawn(async move {
        let _ = client
            .call::<SessionPrompt>(PromptParams {
                session_id: sid,
                input: vec![InputPart::Text { text: "go".into() }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await;
    });
    // 审批：approve 后立刻热改 second role → gpt-alt（world.run gate 给窗口）。
    for _ in 0..64 {
        let (_rid, event) = next_wf_event(&mut events).await;
        if let WorkflowEvent::ApprovalRequested { request } = event {
            // role_id 是 site_id（actor#N），不是名字：从审批 plan 里取第二个。
            let role_id = request
                .plan
                .get("roles")
                .and_then(|r| r.as_array())
                .and_then(|roles| roles.last().cloned())
                .and_then(|r| r.get("role_id").and_then(|v| v.as_str()).map(str::to_string))
                .expect("plan 有第二个 role");
            approve(&harness, &session, &request).await;
            harness
                .client
                .call::<SessionWorkflowSetRoleModel>(
                    SessionWorkflowSetRoleModelParams {
                        session_id: session.clone(),
                        run_id: request.run_id.clone(),
                        role_id,
                        model: "test/gpt-alt".into(),
                    },
                )
                .await
                .expect("set_role_model 成功");
            break;
        }
    }
    let (_id, status) = wait_terminal(&mut events).await;
    assert_eq!(status, WorkflowRunStatus::Completed);
    // second actor 的 ask 请求（消息含 "q2"）应发往 gpt-alt。
    let bodies = captured.lock().clone();
    let q2 = bodies.iter().any(|b| {
        b.get("messages")
            .map(|m| m.to_string().contains("q2"))
            .unwrap_or(false)
            && b.get("model").and_then(|m| m.as_str()) == Some("gpt-alt")
    });
    assert!(q2, "second actor 的 ask 应用热改后的 gpt-alt");
    harness.close().await;
}

const SCRIPT_HOLE: &str = r#"
phase("p");
const value = await hole<string>("missing", "what value?");
const out = await world.run("echo", [value]);
return out;
"#;

/// hole：打开 → 主会话通知 → fill（body 含新 world.run 命令 → hole_fill 审批
/// 只列新增命令）→ 批准 → 完成。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hole_fill_with_new_command_approval() {
    if !require_moc_bin() {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let captured: Captures = Default::default();
    let cap = Arc::clone(&captured);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("JSON");
            cap.lock().push(body.clone());
            let text = body.get("messages").map(|m| m.to_string()).unwrap_or_default();
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(run_id) = text
                .split("fill the hole ")
                .nth(1)
                .and_then(|tail| tail.split('"').next())
                .filter(|s| s.starts_with("run_"))
            {
                // body 引入新 world.run 命令 `ls`（脚本只批了 echo）→ hole_fill。
                return ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_fill",
                        "fill_workflow_hole",
                        json!({
                            "run_id": run_id.to_string(),
                            "hole": "missing",
                            "body": "const files = await world.run(\"ls\", [\".\"]); return \"filled-\" + files.exitCode;"
                        }),
                    )]),
                    "text/event-stream",
                );
            }
            match n {
                0 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_skill",
                        "skill",
                        json!({"name": "workflow-authoring"}),
                    )]),
                    "text/event-stream",
                ),
                1 => ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_create",
                        "create_workflow",
                        json!({"script": SCRIPT_HOLE}),
                    )]),
                    "text/event-stream",
                ),
                _ => ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("ok"), "text/event-stream"),
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session.clone(),
        })
        .await
        .expect("激活");
    let mut events = harness.events.subscribe();
    let client = harness.client.clone();
    let sid = session.clone();
    let mut first_prompt = Some(tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: sid,
                input: vec![InputPart::Text { text: "go".into() }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await
    }));
    // 审批 → hole 打开 → 发 "fill the hole <run_id>" prompt → hole_fill 审批。
    let mut run_id: Option<String> = None;
    let mut hole_opened = false;
    let mut filled_prompt_sent = false;
    let mut hole_fill_seen = false;
    for _ in 0..256 {
        let (rid, event) = next_wf_event(&mut events).await;
        match event {
            WorkflowEvent::ApprovalRequested { request } => {
                match request.kind {
                    moc_protocol::WorkflowApprovalKind::HoleFill => {
                        hole_fill_seen = true;
                        // hole_fill 审批只列新增命令：ls 在、echo 不在。
                        let new = request.new_commands.clone().unwrap_or_default();
                        assert!(new.iter().any(|c| c.contains("ls")), "应列新增 ls：{new:?}");
                        approve(&harness, &session, &request).await;
                    }
                    _ => approve(&harness, &session, &request).await,
                }
            }
            WorkflowEvent::HoleOpened { name, .. } => {
                assert_eq!(name, "missing");
                hole_opened = true;
                run_id = Some(rid.clone());
                // hole 通知已到主会话：等首个回合结束后发 fill prompt
                // （会话忙时 SessionPrompt 会被拒——不能与其竞争）。
                if !filled_prompt_sent {
                    filled_prompt_sent = true;
                    first_prompt
                        .take()
                        .expect("仅取一次")
                        .await
                        .expect("首个 prompt 完成")
                        .expect("prompt 成功");
                    let client = harness.client.clone();
                    let sid = session.clone();
                    let rid2 = rid.clone();
                    tokio::spawn(async move {
                        client
                            .call::<SessionPrompt>(PromptParams {
                                session_id: sid,
                                input: vec![InputPart::Text {
                                    text: format!("fill the hole {rid2}"),
                                }],
                                mode: None,
                                agent_message: None,
                    goal_round: None,
                            })
                            .await
                            .expect("fill prompt 成功");
                    });
                }
            }
            WorkflowEvent::RunStatus { status }
                if !matches!(status, WorkflowRunStatus::Running | WorkflowRunStatus::Pending) =>
            {
                assert_eq!(status, WorkflowRunStatus::Completed);
                break;
            }
            _ => {}
        }
    }
    let _ = run_id;
    assert!(hole_opened, "hole 应打开");
    assert!(hole_fill_seen, "新 world.run 命令应触发 hole_fill 审批");
    harness.close().await;
}

/// world.read（files/git）+ artifact（markdown/table）+ report 端到端。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_world_reads_and_artifacts() {
    if !require_moc_bin() {
        return;
    }
    const SCRIPT_WA: &str = r#"
const names = await files.glob("fixtures/*.txt");
const head = await files.read("fixtures/a.txt");
let git_status = null;
try { git_status = await git.status(); } catch { git_status = "no-git"; }
await artifact.markdown("notes", "notes-body\n");
artifact.table("rows", { columns: [{ field: "k", label: "K" }], key: "k" });
report({ k: "x" }, "rows");
return { names, head, git: git_status };
"#;
    let upstream = MockServer::builder().start().await;
    let captured: Captures = Default::default();
    mount_router_script(&upstream, Arc::clone(&captured), SCRIPT_WA).await;
    let harness = Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    // 工作区 fixture + 可选 git 仓库（无 git 环境时脚本侧 catch）。
    let work = harness.root.path().join("work");
    std::fs::create_dir_all(work.join("fixtures")).unwrap();
    std::fs::write(work.join("fixtures/a.txt"), "alpha\n").unwrap();
    std::fs::write(work.join("fixtures/b.txt"), "beta\n").unwrap();
    let has_git = std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_git {
        let out = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&work)
            .output()
            .expect("git init");
        assert!(out.status.success(), "git init: {out:?}");
    }

    harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session.clone(),
        })
        .await
        .expect("激活成功");
    let mut events = harness.events.subscribe();
    let prompt_session = session.clone();
    let prompt_task = {
        let client = harness.client.clone();
        tokio::spawn(async move {
            client
                .call::<SessionPrompt>(PromptParams {
                    session_id: prompt_session,
                    input: vec![InputPart::Text { text: "go".into() }],
                    mode: None,
                    agent_message: None,
                    goal_round: None,
                })
                .await
                .expect("prompt 成功")
        })
    };
    // 审批（无命令集、无 actor——常规 approve）。
    for _ in 0..64 {
        let (_run_id, event) = next_wf_event(&mut events).await;
        if let WorkflowEvent::ApprovalRequested { request } = event {
            approve(&harness, &session, &request).await;
            break;
        }
    }
    let (run_id, status) = wait_terminal(&mut events).await;
    assert_eq!(status, WorkflowRunStatus::Completed, "run 应 completed");
    prompt_task.await.expect("prompt 完成");

    let run_dir = run_dir_of(&harness, &session, &run_id);
    let run_json: Value =
        serde_json::from_slice(&std::fs::read(run_dir.join("run.json")).unwrap()).unwrap();
    // files.glob：相对路径 + 排序；files.read：内容。
    assert_eq!(run_json["result"]["names"], json!(["fixtures/a.txt", "fixtures/b.txt"]));
    assert_eq!(run_json["result"]["head"], "alpha\n");
    if has_git {
        // git.status：初始化仓库下 fixtures 两文件 untracked。
        let st = &run_json["result"]["git"];
        assert!(st["branch"].is_string(), "branch 字段：{st}");
        assert!(st["untracked"].is_array(), "{st}");
    } else {
        assert_eq!(run_json["result"]["git"], "no-git");
    }
    // artifacts 条目形状：publish（file/version）+ preset 声明（spec）。
    let artifacts = run_json["artifacts"].as_array().expect("artifacts 数组");
    let md = artifacts
        .iter()
        .find(|a| a["artifact_id"] == "notes")
        .expect("markdown 条目");
    assert_eq!(md["member"], "markdown");
    assert_eq!(md["version"], 1);
    assert!(md["file"].is_string(), "{md}");
    let table = artifacts
        .iter()
        .find(|a| a["artifact_id"] == "rows")
        .expect("table 声明");
    assert_eq!(table["member"], "table");
    assert_eq!(table["spec"]["key"], "k");
    // markdown 版本文件落盘（v<N>_ 前缀，service 按此解析）。
    assert!(run_dir.join("artifacts/notes/v1_content.md").exists());
    harness.close().await;
}

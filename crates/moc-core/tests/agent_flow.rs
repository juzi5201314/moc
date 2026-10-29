//! 子代理通信层验收：投递三态、广播、唤醒、yield 交付、批量等待与自动投递。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use moc_protocol::methods::{
    AgentList, AgentListParams, AgentMessage, AgentMessageOutcome, AgentMessageParams, AgentSpawn,
    AgentSpawnParams, AgentStatus, AgentWait, AgentWaitParams, SessionCompact,
    SessionCompactParams, SessionLoad, SessionLoadParams, SessionRedo, SessionRedoParams,
    SessionRewind, SessionRewindParams, SessionUsage, SessionUsageParams, UsageSource,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use support::Harness;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

/// 按请求体内容分派响应：同一个上游同时伺候父会话与多个子代理。
async fn mount_router(
    upstream: &MockServer,
    router: impl Fn(&Value) -> ResponseTemplate + Send + Sync + 'static,
) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求是 JSON");
            router(&body)
        })
        .mount(upstream)
        .await;
}

async fn harness(upstream: &MockServer) -> Harness {
    Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "test/gpt-test:off",
        "",
    )
    .await
}

async fn spawn_child(harness: &Harness, parent_id: &str, task: &str) -> String {
    harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: parent_id.to_owned(),
            task: task.to_owned(),
            title: Some("child".into()),
            model: Some("inherit".into()),
            preset: None,
            output_schema: None,
        })
        .await
        .expect("spawn 成功")
        .agent
        .id
}

async fn wait_children(
    harness: &Harness,
    parent_id: &str,
    agent_ids: Option<Vec<String>>,
    timeout_secs: Option<u64>,
) -> moc_protocol::methods::AgentWaitResult {
    harness
        .client
        .call::<AgentWait>(AgentWaitParams {
            session_id: parent_id.to_owned(),
            agent_ids,
            timeout_secs,
        })
        .await
        .expect("wait 成功")
}

async fn message_agent(
    harness: &Harness,
    parent_id: &str,
    target: &str,
    text: &str,
) -> Result<moc_protocol::methods::AgentMessageResult, moc_protocol::RpcCallError> {
    harness
        .client
        .call::<AgentMessage>(AgentMessageParams {
            session_id: parent_id.to_owned(),
            target: target.to_owned(),
            message: text.to_owned(),
        })
        .await
}

fn yield_call(_id: &str, report: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        support::tool_stream(vec![("call_yield", "yield", json!({"report": report}))]),
        "text/event-stream",
    )
}

fn text_response(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(support::text_stream(text), "text/event-stream")
}

fn delayed(template: ResponseTemplate, milliseconds: u64) -> ResponseTemplate {
    template.set_delay(Duration::from_millis(milliseconds))
}

#[tokio::test]
async fn spawn_wait_returns_yield_report() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("CHILD_TASK") {
            yield_call("call_yield", "CHILD_REPORT")
        } else {
            text_response("PARENT_OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;

    let waited = wait_children(&harness, &parent_id, None, Some(5)).await;
    assert!(waited.settled, "子代理已交付，等待必须立即回报");
    assert!(!waited.timed_out);
    assert_eq!(waited.agents.len(), 1);
    assert_eq!(waited.agents[0].id, child_id);
    assert_eq!(waited.agents[0].status, AgentStatus::Completed);
    assert_eq!(
        waited.agents[0].message.as_ref().expect("交付物").text(),
        "CHILD_REPORT",
        "yield 报告即权威结果"
    );

    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id.clone(),
        })
        .await
        .expect("list 成功");
    assert_eq!(listed.agents.len(), 1);
    assert_eq!(listed.agents[0].status, AgentStatus::Completed);

    let child = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: child_id,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("child transcript 可加载");
    assert_eq!(child.entries[0].message.text(), "CHILD_TASK");
    assert!(
        child
            .entries
            .iter()
            .any(|entry| entry.message.tool_calls().any(|call| call.name == "yield")),
        "子代理必须通过 yield 交付"
    );
    harness.close().await;
}

#[tokio::test]
async fn failed_child_agent_reports_its_error_to_the_parent() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("CHILD_TASK") {
            return ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": {"message": "上游拒绝子代理", "code": "boom"}
            }));
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id]), Some(10)).await;
    assert!(waited.settled);
    assert_eq!(waited.agents[0].status, AgentStatus::Failed);
    let message = waited.agents[0]
        .message
        .as_ref()
        .expect("失败消息必须对父会话可见");
    assert!(
        message.text().contains("Subagent prompt failed"),
        "失败消息应说明原因：{}",
        message.text()
    );

    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id,
        })
        .await
        .expect("list 成功");
    assert!(
        listed.agents[0]
            .last_message
            .as_deref()
            .is_some_and(|text| text.contains("Subagent prompt failed")),
        "列表必须带出失败原因：{:?}",
        listed.agents[0].last_message
    );
    harness.close().await;
}

#[tokio::test]
async fn missing_yield_reminds_three_times_then_falls_back() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("REFUSES_TASK") {
            // 纯文本收尾：既没有 yield，也不调用任何工具。
            text_response("CHILD_ATTEMPT")
        } else {
            text_response("PARENT_OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "REFUSES_TASK").await;

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(20)).await;
    assert!(waited.settled);
    assert_eq!(waited.agents[0].status, AgentStatus::Completed);
    assert_eq!(
        waited.agents[0].message.as_ref().expect("回退结果").text(),
        "CHILD_ATTEMPT",
        "缺 yield 时回退到最后一条 assistant 正文"
    );

    let child = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: child_id,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("child transcript 可加载");
    let reminders: Vec<String> = child
        .entries
        .iter()
        .map(|entry| entry.message.text())
        .filter(|text| text.contains("Reminder"))
        .collect();
    assert_eq!(reminders.len(), 3, "提醒必须恰好三次：{reminders:?}");
    assert!(reminders[0].contains("Reminder 1/3"));
    assert!(reminders[2].contains("Reminder 3/3"));
    harness.close().await;
}

#[tokio::test]
async fn wait_returns_on_first_settle_with_snapshot() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        let text = body.to_string();
        if text.contains("FAST_TASK") {
            return yield_call("call_yield", "FAST_DONE");
        }
        if text.contains("SLOW_TASK") {
            return delayed(yield_call("call_yield", "SLOW_DONE"), 2_000);
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let fast = spawn_child(&harness, &parent_id, "FAST_TASK").await;
    let slow = spawn_child(&harness, &parent_id, "SLOW_TASK").await;

    let waited = wait_children(
        &harness,
        &parent_id,
        Some(vec![fast.clone(), slow.clone()]),
        Some(10),
    )
    .await;
    assert!(waited.settled, "首个终态即返回");
    assert!(!waited.timed_out);
    assert_eq!(waited.agents.len(), 2);
    assert_eq!(waited.agents[0].id, fast);
    assert_eq!(waited.agents[0].status, AgentStatus::Completed);
    assert_eq!(waited.agents[1].id, slow, "快照保持解析顺序，不因终态重排");
    assert_eq!(
        waited.agents[1].status,
        AgentStatus::Running,
        "未结束的代理只回报运行中"
    );
    assert!(waited.agents[1].message.is_none());
    harness.close().await;
}

#[tokio::test]
async fn wait_times_out_without_events() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("SLOW_TASK") {
            return delayed(yield_call("call_yield", "SLOW_DONE"), 30_000);
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let slow = spawn_child(&harness, &parent_id, "SLOW_TASK").await;

    let started = Instant::now();
    let waited = wait_children(&harness, &parent_id, Some(vec![slow]), Some(1)).await;
    assert!(waited.timed_out, "窗口内无事件必须按超时回报");
    assert!(!waited.settled);
    assert_eq!(waited.agents[0].status, AgentStatus::Running);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "超时窗口必须按 timeout_secs 生效，实际 {:?}",
        started.elapsed()
    );
    harness.close().await;
}

#[tokio::test]
async fn wait_without_children_is_rejected() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |_| text_response("PARENT_OK")).await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;

    let error = harness
        .client
        .call::<AgentWait>(AgentWaitParams {
            session_id: parent_id,
            agent_ids: None,
            timeout_secs: Some(1),
        })
        .await
        .expect_err("没有子代理时等待必须明确失败");
    assert!(
        error.to_string().contains("INVALID_PARAMS"),
        "错误必须指明参数问题：{error}"
    );
    harness.close().await;
}

#[tokio::test]
async fn message_routes_running_as_injected_and_idle_as_woken() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        let text = body.to_string();
        // 历史里会同时带着旧任务与旧注入消息，命中顺序必须先看最新一条。
        if text.contains("WAKE_TEXT") {
            return yield_call("call_yield", "WOKEN_DONE");
        }
        if text.contains("INJECT_TEXT") {
            return yield_call("call_yield", "INJECT_DONE");
        }
        if text.contains("SLOW_TASK") {
            return delayed(yield_call("call_yield", "SLOW_DONE"), 800);
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "SLOW_TASK").await;

    let first = message_agent(&harness, &parent_id, &child_id, "INJECT_TEXT")
        .await
        .expect("运行中投递成功");
    assert_eq!(first.receipts.len(), 1);
    assert_eq!(first.receipts[0].outcome, AgentMessageOutcome::Injected);
    assert_eq!(first.receipts[0].agent_id, child_id);

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(10)).await;
    assert!(waited.settled);
    // 注入发生在首轮内还是收尾阶段取决于调度，两种落点都是合法注入。
    let delivered = waited.agents[0].message.as_ref().expect("首轮结果").text();
    assert!(
        matches!(delivered.as_str(), "SLOW_DONE" | "INJECT_DONE"),
        "首轮结果必须来自子代理自己的交付：{delivered}"
    );

    // 注入的消息必须落成带来源元数据的条目，并在模型视图里带 `<agent-message>`。
    let child = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: child_id.clone(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("child transcript 可加载");
    let injected = child
        .entries
        .iter()
        .find(|entry| entry.agent.is_some())
        .expect("注入消息必须落库");
    assert_eq!(injected.message.text(), "INJECT_TEXT");
    let context = injected.context_message().expect("在上下文内");
    assert!(context.text().contains("<agent-message from="));
    assert!(context.text().contains("INJECT_TEXT"));
    let meta = injected.agent.clone().expect("来源元信息");
    assert_eq!(meta.from_id, parent_id);
    let card = harness.updates.lock().iter().any(|update| match update {
        moc_protocol::methods::SessionUpdate::AgentMessage { meta, body } => {
            meta.from_id == parent_id && body == "INJECT_TEXT"
        }
        _ => false,
    });
    assert!(card, "接收侧必须推送 IRC 卡片更新");

    // 子代理已空闲：再投递必须走唤醒，并产生新回合。
    let second = message_agent(&harness, &parent_id, &child_id, "WAKE_TEXT")
        .await
        .expect("空闲投递成功");
    assert_eq!(second.receipts[0].outcome, AgentMessageOutcome::Woken);
    let woken = wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(10)).await;
    assert!(woken.settled);
    assert_eq!(
        woken.agents[0].message.as_ref().expect("唤醒结果").text(),
        "WOKEN_DONE",
        "唤醒消息即新回合的输入"
    );
    harness.close().await;
}

#[tokio::test]
async fn broadcast_reaches_running_and_skips_idle() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        let text = body.to_string();
        if text.contains("FAST_TASK") {
            return yield_call("call_yield", "FAST_DONE");
        }
        if text.contains("SLOW_A") || text.contains("SLOW_B") {
            return delayed(yield_call("call_yield", "SLOW_DONE"), 1_500);
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let idle = spawn_child(&harness, &parent_id, "FAST_TASK").await;
    let running_a = spawn_child(&harness, &parent_id, "SLOW_A").await;
    let running_b = spawn_child(&harness, &parent_id, "SLOW_B").await;
    let settled = wait_children(&harness, &parent_id, Some(vec![idle.clone()]), Some(10)).await;
    assert_eq!(settled.agents[0].status, AgentStatus::Completed);

    let idle_entries = harness.load(&idle).await.entries.len();
    let result = message_agent(&harness, &parent_id, "all", "BROADCAST_TEXT")
        .await
        .expect("广播成功");
    let mut targets: Vec<&str> = result
        .receipts
        .iter()
        .map(|receipt| receipt.agent_id.as_str())
        .collect();
    targets.sort_unstable();
    let mut expected = vec![running_a.as_str(), running_b.as_str()];
    expected.sort_unstable();
    assert_eq!(targets, expected, "广播只覆盖运行中的直接子代理");
    assert!(
        result
            .receipts
            .iter()
            .all(|receipt| receipt.outcome == AgentMessageOutcome::Injected)
    );
    assert_eq!(
        harness.load(&idle).await.entries.len(),
        idle_entries,
        "空闲子代理不得被广播叫醒"
    );
    harness.close().await;
}

#[tokio::test]
async fn child_message_wakes_parent_wait() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        let text = body.to_string();
        if text.contains("PINGER_TASK") && text.contains("CHILD_PING") {
            // 第二次请求：消息已投递，子代理收尾。
            return yield_call("call_yield", "CHILD_DONE");
        }
        if text.contains("PINGER_TASK") {
            return delayed(
                ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![(
                        "call_ping",
                        "message_agent",
                        json!({"target": "parent", "message": "CHILD_PING"}),
                    )]),
                    "text/event-stream",
                ),
                600,
            );
        }
        if text.contains("<agent-message") {
            return text_response("PARENT_SAW_PING");
        }
        if text.contains("PARENT_WAIT") {
            return ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![("call_wait", "wait_agent", json!({}))]),
                "text/event-stream",
            );
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "PINGER_TASK").await;

    let result = harness.prompt(&parent_id, "PARENT_WAIT").await;
    assert_eq!(result.message.text(), "PARENT_SAW_PING");

    let parent = harness.load(&parent_id).await;
    let wait_result = parent
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .map(|result| result.content.clone())
        .find(|content| content.contains("messages_pending"))
        .expect("wait_agent 的结果必须落库");
    assert!(
        wait_result.contains("\"messages_pending\":true"),
        "子代理消息必须打断等待：{wait_result}"
    );
    let ping = parent
        .entries
        .iter()
        .find(|entry| entry.agent.is_some())
        .expect("父会话必须收到带来源的代理消息");
    assert_eq!(ping.message.text(), "CHILD_PING");
    assert_eq!(ping.agent.as_ref().expect("来源元信息").from_id, child_id);

    let progress = harness.updates.lock().iter().any(|update| {
        matches!(
            update,
            moc_protocol::methods::SessionUpdate::ToolProgress { .. }
        )
    });
    assert!(progress, "wait_agent 必须推送进度行");
    harness.close().await;
}

#[tokio::test]
async fn completed_child_result_lands_in_parent_notices() {
    let upstream = MockServer::builder().start().await;
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    mount_router(&upstream, move |body| {
        if body.to_string().contains("CHILD_TASK") {
            return yield_call("call_yield", "CHILD_REPORT");
        }
        recorded.lock().push(body.clone());
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;
    let waited = wait_children(&harness, &parent_id, Some(vec![child_id]), Some(10)).await;
    assert!(waited.settled);
    assert!(bodies.lock().is_empty(), "子代理结果不得进入父会话请求");

    // 子代理完成后父会话再跑一轮：结果必须以 notice 形式进入父的上下文。
    harness.prompt(&parent_id, "PARENT_AFTER").await;
    let bodies = bodies.lock().clone();
    let body = bodies.last().expect("父会话至少发出一次请求").to_string();
    assert!(
        body.contains("Subagent ") && body.contains("Result preview"),
        "父上下文必须包含子代理完成提示：{body}"
    );
    assert!(body.contains("CHILD_REPORT"), "提示必须带结果预览");
    harness.close().await;
}

#[tokio::test]
async fn unknown_target_and_self_target_are_rejected() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |_| text_response("PARENT_OK")).await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;

    let unknown = message_agent(&harness, &parent_id, "ses_unknown0001", "hi")
        .await
        .expect_err("未知目标必须失败");
    assert!(
        unknown.to_string().contains("INVALID_PARAMS"),
        "未知目标错误：{unknown}"
    );
    let itself = message_agent(&harness, &parent_id, &parent_id, "hi")
        .await
        .expect_err("自投递必须失败");
    assert!(
        itself.to_string().contains("INVALID_PARAMS"),
        "自投递错误：{itself}"
    );
    let no_parent = message_agent(&harness, &parent_id, "parent", "hi")
        .await
        .expect_err("顶层会话没有父代理");
    assert!(
        no_parent.to_string().contains("INVALID_PARAMS"),
        "顶层 parent 目标错误：{no_parent}"
    );
    harness.close().await;
}

/// 子代理系统块携带任务（权威顺序压过通用仓库规则），运行期清单只走尾部提醒，
/// 免得兄弟状态一变就让系统前缀缓存失效。
#[tokio::test]
async fn child_system_carries_the_task_and_the_roster_rides_a_tail_reminder() {
    let upstream = MockServer::builder().start().await;
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    mount_router(&upstream, move |body| {
        recorded.lock().push(body.clone());
        if body.to_string().contains("CHILD_TASK") {
            yield_call("call_yield", "CHILD_REPORT")
        } else {
            text_response("PARENT_OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    spawn_child(&harness, &parent_id, "CHILD_TASK").await;
    wait_children(&harness, &parent_id, None, Some(5)).await;

    let bodies = bodies.lock().clone();
    let child = bodies
        .iter()
        .find(|body| body.to_string().contains("Task (authoritative"))
        .expect("子代理请求已记录");
    let messages = child["messages"].as_array().expect("消息数组");
    let system = messages[0]["content"].as_str().expect("系统提示为字符串");
    assert!(system.contains("<subagent>"), "{system}");
    assert!(system.contains("Task (authoritative"), "{system}");
    assert!(
        system.contains("CHILD_TASK"),
        "任务必须进入系统块：{system}"
    );
    assert!(
        !system.contains("<agent-roster>"),
        "运行期清单不得进入系统前缀：{system}"
    );

    let roster_index = messages
        .iter()
        .position(|message| {
            message["content"]
                .as_str()
                .is_some_and(|text| text.contains("<agent-roster>"))
        })
        .expect("清单以尾部提醒注入");
    assert_eq!(messages[roster_index]["role"], "user");
    assert_eq!(
        roster_index,
        messages.len() - 1,
        "清单必须是最后一条提醒：{messages:?}"
    );
    let roster = messages[roster_index]["content"]
        .as_str()
        .expect("清单文本");
    assert!(roster.contains(&parent_id), "清单必须列出父代理：{roster}");
    harness.close().await;
}

/// 请求体里的工具名，保持上游顺序。
fn tool_names(body: &Value) -> Vec<&str> {
    body["tools"].as_array().map_or_else(Vec::new, |tools| {
        tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect()
    })
}

/// 回退是原地截断：不产生新会话、不建立父链接，会话身份与普通顶层会话完全一致。
///
/// 回退后继续提问，系统提示与工具表仍是顶层会话那一套——没有子代理契约、没有
/// `yield`、没有代理名册。
#[tokio::test]
async fn rewind_stays_in_place_and_keeps_plain_session_shape() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |_body| text_response("收到")).await;
    let harness = harness(&upstream).await;
    let source = harness.session(moc_protocol::SessionEngine::Standard).await;
    harness.prompt(&source, "第一轮").await;

    let entries = harness.load(&source).await.entries;
    let target = entries
        .iter()
        .find(|entry| entry.message.role == moc_protocol::Role::User && entry.agent.is_none())
        .expect("回退目标为首条用户消息");
    let rewound = harness
        .client
        .call::<SessionRewind>(SessionRewindParams {
            session_id: source.clone(),
            entry_id: target.id.clone(),
        })
        .await
        .expect("回退成功")
        .session;
    assert_eq!(rewound.id, source, "回退必须留在原会话");
    assert!(rewound.forked_from.is_none());
    assert!(
        !rewound.is_subagent(),
        "回退不得建立父链接：{:?}",
        rewound.parent_session_id
    );

    harness.prompt(&source, "回退后的第一轮").await;
    let requests = upstream.received_requests().await.expect("上游请求");
    assert_eq!(requests.len(), 2, "两轮提示各一条请求");
    let plain: Value = serde_json::from_slice(&requests[0].body).expect("普通会话请求");
    let rewound_req: Value = serde_json::from_slice(&requests[1].body).expect("回退后请求");
    let tools = tool_names(&rewound_req);
    assert_eq!(
        rewound_req["messages"][0], plain["messages"][0],
        "回退后的系统提示必须与普通会话一致"
    );
    assert_eq!(
        tools,
        tool_names(&plain),
        "回退后的工具表必须与普通会话一致"
    );
    assert!(
        !tools.contains(&"yield"),
        "回退会话不得拿到 yield：{tools:?}"
    );
    let rendered = rewound_req.to_string();
    assert!(
        !rendered.contains("<subagent>") && !rendered.contains("<agent-roster>"),
        "回退会话不得携带子代理契约或名册：{rewound_req}"
    );
    harness.close().await;
}

/// 会话身份只由会话头决定，与请求来自谁无关：用户聚焦子代理会话直接提问时，
/// 系统提示与工具表仍必须是子代理那一套——有 `yield`、没有派发工具。
#[tokio::test]
async fn focused_child_session_keeps_subagent_contract_for_user_prompts() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("CHILD_TASK") {
            yield_call("call_yield", "CHILD_REPORT")
        } else {
            text_response("OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;
    wait_children(&harness, &parent_id, None, Some(5)).await;

    harness.prompt(&child_id, "用户直接跟进").await;
    let requests = upstream.received_requests().await.expect("上游请求");
    let body: Value =
        serde_json::from_slice(&requests.last().expect("最后一次请求").body).expect("请求 JSON");
    let system = body["messages"][0]["content"].as_str().expect("系统提示");
    assert!(system.contains("<subagent>"), "{system}");
    let tools = tool_names(&body);
    assert!(
        tools.contains(&"yield"),
        "子代理会话必须拿到 yield：{tools:?}"
    );
    assert!(
        !tools.contains(&"spawn_agent"),
        "子代理不得拿到派发工具：{tools:?}"
    );
    harness.close().await;
}

/// 压缩摘要请求必须复用子代理的系统前缀：身份、交付约定和任务都在系统提示里，
/// 压缩时漏掉这一段会让缓存前缀在第 0 条消息就分叉。
#[tokio::test]
async fn subagent_compaction_reuses_the_subagent_system_prefix() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body["stream"] != true {
            // 摘要内容不合格时 soft 会重试并最终走确定性兜底，压缩仍然成立。
            return ResponseTemplate::new(200).set_body_json(support::complete("压缩小结"));
        }
        if body.to_string().contains("子代理继续") {
            text_response("好的")
        } else {
            yield_call("call_yield", "CHILD_REPORT")
        }
    })
    .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "test/gpt-test:off",
        "[compaction]\nsoft_method='moc'\nmethod_order=['soft']\nauto=false\nkeep_recent_tokens=0\n",
    )
    .await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;
    wait_children(&harness, &parent_id, None, Some(5)).await;
    harness.prompt(&child_id, "子代理继续").await;

    harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: child_id.clone(),
            instructions: None,
        })
        .await
        .expect("子代理会话必须能压缩");

    let requests = upstream.received_requests().await.expect("上游请求");
    let bodies: Vec<Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("请求 JSON"))
        .collect();
    let plain = bodies
        .iter()
        .rev()
        .find(|body| body["stream"] == true)
        .expect("子代理普通请求");
    let compaction = bodies
        .iter()
        .find(|body| body["stream"] != true)
        .expect("压缩摘要请求");

    let system = plain["messages"][0]["content"]
        .as_str()
        .expect("系统提示为字符串");
    assert!(system.contains("<subagent>"), "{system}");
    assert!(system.contains("CHILD_TASK"), "{system}");
    assert_eq!(
        compaction["messages"][0], plain["messages"][0],
        "压缩请求必须逐字节复用子代理系统提示"
    );
    assert_eq!(
        compaction["tools"], plain["tools"],
        "压缩请求必须携带同一份工具表"
    );
    harness.close().await;
}

/// 子代理请求的系统提示含 `<subagent>` 块；用它区分父/子请求。
fn is_subagent_request(body: &Value) -> bool {
    body["messages"][0]["content"]
        .as_str()
        .is_some_and(|text| text.contains("<subagent>"))
}

/// `explore` preset 只应裁掉写入/计划/委托面，基础与通信面全开。
#[tokio::test]
async fn explore_preset_denies_plan_and_control_but_keeps_the_rest() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if is_subagent_request(body) {
            yield_call("call_yield", "EXPLORE_DONE")
        } else {
            text_response("PARENT_OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: parent_id.clone(),
            task: "EXPLORE_TASK".into(),
            title: None,
            model: Some("inherit".into()),
            preset: Some("explore".into()),
            output_schema: None,
        })
        .await
        .expect("explore preset spawn 成功")
        .agent
        .id;
    wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(10)).await;

    let requests = upstream.received_requests().await.expect("上游请求");
    let child: Value = requests
        .iter()
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .find(|body| is_subagent_request(body) && body.to_string().contains("EXPLORE_TASK"))
        .expect("子代理请求已记录");
    let tools = tool_names(&child);
    for denied in [
        "edit",
        "write",
        "spawn_agent",
        "stop_agent",
        "wait_agent",
        "plan",
        "exit_plan",
        "clm_context",
        "ask",
    ] {
        assert!(
            !tools.contains(&denied),
            "explore 不得拿到 {denied}：{tools:?}"
        );
    }
    // `web_search` 依赖 provider 能力，harness 未配置——不列入保留面断言。
    for kept in [
        "read",
        "grep",
        "find",
        "bash",
        "todo",
        "job",
        "skill",
        "run_code",
        "web_fetch",
        "yield",
        "list_agents",
        "message_agent",
    ] {
        assert!(tools.contains(&kept), "explore 必须保留 {kept}：{tools:?}");
    }

    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id,
        })
        .await
        .expect("list 成功");
    assert_eq!(listed.agents[0].preset.as_deref(), Some("explore"));

    let info = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: child_id,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("child transcript 可加载")
        .session;
    assert_eq!(info.agent_preset.as_deref(), Some("explore"));
    harness.close().await;
}

/// 未知 preset 名必须报出候选清单。
#[tokio::test]
async fn unknown_preset_is_rejected_with_available_names() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |_| text_response("PARENT_OK")).await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;

    let error = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: parent_id,
            task: "X".into(),
            title: None,
            model: None,
            preset: Some("no_such_preset".into()),
            output_schema: None,
        })
        .await
        .expect_err("未知 preset 必须失败");
    let message = error.to_string();
    assert!(
        message.contains("explore"),
        "错误必须列出可用 preset：{message}"
    );
    harness.close().await;
}

/// 默认 `max_depth=1` 时 depth-1 子代理拿不到 spawn；提到 2 后 depth-1 可委托、
/// depth-2 仍到顶。
#[tokio::test]
async fn nested_delegation_respects_max_depth() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        let text = body.to_string();
        let is_child = is_subagent_request(body);
        if is_child && text.contains("GRANDCHILD_TASK") {
            return yield_call("call_yield", "GRAND_DONE");
        }
        if is_child {
            let has_tool_result = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
            if has_tool_result {
                return yield_call("call_yield", "CHILD_DONE");
            }
            return ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![(
                    "call_spawn",
                    "spawn_agent",
                    json!({"task": "GRANDCHILD_TASK"}),
                )]),
                "text/event-stream",
            );
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "test/gpt-test:off",
        "[agents]\nmax_depth=2\n",
    )
    .await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(15)).await;
    assert!(waited.settled, "子代理完成委托并交付");
    assert_eq!(waited.agents[0].status, AgentStatus::Completed);

    // depth-1 交付时孙代理可能还在跑；轮询到孙代理终态，保证它的请求已发出。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let child_listed = harness
            .client
            .call::<AgentList>(AgentListParams {
                session_id: child_id.clone(),
            })
            .await
            .expect("子代理可列出自己的后代");
        assert_eq!(child_listed.agents.len(), 1, "子代理名下必须有孙代理");
        if matches!(
            child_listed.agents[0].status,
            AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Stopped
        ) {
            break;
        }
        assert!(Instant::now() < deadline, "孙代理未在时限内完成");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let requests = upstream.received_requests().await.expect("上游请求");
    let bodies: Vec<Value> = requests
        .iter()
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .collect();
    // 任务文本只出现在子代理系统块（messages[0]）；用整条 body 会把
    // depth-1 第二轮请求里 spawn_agent 的参数误判成孙代理请求。
    fn system(body: &Value) -> &str {
        body["messages"][0]["content"].as_str().unwrap_or("")
    }
    let depth_one = bodies
        .iter()
        .find(|body| is_subagent_request(body) && system(body).contains("CHILD_TASK"))
        .expect("depth-1 请求已记录");
    assert!(
        tool_names(depth_one).contains(&"spawn_agent"),
        "max_depth=2 时 depth-1 必须拿到 spawn_agent"
    );
    let depth_two = bodies
        .iter()
        .find(|body| is_subagent_request(body) && system(body).contains("GRANDCHILD_TASK"))
        .expect("depth-2 请求已记录");
    assert!(
        !tool_names(depth_two).contains(&"spawn_agent"),
        "depth-2 已到委托上限，不得再派发"
    );
    harness.close().await;
}

/// `output_schema` 声明后 yield 必须带 `result` 并通过校验；结果随 wait 快照回父。
#[tokio::test]
async fn structured_yield_validates_result_and_surfaces_it_in_wait() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if !is_subagent_request(body) {
            return text_response("PARENT_OK");
        }
        let has_tool_result = body["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
        if !has_tool_result {
            // 第一轮故意缺 result——工具层应拒绝并让子代理重试。
            return yield_call("call_yield", "MISSING_RESULT");
        }
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![(
                "call_yield",
                "yield",
                json!({"report": "DONE", "result": {"count": 3}}),
            )]),
            "text/event-stream",
        )
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: parent_id.clone(),
            task: "STRUCTURED_TASK".into(),
            title: None,
            model: Some("inherit".into()),
            preset: None,
            output_schema: Some(json!({
                "type": "object",
                "properties": {"count": {"type": "integer"}},
                "required": ["count"],
            })),
        })
        .await
        .expect("结构化 spawn 成功")
        .agent
        .id;

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(10)).await;
    assert!(waited.settled);
    assert_eq!(
        waited.agents[0].structured_result,
        Some(json!({"count": 3})),
        "结构化结果必须随 wait 快照回父"
    );

    let child = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: child_id,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("child transcript 可加载");
    let results: Vec<String> = child
        .entries
        .iter()
        .flat_map(|entry| {
            entry
                .message
                .tool_results()
                .map(|result| result.content.clone())
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        results.iter().any(|text| text.contains("result")),
        "缺 result 的 yield 必须被拒：{results:?}"
    );
    harness.close().await;
}

/// `tasks[]` 批量 spawn：共享 `context` 前缀拼到每个任务上；单项失败不拖垮整批。
#[tokio::test]
async fn batch_spawn_creates_agents_and_reports_per_item_errors() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if is_subagent_request(body) {
            return yield_call("call_yield", "BATCH_DONE");
        }
        let has_tool_result = body["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
        if has_tool_result {
            return text_response("PARENT_DONE");
        }
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![(
                "call_spawn",
                "spawn_agent",
                json!({
                    "context": "SHARED_CONTEXT",
                    "tasks": [
                        {"task": "TASK_ONE"},
                        {"task": "TASK_TWO", "title": "second"},
                        {"task": ""}
                    ]
                }),
            )]),
            "text/event-stream",
        )
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    harness.prompt(&parent_id, "开批量子代理").await;

    let loaded = harness.load(&parent_id).await;
    let spawn_result = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|result| result.content.contains("agents"))
        .expect("批量 spawn 结果已落盘");
    let result_json: Value = serde_json::from_str(&spawn_result.content).expect("批量结果为 JSON");
    assert_eq!(
        result_json["agents"].as_array().map(Vec::len),
        Some(2),
        "两条有效任务必须各建一个代理：{result_json}"
    );
    let errors = result_json["errors"].as_array().expect("errors 数组");
    assert_eq!(errors.len(), 1, "空任务必须记为单项错误：{result_json}");
    assert_eq!(errors[0]["index"], 2);

    // wait 在首个 settle 即返回；轮询到两个都终态，保证第二个子代理的请求已发出。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let listed = harness
            .client
            .call::<AgentList>(AgentListParams {
                session_id: parent_id.clone(),
            })
            .await
            .expect("list 成功");
        assert_eq!(listed.agents.len(), 2, "批量必须建起两个代理");
        eprintln!("AGENTS {:?}", listed.agents);
        if listed.agents.iter().all(|agent| {
            matches!(
                agent.status,
                AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Stopped
            )
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "批量子代理未在时限内完成");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let requests = upstream.received_requests().await.expect("上游请求");
    for request in &requests {
        let body: Value = serde_json::from_slice(&request.body).expect("请求 JSON");
        eprintln!(
            "REQ subagent={} shared={} sys={:.80}",
            is_subagent_request(&body),
            body.to_string().contains("SHARED_CONTEXT"),
            body["messages"][0]["content"]
                .as_str()
                .unwrap_or("<nonstring>")
        );
    }
    let shared_count = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| is_subagent_request(body) && body.to_string().contains("SHARED_CONTEXT"))
        .count();
    assert_eq!(shared_count, 2, "共享 context 必须拼进每个子任务");
    harness.close().await;
}

/// `task` 与 `tasks` 互斥，批量不得超过 16 项。
#[test]
fn spawn_schema_rejects_task_task_conflict_and_batch_bounds() {
    use moc_core::tools::{Tool, agent::SpawnAgentTool};
    let validator = jsonschema::validator_for(&SpawnAgentTool.def().parameters)
        .expect("spawn_agent schema 必须是合法 JSON Schema");
    for args in [
        json!({}),
        json!({"task": "a", "tasks": [{"task": "b"}]}),
        json!({"tasks": []}),
        json!({"tasks": [{"task": "a"}, {"task": "a"}, {"task": "a"}, {"task": "a"},
                         {"task": "a"}, {"task": "a"}, {"task": "a"}, {"task": "a"},
                         {"task": "a"}, {"task": "a"}, {"task": "a"}, {"task": "a"},
                         {"task": "a"}, {"task": "a"}, {"task": "a"}, {"task": "a"},
                         {"task": "a"}]}),
        json!({"tasks": [{"title": "no task"}]}),
    ] {
        let errors: Vec<String> = validator
            .iter_errors(&args)
            .map(|error| error.to_string())
            .collect();
        assert!(!errors.is_empty(), "{args} 必须被拒：{errors:?}");
    }
    for args in [
        json!({"task": "a"}),
        json!({"tasks": [{"task": "a"}]}),
        json!({"tasks": [{"task": "a"}, {"task": "b", "preset": "explore"}],
               "context": "shared"}),
    ] {
        assert!(
            validator.is_valid(&args),
            "{args} 必须合法：{:?}",
            validator.iter_errors(&args).collect::<Vec<_>>()
        );
    }
}

/// 回退截断 spawn 记录后子代理成孤儿：运行中被停杀、从名册与 wait 目标消失、
/// 标记落盘持久化；redo 接回 spawn 引用后重新可见，但已停的代理不复活。
#[tokio::test]
async fn rewinding_past_a_spawn_orphans_stops_and_hides_the_child() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        // 子代理判定必须在任务名匹配之前：父会话第二轮请求的工具结果里也回显
        // 任务文本，先匹配任务名会把父请求错路由到延迟分支。
        if is_subagent_request(body) && body.to_string().contains("SLOW_CHILD_TASK") {
            // 子代理迟迟不交付：回退发生时它仍在运行。
            return delayed(yield_call("call_yield", "CHILD_DONE"), 30_000);
        }
        if is_subagent_request(body) {
            return yield_call("call_yield", "CHILD_DONE");
        }
        let has_tool_result = body["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
        if has_tool_result {
            return text_response("PARENT_DONE");
        }
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![(
                "call_spawn",
                "spawn_agent",
                json!({"tasks": [{"task": "SLOW_CHILD_TASK"}]}),
            )]),
            "text/event-stream",
        )
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    harness.prompt(&parent_id, "开一个子代理").await;

    let loaded = harness.load(&parent_id).await;
    let spawn_result = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|result| result.content.contains("agent_id"))
        .expect("spawn 结果已落盘");
    let result_json: Value =
        serde_json::from_str(&spawn_result.content).expect("spawn 结果为 JSON");
    let child_id = result_json["agents"][0]["agent_id"]
        .as_str()
        .expect("结果带 agent_id")
        .to_owned();
    let user_entry_id = loaded
        .entries
        .iter()
        .find(|entry| entry.message.role == moc_protocol::Role::User && entry.agent.is_none())
        .expect("发起 spawn 的用户提示")
        .id
        .clone();
    let tip_entry_id = loaded.entries.last().expect("转录非空").id.clone();

    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id.clone(),
        })
        .await
        .expect("list 成功");
    assert_eq!(listed.agents.len(), 1, "回退前子代理在册");
    assert_eq!(listed.agents[0].id, child_id);

    // 回退到发起 spawn 的那条用户消息：消息本身回到输入框，其后整段被截走。
    harness
        .client
        .call::<SessionRewind>(SessionRewindParams {
            session_id: parent_id.clone(),
            entry_id: user_entry_id,
        })
        .await
        .expect("回退成功");

    // 名册与 wait 都不再认这个子代理；停杀状态即时推送。
    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id.clone(),
        })
        .await
        .expect("list 成功");
    assert!(listed.agents.is_empty(), "孤儿子代理必须退出名册");
    let waited = harness
        .client
        .call::<AgentWait>(AgentWaitParams {
            session_id: parent_id.clone(),
            agent_ids: Some(vec![child_id.clone()]),
            timeout_secs: None,
        })
        .await;
    assert!(waited.is_err(), "孤儿必须不是可等待目标：{waited:?}");
    let stopped_update = harness.updates.lock().iter().any(|update| {
        matches!(
            update,
            moc_protocol::methods::SessionUpdate::AgentStatusChanged {
                agent_id,
                status,
                ..
            } if *agent_id == child_id && *status == AgentStatus::Stopped
        )
    });
    assert!(stopped_update, "孤儿停杀必须推送状态变化");

    // 孤儿标记随截断落盘到父会话 sidecar。
    let store = moc_core::session::store::SessionStore::new(harness.root.path().join("sessions"));
    let paths = store
        .find(&parent_id)
        .await
        .expect("查父会话")
        .expect("父会话已落盘");
    let marks_file = paths.sidecar.join("orphaned_agents");
    let marks = std::fs::read_to_string(&marks_file).expect("孤儿标记文件已写盘");
    assert!(
        marks.lines().any(|line| line == child_id),
        "标记文件必须含孤儿 id：{marks}"
    );

    // redo 把 spawn 引用接回：名册恢复可见、标记清空；已停的代理不复活。
    harness
        .client
        .call::<SessionRedo>(SessionRedoParams {
            session_id: parent_id.clone(),
            entry_id: tip_entry_id,
        })
        .await
        .expect("前滚成功");
    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id.clone(),
        })
        .await
        .expect("list 成功");
    assert_eq!(listed.agents.len(), 1, "redo 后子代理回到名册");
    assert_eq!(listed.agents[0].id, child_id);
    assert_eq!(
        listed.agents[0].status,
        AgentStatus::Stopped,
        "停杀不随前滚复活"
    );
    assert!(!marks_file.exists(), "redo 必须清空孤儿标记文件");
    harness.close().await;
}

/// Fusion 会话的 harness：lead=`gpt-test`、sidekick=`gpt-cheap`，上游按
/// `body["model"]` 路由出两套应答。
async fn fusion_harness(upstream: &MockServer) -> Harness {
    Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "fusion",
        "fusion",
        "[providers.test.models.gpt-cheap]\ncontext_window=1000000\nmax_output_tokens=4096\n\
         [models.fusion]\nlead='test/gpt-test'\nsidekick='test/gpt-cheap'\n",
    )
    .await
}

/// Fusion lead：系统提示带 `<fusion>` 契约与 `sidekick` 工具；两次 handoff 走
/// spawned/woken 且复用同一个执行侧会话（上下文延续）；阻塞报告直达工具结果、
/// 不再回投完成通知；普通子代理继承的是 lead 模型而非组合 selector。
#[tokio::test]
async fn fusion_lead_hands_off_to_a_persistent_sidekick() {
    let upstream = MockServer::builder().start().await;
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    let handoffs = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&handoffs);
    mount_router(&upstream, move |body| {
        recorded.lock().push(body.clone());
        if body["model"] == "gpt-cheap" {
            // sidekick：唤醒后上下文里已带着首次 handoff。
            let report = if body.to_string().contains("HANDOFF_TWO") {
                "SIDEKICK_REPORT_2"
            } else {
                "SIDEKICK_REPORT"
            };
            return delayed(yield_call("call_yield", report), 50);
        }
        if is_subagent_request(body) {
            return yield_call("call_yield", "CHILD_DONE");
        }
        // lead：末条是工具结果就收尾，否则发起第 n 次 handoff。
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last());
        if last.is_some_and(|message| message["role"] == "tool") {
            return text_response("LEAD_DONE");
        }
        let index = counter.fetch_add(1, Ordering::SeqCst);
        let call = format!("call_sk{index}");
        let message = if index == 0 {
            "HANDOFF_ONE"
        } else {
            "HANDOFF_TWO"
        };
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![(
                call.as_str(),
                "sidekick",
                json!({"message": message}),
            )]),
            "text/event-stream",
        )
    })
    .await;
    let harness = fusion_harness(&upstream).await;
    let lead_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    assert_eq!(
        harness.prompt(&lead_id, "开工").await.message.text(),
        "LEAD_DONE"
    );
    assert_eq!(
        harness.prompt(&lead_id, "继续").await.message.text(),
        "LEAD_DONE"
    );

    let bodies = bodies.lock().clone();
    let lead_reqs: Vec<&Value> = bodies
        .iter()
        .filter(|body| body["model"] == "gpt-test" && !is_subagent_request(body))
        .collect();
    assert!(lead_reqs.len() >= 4, "两轮提示各两条 lead 请求");
    let system = lead_reqs[0]["messages"][0]["content"]
        .as_str()
        .expect("系统提示");
    assert!(system.contains("<fusion>"), "{system}");
    assert!(
        tool_names(lead_reqs[0]).contains(&"sidekick"),
        "lead 工具表必须含 sidekick"
    );
    // 阻塞 handoff 的报告走工具结果直达；后续请求不得再出现完成通知。
    for body in &lead_reqs[1..] {
        assert!(
            !body.to_string().contains("Subagent \"Sidekick\""),
            "完成通知不得重复投递：{body}"
        );
    }

    let sidekick_reqs: Vec<&Value> = bodies
        .iter()
        .filter(|body| body["model"] == "gpt-cheap")
        .collect();
    assert_eq!(sidekick_reqs.len(), 2, "两次 handoff 各一轮 sidekick 请求");
    let sidekick_tools = tool_names(sidekick_reqs[0]);
    for denied in ["ask", "sidekick", "spawn_agent", "stop_agent", "wait_agent"] {
        assert!(
            !sidekick_tools.contains(&denied),
            "sidekick 不得拿到 {denied}：{sidekick_tools:?}"
        );
    }
    for kept in ["yield", "message_agent", "bash", "edit"] {
        assert!(
            sidekick_tools.contains(&kept),
            "sidekick 必须保留 {kept}：{sidekick_tools:?}"
        );
    }
    let sidekick_system = sidekick_reqs[0]["messages"][0]["content"]
        .as_str()
        .expect("sidekick 系统提示");
    assert!(sidekick_system.contains("<subagent>"), "{sidekick_system}");
    assert!(
        sidekick_system.contains("Preset: sidekick"),
        "{sidekick_system}"
    );
    assert!(
        !sidekick_system.contains("<fusion>"),
        "sidekick 不得拿到 lead 契约：{sidekick_system}"
    );
    let woken = sidekick_reqs[1].to_string();
    assert!(woken.contains("HANDOFF_ONE"), "上下文必须跨 handoff 延续");
    assert!(woken.contains("HANDOFF_TWO"), "唤醒消息必须进入上下文");

    let loaded = harness.load(&lead_id).await;
    let handoff_results: Vec<Value> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .filter_map(|result| serde_json::from_str::<Value>(&result.content).ok())
        .filter(|value| value["delivery"].is_string())
        .collect();
    assert_eq!(handoff_results.len(), 2, "两次 handoff 的结果");
    assert_eq!(handoff_results[0]["delivery"], "spawned");
    assert_eq!(handoff_results[0]["status"], "completed");
    assert_eq!(handoff_results[0]["report"], "SIDEKICK_REPORT");
    assert_eq!(handoff_results[1]["delivery"], "woken");
    assert_eq!(handoff_results[1]["report"], "SIDEKICK_REPORT_2");
    assert_eq!(
        handoff_results[0]["agent_id"], handoff_results[1]["agent_id"],
        "同一边 kick 会话必须复用"
    );

    // 普通子代理 inherit 的是 lead 的真实 selector，不是组合写法。
    let spawned = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: lead_id.clone(),
            task: "CHILD_TASK".into(),
            title: None,
            model: Some("inherit".into()),
            preset: None,
            output_schema: None,
        })
        .await
        .expect("spawn 成功");
    assert_eq!(spawned.agent.model, "test/gpt-test:off");
    wait_children(&harness, &lead_id, Some(vec![spawned.agent.id]), Some(10)).await;

    // session/usage 聚合整棵会话树：自身 + sidekick + 普通子代理各占一行。
    let usage = harness
        .client
        .call::<SessionUsage>(SessionUsageParams {
            session_id: lead_id.clone(),
        })
        .await
        .expect("usage 查询成功");
    let find = |source: UsageSource, model: &str| {
        usage
            .models
            .iter()
            .find(|summary| summary.source == source && summary.model == model)
            .unwrap_or_else(|| panic!("缺少 {source:?} 的 {model} 用量行"))
    };
    assert!(find(UsageSource::Session, "test/gpt-test:off").calls >= 4);
    assert_eq!(find(UsageSource::Sidekick, "test/gpt-cheap:off").calls, 2);
    assert!(find(UsageSource::Subagent, "test/gpt-test:off").calls >= 1);
    assert_eq!(usage.models.len(), 3, "{:?}", usage.models);
    let calls: u64 = usage.models.iter().map(|summary| summary.calls).sum();
    let input: u64 = usage
        .models
        .iter()
        .map(|summary| summary.input_tokens)
        .sum();
    assert_eq!(
        (usage.total.calls, usage.total.input_tokens),
        (calls, input),
        "合计应等于各行之和"
    );
    harness.close().await;
}

/// 收尾窗口投递：`yield` 响应延迟期间注入的消息必须让子代理再跑一轮作答，
/// 不能只在结束后落盘而无人应答。
#[tokio::test]
async fn injected_message_during_closing_window_is_answered() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("PARENT_LATE") {
            return yield_call("call_yield", "FINAL_REPORT");
        }
        if is_subagent_request(body) {
            // 首轮延迟交付，给收尾窗口留投递时间。
            return delayed(yield_call("call_yield", "FIRST_REPORT"), 800);
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "SLOW_TASK").await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    let receipt = message_agent(&harness, &parent_id, &child_id, "PARENT_LATE")
        .await
        .expect("收尾窗口投递");
    assert_eq!(receipt.receipts[0].outcome, AgentMessageOutcome::Injected);

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id]), Some(10)).await;
    assert!(waited.settled);

    // 子代理必须再发一次请求把注入消息答掉：消息进入后续请求的上下文。
    let requests = upstream.received_requests().await.expect("上游请求");
    let answered = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .any(|body| is_subagent_request(&body) && body.to_string().contains("PARENT_LATE"));
    assert!(answered, "注入消息必须进入子代理的模型请求");
    harness.close().await;
}

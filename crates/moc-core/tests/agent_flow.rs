//! 子代理通信层验收：投递三态、广播、唤醒、yield 交付、批量等待与自动投递。

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use moc_protocol::methods::{
    AgentList, AgentListParams, AgentMessage, AgentMessageOutcome, AgentMessageParams, AgentSpawn,
    AgentSpawnParams, AgentStatus, AgentWait, AgentWaitParams, SessionLoad, SessionLoadParams,
    SessionRewind, SessionRewindParams,
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;

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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;

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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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

/// 回退分叉是完全独立的会话：既没有父链接，也没有任何子代理效果。
///
/// 身份、系统提示与工具表都必须与普通顶层会话逐字一致；出身只落在会话头的
/// `forked_from` 上，供用户端展示，模型侧永远看不到。
#[tokio::test]
async fn rewind_fork_matches_a_plain_session_and_keeps_no_parent_link() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |_body| text_response("收到")).await;
    let harness = harness(&upstream).await;
    let source = harness.session(moc_protocol::SessionMode::Normal).await;
    harness.prompt(&source, "第一轮").await;

    let entries = harness.load(&source).await.entries;
    let target = entries
        .iter()
        .find(|entry| entry.message.role == moc_protocol::Role::User && entry.agent.is_none())
        .expect("回退目标为首条用户消息");
    let fork = harness
        .client
        .call::<SessionRewind>(SessionRewindParams {
            session_id: source.clone(),
            entry_id: target.id.clone(),
        })
        .await
        .expect("回退成功")
        .session;
    assert_eq!(fork.forked_from.as_deref(), Some(source.as_str()));
    assert!(
        !fork.is_subagent(),
        "分叉不得建立父链接：{:?}",
        fork.parent_session_id
    );

    harness.prompt(&fork.id, "分叉后的第一轮").await;
    let requests = upstream.received_requests().await.expect("上游请求");
    assert_eq!(requests.len(), 2, "两轮提示各一条请求");
    let plain: Value = serde_json::from_slice(&requests[0].body).expect("普通会话请求");
    let forked: Value = serde_json::from_slice(&requests[1].body).expect("分叉请求");
    let fork_tools = tool_names(&forked);
    assert_eq!(
        forked["messages"][0], plain["messages"][0],
        "分叉的系统提示必须与普通会话一致"
    );
    assert_eq!(
        fork_tools,
        tool_names(&plain),
        "分叉的工具表必须与普通会话一致"
    );
    assert!(
        !fork_tools.contains(&"yield"),
        "分叉不得拿到 yield：{fork_tools:?}"
    );
    let rendered = forked.to_string();
    assert!(
        !rendered.contains("<subagent>") && !rendered.contains("<agent-roster>"),
        "分叉不得携带子代理契约或名册：{forked}"
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
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
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

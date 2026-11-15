//! 子代理通信层验收：投递三态、广播、唤醒、yield 交付、批量等待与自动投递。

use crate::support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::support::{Harness, transcript_entries};
use moc_llm::Role;
use moc_protocol::methods::{
    AgentList, AgentListParams, AgentMessage, AgentMessageOutcome, AgentMessageParams, AgentSpawn,
    AgentSpawnParams, AgentStatus, AgentWait, AgentWaitParams, InputPart, PromptParams,
    SessionCancel, SessionCancelParams, SessionCompact, SessionCompactParams, SessionLoad,
    SessionLoadParams, SessionModelOverflowAction, SessionPrompt, SessionRedo, SessionRedoParams,
    SessionRewind, SessionRewindParams, SessionSetModel, SessionSetModelParams, SessionUsage,
    SessionUsageParams, UsageSource,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
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
            role: "explorer".into(),
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

/// sidekick 声明了 `output_schema`：`yield` 必须带合规的 `result`。
fn sidekick_yield_call(report: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        support::tool_stream(vec![(
            "call_yield",
            "yield",
            json!({
                "report": report,
                "result": {
                    "status": "done",
                    "changes": [{"path": "src/lib.rs", "summary": "改了一个文件"}],
                    "verification": [{"command": "cargo test", "outcome": "passed"}],
                    "open_issues": []
                }
            }),
        )]),
        "text/event-stream",
    )
}

fn text_response(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(support::text_stream(text), "text/event-stream")
}

mod behavior;
mod configuration;
mod lifecycle;
mod messaging;
mod waiting;

/// 请求体里的工具名，保持上游顺序。
fn tool_names(body: &Value) -> Vec<&str> {
    body["tools"].as_array().map_or_else(Vec::new, |tools| {
        tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect()
    })
}

/// 子代理请求的系统提示含 `<subagent>` 块；用它区分父/子请求。
fn is_subagent_request(body: &Value) -> bool {
    body["messages"][0]["content"]
        .as_str()
        .is_some_and(|text| text.contains("<subagent>"))
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

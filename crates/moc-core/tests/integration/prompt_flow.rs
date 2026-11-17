//! 固定上游响应下验证 core 的 JSON-RPC 提示闭环。

use crate::support;

use crate::support::Harness;

fn text_stream(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "response",
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": text},
                "finish_reason": "stop"
            }]
        })
    )
}

fn complete(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "summary",
        "model": "gpt-test",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }]
    })
}
use parking_lot::Mutex;
use std::sync::Arc;

use moc_core::{Core, CoreConfig};
use moc_llm::Client;
use moc_protocol::methods::{
    ClientCapabilities, ClientInfo, ConfigAction, ConfigManage, ConfigReply, ConfigScope,
    DiffLineKind, InputPart, PlanAction, PlanAnnotation, PlanStatus, PromptMode, PromptParams,
    SessionCompact, SessionCompactParams, SessionEngine, SessionList, SessionListParams,
    SessionLoad, SessionLoadParams, SessionLoadResult, SessionModelOverflowAction, SessionNew,
    SessionNewParams, SessionPlanApprove, SessionPlanApproveParams, SessionPlanDiff,
    SessionPlanDiffParams, SessionPlanMode, SessionPlanModeParams, SessionPlanRevise,
    SessionPlanReviseParams, SessionPrompt, SessionSetModel, SessionSetModelParams, SessionUnload,
    SessionUnloadParams, SessionUpdate, SessionUpdateMethod, SessionUsage, SessionUsageParams,
    UpstreamField,
};
use moc_protocol::{ClientBuilder, Server, ServerOptions, transport};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
mod behavior;
mod context;
mod sessions;
mod streaming;
mod tools;

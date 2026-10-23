//! 宿主和 worker 共用的 JSON 通信协议。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 包含 JSON 转义与 envelope 的传输帧硬上限；读端分配前即检查。
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    pub code: String,
    pub memory_limit_bytes: usize,
    pub max_output_bytes: usize,
    pub max_message_bytes: usize,
    pub max_pending_calls: usize,
    pub max_total_calls: usize,
    pub timeout_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    Start {
        execution_id: u64,
        request: StartRequest,
    },
    Complete {
        execution_id: u64,
        completions: Vec<HostCompletion>,
    },
    Cancel {
        execution_id: u64,
        code: String,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestMessage {
    Step { execution_id: u64, step: GuestStep },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRequest {
    pub id: u32,
    pub operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Tool {
        name: String,
        arguments: Value,
    },
    Servers,
    Search {
        query: Option<String>,
        server: Option<String>,
        offset: Option<usize>,
        limit: Option<usize>,
    },
    Describe {
        name: String,
    },
    Store {
        key: String,
        value: Value,
    },
    Load {
        key: String,
    },
    Remove {
        key: String,
    },
    Keys,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCompletion {
    pub id: u32,
    pub result: Result<Value, ScriptError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptToolResult {
    pub content: String,
    pub summary: Option<String>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
    pub status: String,
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptError {
    pub code: String,
    pub message: String,
    pub result: Option<ScriptToolResult>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestStep {
    pub requests: Vec<HostRequest>,
    pub state: GuestState,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestState {
    Pending,
    Done { value: Value },
    Failed { error: ScriptError },
}

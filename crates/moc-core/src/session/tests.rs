use crate::session::cache_budget::serialized_bytes;
use crate::session::entries::EntryPayload;
use crate::session::entries::SessionOrigin;
use crate::session::projection::truncate_text;
use moc_llm::Message;
use moc_llm::ToolResult;
use moc_llm::{ContentPart, Role, ToolCall};
use moc_protocol::SessionAutonomy;
use moc_protocol::SessionInfo;
use moc_protocol::SessionPermission;
use moc_protocol::ToolExecution;
use moc_protocol::TranscriptChildCall;
use moc_protocol::methods::{
    AgentMessageMeta, AgentMessageOutcome, SessionEngine, SessionRenameParams, ToolStatus,
};

use super::*;

mod behavior;
mod history;
mod lease;
mod persistence;
mod rewind;

async fn create_plugin_state_session(table: &SessionTable) -> SessionInfo {
    table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            SessionOrigin::root(),
            false,
        )
        .await
        .expect("创建会话")
}

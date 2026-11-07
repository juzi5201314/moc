//! 历史页的大结果传输：冻结一次结果，逐块读取，原 `session/load_page` 契约不变。
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{SessionLoadPageParams, SessionLoadPageResult};
use crate::{ClientRequest, Method};

#[derive(Debug, Clone, Copy)]
pub struct SessionHistory;

impl Method for SessionHistory {
    const NAME: &'static str = "session/history";
    type Params = SessionHistoryParams;
    type Result = SessionHistoryResult;
}
impl ClientRequest for SessionHistory {}

/// 块偏移以冻结 JSON 的 UTF-8 字节计；同一偏移可重读，直到 close 或过期。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionHistoryParams {
    Open { page: SessionLoadPageParams },
    Read { transfer_id: String, offset: u64 },
    Close { transfer_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionHistoryResult {
    Page {
        page: Box<SessionLoadPageResult>,
    },
    /// data 按序拼接后反序列化为完整 SessionLoadPageResult；末块 next_offset 为 None。
    Chunk {
        transfer_id: String,
        offset: u64,
        data: String,
        next_offset: Option<u64>,
    },
    Closed,
}

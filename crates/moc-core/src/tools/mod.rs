//! 统一工具执行接口、上下文、结果及按模式筛选的注册表。

pub mod ask;
pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod job;
pub mod plan;
pub mod read;
pub mod todo;
pub mod write;
pub use job::JobManager;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::ResolvedConfig;
use crate::hashline::AnchorStore;
use crate::session::SessionTable;
use crate::session::state::SessionState;
use crate::session::store::SessionPaths;
use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::{RequestContext, SessionMode};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 工具风险类别；授权在工具执行入口统一处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Read,
    Write,
    Exec,
}

/// 成功、可继续的失败、或提交计划后的轮次终止。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolControl {
    #[default]
    Continue,
    EndTurn,
}

/// 一个工具的完整模型可见结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
    pub summary: Option<String>,
    pub control: ToolControl,
    pub status: Option<moc_protocol::methods::ToolStatus>,
    /// 文件变更的结构化 diff；仅 `edit` / `write` 成功时给出，随
    /// `SessionUpdate::ToolFinished` 透传给前端渲染。
    pub diff: Option<moc_protocol::methods::ToolDiff>,
}

impl ToolOutcome {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            summary: None,
            control: ToolControl::Continue,
            status: None,
            diff: None,
        }
    }

    pub fn failure(code: &str, message: impl AsRef<str>) -> Self {
        Self {
            content: format!("[{code}] {}", message.as_ref()),
            is_error: true,
            summary: None,
            control: ToolControl::Continue,
            status: None,
            diff: None,
        }
    }

    pub fn end_turn(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            summary: None,
            control: ToolControl::EndTurn,
            status: None,
            diff: None,
        }
    }
}

/// 工具边界上的结构化错误，转换为普通 tool_result 后可由模型修复。
#[derive(Debug, thiserror::Error)]
#[error("[{code}] {message}{hint}")]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    hint: String,
}

impl ToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: String::new(),
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = format!("\n提示：{}", hint.into());
        self
    }

    pub fn into_outcome(self) -> ToolOutcome {
        ToolOutcome {
            content: self.to_string(),
            is_error: true,
            summary: None,
            control: ToolControl::Continue,
            status: (self.code == "E_CANCELLED").then_some(moc_protocol::ToolStatus::Cancelled),
            diff: None,
        }
    }
}

/// 廉价克隆的每次工具调用上下文。
#[derive(Clone)]
pub struct ToolCtx {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: PathBuf,
    pub paths: SessionPaths,
    pub config: Arc<ResolvedConfig>,
    pub state: Arc<tokio::sync::Mutex<SessionState>>,
    pub anchors: Arc<AnchorStore>,
    pub request: RequestContext,
    pub sessions: Arc<SessionTable>,
    pub cancel: CancellationToken,
    pub jobs: Arc<job::JobManager>,
}

/// 工具实例由注册表持有；实现将同步 schema 与异步执行分开。
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn tier(&self) -> Tier;
    fn parallel_safe(&self) -> bool;
    fn def(&self) -> ToolDef;
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: vec![
                Arc::new(read::ReadTool),
                Arc::new(grep::GrepTool),
                Arc::new(find::FindTool),
                Arc::new(todo::TodoTool),
                Arc::new(job::JobTool),
                Arc::new(bash::BashTool),
                Arc::new(edit::EditTool),
                Arc::new(write::WriteTool),
                Arc::new(ask::AskTool),
                Arc::new(plan::SubmitPlanTool),
            ],
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.name() == name).cloned()
    }

    pub fn definitions(
        &self,
        mode: SessionMode,
        ask_supported: bool,
        config: &crate::config::ToolsConfig,
    ) -> Vec<ToolDef> {
        self.tools
            .iter()
            .filter(|tool| {
                let name = tool.name();
                (match mode {
                    SessionMode::Normal => name != "submit_plan",
                    SessionMode::Ask => matches!(
                        name,
                        "read" | "grep" | "find" | "bash" | "todo" | "job" | "ask"
                    ),
                    SessionMode::Plan => matches!(
                        name,
                        "read" | "grep" | "find" | "bash" | "todo" | "job" | "ask" | "submit_plan"
                    ),
                }) && (name != "ask" || ask_supported)
                    && (name != "todo" || config.todo.enabled)
                    && (name != "job" || config.job.enabled)
            })
            .map(|tool| {
                let mut definition = tool.def();
                definition.description = Some(tool.description().to_owned());
                definition
            })
            .collect()
    }

    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }
}

/// 保留输出首尾，并仅在截断标记中插入归档路径。
pub fn truncate_output_with_artifact(
    text: &str,
    head_bytes: usize,
    tail_bytes: usize,
    artifact_path: &Path,
) -> String {
    if text.len() <= head_bytes.saturating_add(tail_bytes) {
        return text.to_owned();
    }
    let head_end = floor_char_boundary(text, head_bytes.min(text.len()));
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_bytes));
    let omitted = tail_start.saturating_sub(head_end);
    format!(
        "{}\n[… {omitted} 字节已省略；完整输出：{}]\n{}",
        &text[..head_end],
        artifact_path.display(),
        &text[tail_start..]
    )
}

fn floor_char_boundary(text: &str, mut position: usize) -> usize {
    while !text.is_char_boundary(position) {
        position -= 1;
    }
    position
}

fn ceil_char_boundary(text: &str, mut position: usize) -> usize {
    while position < text.len() && !text.is_char_boundary(position) {
        position += 1;
    }
    position
}

fn schema(properties: Value, required: &[&str]) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

pub fn resolve_tool_path(cwd: &std::path::Path, value: &str) -> Result<PathBuf, ToolError> {
    let supplied = std::path::Path::new(value);
    let joined = if supplied.is_absolute() {
        supplied.to_owned()
    } else {
        cwd.join(supplied)
    };
    match std::fs::canonicalize(&joined) {
        Ok(path) => return Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ToolError::new("E_ACCESS", error.to_string())),
    }
    let parent = joined
        .parent()
        .ok_or_else(|| ToolError::new("E_ACCESS", "无效文件路径"))?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|error| ToolError::new("E_ACCESS", format!("无法访问父目录：{error}")))?;
    let filename = joined
        .file_name()
        .ok_or_else(|| ToolError::new("E_ACCESS", "无效文件名"))?;
    Ok(parent.join(filename))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_truncation_preserves_utf8_boundaries_and_both_edges() {
        let output =
            truncate_output_with_artifact("前中后尾外", 6, 6, Path::new("/tmp/output.txt"));
        assert!(output.starts_with("前中"));
        assert!(output.ends_with("尾外"));
        assert!(output.contains("已省略"));
    }

    #[test]
    fn truncation_does_not_rewrite_literal_file_content() {
        let text = "未归档：正文完整输出：未归档abcdefghijklmnopqrstuvwxyz未归档";
        let output = truncate_output_with_artifact(text, 48, 9, Path::new("/tmp/output.txt"));
        assert!(output.starts_with("未归档：正文完整输出：未归档"));
        assert!(output.ends_with("未归档"));
        assert_eq!(output.matches("/tmp/output.txt").count(), 1);
    }
}

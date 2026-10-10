//! 统一工具执行接口、上下文、结果及按模式筛选的注册表。

pub mod ask;
pub mod bash;
pub mod catalog;
pub mod edit;
pub mod find;
pub mod grep;
mod scope;
pub(crate) use scope::resolve_scan_root;
pub mod job;
pub(crate) mod job_log;
pub mod mcp;
pub mod output;
pub mod plan;
pub mod ptc;
pub mod read;
pub mod todo;
pub mod write;
pub use job::JobManager;
pub use output::ToolOutput;

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::ResolvedConfig;
use crate::hashline::AnchorStore;
use crate::session::SessionTable;
use crate::session::state::SessionState;
use crate::session::store::SessionPaths;
use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::{RequestContext, SessionMode, SessionPermission};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 工具风险类别；授权在工具执行入口统一处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Read,
    Write,
    Exec,
}

/// 成功、可继续的失败、或提交计划后的提示终止。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolControl {
    #[default]
    Continue,
    EndTurn,
}

/// 工具的原始输出来源与执行结果；模型可见文本由统一出口生成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub content: ToolOutput,
    pub is_error: bool,
    pub structured_content: Option<Value>,
    pub error_code: Option<String>,
    /// 结果携带的图片；仅 MCP 桥接工具会填充。
    pub images: Vec<moc_llm::ImageSource>,
    pub control: ToolControl,
    pub status: Option<moc_protocol::methods::ToolStatus>,
    /// 文件变更的结构化 diff；仅 `edit` / `write` 成功时给出，随
    /// `SessionUpdate::ToolFinished` 透传给前端渲染。
    pub diff: Option<moc_protocol::methods::ToolDiff>,
}

impl ToolOutcome {
    pub fn success(content: impl Into<ToolOutput>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            structured_content: None,
            error_code: None,
            images: Vec::new(),
            control: ToolControl::Continue,
            status: None,
            diff: None,
        }
    }

    pub fn failure(code: &str, message: impl AsRef<str>) -> Self {
        Self {
            content: format!("[{code}] {}", message.as_ref()).into(),
            is_error: true,
            structured_content: None,
            error_code: Some(code.to_owned()),
            images: Vec::new(),
            control: ToolControl::Continue,
            status: None,
            diff: None,
        }
    }

    pub fn end_prompt(content: impl Into<ToolOutput>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            structured_content: None,
            error_code: None,
            images: Vec::new(),
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
        self.hint = format!("\nHint: {}", hint.into());
        self
    }

    pub fn into_outcome(self) -> ToolOutcome {
        ToolOutcome {
            content: self.to_string().into(),
            is_error: true,
            structured_content: None,
            error_code: Some(self.code.to_owned()),
            images: Vec::new(),
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
    pub prompt_id: String,
    pub cwd: PathBuf,
    pub paths: SessionPaths,
    pub config: Arc<ResolvedConfig>,
    pub state: Arc<tokio::sync::Mutex<SessionState>>,
    pub anchors: Arc<AnchorStore>,
    pub request: RequestContext,
    pub sessions: Arc<SessionTable>,
    pub(crate) core: Arc<crate::service::CoreInner>,
    pub catalog: Arc<catalog::ToolCatalog>,
    pub mode: SessionMode,
    pub permission: SessionPermission,
    pub nested: bool,
    pub cancel: CancellationToken,
    pub jobs: Arc<job::JobManager>,
    /// 当前 workspace 的惰性 OpenViking client；未配置时为 None。
    pub openviking: Option<Arc<moc_openviking::OpenVikingClient>>,
    pub openviking_peer: Option<String>,
    /// 该项目的 MCP 管理器（空配置时为空管理器）。
    pub mcp: Arc<moc_mcp::McpManager>,
    /// 该项目的 skill 索引。
    pub skills: Arc<crate::skills::SkillIndex>,
    /// 当前正在执行的外层工具调用 ID；PTC 用它建立父子调用关系。
    pub tool_call_id: String,
    /// 本会话已加载的 skill 名。
    pub loaded_skills: Arc<parking_lot::RwLock<crate::skills::LoadedSkills>>,
}
impl ToolCtx {
    pub fn resource_context(&self) -> crate::resources::ResourceContext<'_> {
        crate::resources::ResourceContext {
            session_id: &self.session_id,
            cwd: &self.cwd,
            paths: &self.paths,
            skills: &self.skills,
            sessions: &self.sessions,
            openviking_configured: !self.config.openviking.url.trim().is_empty(),
        }
    }

    pub fn artifact_store(&self) -> crate::resources::ArtifactStore {
        crate::resources::ArtifactStore::new(self.session_id.clone(), self.paths.clone())
    }
}

/// 工具实例由注册表持有；实现将同步 schema 与异步执行分开。
///
/// `name` / `description` 返回动态 `&str`：MCP 桥接工具的标识来自运行期
/// 协商结果，不能是 `'static`；内置工具返回字面量即可自动兼容。
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn tier(&self) -> Tier;
    fn parallel_safe(&self) -> bool;
    fn def(&self) -> ToolDef;
    fn output_schema(&self) -> Option<&Value> {
        None
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>>;
}

#[derive(Default)]
pub struct ToolRegistry {
    builtins: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            builtins: vec![
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
                Arc::new(crate::tools::ptc::RunCodeTool),
                Arc::new(crate::skills::tool::SkillTool),
            ],
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.builtins
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
    }

    pub fn definitions(&self, ask_supported: bool, config: &ResolvedConfig) -> Vec<ToolDef> {
        self.builtins
            .iter()
            .filter(|tool| crate::service::mode::tool_enabled(tool.name(), ask_supported, config))
            .map(|tool| {
                let mut definition = tool.def();
                definition.description = Some(tool.description().to_owned());
                definition
            })
            .collect()
    }

    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.builtins
    }

    pub fn catalog(
        &self,
        snapshot: &moc_mcp::McpSnapshot,
        manager: Arc<moc_mcp::McpManager>,
    ) -> Result<catalog::ToolCatalog, ToolError> {
        catalog::ToolCatalog::new(self, snapshot, manager)
    }
}

pub(crate) fn schema(properties: Value, required: &[&str]) -> Value {
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
        .ok_or_else(|| ToolError::new("E_ACCESS", "invalid file path"))?;
    let parent = std::fs::canonicalize(parent).map_err(|error| {
        ToolError::new(
            "E_ACCESS",
            format!("cannot access parent directory: {error}"),
        )
    })?;
    let filename = joined
        .file_name()
        .ok_or_else(|| ToolError::new("E_ACCESS", "invalid filename"))?;
    Ok(parent.join(filename))
}

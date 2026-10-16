//! 统一工具执行接口、上下文、结果及按模式筛选的注册表。

pub mod agent;
pub mod ask;
pub mod bash;
pub mod catalog;
pub mod edit;
pub mod find;
pub mod grep;
mod scope;
pub(crate) use scope::resolve_scan_root;
pub(crate) mod clm;
pub mod job;
pub(crate) mod job_log;
pub mod mcp;
pub mod output;
pub mod plan;
pub mod ptc;
pub mod read;
pub mod todo;
pub mod websearch;
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
    pub allow_agents: bool,
    /// 当前上下文是否子代理；决定 `yield` 是否可用。
    pub subagent: bool,
    pub nested: bool,
    pub cancel: CancellationToken,
    pub(crate) guard: Arc<crate::service::prompt::guard::PromptGuard>,
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
        crate::resources::ArtifactStore::new(self.paths.clone())
    }
}

/// 工具实例由注册表持有；实现将同步 schema 与异步执行分开。
///
/// `name` / `description` 返回动态 `&str`：MCP 桥接工具的标识来自运行期
/// 协商结果，不能是 `'static`；内置工具返回字面量即可自动兼容。
///
/// `description`、schema 字段说明与结果文本都是模型可见提示词，编写遵循
/// [`crate::system_prompt`] 模块文档的「提示词编写规范」：只写该工具自己的用法，
/// 不复述工具政策或 schema 已表达的约束。
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

/// 工具可见性范围：同一个 registry 在顶层会话与子代理下的差异。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolScope {
    /// 是否允许创建、停止与等待子代理。
    pub control_agents: bool,
    /// 是否是子代理上下文（`yield` 只在子代理可用）。
    pub subagent: bool,
}

impl ToolScope {
    /// 顶层会话：全部代理控制工具可用。
    pub fn top() -> Self {
        Self {
            control_agents: true,
            subagent: false,
        }
    }
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
                Arc::new(write::WriteTool),
                Arc::new(bash::BashTool),
                Arc::new(edit::EditTool),
                Arc::new(ask::AskTool),
                Arc::new(agent::SpawnAgentTool),
                Arc::new(agent::ListAgentsTool),
                Arc::new(agent::MessageAgentTool),
                Arc::new(agent::StopAgentTool),
                Arc::new(agent::WaitAgentTool),
                Arc::new(agent::YieldTool),
                Arc::new(plan::WritePlanTool),
                Arc::new(plan::ExitPlanTool),
                Arc::new(clm::ClmContextTool),
                Arc::new(crate::tools::ptc::RunCodeTool),
                Arc::new(crate::skills::tool::SkillTool),
                Arc::new(websearch::WebSearchTool),
                Arc::new(websearch::WebFetchTool),
            ],
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.builtins
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
    }

    pub fn definitions(
        &self,
        mode: SessionMode,
        ask_supported: bool,
        config: &ResolvedConfig,
        scope: ToolScope,
    ) -> Vec<ToolDef> {
        self.builtins
            .iter()
            .filter(|tool| match tool.name() {
                // 创建与回收子代理只在顶层会话可用；子代理只能通信。
                name if Self::is_parent_only_tool(name) => scope.control_agents,
                // 交付终态只对子代理有意义：顶层会话没有父代理可交付。
                "yield" => scope.subagent,
                _ => true,
            })
            .filter(|tool| {
                crate::service::mode::mode_allows(mode, tool.name(), tool.tier())
                    && crate::service::mode::tool_enabled(tool.name(), ask_supported, config)
            })
            .map(|tool| {
                let mut definition = tool.def();
                definition.description = Some(tool.description().to_owned());
                definition
            })
            .collect()
    }

    /// 创建/回收/等待子代理的工具；子代理上下文一律剔除。
    fn is_parent_only_tool(name: &str) -> bool {
        matches!(name, "spawn_agent" | "stop_agent" | "wait_agent")
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

/// 写目标越出工作区时的解析结果：返回真实的写入落点。
///
/// 已存在的目标直接 canonicalize；尚未存在的目标展开最近存在祖先的符号链接，
/// 再把剩余组件按词法折叠成落点，因此工作区内的符号链接与尾段 `..` 都不能
/// 把写入引到工作区外。返回 `None` 表示目标在工作区内或无法解析。
pub fn workspace_escape(
    cwd: &std::path::Path,
    value: impl AsRef<std::path::Path>,
) -> Option<PathBuf> {
    let supplied = value.as_ref();
    // OpenViking 写入是远程写入，不属于会话工作区：按越界处理并在工作区权限下
    // 走一次性授权。只读的 MOC 内部 URI 由 ResourceRouter 直接拒绝写入。
    if let Some(text) = supplied.to_str()
        && crate::resources::is_writable_resource_uri(text)
    {
        return Some(supplied.to_owned());
    }
    let root = std::fs::canonicalize(cwd).ok()?;
    let joined = if supplied.is_absolute() {
        supplied.to_owned()
    } else {
        cwd.join(supplied)
    };
    let landing = resolve_landing(&joined)?;
    (!landing.starts_with(&root)).then_some(landing)
}

/// canonicalize 路径中已存在的最深前缀，再按文件系统语义词法折叠剩余组件。
///
/// 尾段组件在写入前不存在，其中的 `..` 只会抵消这些待创建组件本身，因此词法
/// 折叠与 `create_dir_all` 之后的真实落点一致；已存在部分的符号链接由
/// canonicalize 展开，不能借链接逃出工作区。
fn resolve_landing(joined: &std::path::Path) -> Option<PathBuf> {
    let mut existing = joined;
    loop {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            let tail = joined.strip_prefix(existing).ok()?;
            let mut landing = resolved;
            for component in tail.components() {
                match component {
                    std::path::Component::Normal(part) => landing.push(part),
                    std::path::Component::ParentDir => {
                        landing.pop();
                    }
                    std::path::Component::CurDir => {}
                    std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                        return None;
                    }
                }
            }
            return Some(landing);
        }
        existing = existing.parent()?;
    }
}

/// 写入路径的工作区边界复校验：授权入口之外的调用不得越界写入。
///
/// 与授权阶段共用 [`workspace_escape`]，保证判定与解析同源；`full` 权限
/// （yolo，或本次调用已获一次性授权）不做限制。
pub fn ensure_workspace_write(
    cwd: &std::path::Path,
    permission: SessionPermission,
    value: impl AsRef<std::path::Path>,
) -> Result<(), ToolError> {
    if permission != SessionPermission::Workspace {
        return Ok(());
    }
    let Some(landing) = workspace_escape(cwd, value) else {
        return Ok(());
    };
    Err(ToolError::new(
        "E_PERMISSION_DENIED",
        format!(
            "{} is outside the session workspace and was not approved; approve the write from an interactive frontend or switch the session permission",
            landing.display()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(directory: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let root = directory.path().join("work");
        let outside = directory.path().join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        (
            root.canonicalize().unwrap(),
            outside.canonicalize().unwrap(),
        )
    }

    #[test]
    fn workspace_escape_only_reports_targets_outside_the_root() {
        let directory = tempfile::tempdir().unwrap();
        let (root, outside) = workspace(&directory);
        assert!(workspace_escape(&root, "src/main.rs").is_none());
        assert!(workspace_escape(&root, "src/new/deep.rs").is_none());
        assert!(workspace_escape(&root, "src/../src/main.rs").is_none());
        assert!(workspace_escape(&root, ".").is_none());
        let absolute = workspace_escape(&root, &outside.join("notes.txt").display().to_string())
            .expect("绝对路径越界");
        assert_eq!(absolute, outside.join("notes.txt"));
        let parent = workspace_escape(&root, "../outside/notes.txt").expect("相对路径越界");
        assert_eq!(parent, outside.join("notes.txt"));
    }

    #[test]
    fn unresolved_parent_components_cannot_hide_an_escape() {
        let directory = tempfile::tempdir().unwrap();
        let (root, _) = workspace(&directory);
        // 尾段组件尚不存在，`..` 会在 create_dir_all 补出组件后再折叠：落点必须
        // 按词法折叠判定，不能把带 `..` 的字面路径当作工作区内。
        for value in ["z/../../x", "y/deep/../../../../x"] {
            let escape = workspace_escape(&root, value).expect("尾段 `..` 必须判为越界");
            assert!(
                !escape.starts_with(&root),
                "{value} -> {}",
                escape.display()
            );
        }
        let absolute = root.join("z/../../x").display().to_string();
        let escape = workspace_escape(&root, &absolute).expect("绝对路径变体必须判为越界");
        assert_eq!(escape, directory.path().join("x"));
        // 对照组语义不变：已存在组件之上的 `..` 与其他越界仍然拒绝，区内路径不受影响。
        assert!(workspace_escape(&root, "src/new/deep.rs").is_none());
        assert_eq!(
            workspace_escape(&root, "src/../../evil2.txt").expect("已存在组件上的 `..` 越界"),
            directory.path().join("evil2.txt")
        );
        assert_eq!(
            workspace_escape(&root, "../outside/notes.txt").expect("父级相对路径越界"),
            directory.path().join("outside/notes.txt")
        );
    }

    #[test]
    fn unapproved_writes_outside_the_workspace_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let (root, outside) = workspace(&directory);
        let error =
            ensure_workspace_write(&root, SessionPermission::Workspace, "z/../../x").unwrap_err();
        assert_eq!(error.code, "E_PERMISSION_DENIED");
        assert!(error.message.contains("outside the session workspace"));
        assert!(
            ensure_workspace_write(
                &root,
                SessionPermission::Workspace,
                outside.join("absolute.txt")
            )
            .is_err()
        );
        // full 权限（yolo 或本次调用已获授权）与工作区内的目标不受限制。
        assert!(ensure_workspace_write(&root, SessionPermission::Full, "z/../../x").is_ok());
        assert!(
            ensure_workspace_write(&root, SessionPermission::Workspace, "src/new/deep.rs").is_ok()
        );
        assert!(
            ensure_workspace_write(
                &root,
                SessionPermission::Workspace,
                root.join("src/main.rs")
            )
            .is_ok()
        );
        // OpenViking 写入是远程写入：工作区权限下按越界处理，需要一次性授权。
        for uri in ["viking://~/notes/memory.md", "ov://repo/notes.md"] {
            assert!(workspace_escape(&root, uri).is_some(), "{uri}");
            assert!(
                ensure_workspace_write(&root, SessionPermission::Workspace, uri).is_err(),
                "{uri}"
            );
            assert!(ensure_workspace_write(&root, SessionPermission::Full, uri).is_ok());
        }
        // 只读的 MOC 内部 URI 不在此列：写入由 ResourceRouter 直接拒绝。
        assert!(workspace_escape(&root, "artifact://artifacts/out.txt").is_none());
        assert!(workspace_escape(&root, "plain://dir/file").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_escape_follows_symlinks_out_of_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let (root, outside) = workspace(&directory);
        std::fs::write(outside.join("target.txt"), "outside\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("dir-link")).unwrap();
        std::os::unix::fs::symlink(outside.join("target.txt"), root.join("file-link")).unwrap();
        // 目录链接下新建文件、以及指向文件外目标的链接，都必须判为越界。
        let escaped = workspace_escape(&root, "dir-link/new.txt").expect("目录链接逃逸");
        assert_eq!(escaped, outside.join("new.txt"));
        let file = workspace_escape(&root, "file-link").expect("文件链接逃逸");
        assert_eq!(file, outside.join("target.txt"));
        // 工作区内的链接仍可正常写入，不产生误报。
        std::os::unix::fs::symlink(root.join("src"), root.join("internal-link")).unwrap();
        assert!(workspace_escape(&root, "internal-link/main.rs").is_none());
    }
}

//! 工具的公共接口：执行上下文、结果类型，以及按模式筛选的注册表。

pub mod agent;
pub mod ask;
pub mod bash;
pub mod catalog;
pub mod declarations;
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
pub mod plugin;
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
use crate::sandbox::{SandboxGrants, SandboxPolicy, SandboxScratch, SandboxSupport};
use crate::session::SessionTable;
use crate::session::state::SessionState;
use crate::session::store::SessionPaths;
use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::{RequestContext, SessionMode, SessionPermission};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 工具的来源：决定模式白名单与结果归属，也用于拒绝插件嵌套调用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOrigin {
    Builtin,
    Mcp,
    Plugin,
}

/// 工具的风险等级；授权统一在工具执行入口处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Read,
    Write,
    Exec,
}

/// 工具执行后提示是否继续：`Continue` 照常进入下一轮，`EndTurn` 结束本轮（如提交计划后）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolControl {
    #[default]
    Continue,
    EndTurn,
}

/// 工具的原始输出与执行状态；模型看到的文本由统一的出口生成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub content: ToolOutput,
    pub is_error: bool,
    pub structured_content: Option<Value>,
    pub error_code: Option<String>,
    /// 结果附带的图片；只有 MCP 桥接工具会填。
    pub images: Vec<moc_llm::ImageSource>,
    pub control: ToolControl,
    pub status: Option<moc_protocol::methods::ToolStatus>,
    /// 文件变更的结构化 diff。只在 `edit`/`write` 成功时给出，经
    /// `SessionUpdate::ToolFinished` 原样交给前端渲染。
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

/// 工具层的结构化错误；转成普通 tool_result 后模型可以据此修正。
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

/// 单次工具调用的上下文，克隆开销很小。
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
    /// 会话方言：决定工具可达面（schema 与可调集合）。
    pub mode: SessionMode,
    /// 本次提示的执行契约（plan 叠加态 / ask 单次契约）：只收紧执行许可。
    pub contract: crate::service::mode::ModeContract,
    pub permission: SessionPermission,
    pub allow_agents: bool,
    /// 当前是否在子代理里；决定 `yield` 是否可用。
    pub subagent: bool,
    pub nested: bool,
    pub cancel: CancellationToken,
    pub(crate) guard: Arc<crate::service::prompt::guard::PromptGuard>,
    pub jobs: Arc<job::JobManager>,
    /// 该项目的 MCP 管理器（没有配置时是空管理器）。
    pub mcp: Arc<moc_mcp::McpManager>,
    /// 该项目的 skill 索引。
    pub skills: Arc<crate::skills::SkillIndex>,
    /// 该项目的插件宿主（没有插件时是空宿主）。
    pub plugins: Arc<moc_plugin::PluginHost>,
    /// 本次提示使用的模型选择器，供插件上下文读取。
    pub model: String,
    /// 当前正在执行的外层工具调用 ID；PTC 用它关联父子调用。
    pub tool_call_id: String,
    /// 本会话已加载的 skill 名。
    pub loaded_skills: Arc<parking_lot::RwLock<crate::skills::LoadedSkills>>,
    /// 当前平台的沙盒能力。
    pub(crate) sandbox_support: SandboxSupport,
    /// 本会话累积的沙盒路径/命令授权。
    pub(crate) sandbox_grants: Arc<parking_lot::RwLock<SandboxGrants>>,
    /// 本次工具调用新获得的授权；父代理与子代理默认不继承。
    pub(crate) call_grants: SandboxGrants,
    /// 会话级 scratch；创建失败时为 `None`，非 full 模式会报错。
    pub(crate) sandbox_scratch: Option<Arc<SandboxScratch>>,
}
impl ToolCtx {
    /// 用当前会话模式、配置、会话授权与本次调用授权构造沙盒策略。
    pub(crate) fn sandbox_policy(&self) -> SandboxPolicy {
        let session = self.sandbox_grants.read();
        let mut policy = SandboxPolicy::new(
            self.permission,
            self.cwd.clone(),
            &self.config.sandbox,
            &session,
            &self.call_grants,
            self.sandbox_scratch.clone(),
        );
        policy.overlay_home &= self.sandbox_support.home_overlay;
        if self.paths.compressed() {
            policy.inherit_read_denials(
                &self.paths.transcript,
                &crate::session::log::history_view_path(&self.paths),
            );
        }
        policy
    }
    pub fn resource_context(&self) -> crate::resources::ResourceContext<'_> {
        crate::resources::ResourceContext {
            session_id: &self.session_id,
            cwd: &self.cwd,
            paths: &self.paths,
            skills: &self.skills,
            sessions: &self.sessions,
        }
    }

    pub fn artifact_store(&self) -> crate::resources::ArtifactStore {
        crate::resources::ArtifactStore::new(self.paths.clone())
    }

    /// 交给插件的会话快照；字段与 `moc.d.ts` 的 `Session` 一一对应。
    ///
    /// `mode` 沿用旧词表：契约激活时报契约名（`ask`/`plan`），否则报方言名——
    /// 插件的 `modes` 声明与契约同词表，`writable` 反映契约是否只读。
    pub fn plugin_call_context(&self) -> moc_plugin::CallContext {
        let mode = match self.contract {
            crate::service::mode::ModeContract::None => serde_json::to_value(self.mode)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default(),
            contract => contract.name().to_owned(),
        };
        moc_plugin::CallContext {
            session_id: self.session_id.clone(),
            cwd: self.cwd.display().to_string(),
            mode,
            permission: self.permission.as_str().to_owned(),
            model: self.model.clone(),
            is_subagent: self.subagent,
            writable: !self.contract.read_only(),
        }
    }
}

/// 工具实现。注册表持有工具实例；schema 同步生成，执行是异步的。
///
/// `name`/`description` 返回非 `'static` 的 `&str`：MCP 桥接工具的名字来自运行时
/// 协商；内置工具直接返回字面量即可。
///
/// `description`、schema 字段说明和结果文本都是模型可见的提示词，按
/// [`crate::system_prompt`] 模块文档里的「提示词编写规范」编写：只写这个工具自己的
/// 用法，不重复工具政策，也不重复 schema 已经表达的约束。
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn tier(&self) -> Tier;
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    /// 是否出现在方言工具面（schema/声明）里：只归方言，契约切换不重写工具表。
    /// 默认等价于无契约的执行面；插件工具只按方言判定，契约掩码在执行许可检查。
    fn declared_in(&self, mode: SessionMode) -> bool {
        self.permitted_in(mode, crate::service::mode::ModeContract::None)
    }
    /// 方言 + 契约下是否允许执行。默认按内置白名单与风险等级判定；插件工具用
    /// 自己声明的 `modes`（契约词表）覆盖契约层。
    fn permitted_in(
        &self,
        mode: SessionMode,
        contract: crate::service::mode::ModeContract,
    ) -> bool {
        crate::service::mode::mode_allows(mode, contract, self.name(), self.tier(), self.origin())
    }
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

/// 工具可见范围：同一个注册表在顶层会话和子代理里暴露的工具不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolScope {
    /// 是否允许创建、停止和等待子代理。
    pub control_agents: bool,
    /// 是否在子代理里（`yield` 只对子代理开放）。
    pub subagent: bool,
}

impl ToolScope {
    /// 顶层会话：所有代理控制工具都可用。
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

    /// 模型可见的工具定义：内置工具 + 调用方传入的插件工具。
    ///
    /// 工具面只随方言变化（契约不动 schema，保障请求前缀缓存稳定）；方言判定走
    /// [`Tool::declared_in`]，配置开关与代理范围另外过滤。
    pub fn definitions(
        &self,
        mode: SessionMode,
        ask_supported: bool,
        config: &ResolvedConfig,
        scope: ToolScope,
        extra: &[Arc<dyn Tool>],
    ) -> Vec<ToolDef> {
        self.builtins
            .iter()
            .chain(extra.iter())
            .filter(|tool| match tool.name() {
                // 创建、停止和等待子代理只在顶层会话可用；子代理只能通信。
                name if Self::is_parent_only_tool(name) => scope.control_agents,
                // 交付最终结果只对子代理有意义：顶层会话没有父代理可交付。
                "yield" => scope.subagent,
                _ => true,
            })
            .filter(|tool| {
                tool.declared_in(mode)
                    && crate::service::mode::tool_enabled(
                        tool.name(),
                        tool.origin(),
                        ask_supported,
                        config,
                    )
            })
            .map(|tool| {
                let mut definition = tool.def();
                if definition.description.is_none() {
                    definition.description = Some(tool.description().to_owned());
                }
                definition
            })
            .collect()
    }

    /// 创建、停止、等待子代理的工具；子代理里全部去掉。
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
        plugins: Vec<Arc<dyn Tool>>,
    ) -> Result<catalog::ToolCatalog, ToolError> {
        catalog::ToolCatalog::new(self, snapshot, manager, plugins)
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

/// 写入目标越出工作区时，返回实际会写到的路径。
///
/// 已存在的目标直接 canonicalize。不存在的目标先展开最近一个已存在祖先上的符号链接，
/// 再按词法折叠剩余部分，所以工作区内的符号链接和末尾的 `..` 都不能把写入引到工作区外。
/// 返回 `None` 表示目标在工作区内或无法解析。
pub fn workspace_escape(
    cwd: &std::path::Path,
    value: impl AsRef<std::path::Path>,
) -> Option<PathBuf> {
    let supplied = value.as_ref();
    let root = std::fs::canonicalize(cwd).ok()?;
    let joined = if supplied.is_absolute() {
        supplied.to_owned()
    } else {
        cwd.join(supplied)
    };
    let landing = crate::sandbox::resolve_landing(&joined)?;
    (!landing.starts_with(&root)).then_some(landing)
}

/// 执行阶段复校验：授权入口与工具执行共用同一份沙盒策略。
pub(crate) fn ensure_path_access(
    ctx: &ToolCtx,
    value: &str,
    access: crate::sandbox::Access,
) -> Result<(), ToolError> {
    if ctx.permission == SessionPermission::Full || crate::resources::is_resource_uri(value) {
        return Ok(());
    }
    let path = std::path::Path::new(value);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd.join(path)
    };
    match ctx.sandbox_policy().check(&path, access) {
        crate::sandbox::Decision::Allow => Ok(()),
        crate::sandbox::Decision::Ask | crate::sandbox::Decision::Deny => Err(ToolError::new(
            "E_SANDBOX_DENIED",
            format!(
                "[sandbox: file access denied under {} mode] {} is outside the allowed roots and was not approved",
                ctx.permission.as_str(),
                path.display()
            ),
        )),
    }
}

/// 执行写入前再校验一次工作区边界，防止绕过授权入口的调用越界写入。
///
/// `full` 不受限制；`workspace` 与 `workspace-only` 都拒绝工作区外目标。生产路径使用
/// [`ensure_path_access`]，本函数保留给只检查工作区边界的调用方和测试。
pub fn ensure_workspace_write(
    cwd: &std::path::Path,
    permission: SessionPermission,
    value: impl AsRef<std::path::Path>,
) -> Result<(), ToolError> {
    if permission == SessionPermission::Full {
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
        let absolute = workspace_escape(&root, outside.join("notes.txt").display().to_string())
            .expect("绝对路径越界");
        assert_eq!(absolute, outside.join("notes.txt"));
        let parent = workspace_escape(&root, "../outside/notes.txt").expect("相对路径越界");
        assert_eq!(parent, outside.join("notes.txt"));
    }

    #[test]
    fn unresolved_parent_components_cannot_hide_an_escape() {
        let directory = tempfile::tempdir().unwrap();
        let (root, _) = workspace(&directory);
        // 末尾的组件还不存在，`..` 要等 create_dir_all 补出组件后才折叠：必须按词法折叠
        // 后的结果判断，不能因为字面路径带 `..` 就当它在工作区内。
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
        // 对照组：已存在组件上的 `..` 和其它越界照样拒绝，工作区内的路径不受影响。
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
        assert!(
            ensure_workspace_write(
                &root,
                SessionPermission::WorkspaceOnly,
                outside.join("absolute.txt")
            )
            .is_err(),
            "workspace-only 也必须拒绝工作区外写入"
        );
        // full 权限（yolo 或本次调用已获授权）以及工作区内的目标都不受限制。
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
        // 只读的 MOC 内部 URI 不算越界：写入会被 ResourceRouter 直接拒绝。
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
        // 在目录链接下新建文件、写指向外部文件的链接，都必须判为越界。
        let escaped = workspace_escape(&root, "dir-link/new.txt").expect("目录链接逃逸");
        assert_eq!(escaped, outside.join("new.txt"));
        let file = workspace_escape(&root, "file-link").expect("文件链接逃逸");
        assert_eq!(file, outside.join("target.txt"));
        // 工作区内的链接照常写入，不能误报。
        std::os::unix::fs::symlink(root.join("src"), root.join("internal-link")).unwrap();
        assert!(workspace_escape(&root, "internal-link/main.rs").is_none());
    }
}

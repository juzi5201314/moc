//! 统一工具执行接口、上下文、结果及按模式筛选的注册表。

pub mod ask;
pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod job;
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
        self.hint = format!("\n提示：{}", hint.into());
        self
    }

    pub fn into_outcome(self) -> ToolOutcome {
        ToolOutcome {
            content: self.to_string().into(),
            is_error: true,
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
    pub(crate) ptc_slots: Arc<tokio::sync::Semaphore>,
    pub(crate) runtime: Arc<crate::service::SessionRuntime>,
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
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>>;
}

#[derive(Default)]
pub struct ToolRegistry {
    builtins: Vec<Arc<dyn Tool>>,
    /// 动态工具（MCP 桥接）；每次提示请求前由 core 按最新快照整体替换。
    dynamic: parking_lot::RwLock<Vec<Arc<dyn Tool>>>,
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
            dynamic: parking_lot::RwLock::new(Vec::new()),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.builtins
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
            .or_else(|| {
                self.dynamic
                    .read()
                    .iter()
                    .find(|tool| tool.name() == name)
                    .cloned()
            })
    }

    /// 替换动态工具集；传入前须按名字排序以保证请求内顺序稳定。
    pub fn set_dynamic(&self, tools: Vec<Arc<dyn Tool>>) {
        *self.dynamic.write() = tools;
    }

    pub fn definitions(
        &self,
        mode: SessionMode,
        ask_supported: bool,
        config: &ResolvedConfig,
    ) -> Vec<ToolDef> {
        let mut definitions: Vec<ToolDef> = self
            .builtins
            .iter()
            .filter(|tool| {
                let name = tool.name();
                (match mode {
                    SessionMode::Normal => name != "submit_plan",
                    SessionMode::Ask => matches!(
                        name,
                        "read" | "grep" | "find" | "bash" | "todo" | "job" | "ask" | "skill"
                    ),
                    SessionMode::Plan => matches!(
                        name,
                        "read"
                            | "grep"
                            | "find"
                            | "bash"
                            | "todo"
                            | "job"
                            | "ask"
                            | "skill"
                            | "submit_plan"
                    ),
                    SessionMode::Ptc => name == "run_code",
                }) && (name != "ask" || ask_supported)
                    && (name != "todo" || config.tools.todo.enabled)
                    && (name != "job" || config.tools.job.enabled)
                    && (name != "skill" || config.skills.enabled)
            })
            .map(|tool| {
                let mut definition = tool.def();
                definition.description = Some(tool.description().to_owned());
                definition
            })
            .collect();
        // 动态（MCP）工具：Normal 全量放行；Ask/Plan 只保留只读工具。
        let dynamic = self.dynamic.read();
        definitions.extend(
            dynamic
                .iter()
                .filter(|tool| match mode {
                    SessionMode::Normal => true,
                    SessionMode::Ask | SessionMode::Plan => tool.tier() == Tier::Read,
                    SessionMode::Ptc => false,
                })
                .map(|tool| {
                    let mut definition = tool.def();
                    definition.description = Some(tool.description().to_owned());
                    definition
                }),
        );
        definitions
    }

    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.builtins
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
        .ok_or_else(|| ToolError::new("E_ACCESS", "无效文件路径"))?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|error| ToolError::new("E_ACCESS", format!("无法访问父目录：{error}")))?;
    let filename = joined
        .file_name()
        .ok_or_else(|| ToolError::new("E_ACCESS", "无效文件名"))?;
    Ok(parent.join(filename))
}

#[cfg(test)]
mod dynamic_registry_tests {
    use super::*;
    use moc_protocol::methods::SessionMode;
    use std::sync::Arc;

    struct FakeTool {
        name: String,
        read_only: bool,
    }

    impl Tool for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "动态工具"
        }
        fn tier(&self) -> Tier {
            if self.read_only {
                Tier::Read
            } else {
                Tier::Exec
            }
        }
        fn parallel_safe(&self) -> bool {
            self.read_only
        }
        fn def(&self) -> ToolDef {
            ToolDef::new(self.name.clone(), serde_json::json!({"type": "object"}))
        }
        fn run<'a>(
            &'a self,
            _ctx: &'a ToolCtx,
            _args: Value,
        ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
            unreachable!("registry 测试不执行动态工具")
        }
    }

    fn config() -> crate::config::ResolvedConfig {
        // 仅供 definitions() 计数测试使用的最小配置。
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let raw = "[models.roles]\ndefault='p/model'\nsmol='p/model'\nslow='p/model'\nplan='p/model'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\n[providers.p.models.model]\n";
        std::fs::write(home.path().join("config.toml"), raw).unwrap();
        let core = crate::CoreConfig {
            home: home.path().to_owned(),
            cwd: cwd.path().to_owned(),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
        };
        crate::ResolvedConfig::load(&core)
            .unwrap()
            .for_project(cwd.path())
            .unwrap()
    }

    #[test]
    fn dynamic_tools_are_resolvable_and_mode_filtered() {
        let registry = ToolRegistry::new();
        registry.set_dynamic(vec![
            Arc::new(FakeTool {
                name: "mcp__github__edit_file".into(),
                read_only: false,
            }),
            Arc::new(FakeTool {
                name: "mcp__github__list_issues".into(),
                read_only: true,
            }),
        ]);

        assert!(registry.get("mcp__github__list_issues").is_some());
        assert!(registry.get("mcp__github__edit_file").is_some());
        assert!(registry.get("read").is_some());

        let normal = registry.definitions(SessionMode::Normal, true, &config());
        assert_eq!(normal.len(), 13, "11 内置（无 submit_plan）+ 2 动态");
        let ptc = registry.definitions(SessionMode::Ptc, true, &config());
        assert_eq!(ptc.len(), 1);
        assert_eq!(ptc[0].name, "run_code");

        let ask = registry.definitions(SessionMode::Ask, true, &config());
        assert_eq!(ask.len(), 9, "Ask 模式只放行只读动态工具");
        assert!(ask.iter().any(|d| d.name == "mcp__github__list_issues"));
        assert!(!ask.iter().any(|d| d.name == "mcp__github__edit_file"));

        // 内置工具不受动态替换影响。
        registry.set_dynamic(Vec::new());
        assert!(registry.get("mcp__github__list_issues").is_none());
        assert_eq!(
            registry
                .definitions(SessionMode::Normal, true, &config())
                .len(),
            11
        );
    }
}

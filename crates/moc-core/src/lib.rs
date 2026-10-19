//! moc-core：会话持久化、工具执行和提示编排。
//!
//! [`moc_protocol`] 是稳定的前端 JSON-RPC 边界，[`moc_llm`] 负责兼容上游协议；
//! 本 crate 管理会话状态、工具和上下文的生命周期。

pub(crate) mod agents;
pub mod chart;
pub mod clm;
pub mod compact;
mod concurrency;
pub mod config;
mod error;
pub mod hashline;
pub mod ids;
mod paths;
pub mod resources;
pub mod service;
pub mod session;
pub mod shell;
pub mod skills;
pub mod system_prompt;
pub mod tools;
pub mod websearch;
pub mod workspace;
pub use service::Core;

pub use config::model::{ModelCapabilities, ResolvedModel, ThinkingLevel};
pub use config::{ResolvedConfig, resolve_models};
pub use error::CoreError;

/// [`CoreConfig`] 未显式提供时使用的路径与连接配置。
#[derive(Clone)]
pub struct CoreConfig {
    /// 全局配置、会话与 sidecar 所在目录（`$MOC_HOME` 或 `~/.moc`）。
    pub home: std::path::PathBuf,
    /// `session/new` 未指定工作目录时使用的进程当前目录。
    pub cwd: std::path::PathBuf,
    /// 按给定顺序合并的命令行配置覆盖层。
    pub overlays: Vec<std::path::PathBuf>,
    /// 会话存储目录覆盖，用于 CLI `--session-dir`。
    pub session_dir: Option<std::path::PathBuf>,
    /// 可注入的 LLM 客户端。
    pub client: moc_llm::Client,
    /// `run_code` 使用的独立 QuickJS worker 可执行文件；未配置时 PTC 不可用。
    pub ptc_worker: Option<PtcWorkerCommand>,
}

/// 启动 `moc-ptc-worker` 的命令。CLI 默认指向同目录的 worker 二进制。
#[derive(Clone, Debug)]
pub struct PtcWorkerCommand {
    pub program: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        let home = std::env::var_os("MOC_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| crate::paths::home_dir().map(|path| path.join(".moc")))
            .unwrap_or_else(|| std::path::PathBuf::from(".moc"));
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        Self {
            home,
            cwd,
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
            ptc_worker: None,
        }
    }
}

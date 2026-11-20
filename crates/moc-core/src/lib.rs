//! moc-core：会话持久化、工具执行和提示编排。
//!
//! [`moc_protocol`] 是稳定的前端 JSON-RPC 边界，[`moc_llm`] 负责兼容上游协议；
//! 本 crate 管理会话状态、工具和上下文的生命周期。

// 本地化 catalog 编译期内嵌；i18n! 必须展开在 crate 根，`t!` 才能经 crate::_rust_i18n_t 解析。
rust_i18n::i18n!("../../locales", fallback = "zh");

// 单元测试复用集成测试的 RPC harness，并保留对内部同步点的访问。
#[cfg(test)]
extern crate self as moc_core;

pub(crate) mod agents;
pub mod chart;
pub mod clm;
pub mod compact;
mod concurrency;
pub mod config;
mod error;
pub mod fs;
pub(crate) mod goal;
pub mod hashline;
pub mod i18n;
pub mod ids;
pub mod limits;
pub mod memory;
mod paths;
pub mod procstats;
pub mod resources;
pub mod sandbox;
pub mod service;
pub mod session;
pub mod shell;
pub mod skills;
pub mod system_prompt;
mod tips;
pub mod tools;
pub mod webfetch;
pub mod workflow;
pub use service::{Core, CoreServer};

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
    /// `.agents` 用户级约定（`~/.agents/mcp.json`、`~/.agents/skills`）的挂靠
    /// 目录。`None` 取真实用户主目录；测试注入隔离目录，避免扫到宿主机环境。
    pub user_home: Option<std::path::PathBuf>,
    /// 按给定顺序合并的命令行配置覆盖层。
    pub overlays: Vec<std::path::PathBuf>,
    /// 会话存储目录覆盖，用于 CLI `--session-dir`。
    pub session_dir: Option<std::path::PathBuf>,
    /// 可注入的 LLM 客户端。
    pub client: moc_llm::Client,
    /// `execute` 使用的独立 QuickJS worker 可执行文件；未配置时 PTC 不可用。
    pub ptc_worker: Option<PtcWorkerCommand>,
}

/// 启动 PTC worker 的命令。CLI 默认复用当前 `moc` 并传入隐藏 worker 参数。
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
            user_home: None,
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
            ptc_worker: None,
        }
    }
}

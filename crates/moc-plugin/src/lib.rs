//! moc 插件系统：本地目录发现、QuickJS 运行时与宿主桥接。
//!
//! 插件是本地目录里的 ES 模块，由宿主在专用线程内用 QuickJS 执行；对外契约见
//! `docs/plugins.md` 与 `js/moc.d.ts`。本 crate 不依赖 moc 的协议与工具类型，
//! core 只需提供 [`host::CallContext`] 与一个 [`host::HostBridge`]。

pub mod descriptor;
pub mod discovery;
pub mod error;
pub mod host;
pub mod loader;
pub mod manifest;
pub mod runtime;
pub mod schema;

pub use descriptor::{
    AgentPresetDecl, CommandDecl, ConfigDecl, ConfigKind, Descriptor, HookKind, ModeMask, ToolDecl,
};
pub use error::{PluginError, Result};
pub use host::{
    AfterToolCall, BeforeToolCall, CallContext, HostBridge, InjectedMessage, MessageRewrite,
    PersistenceBridge, PersistenceOp, PluginHost, PluginInfo, PluginNotice, Settings, ToolResult,
    fingerprint,
};
pub use manifest::Manifest;
pub use runtime::API_VERSION;

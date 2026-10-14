//! moc-tui：moc 的终端交互前端。
//!
//! 基于 ratatui + crossterm（与 grok-build 同款技术栈），内嵌 core 走与
//! 其他前端完全相同的 JSON-RPC 协议（见 [`protocol`]）。版面参照 grok-build：
//! 内容左右留边距，顶部信息条与输入框下边框图例夹住转写区；用户消息是带底色的圆角面板，
//! 助手回复带「耗时 + 时间」尾注，思考内容展开成引文带。
//! 交互质感同样对齐：鼠标操控、折叠开关、上下文详情弹层与圆角输入框。
//!
//! 全 crate 不含 `unsafe`：终端、协议与核心交互都走安全 API（构建期钉住）。

#![forbid(unsafe_code)]

pub mod app;
pub mod clipboard;
pub mod config;
pub mod event;
pub mod input;
pub mod interaction;
pub mod modal;
pub mod protocol;
pub mod queue;
pub mod render;
pub mod settings;
pub mod subagent;
pub mod terminal;
pub mod theme;
pub mod transcript;
pub mod transcript_restore;
pub mod widgets;

pub use event::{TuiOptions, run};
pub use transcript::DetailLevel;

/// 前端版本（与 crate 版本一致，握手时上报）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

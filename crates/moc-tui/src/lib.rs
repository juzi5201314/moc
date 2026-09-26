//! moc-tui：moc 的终端交互前端。
//!
//! 基于 ratatui + crossterm（与 grok-build 同款技术栈），内嵌 core 走与
//! 其他前端完全相同的 JSON-RPC 协议（见 [`protocol`]）。布局信息密度参照
//! omp：紧凑的转写区、工具调用盒、todo 树与底部 statusline；交互质感参照
//! grok-build：鼠标操控、折叠开关、圆角输入框。

pub mod app;
pub mod clipboard;
pub mod event;
pub mod input;
pub mod modal;
pub mod protocol;
pub mod render;
pub mod terminal;
pub mod theme;
pub mod transcript;
pub mod transcript_restore;
pub mod widgets;

pub use event::{TuiOptions, run};

/// 前端版本（与 crate 版本一致，握手时上报）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

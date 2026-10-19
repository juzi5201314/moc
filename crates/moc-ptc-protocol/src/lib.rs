//! PTC 宿主与 worker 共用的 JSON 协议和 JS 前置代码。

pub mod wire;

/// 在 worker 的 QuickJS 上下文中求值的控制对象；宿主和 guest 的边界只在这里。
pub const PRELUDE: &str = include_str!("prelude.js");

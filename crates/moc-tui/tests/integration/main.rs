//! 全部集成测试合并为单个 target，与其他 crate 同一布局（tests/integration/main.rs +
//! tests/integration/*.rs）。
//!
//! 过滤单个文件的用例：`cargo test -p moc-tui --test integration tier1`
mod e2e;
mod e2e_ui_ux_overhaul;

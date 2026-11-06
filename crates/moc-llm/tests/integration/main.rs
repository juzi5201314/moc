//! 全部集成测试合并为单个 target：原先 tests/ 顶层每个文件都会独立编译并链接
//! 一份测试二进制，合并为模块后整个 crate 只链接一次。
//!
//! 过滤单个文件的用例：`cargo test -p moc-llm --test integration flows`
mod errors;
mod flows;
mod traffic;
mod wire_dialects;

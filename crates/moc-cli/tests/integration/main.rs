//! 集成测试合并为单个 target，与其他 crate 同一布局（tests/integration/main.rs +
//! tests/integration/*.rs）。后续新增测试文件放进 integration/ 并在下方登记，
//! 避免每个文件再各自链接一份二进制。
mod cli_flow;

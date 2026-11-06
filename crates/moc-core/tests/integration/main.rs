//! 全部集成测试合并为单个 target：原先 tests/ 顶层每个文件都会独立编译并链接
//! 一份约 300MB 的测试二进制（20+ 个 target），合并为模块后整个 crate 只链接
//! 一次；改动 moc-core 后的重链接也从每个文件一次降为总共一次。
//!
//! 过滤单个文件的用例：`cargo test -p moc-core --test integration agent_flow`
mod support;

mod agent_flow;
mod autonomy_flow;
mod bash_flow;
mod config_management;
mod context_content_flow;
mod fast_mode_flow;
mod fs_create_dir_flow;
mod fs_list_flow;
mod goal_flow;
mod grok_build_compaction;
mod loop_guard_flow;
mod model_selection_flow;
mod plugin_flow;
mod plugin_persistence;
mod prompt_flow;
mod ptc_flow;
mod recovery_flow;
mod resource_uri_flow;
mod retry_flow;
mod sandbox_flow;
mod soft_checkpoint_guard;
mod web_state_flow;
mod workflow_flow;

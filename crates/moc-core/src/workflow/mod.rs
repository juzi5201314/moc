//! Workflow 核心：facade 声明（单一事实源）、编译器、执行引擎与 journal。

pub mod compile;
pub mod engine;
pub mod facade;
pub mod runtime;
pub mod skill;

/// workflow 唯一生效谓词：`[workflow] enabled` 全局开关 AND 会话已激活 AND
/// 非子代理。所有"会话是否能用 workflow"的判定都走它，不得散写表达式。
pub(crate) fn workflows_active(
    state: &crate::session::state::SessionState,
    config: &crate::config::WorkflowConfig,
) -> bool {
    config.enabled && state.workflow_activated && !state.session.is_subagent()
}

//! 插件工具桥接：把 moc-plugin 注册的工具适配成 [`Tool`]。
//!
//! 插件工具在哪些会话模式可见由插件自己声明（`modes`）；
//! 能力边界仍由 `moc.tools.call()` 的只读白名单决定，见 [`ReadOnlyBridge`]。

use std::pin::Pin;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_plugin::ModeMask;
use moc_protocol::SessionEngine;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOrigin, ToolOutcome};
use crate::service::mode::Mode;

pub struct PluginTool {
    plugin: String,
    name: String,
    description: String,
    parameters: Value,
    modes: ModeMask,
    parallel: bool,
}

impl PluginTool {
    /// 按插件声明构造；`plugin` 只用于报错与日志。
    pub fn new(plugin: impl Into<String>, declaration: &moc_plugin::ToolDecl) -> Self {
        Self {
            plugin: plugin.into(),
            name: declaration.name.clone(),
            description: declaration.description.clone(),
            parameters: declaration.parameters.clone(),
            modes: declaration.modes,
            parallel: declaration.parallel,
        }
    }
}

impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    /// 插件工具的能力边界由 QuickJS 运行时与 `tools.call` 白名单决定；这里的等级只
    /// 用于满足 [`Tool`] 契约，执行许可由 [`PluginTool::permitted_in`] 覆盖。
    fn tier(&self) -> Tier {
        Tier::Read
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Plugin
    }

    /// 引擎工具面：插件工具只在 Standard/CLM 引擎的面里（PTC 不开放），模式掩码不在
    /// 这一层判定——模式切换不能重写 schema。
    fn declared_in(&self, engine: SessionEngine) -> bool {
        matches!(
            engine,
            SessionEngine::Standard | SessionEngine::ClmExperimental
        )
    }

    /// 执行许可：引擎可达 + 插件声明的 `modes`（模式词表）覆盖当前模式。
    fn permitted_in(&self, engine: SessionEngine, mode: Mode) -> bool {
        self.declared_in(engine) && self.modes.contains(mode_mask(mode))
    }

    /// 插件线程按插件串行；默认整批当作屏障，声明 `parallel: true` 才允许并发。
    fn parallel_safe(&self) -> bool {
        self.parallel
    }

    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name.clone(),
            description: Some(self.description.clone()),
            parameters: self.parameters.clone(),
            strict: None,
        }
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let call_context = ctx.plugin_call_context();
            let result = ctx
                .plugins
                .call_tool(
                    &self.name,
                    args,
                    &call_context,
                    Arc::new(ReadOnlyBridge { ctx: ctx.clone() }),
                )
                .await
                .map_err(|error| {
                    ToolError::new(
                        error.code,
                        format!("插件 `{}`: {}", self.plugin, error.message),
                    )
                })?;
            let mut outcome = ToolOutcome::success(result.content);
            outcome.structured_content = result.structured;
            if result.is_error {
                outcome.is_error = true;
                outcome.error_code = Some("E_PLUGIN_TOOL".into());
            }
            Ok(outcome)
        })
    }
}

/// 执行模式对应的插件 `modes` 掩码位：插件声明的是「哪些执行模式可以调用我」。
fn mode_mask(mode: Mode) -> ModeMask {
    match mode {
        Mode::Default => ModeMask::DEFAULT,
        Mode::Ask => ModeMask::ASK,
        Mode::Plan => ModeMask::PLAN,
    }
}

/// 插件可以回调的宿主工具白名单：纯读取与联网检索。
///
/// 只按 `Tier::Read` 放行会连带放出 `job`（能杀后台任务）、`ask`（要人机交互）、
/// `todo`、`plan`/`exit_plan`（会结束回合）和 `skill`，所以这里显式列举。
const HOST_TOOLS: &[&str] = &["read", "grep", "find", "web_search", "web_fetch"];

fn host_tool_allowed(name: &str) -> bool {
    HOST_TOOLS.contains(&name)
}

/// 插件可以回调的宿主能力：白名单里的只读内置工具。
struct ReadOnlyBridge {
    ctx: ToolCtx,
}

impl moc_plugin::HostBridge for ReadOnlyBridge {
    fn call_tool<'a>(
        &'a self,
        name: String,
        args: Value,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<moc_plugin::ToolResult, moc_plugin::PluginError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            if !host_tool_allowed(&name) {
                return Err(moc_plugin::PluginError::new(
                    "E_PLUGIN_TOOLS_UNAVAILABLE",
                    format!("插件只能调用 {}，`{name}` 不在此列", HOST_TOOLS.join("/")),
                ));
            }
            // 当前模式不可见时同样落到这条错误里，插件不需要区分两种原因。
            if self.ctx.catalog.get(&name).is_none() {
                return Err(moc_plugin::PluginError::new(
                    "E_PLUGIN_TOOLS_UNAVAILABLE",
                    format!("工具 `{name}` 当前模式不可见"),
                ));
            }
            let call = moc_llm::ToolCall::new(
                format!("plugin_tool_{}", self.ctx.tool_call_id),
                name,
                args.to_string(),
            );
            let outcome = crate::service::prompt::tools::execute_nested_tool(
                &self.ctx.core,
                &self.ctx,
                &call,
            )
            .await
            .map_err(|error| moc_plugin::PluginError::new("E_PLUGIN_TOOL", error.to_string()))?;
            // 插件拿到的永远是纯文本；结构化结果交给宿主自己的呈现层。
            let content = match outcome.content {
                crate::tools::ToolOutput::Text(text) => text,
                crate::tools::ToolOutput::File { path, suffix } => {
                    format!("{}{suffix}", path.display())
                }
                crate::tools::ToolOutput::Excerpt { text, path } => {
                    format!("{text}\n(artifact: {})", path.display())
                }
            };
            Ok(moc_plugin::ToolResult {
                content,
                structured: outcome.structured_content,
                is_error: outcome.is_error,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{PluginTool, host_tool_allowed};
    use crate::service::mode::Mode;
    use crate::tools::Tool;
    use moc_plugin::ModeMask;
    use moc_protocol::SessionEngine;
    use serde_json::json;

    fn tool(modes: ModeMask) -> PluginTool {
        PluginTool::new(
            "demo",
            &moc_plugin::ToolDecl {
                name: "demo".into(),
                description: "演示".into(),
                parameters: json!({"type": "object"}),
                modes,
                parallel: false,
            },
        )
    }

    #[test]
    fn permission_follows_declared_modes() {
        use Mode::*;
        let all = tool(ModeMask::ALL);
        for engine in [SessionEngine::Standard, SessionEngine::ClmExperimental] {
            assert!(all.declared_in(engine), "{engine:?}");
            for mode in [Default, Ask, Plan] {
                assert!(all.permitted_in(engine, mode), "{engine:?} {mode:?}");
            }
        }
        assert!(!all.declared_in(SessionEngine::Ptc));
        assert!(!all.permitted_in(SessionEngine::Ptc, Default));

        let default_only = tool(ModeMask::DEFAULT);
        assert!(default_only.declared_in(SessionEngine::Standard));
        assert!(
            default_only.declared_in(SessionEngine::ClmExperimental),
            "default 覆盖 CLM 实验引擎"
        );
        assert!(default_only.permitted_in(SessionEngine::Standard, Default));
        assert!(!default_only.permitted_in(SessionEngine::Standard, Ask));
        assert!(!default_only.permitted_in(SessionEngine::Standard, Plan));

        // 模式掩码不影响 schema：plan-only 插件仍在引擎面里，只在 plan 模式下可执行。
        let plan_only = tool(ModeMask::PLAN);
        assert!(plan_only.declared_in(SessionEngine::Standard));
        assert!(plan_only.permitted_in(SessionEngine::Standard, Plan));
        assert!(!plan_only.permitted_in(SessionEngine::Standard, Default));
    }

    #[test]
    fn host_tool_allowlist_keeps_the_surface_read_only() {
        for name in ["read", "grep", "find", "web_search", "web_fetch"] {
            assert!(host_tool_allowed(name), "{name} 应在白名单里");
        }
        // 这些工具会改状态、要人机交互或结束回合，不属于插件能力面。
        for name in [
            "job",
            "ask",
            "todo",
            "plan",
            "exit_plan",
            "skill",
            "bash",
            "write",
            "edit",
            "run_code",
            "spawn_agent",
            "sidekick",
            "mcp__demo__search",
        ] {
            assert!(!host_tool_allowed(name), "{name} 不得出现在白名单里");
        }
    }
}

//! 插件工具桥接：把 moc-plugin 注册的工具适配成 [`Tool`]。
//!
//! 风险等级由插件声明（`read`/`write`/`exec`），直接映射到宿主 [`Tier`]；只读模式因此
//! 仍然可以调用声明为 `read` 的插件工具。工具内部通过 `moc.tools.call()` 回调宿主时
//! 只能落到只读内置工具上，见 [`ReadOnlyBridge`]。

use std::pin::Pin;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_plugin::ToolTier as DeclaredTier;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOrigin, ToolOutcome};

pub struct PluginTool {
    plugin: String,
    name: String,
    description: String,
    parameters: Value,
    tier: Tier,
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
            tier: match declaration.tier {
                DeclaredTier::Read => Tier::Read,
                DeclaredTier::Write => Tier::Write,
                DeclaredTier::Exec => Tier::Exec,
            },
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

    fn tier(&self) -> Tier {
        self.tier
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Plugin
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

/// 插件可以回调的宿主工具白名单：纯读取与联网检索。
///
/// 只按 `Tier::Read` 放行会连带放出 `job`（能杀后台任务）、`ask`（要人机交互）、
/// `todo`、`write_plan`/`exit_plan`（会结束回合）和 `skill`，所以这里显式列举。
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
    use super::host_tool_allowed;

    #[test]
    fn host_tool_allowlist_keeps_the_surface_read_only() {
        for name in ["read", "grep", "find", "web_search", "web_fetch"] {
            assert!(host_tool_allowed(name), "{name} 应在白名单里");
        }
        // `tier = read` 不代表可以借给插件：这些会改状态、要人机交互或结束回合。
        for name in [
            "job",
            "ask",
            "todo",
            "write_plan",
            "exit_plan",
            "skill",
            "bash",
            "write",
            "edit",
            "run_code",
            "spawn_agent",
            "mcp__demo__search",
        ] {
            assert!(!host_tool_allowed(name), "{name} 不得出现在白名单里");
        }
    }
}

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

/// 插件可以回调的宿主能力：只读内置工具，且不含 `run_code` 与插件工具本身。
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
            let tool = self.ctx.catalog.get(&name).ok_or_else(|| {
                moc_plugin::PluginError::new(
                    "E_PLUGIN_TOOLS_UNAVAILABLE",
                    format!("工具 `{name}` 不存在或当前模式不可见"),
                )
            })?;
            if tool.tier() != Tier::Read
                || tool.origin() == ToolOrigin::Plugin
                || name == "run_code"
            {
                return Err(moc_plugin::PluginError::new(
                    "E_PLUGIN_TOOLS_UNAVAILABLE",
                    format!("插件只能调用只读内置工具，`{name}` 不在此列"),
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

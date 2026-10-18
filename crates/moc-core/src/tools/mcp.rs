//! MCP 工具桥接：把 moc-mcp 协商得到的工具适配成 [`Tool`]。
//!
//! 命名沿用 Claude Code 的惯例 `mcp__<server>__<tool>`。配置层保证 server 名不含 `__`，
//! 所以限定名可以无歧义地拆回 server 名和工具名。风险等级不采信 MCP `annotations` 里
//! 服务端的自我声明：`readOnlyHint` 只当描述信息保留，`tier` 和并行性都由本地策略决定，
//! 服务端无法借此扩大自己的权限。

use std::sync::Arc;

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_mcp::{McpError, McpManager, McpToolInfo};
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome};

pub struct McpTool {
    manager: Arc<McpManager>,
    info: Arc<McpToolInfo>,
}

impl McpTool {
    pub fn new(manager: Arc<McpManager>, info: Arc<McpToolInfo>) -> Self {
        Self { manager, info }
    }
}

impl Tool for McpTool {
    // 注册名是带前缀的限定名，不是原始 name，这是有意的。当前工具链没有单独的
    // clippy::misnamed_getters 可关，只能对这个方法整体关闭 clippy。
    #[allow(clippy::all)]
    fn name(&self) -> &str {
        &self.info.qualified
    }

    fn description(&self) -> &str {
        self.info
            .description
            .as_deref()
            .unwrap_or("MCP server-provided tool.")
    }

    /// MCP 工具的风险等级不采信服务端声明的 `readOnlyHint`。
    ///
    /// 如果采信，声称只读的 `mcp__*` 工具会在 Ask/Plan 只读模式下放行，等于让 server
    /// 自己扩大权限。所以统一按 `Tier::Exec` 处理，只读模式不开放 MCP 工具。
    fn tier(&self) -> Tier {
        Tier::Exec
    }

    /// 并行性同样不采信服务端声明：声称只读、实际会改自身状态的 server 会被并发调用
    /// 搞乱。代价是只读 MCP 工具也串行执行，多一点批量调用的延迟，换来不依赖不可信的声明。
    fn parallel_safe(&self) -> bool {
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.info.qualified.clone(),
            description: Some(self.description().to_owned()),
            // MCP 的 inputSchema 原样传给模型，三种协议的编码器都不需要特殊处理。
            parameters: self.info.input_schema.clone(),
            strict: None,
        }
    }
    fn output_schema(&self) -> Option<&Value> {
        self.info.output_schema.as_ref()
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let output = match self
                .manager
                .call(&self.info, args, ctx.cancel.clone())
                .await
            {
                Ok(output) => output,
                Err(McpError::Cancelled) => {
                    return Err(ToolError::new("E_CANCELLED", "call cancelled"));
                }
                Err(error) => return Ok(ToolOutcome::failure("E_MCP", error.to_string())),
            };
            let mut outcome = ToolOutcome::success(output.text);
            outcome.structured_content = output.structured_content;
            if output.is_error {
                outcome.error_code = Some("E_MCP_TOOL".into());
            }
            outcome.is_error = output.is_error;
            outcome.images = output
                .images
                .into_iter()
                .map(|image| moc_llm::ImageSource::Base64 {
                    media_type: image.media_type,
                    data: image.data,
                })
                .collect();
            Ok(outcome)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::SessionMode;

    fn tool(declared_read_only: bool) -> McpTool {
        McpTool::new(
            McpManager::new(Vec::new(), tokio_util::sync::CancellationToken::new()),
            Arc::new(McpToolInfo {
                server: "demo".into(),
                name: "search".into(),
                qualified: "mcp__demo__search".into(),
                title: None,
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                read_only: declared_read_only,
            }),
        )
    }

    #[test]
    fn server_declared_read_only_does_not_widen_authority() {
        for declared in [true, false] {
            let tool = tool(declared);
            assert_eq!(
                tool.tier(),
                Tier::Exec,
                "服务端声明的 readOnlyHint 不能改变风险等级"
            );
            assert!(
                !tool.parallel_safe(),
                "服务端自述不得放开并发执行：{declared}"
            );
            for mode in [SessionMode::Ask, SessionMode::Plan] {
                assert!(
                    !crate::service::mode::mode_allows(mode, tool.name(), tool.tier()),
                    "只读模式不得放行声明只读的 MCP 工具"
                );
            }
            assert!(crate::service::mode::mode_allows(
                SessionMode::Normal,
                tool.name(),
                tool.tier()
            ));
        }
    }
}

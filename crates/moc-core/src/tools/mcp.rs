//! MCP server 工具桥接：把 moc-mcp 的协商结果适配为 [`Tool`]。
//!
//! 命名沿用 Claude Code 生态惯例 `mcp__<server>__<tool>`；server 名在配置层
//! 保证不含 `__`，限定名因此可无歧义反解。风险类别不取 MCP `annotations`
//! 的服务端自述：`readOnlyHint` 只作为描述信息保留，`tier` 与并行性都由本地
//! 策略决定，服务端无法据此扩大自己的权限。

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
    // 工具的注册名是带前缀的限定名而非原始 name；该 getter 有意如此，
    // clippy 的 misnamed_getter 在本工具链上不可用，这里整体屏蔽单方法。
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

    /// MCP 工具的风险类别不取服务端自述的 `readOnlyHint`。
    ///
    /// 自述只读的 `mcp__*` 工具会在 Ask/Plan 只读模式下被放行，等于让 server 自行
    /// 扩大权限；因此一律按 `Tier::Exec` 处理，只读模式不开放 MCP 工具。
    fn tier(&self) -> Tier {
        Tier::Exec
    }

    /// 并行性同样不取服务端自述：声明只读却在自身状态上写入的 server 会被并发调用
    /// 打乱。取舍是只读 MCP 工具也串行执行，用一批调用的延迟换掉一条不可信输入。
    fn parallel_safe(&self) -> bool {
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.info.qualified.clone(),
            description: Some(self.description().to_owned()),
            // MCP inputSchema 原样透传，三种协议编码器零改动。
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
                "服务端自述的 readOnlyHint 不得提升风险类别"
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

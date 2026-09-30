//! MCP server 工具桥接：把 moc-mcp 的协商结果适配为 [`Tool`]。
//!
//! 命名沿用 Claude Code 生态惯例 `mcp__<server>__<tool>`；server 名在配置层
//! 保证不含 `__`，限定名因此可无歧义反解。风险类别取自 MCP `annotations`：
//! `readOnlyHint` → 只读且可并行，其余按 Exec 处理，审批与模式限制随之生效。

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
            .unwrap_or("MCP server 提供的工具。")
    }

    fn tier(&self) -> Tier {
        if self.info.read_only {
            Tier::Read
        } else {
            Tier::Exec
        }
    }

    fn parallel_safe(&self) -> bool {
        self.info.read_only
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
                    return Err(ToolError::new("E_CANCELLED", "调用被取消"));
                }
                Err(error) => return Ok(ToolOutcome::failure("E_MCP", error.to_string())),
            };
            let mut outcome = ToolOutcome::success(output.text);
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

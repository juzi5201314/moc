//! 常驻工具上下文：PTC 只内联核心签名，各引擎共享按需能力目录。
//!
//! 完整低频定义由 tool:// 文档读取，参数仍来自同一个工具注册表。
//! MCP 的紧凑索引作为尾部提醒，避免服务器变化改写系统前缀。

use moc_protocol::SessionEngine;

use super::{ToolOrigin, catalog::ToolCatalog};
use crate::config::ResolvedConfig;

/// MCP 索引的 token 预算：上下文窗口的百分比与下限取较大者。
const MCP_INDEX_BUDGET_PERCENT: u64 = 1;
const MCP_INDEX_BUDGET_MIN_TOKENS: u64 = 2048;
/// 内置工具描述截断长度；完整描述对该集合收益有限。
const DESCRIPTION_MAX_CHARS: usize = 110;
/// TS 转换的最大嵌套深度；更深或无法表达的关键字降级为 `unknown`。
const TS_MAX_DEPTH: usize = 5;

/// 渲染常驻能力目录，以及 PTC 引擎所需的核心工具签名。
pub fn render_signatures(
    catalog: &ToolCatalog,
    engine: SessionEngine,
    ask_supported: bool,
    workflows_active: bool,
    config: &ResolvedConfig,
) -> Option<String> {
    let directory = super::documentation::index(
        catalog,
        engine,
        crate::service::mode::Mode::Default,
        ask_supported,
        config,
    );
    if engine != SessionEngine::Ptc {
        return Some(directory);
    }
    let mut lines = vec![
        "Tool signatures for execute: `const r = await tools.<name>(args)`; `?` marks optional properties. ToolResult fields are documented under execute.".to_owned(),
    ];
    let mut builtins = 0usize;
    // 声明渲染引擎面：模式只拦执行，不改写这段签名（系统前缀因此跨模式稳定）。
    for tool in catalog.visible(
        engine,
        crate::service::mode::Mode::Default,
        ask_supported,
        workflows_active,
        config,
    ) {
        if tool.origin() == ToolOrigin::Mcp || catalog.scope.defers(tool.name()) {
            continue;
        }
        builtins += 1;
        lines.push(format!(
            "tools.{}(args: {}): Promise<ToolResult> — {}",
            tool.name(),
            ts_type(&catalog.definition(tool.as_ref()).parameters, TS_MAX_DEPTH),
            short_description(tool.description()),
        ));
    }
    if builtins > 0 {
        Some(format!(
            "{directory}\n{}",
            wrap("tool-declarations", &lines)
        ))
    } else {
        Some(directory)
    }
}

/// 渲染 `<mcp-tool-index>` 片段：MCP 工具的名字与一行描述，按窗口预算截断。
///
/// 仅 Ptc 引擎且存在可见 MCP 工具时返回 `Some`。MCP 服务器可以在会话中途刷新工具
/// 列表，所以这段随每轮尾部提醒发送，不进系统前缀。
pub fn render_mcp_index(
    catalog: &ToolCatalog,
    engine: SessionEngine,
    ask_supported: bool,
    config: &ResolvedConfig,
    context_window: u64,
) -> Option<String> {
    if engine != SessionEngine::Ptc {
        return None;
    }
    let mcp: Vec<_> = catalog
        .visible(
            engine,
            crate::service::mode::Mode::Default,
            ask_supported,
            false,
            config,
        )
        .filter(|tool| tool.origin() == ToolOrigin::Mcp)
        .collect();
    if mcp.is_empty() {
        return None;
    }
    let mut lines =
        vec!["MCP tool index; fetch full schemas with catalog.describe(name):".to_owned()];
    let budget = context_window * MCP_INDEX_BUDGET_PERCENT / 100;
    let mut budget = budget.max(MCP_INDEX_BUDGET_MIN_TOKENS);
    let mut degraded = 0usize;
    for tool in mcp {
        let line = format!(
            "{} — {}",
            tool.name(),
            short_description(tool.description())
        );
        let cost = crate::compact::estimate_tokens(&line);
        if cost <= budget {
            budget -= cost;
            lines.push(line);
            continue;
        }
        let bare = tool.name().to_owned();
        let cost = crate::compact::estimate_tokens(&bare);
        if cost <= budget {
            budget -= cost;
            lines.push(bare);
            degraded += 1;
            continue;
        }
        degraded += 1;
    }
    if degraded > 0 {
        lines.push(format!(
            "({degraded} MCP tools omitted; call catalog.search({{query:\"\"}}) to list them)"
        ));
    }
    Some(wrap("mcp-tool-index", &lines))
}

fn wrap(tag: &str, lines: &[String]) -> String {
    format!("<{tag}>\n{}\n</{tag}>", lines.join("\n"))
}

/// 取描述的第一句并截断，控制每行成本。
fn short_description(description: &str) -> String {
    let head = description
        .split_once(". ")
        .map_or(description, |(head, _)| head);
    let mut out: String = head.chars().take(DESCRIPTION_MAX_CHARS).collect();
    if out.chars().count() < head.chars().count() {
        out.push('…');
    } else if !out.ends_with('.') {
        out.push('.');
    }
    out
}

/// JSON Schema → TS 类型文本：覆盖内置工具 schema 实际使用的关键字
/// （object/properties/required、array/items、enum、anyOf、标量），其余降级 `unknown`。
fn ts_type(schema: &serde_json::Value, depth: usize) -> String {
    if depth == 0 {
        return "unknown".into();
    }
    if let Some(variants) = schema
        .get("enum")
        .and_then(serde_json::Value::as_array)
        .filter(|list| !list.is_empty())
        .map(|list| {
            list.iter()
                .map(|value| match value {
                    serde_json::Value::String(text) => format!("\"{text}\""),
                    serde_json::Value::Number(number) => number.to_string(),
                    serde_json::Value::Bool(flag) => flag.to_string(),
                    _ => "unknown".to_owned(),
                })
                .collect::<Vec<_>>()
        })
    {
        return variants.join("|");
    }
    if let Some(variants) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(serde_json::Value::as_array)
        .filter(|list| !list.is_empty())
    {
        // 降级产生的重复 unknown 只留一份；全部退化为 unknown 时不再显示联合。
        let mut resolved: Vec<String> = Vec::new();
        for variant in variants {
            let text = ts_type(variant, depth - 1);
            if !resolved.contains(&text) {
                resolved.push(text);
            }
        }
        return resolved.join("|");
    }
    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("string") => "string".into(),
        Some("integer" | "number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => {
            let items = schema
                .get("items")
                .map(|item| ts_type(item, depth - 1))
                .unwrap_or_else(|| "unknown".into());
            format!("Array<{items}>")
        }
        Some("object") => {
            let Some(properties) = schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
            else {
                return "object".into();
            };
            if properties.is_empty() {
                return "{}".into();
            }
            let required: Vec<&str> = schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .map(|list| list.iter().filter_map(serde_json::Value::as_str).collect())
                .unwrap_or_default();
            let mut properties: Vec<_> = properties.iter().collect();
            // 工作区的 feature 合并可能启用 preserve_order；签名顺序不能依赖 JSON Map 后端。
            properties.sort_unstable_by_key(|(left, _)| *left);
            let fields: Vec<String> = properties
                .into_iter()
                .map(|(name, property)| {
                    let optional = if required.contains(&name.as_str()) {
                        ""
                    } else {
                        "?"
                    };
                    format!("{name}{optional}: {}", ts_type(property, depth - 1))
                })
                .collect();
            format!("{{ {} }}", fields.join(", "))
        }
        _ => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use moc_mcp::{McpManager, McpSnapshot, McpToolInfo};
    use serde_json::json;
    use std::sync::Arc;

    fn config() -> ResolvedConfig {
        let root = tempfile::tempdir().expect("临时目录");
        ResolvedConfig::load(&crate::CoreConfig {
            home: root.path().join("home"),
            cwd: root.path().to_owned(),
            ..Default::default()
        })
        .expect("默认配置")
    }

    fn catalog_with(mcp_tools: Vec<Arc<McpToolInfo>>) -> ToolCatalog {
        ToolCatalog::new(
            &ToolRegistry::new(),
            &McpSnapshot {
                tools: mcp_tools,
                ..Default::default()
            },
            McpManager::new(Vec::new(), tokio_util::sync::CancellationToken::new()),
            Vec::new(),
            crate::tools::ToolScope::top(),
        )
        .expect("工具目录")
    }

    fn mcp_tool(name: &str, description: &str) -> Arc<McpToolInfo> {
        Arc::new(McpToolInfo {
            server: "demo".into(),
            name: name.into(),
            qualified: format!("mcp__demo__{name}"),
            title: None,
            description: Some(description.into()),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            read_only: true,
        })
    }

    #[test]
    fn mcp_tools_are_indexed_and_budget_truncated() {
        let config = config();
        // 长描述压过 2048 token 的预算下限，保证截断路径必然触发。
        let tools: Vec<_> = (0..160)
            .map(|index| {
                mcp_tool(
                    &format!("tool_{index:03}"),
                    "Serve the purpose of filling the index budget with a long description",
                )
            })
            .collect();
        let catalog = catalog_with(tools);
        let text = render_mcp_index(&catalog, SessionEngine::Ptc, false, &config, 1_000).unwrap();
        assert!(text.starts_with("<mcp-tool-index>"));
        assert!(
            text.contains("mcp__demo__tool_000 —"),
            "预算内保留完整行：{text}"
        );
        assert!(
            text.contains("MCP tools omitted; call catalog.search"),
            "超预算应截断并指向 catalog.search：{text}"
        );
        // 大窗口放得下全部条目。
        let text =
            render_mcp_index(&catalog, SessionEngine::Ptc, false, &config, 2_000_000).unwrap();
        assert!(text.contains("mcp__demo__tool_159"));
        assert!(!text.contains("MCP tools omitted"));
    }

    /// MCP 集合随时可能刷新：它只能改变尾部索引，不能改写系统提示里的签名。
    #[test]
    fn signatures_are_stable_when_mcp_tools_change() {
        let config = config();
        let plain = catalog_with(Vec::new());
        let loaded = catalog_with(
            (0..160)
                .map(|index| {
                    mcp_tool(
                        &format!("tool_{index:03}"),
                        "Serve the purpose of filling the index budget with a long description",
                    )
                })
                .collect(),
        );
        assert_eq!(
            render_signatures(&plain, SessionEngine::Ptc, false, false, &config),
            render_signatures(&loaded, SessionEngine::Ptc, false, false, &config),
            "MCP 工具变化不得改写系统提示里的签名"
        );
        assert_ne!(
            render_mcp_index(&plain, SessionEngine::Ptc, false, &config, 200_000),
            render_mcp_index(&loaded, SessionEngine::Ptc, false, &config, 200_000),
            "变化的只有尾部索引"
        );
    }

    #[test]
    fn enum_anyof_and_depth_fallback() {
        assert_eq!(
            ts_type(&json!({"type": "string", "enum": ["a", "b"]}), 3),
            "\"a\"|\"b\""
        );
        assert_eq!(
            ts_type(&json!({"anyOf": [{"type": "string"}, {"type": "null"}]}), 3),
            "string|null"
        );
        assert_eq!(
            ts_type(&json!({"type": "object", "properties": {}}), 3),
            "{}"
        );
        assert_eq!(
            ts_type(&json!({"type": "array", "items": {"type": "integer"}}), 3),
            "Array<number>"
        );
        // 深度耗尽与未知关键字降级为 unknown。
        assert_eq!(ts_type(&json!({"type": "string"}), 0), "unknown");
        assert_eq!(ts_type(&json!({"$ref": "#/x"}), 3), "unknown");
    }

    #[test]
    fn descriptions_keep_first_sentence_and_are_capped() {
        assert_eq!(
            short_description("Read files. More detail follows."),
            "Read files."
        );
        assert_eq!(
            short_description("Already terminated."),
            "Already terminated."
        );
        let long = "x".repeat(200);
        let short = short_description(&long);
        assert!(short.chars().count() <= DESCRIPTION_MAX_CHARS + 1);
        assert!(short.ends_with('…'));
    }
}

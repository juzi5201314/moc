//! PTC 模式的工具声明：把当前可见工具生成 TS 签名放进系统提示。
//!
//! 没有声明时模型对参数形状一无所知，每个未知工具都要 `catalog.search` →
//! `catalog.describe` 两轮宿主往返才能安全调用。内置工具集合固定且 schema 浅，
//! 给完整签名（成本与普通模式的原生工具表相当）；MCP 工具数量不可控，只给
//! 名字加一行描述的紧凑索引，完整 schema 仍走 `catalog.describe`，并按上下文
//! 窗口预算截断（算法对齐 skills 目录）。

use moc_protocol::SessionMode;

use super::{ToolOrigin, catalog::ToolCatalog};
use crate::config::ResolvedConfig;

/// MCP 索引的 token 预算：上下文窗口的百分比与下限取较大者。
const MCP_INDEX_BUDGET_PERCENT: u64 = 1;
const MCP_INDEX_BUDGET_MIN_TOKENS: u64 = 2048;
/// 内置工具描述截断长度；完整描述对该集合收益有限。
const DESCRIPTION_MAX_CHARS: usize = 110;
/// TS 转换的最大嵌套深度；更深或无法表达的关键字降级为 `unknown`。
const TS_MAX_DEPTH: usize = 5;

/// 渲染 `<tool-declarations>` 片段；仅 Ptc 模式且存在可见工具时返回 `Some`。
pub fn render(
    catalog: &ToolCatalog,
    mode: SessionMode,
    ask_supported: bool,
    config: &ResolvedConfig,
    context_window: u64,
) -> Option<String> {
    if mode != SessionMode::Ptc {
        return None;
    }
    let mut builtins = Vec::new();
    let mut mcp = Vec::new();
    for tool in catalog.visible(mode, ask_supported, config) {
        match tool.origin() {
            ToolOrigin::Mcp => mcp.push(tool),
            _ => builtins.push(tool),
        }
    }
    if builtins.is_empty() && mcp.is_empty() {
        return None;
    }
    let mut lines = vec![
        "Tool signatures for run_code: `const r = await tools.<name>(args)`; `?` marks optional properties. ToolResult fields are documented under run_code.".to_owned(),
    ];
    for tool in builtins {
        lines.push(format!(
            "tools.{}(args: {}): Promise<ToolResult> — {}",
            tool.name(),
            ts_type(&tool.def().parameters, TS_MAX_DEPTH),
            short_description(tool.description()),
        ));
    }
    if !mcp.is_empty() {
        lines.push("MCP tool index; fetch full schemas with catalog.describe(name):".to_owned());
        let budget = context_window * MCP_INDEX_BUDGET_PERCENT / 100;
        let mut budget = budget.max(MCP_INDEX_BUDGET_MIN_TOKENS);
        let mut degraded = 0usize;
        for tool in mcp {
            let line = format!("{} — {}", tool.name(), short_description(tool.description()));
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
    }
    Some(format!(
        "<tool-declarations>\n{}\n</tool-declarations>",
        lines.join("\n")
    ))
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
            let Some(properties) = schema.get("properties").and_then(serde_json::Value::as_object)
            else {
                return "object".into();
            };
            if properties.is_empty() {
                return "{}".into();
            }
            let required: Vec<&str> = schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect()
                })
                .unwrap_or_default();
            let fields: Vec<String> = properties
                .iter()
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
    fn only_ptc_mode_renders_declarations() {
        let config = config();
        let catalog = catalog_with(Vec::new());
        assert!(
            render(&catalog, SessionMode::Normal, false, &config, 200_000).is_none(),
            "普通模式由原生工具表提供 schema"
        );
        assert!(render(&catalog, SessionMode::Ptc, false, &config, 200_000).is_some());
    }

    #[test]
    fn builtin_signatures_carry_required_and_optional() {
        let config = config();
        let catalog = catalog_with(Vec::new());
        let text = render(&catalog, SessionMode::Ptc, false, &config, 200_000).unwrap();
        assert!(text.starts_with("<tool-declarations>"));
        // serde_json 的对象键按字母序输出，断言按同序书写。
        assert!(
            text.contains(
                "tools.read(args: { limit?: number, offset?: number, path: string }): Promise<ToolResult>"
            ),
            "read 签名应含必填与可选参数：{text}"
        );
        assert!(
            text.contains("op: \"list\"|\"output\"|\"kill\"|\"wait\""),
            "枚举参数应转成字面量联合：{text}"
        );
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
        let text = render(&catalog, SessionMode::Ptc, false, &config, 1_000).unwrap();
        assert!(
            text.contains("mcp__demo__tool_000 —"),
            "预算内保留完整行：{text}"
        );
        assert!(
            text.contains("MCP tools omitted; call catalog.search"),
            "超预算应截断并指向 catalog.search：{text}"
        );
        // 大窗口放得下全部条目。
        let text = render(&catalog, SessionMode::Ptc, false, &config, 2_000_000).unwrap();
        assert!(text.contains("mcp__demo__tool_159"));
        assert!(!text.contains("MCP tools omitted"));
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
        assert_eq!(short_description("Already terminated."), "Already terminated.");
        let long = "x".repeat(200);
        let short = short_description(&long);
        assert!(short.chars().count() <= DESCRIPTION_MAX_CHARS + 1);
        assert!(short.ends_with('…'));
    }
}

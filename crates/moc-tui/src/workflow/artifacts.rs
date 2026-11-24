use moc_i18n::t;

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;
use std::collections::BTreeMap;

/// report 项按 artifact_id 路由出的数据源（本 artifact 的 item 列表，按到达序）。
pub(super) fn artifact_items<'a>(artifact: &Value, reports: &'a [Value]) -> Vec<&'a Value> {
    let id = artifact
        .get("artifact_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    reports
        .iter()
        .filter(|r| r.get("artifact_id").and_then(|v| v.as_str()) == Some(id))
        .filter_map(|r| r.get("item"))
        .collect()
}

/// `ArtifactField` 的 dot path 取值（"timing.after" → item.timing.after）。
pub(super) fn field_get<'a>(item: &'a Value, field: &str) -> Option<&'a Value> {
    let mut cur = item;
    for part in field.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

pub(super) fn field_text(item: &Value, field: &str) -> String {
    field_get(item, field)
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_else(|| "—".into())
}

pub(super) fn field_label(field: &Value) -> String {
    field
        .get("label")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            field
                .get("field")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string()
        })
        + field
            .get("unit")
            .and_then(|v| v.as_str())
            .map(|u| format!(" {u}"))
            .as_deref()
            .unwrap_or("")
}

/// artifact 按 member 真渲染：数据源 = 该 artifact_id 的 report 项（spec 语义：
/// table 按 key 去重替换、metrics 取最新值、board 按 status 分列且 key 移动、
/// chart 画文本折线/散点；file/markdown 预览内容）。
pub(super) fn artifact_lines(
    artifact: &Value,
    reports: &[Value],
    version: usize,
    versions: usize,
    width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme.gray_dim);
    let accent = Style::new().fg(theme.accent_model);
    let member = artifact
        .get("member")
        .and_then(|v| v.as_str())
        .unwrap_or("file");
    let spec = artifact.get("spec").cloned().unwrap_or(Value::Null);
    let id = artifact
        .get("artifact_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let title = spec
        .get("title")
        .and_then(|v| v.as_str())
        .or_else(|| artifact.get("title").and_then(|v| v.as_str()))
        .unwrap_or(id);
    let ver = if versions > 1 {
        format!(" v{version}/{versions}")
    } else {
        String::new()
    };
    let mut out = vec![Line::from(Span::styled(
        format!("  [{member}] {title}{ver}"),
        accent,
    ))];
    let items = artifact_items(artifact, reports);
    match member {
        // file/markdown 条目是 {artifact_id, member, version, file,
        // content_type?, title?, description?, primary?}；内容按 'o' 打开。
        "file" | "markdown" => {
            let file = artifact.get("file").and_then(|v| v.as_str()).unwrap_or("?");
            let mime = artifact
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream");
            out.push(Line::from(Span::styled(
                format!("    📄 {file} ({mime})"),
                dim,
            )));
        }
        "chart" => {
            out.extend(chart_artifact_lines(
                &spec,
                &items,
                width.saturating_sub(6),
                theme,
            ));
        }
        "table" => {
            let columns = spec
                .get("columns")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            let key = spec.get("key").and_then(|v| v.as_str());
            // key 去重：同 key 的后续项替换（保留最后值、原序）。
            let mut order: Vec<String> = Vec::new();
            let mut rows: BTreeMap<String, &Value> = BTreeMap::new();
            for (i, item) in items.iter().enumerate() {
                let k = key
                    .map(|k| field_text(item, k))
                    .unwrap_or_else(|| i.to_string());
                if !rows.contains_key(&k) {
                    order.push(k.clone());
                }
                rows.insert(k, item);
            }
            let header = columns
                .iter()
                .map(field_label)
                .collect::<Vec<_>>()
                .join("  ");
            out.push(Line::from(Span::styled(format!("    {header}"), accent)));
            for k in &order {
                let cells = columns
                    .iter()
                    .map(|c| {
                        field_text(
                            rows[k],
                            c.get("field").and_then(|v| v.as_str()).unwrap_or(""),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("  ");
                out.push(Line::from(Span::styled(format!("    {cells}"), dim)));
            }
            if order.is_empty() {
                out.push(Line::from(Span::styled(
                    t!("tui.wf.no_data").into_owned(),
                    dim,
                )));
            }
        }
        "metrics" => {
            let fields = spec
                .get("metrics")
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default();
            // 每字段取最后一个含该字段的 item 的值。
            for field in &fields {
                let path = field.get("field").and_then(|v| v.as_str()).unwrap_or("");
                let value = items
                    .iter()
                    .rev()
                    .find_map(|item| field_get(item, path))
                    .map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .unwrap_or_else(|| "—".into());
                out.push(Line::from(Span::styled(
                    format!("    {} = {}", field_label(field), value),
                    dim,
                )));
            }
        }
        "board" => {
            let key_f = spec.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let status_f = spec.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let columns: Vec<String> = spec
                .get("columns")
                .and_then(|c| c.as_array())
                .map(|c| {
                    c.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let title_f = spec
                .get("card_title")
                .or_else(|| spec.get("cardTitle"))
                .and_then(|v| v.as_str())
                .unwrap_or(key_f);
            // key 去重：后到的同 key 卡移动/更新（位置按最新 status）。
            let mut order: Vec<String> = Vec::new();
            let mut cards: BTreeMap<String, &Value> = BTreeMap::new();
            for item in &items {
                let k = field_text(item, key_f);
                if !cards.contains_key(&k) {
                    order.push(k.clone());
                }
                cards.insert(k, item);
            }
            let mut cols: Vec<(String, Vec<String>)> =
                columns.iter().map(|c| (c.clone(), Vec::new())).collect();
            cols.push(("other".into(), Vec::new()));
            for k in &order {
                let item = cards[k];
                let status = field_text(item, status_f);
                let title = field_text(item, title_f);
                let index = cols
                    .iter()
                    .position(|(name, _)| *name == status)
                    .unwrap_or(cols.len() - 1);
                cols[index].1.push(title);
            }
            for (name, card_titles) in &cols {
                out.push(Line::from(Span::styled(
                    format!("    ▸ {name} ×{}", card_titles.len()),
                    accent,
                )));
                for card in card_titles {
                    out.push(Line::from(Span::styled(format!("      {card}"), dim)));
                }
            }
        }
        _ => {
            out.push(Line::from(Span::styled(
                format!(
                    "    {}",
                    crate::render::wrap::truncate(spec.to_string(), width.saturating_sub(4))
                ),
                dim,
            )));
        }
    }
    out
}

/// chart artifact 的文本渲染：x/y 字段取点，按 type 画散点/柱状行 + 数值表；
/// raster Canvas 路径在 modal 内不可承载，文本图保证数据全真。
pub(super) fn chart_artifact_lines(
    spec: &Value,
    items: &[&Value],
    width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme.gray_dim);
    let accent = Style::new().fg(theme.accent_model);
    let kind = spec.get("type").and_then(|v| v.as_str()).unwrap_or("line");
    let x_f = spec
        .get("x")
        .and_then(|x| x.get("field"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let y_fields: Vec<&Value> = spec
        .get("y")
        .map(|y| {
            if let Some(arr) = y.as_array() {
                arr.iter().collect()
            } else {
                vec![y]
            }
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    // 数值序列。
    let mut series: Vec<(String, Vec<(f64, f64)>)> = Vec::new();
    for y in &y_fields {
        let y_f = y.get("field").and_then(|v| v.as_str()).unwrap_or("");
        let points: Vec<(f64, f64)> = items
            .iter()
            .filter_map(|item| {
                let x = field_get(item, x_f)?.as_f64()?;
                let yv = field_get(item, y_f)?.as_f64()?;
                Some((x, yv))
            })
            .collect();
        series.push((field_label(y), points));
    }
    // 文本 sparkline：每条 series 一行高度标尺。
    for (label, points) in &series {
        if points.is_empty() {
            continue;
        }
        let lo = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let hi = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        const TICKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let span = (hi - lo).max(f64::EPSILON);
        let glyphs: String = points
            .iter()
            .map(|p| TICKS[(((p.1 - lo) / span) * 7.0).round() as usize])
            .collect();
        let marker = match kind {
            "bar" => "▮",
            "scatter" => "∙",
            _ => "─",
        };
        out.push(Line::from(Span::styled(
            format!("    {marker} {label} [{lo:.2}..{hi:.2}]  {glyphs}"),
            accent,
        )));
        // 末尾值表（x→y 对齐，超宽截断）。
        let table = points
            .iter()
            .map(|(x, y)| format!("{x:.2}→{y:.2}"))
            .collect::<Vec<_>>()
            .join("  ");
        out.push(Line::from(Span::styled(
            format!("      {}", crate::render::wrap::truncate(&table, width)),
            dim,
        )));
    }
    if series.iter().all(|(_, p)| p.is_empty()) {
        out.push(Line::from(Span::styled(
            t!("tui.wf.no_points").into_owned(),
            dim,
        )));
    }
    if let Some(v) = spec
        .get("baseline")
        .and_then(|b| b.get("field"))
        .and_then(|v| v.as_str())
        .and_then(|baseline| {
            items
                .iter()
                .find_map(|item| field_get(item, baseline).and_then(|v| v.as_f64()))
        })
    {
        out.push(Line::from(Span::styled(
            format!("    ── baseline {v:.2}"),
            dim,
        )));
    }
    out
}

pub(super) fn cost_label(usage: &Value) -> String {
    usage
        .get("cost_usd")
        .and_then(|c| c.as_f64())
        .map(|c| format!(" (${c:.4})"))
        .unwrap_or_default()
}

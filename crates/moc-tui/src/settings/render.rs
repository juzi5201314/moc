use super::*;
use crate::render::wrap::str_width;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};

fn cells(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}
fn label(frame: &mut Frame, area: Rect, text: impl Into<String>, style: Style) {
    frame.render_widget(Paragraph::new(text.into()).style(style), area);
}
fn button(
    frame: &mut Frame,
    hits: &mut Vec<(Rect, Hit)>,
    x: &mut u16,
    y: u16,
    end: u16,
    (text, hit): (&str, Hit),
    theme: &Theme,
) {
    let width = cells(str_width(text)).saturating_add(2);
    if x.saturating_add(width) > end {
        return;
    }
    let area = Rect::new(*x, y, width, 1);
    label(
        frame,
        area,
        format!(" {text} "),
        Style::new().fg(theme.text_primary).bg(theme.bg_visual),
    );
    hits.push((area, hit));
    *x = x.saturating_add(width + 1);
}

pub fn draw(frame: &mut Frame, panel: &mut SettingsPanel, theme: &Theme) {
    let outer = frame.area();
    frame.render_widget(Clear, outer);
    frame
        .buffer_mut()
        .set_style(outer, Style::new().bg(theme.bg_base).fg(theme.text_primary));
    let area = Rect::new(
        outer.x.saturating_add(2),
        outer.y,
        outer.width.saturating_sub(4),
        outer.height,
    );
    if area.width < 12 || area.height < 7 {
        label(
            frame,
            area,
            "终端过小；请放大窗口。Esc 返回",
            Style::new().fg(theme.text_primary),
        );
        return;
    }
    let mut hits = Vec::new();
    let scope = match panel.scope {
        ConfigScope::Global => "全局",
        ConfigScope::Project => "项目",
    };
    label(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        format!(
            "/config  ·  {scope}{}{}",
            if panel.dirty() {
                "  [有未保存修改]"
            } else {
                ""
            },
            if panel.busy { "  正在处理…" } else { "" }
        ),
        Style::new().add_modifier(Modifier::BOLD),
    );
    let mut x = area.x;
    let mut y = area.y + 2;
    for (index, tab) in TABS.iter().enumerate() {
        let width = cells(str_width(tab)).saturating_add(2);
        if x.saturating_add(width) > area.right() {
            x = area.x;
            y += 1;
        }
        if y >= area.y + 5 {
            break;
        }
        let tab_area = Rect::new(x, y, width.min(area.width), 1);
        label(
            frame,
            tab_area,
            format!(" {tab} "),
            if panel.tab == index {
                Style::new()
                    .bg(theme.bg_visual)
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.gray)
            },
        );
        hits.push((tab_area, Hit::Tab(index)));
        x = x.saturating_add(width + 1);
    }
    let toolbar_y = y.saturating_add(2).min(area.bottom() - 4);
    let mut x = area.x;
    button(
        frame,
        &mut hits,
        &mut x,
        toolbar_y,
        area.right(),
        ("保存 Ctrl+S", Hit::Save),
        theme,
    );
    button(
        frame,
        &mut hits,
        &mut x,
        toolbar_y,
        area.right(),
        ("来源 F2", Hit::Scope),
        theme,
    );
    match TABS[panel.tab] {
        "供应商" | "模型" => {
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("添加 A", Hit::Add),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("获取模型 F", Hit::Fetch),
                theme,
            );
        }
        "MCP" => button(
            frame,
            &mut hits,
            &mut x,
            toolbar_y,
            area.right(),
            ("添加 A", Hit::Add),
            theme,
        ),
        "OpenViking" => button(
            frame,
            &mut hits,
            &mut x,
            toolbar_y,
            area.right(),
            ("工作区 W", Hit::Workspace),
            theme,
        ),
        "会话" => button(
            frame,
            &mut hits,
            &mut x,
            toolbar_y,
            area.right(),
            ("维护预览 G", Hit::Maintenance),
            theme,
        ),
        _ => {}
    }
    button(
        frame,
        &mut hits,
        &mut x,
        toolbar_y,
        area.right(),
        ("高级 F4", Hit::Advanced),
        theme,
    );
    let desc_height = (area.height / 4).clamp(2, 6);
    let footer_y = area.bottom().saturating_sub(desc_height + 2);
    let body = Rect::new(
        area.x,
        toolbar_y + 2,
        area.width,
        footer_y.saturating_sub(toolbar_y + 3),
    );
    let footer = Rect::new(area.x, footer_y + 1, area.width, desc_height);
    let mut description="Tab / Shift+Tab 切换选项卡 · ↑↓ 选择 · Enter 编辑 · Space 切换 · / 搜索 · R 重置继承 · Delete 删除 · Esc 返回".to_owned();
    match &panel.view {
        View::Fields => {
            if panel.data.is_none() {
                label(frame, body, "正在读取配置…", Style::new().fg(theme.gray));
            } else if panel.visible.is_empty() {
                label(
                    frame,
                    body,
                    if panel.filter.is_empty() {
                        "这里还没有配置项。供应商 / 模型 / MCP 可以按 A 添加。首次配置：先添加供应商，填写地址与密钥，再获取并勾选模型。"
                    } else {
                        "没有匹配项；按 / 修改或清空搜索。"
                    },
                    Style::new().fg(theme.gray),
                );
            }
            let height = usize::from(body.height);
            if panel.selected < panel.offset {
                panel.offset = panel.selected;
            }
            if panel.selected >= panel.offset.saturating_add(height) {
                panel.offset = panel.selected.saturating_sub(height.saturating_sub(1));
            }
            for (row, index) in panel
                .visible
                .iter()
                .enumerate()
                .skip(panel.offset)
                .take(height)
            {
                let field = &panel.fields[*index];
                let value = panel.value(&field.path).unwrap_or(&Value::Null);
                let source = if field.path.first().is_some_and(|part| part == "tui") {
                    ""
                } else if panel
                    .data
                    .as_ref()
                    .and_then(|data| get(&data.document, &field.path))
                    .is_some()
                {
                    "● "
                } else {
                    "  "
                };
                let prefix = if field.path.first().is_some_and(|part| part == "providers") {
                    if field.path.get(2).is_some_and(|part| part == "models") {
                        format!("{}/{} · ", field.path[1], field.path[3])
                    } else {
                        format!("{} · ", field.path[1])
                    }
                } else if field.path.starts_with(&["mcp".into(), "servers".into()]) {
                    format!("{} · ", field.path[2])
                } else {
                    String::new()
                };
                let name = format!("{source}{prefix}{}", field.label);
                let value = display_value(value, &field.kind);
                let row_area = Rect::new(body.x, body.y + cells(row - panel.offset), body.width, 1);
                let style = if row == panel.selected {
                    Style::new().bg(theme.bg_visual).fg(theme.text_primary)
                } else {
                    Style::new().fg(theme.gray)
                };
                frame.render_widget(Block::default().style(style), row_area);
                let left = body.width.saturating_mul(3) / 5;
                label(
                    frame,
                    Rect::new(row_area.x, row_area.y, left, 1),
                    name,
                    style,
                );
                label(
                    frame,
                    Rect::new(
                        row_area.x + left + 1,
                        row_area.y,
                        body.width.saturating_sub(left + 1),
                        1,
                    ),
                    value,
                    style,
                );
                hits.push((row_area, Hit::Row(row)));
            }
            if let Some(field) = panel.field() {
                description = format!(
                    "{}\n{}\n{}",
                    field.path.join(" / "),
                    field.description,
                    if TABS[panel.tab] == "TUI" {
                        "Ctrl+S 仅保存独立 tui.toml；core 草稿保留。"
                    } else {
                        "Ctrl+S 保存当前 core 来源；● 表示当前文件中的显式覆盖。"
                    }
                );
            }
        }
        View::Edit {
            target,
            kind,
            input,
            reveal,
        } => {
            let title = match target {
                EditTarget::Field(path) => format!("编辑 {}", path.join(" / ")),
                EditTarget::Provider => "添加供应商 · 输入标识，例如 local 或 gateway".into(),
                EditTarget::Model(provider) => format!("添加模型 · {provider} · 填写实际模型 ID"),
                EditTarget::Mcp => "添加 MCP 服务 · 输入标识".into(),
                EditTarget::ModelDefaults => "添加模型默认模式 · 例如 gpt-*".into(),
                EditTarget::Rebind(binding) => format!("重绑 {} · 输入新目录路径", binding.peer_id),
                EditTarget::Filter => "搜索字段 · 留空清除过滤".into(),
            };
            label(
                frame,
                Rect::new(body.x, body.y, body.width, 1),
                title,
                Style::new().fg(theme.accent_model),
            );
            let editor = Rect::new(
                body.x,
                body.y + 2,
                body.width,
                body.height.saturating_sub(2),
            );
            let secret = matches!(kind, FieldKind::Secret) && !*reveal;
            let text = if secret {
                input
                    .text
                    .chars()
                    .map(|character| if character == '\n' { '\n' } else { '•' })
                    .collect()
            } else {
                input.text.clone()
            };
            let lines: Vec<_> = text
                .split('\n')
                .flat_map(|line| {
                    crate::render::wrap::wrap_text(line, usize::from(editor.width.max(1)))
                })
                .collect();
            let prefix = &input.text[..input.cursor];
            let prefix = if secret {
                prefix
                    .chars()
                    .map(|character| if character == '\n' { '\n' } else { '•' })
                    .collect::<String>()
            } else {
                prefix.into()
            };
            let cursor_lines: Vec<_> = prefix
                .split('\n')
                .flat_map(|line| {
                    crate::render::wrap::wrap_text(line, usize::from(editor.width.max(1)))
                })
                .collect();
            let cursor_y = cursor_lines.len().saturating_sub(1);
            let scroll = cursor_y.saturating_sub(usize::from(editor.height.saturating_sub(1)));
            let visible = lines
                .into_iter()
                .skip(scroll)
                .take(usize::from(editor.height))
                .map(Line::from)
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(visible).style(Style::new().bg(theme.bg_visual)),
                editor,
            );
            if editor.height > 0 {
                let column = cursor_lines.last().map_or(0, |line| str_width(line));
                frame.set_cursor_position((
                    editor.x + cells(column).min(editor.width.saturating_sub(1)),
                    editor.y + cells(cursor_y - scroll),
                ));
            }
            description=match kind{
                FieldKind::Choice(options)=>format!("↑↓ 选择，Enter 应用；可用值：{}\nEsc 放弃本字段修改",options.join(" / ")),
                FieldKind::Strings=>"每行一项，Enter 换行，Ctrl+Enter 应用。留空删除覆盖项并继承默认。\nEsc 放弃本字段修改。".into(),
                FieldKind::Object|FieldKind::Multiline=>"Enter 换行，Ctrl+Enter 应用；JSON 对象可直接粘贴。\nEsc 放弃本字段修改。".into(),
                FieldKind::Secret=>"密钥默认遮蔽；F6 显示／隐藏，Ctrl+U 清空，Enter 应用。支持 env:变量名。".into(),
                _=>"Enter 应用字段到草稿，Ctrl+U 清空，Esc 放弃；修改后 Ctrl+S 保存。".into(),
            };
        }
        View::Models {
            provider,
            rows,
            checked,
            selected,
            ..
        } => {
            label(
                frame,
                Rect::new(body.x, body.y, body.width, 1),
                format!(
                    "{provider} · 可用 {} 个 · 已勾选 {} 个",
                    rows.len(),
                    checked.len()
                ),
                Style::new().fg(theme.accent_model),
            );
            let height = usize::from(body.height.saturating_sub(2));
            let start = selected.saturating_sub(height.saturating_sub(1));
            for (index, row) in rows.iter().enumerate().skip(start).take(height) {
                let exists = panel
                    .effective
                    .get("providers")
                    .and_then(|providers| providers.get(provider))
                    .and_then(|provider| provider.get("models"))
                    .and_then(|models| models.get(&row.id))
                    .is_some();
                let text = format!(
                    "{} {}  {}",
                    if exists {
                        "[已配置]"
                    } else if checked.contains(&row.id) {
                        "[x]"
                    } else {
                        "[ ]"
                    },
                    row.id,
                    if row.name == row.id { "" } else { &row.name }
                );
                let area = Rect::new(body.x, body.y + 2 + cells(index - start), body.width, 1);
                label(
                    frame,
                    area,
                    text,
                    if index == *selected {
                        Style::new().bg(theme.bg_visual)
                    } else {
                        Style::new().fg(theme.gray)
                    },
                );
                hits.push((area, Hit::Check(index)));
            }
            description="Space 或点击勾选，Enter 仅添加勾选模型到草稿，Esc 返回；不会默认导入全部模型。添加后仍需核对上下文、输入能力与定价，再 Ctrl+S 保存。".into();
            let mut x = body.x;
            button(
                frame,
                &mut hits,
                &mut x,
                footer_y,
                body.right(),
                ("添加勾选模型 Enter", Hit::Apply),
                theme,
            );
        }
        View::Workspaces { rows, selected, .. } => {
            let height = usize::from(body.height);
            let start = selected.saturating_sub(height.saturating_sub(1));
            for (index, row) in rows.iter().enumerate().skip(start).take(height) {
                let area = Rect::new(body.x, body.y + cells(index - start), body.width, 1);
                label(
                    frame,
                    area,
                    format!(
                        "{} {}{}",
                        if row.exists {
                            "[存在]"
                        } else {
                            "[目录失效 · 待手动处理]"
                        },
                        row.path,
                        if row.shared {
                            " · 共享知识域"
                        } else {
                            ""
                        }
                    ),
                    if index == *selected {
                        Style::new().bg(theme.bg_visual)
                    } else {
                        Style::new().fg(theme.gray)
                    },
                );
                hits.push((area, Hit::Check(index)));
            }
            description = if let Some(row) = rows.get(*selected) {
                format!(
                    "peer: {}\n服务: {}\nEnter 迁移重绑新目录，Delete 手动清理。失效仅标记，不会自动删除任何记忆。",
                    row.peer_id, row.url
                )
            } else {
                "尚无已登记的知识域；启用 OpenViking 后会按工作区登记。".into()
            };
            let mut x = body.x;
            button(
                frame,
                &mut hits,
                &mut x,
                footer_y,
                body.right(),
                ("迁移 Enter", Hit::Apply),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                footer_y,
                body.right(),
                ("清理 Delete", Hit::Delete),
                theme,
            );
        }
        View::Maintenance(report) => {
            let mut lines = vec![Line::from(format!(
                "{}：归档 {} 个会话，清理 {} 个产物（{} bytes），保护 {} 项",
                if report.applied {
                    "维护完成"
                } else {
                    "维护预览"
                },
                report.archived.len(),
                report.removed.len(),
                report.bytes,
                report.protected
            ))];
            lines.extend(
                report
                    .archived
                    .iter()
                    .map(|path| format!("归档 {path}"))
                    .chain(report.removed.iter().map(|path| format!("删除 {path}")))
                    .skip(panel.offset)
                    .map(Line::from),
            );
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
            description="归档可以自动恢复；只清理超过保留期限且日志、归档和备份均未引用的产物。↑↓ 查看清单；Enter 再次确认后执行，Esc 返回。".into();
        }
        View::Confirm { text, .. } => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(text.as_str()),
                    Line::from(""),
                    Line::from(Span::styled(
                        "按 Y 明确确认；N 或 Esc 取消",
                        Style::new()
                            .fg(theme.accent_model)
                            .add_modifier(Modifier::BOLD),
                    )),
                ])
                .wrap(Wrap { trim: false }),
                body,
            );
            description = "确认前没有执行删除操作。".into();
        }
    }
    frame.render_widget(
        Paragraph::new(description)
            .style(Style::new().fg(theme.gray))
            .wrap(Wrap { trim: false }),
        footer,
    );
    if let Some(error) = &panel.error {
        label(
            frame,
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
            format!("错误：{error}"),
            Style::new().fg(theme.text_primary).bg(theme.bg_visual),
        );
    } else if let Some(message) = &panel.message {
        label(
            frame,
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
            message.clone(),
            Style::new().fg(theme.accent_model),
        );
    }
    panel.hits = hits;
}

fn display_value(value: &Value, kind: &FieldKind) -> String {
    if value.is_null() {
        return "自动／未指定".into();
    }
    if matches!(kind, FieldKind::Secret) {
        return if value.as_str().is_some_and(|text| !text.is_empty()) {
            "••••••"
        } else {
            "未设置"
        }
        .into();
    }
    match value {
        Value::Bool(true) => "开启".into(),
        Value::Bool(false) => "关闭".into(),
        Value::String(text) => text.lines().next().unwrap_or_default().into(),
        Value::Object(values) => format!("{{ {} 项 }}", values.len()),
        Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(", "),
        _ => value.to_string(),
    }
}

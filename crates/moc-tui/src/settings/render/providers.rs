use super::super::Hit;
use super::super::SettingsPanel;
use super::super::View;
use super::super::providers::ProviderAction;
use super::Footer;
use super::cells;
use super::editor::edit_line;
use super::fields::draw_fields;
use super::label;
use super::scrollbar;
use super::short_bytes;
use crate::render::labels;
use crate::render::modal::window_offset;
use crate::settings::providers::APIS;
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;

/// 供应商列表和详情共用一个页面，窄终端用可切换的单栏布局。
pub(super) fn draw_providers(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> Footer {
    if panel.data.is_none() {
        return draw_fields(frame, panel, theme, body, hits);
    }
    let ids = panel.provider_ids();
    let wide = body.width >= 72;
    let sidebar = if wide {
        Rect { width: 22, ..body }
    } else {
        body
    };
    if wide || panel.providers.sidebar {
        let height = usize::from(sidebar.height);
        let cursor = panel.providers.cursor;
        let total = ids.len() + 1;
        panel.providers.offset = if panel.providers.scroll.reveal_selection {
            window_offset(panel.providers.offset, cursor, height, total)
        } else {
            panel.providers.offset.min(total.saturating_sub(height))
        };
        panel.providers.scroll.max_offset = total.saturating_sub(height);
        panel.providers.scroll.reveal_selection = false;
        for index in (panel.providers.offset..=ids.len()).take(height) {
            let text = match ids.get(index) {
                Some(id) => {
                    let count = panel.effective["providers"][id]["models"]
                        .as_object()
                        .map_or(0, serde_json::Map::len);
                    format!("{id} · {count}")
                }
                None => "+ 添加供应商".into(),
            };
            let selected = panel.provider_selected(&ids, index);
            let style = if selected {
                Style::new().bg(theme.bg_visual).fg(theme.accent_model)
            } else {
                Style::new().fg(theme.text_secondary)
            };
            let area = Rect::new(
                sidebar.x,
                sidebar.y + cells(index - panel.providers.offset),
                sidebar.width,
                1,
            );
            label(
                frame,
                area,
                &format!("{} {text}", if selected { "❯" } else { " " }),
                style,
            );
            hits.push((area, Hit::Provider(index)));
        }
    }
    if !wide && panel.providers.sidebar {
        return Footer {
            description: "选择供应商后进入连接和模型配置；零模型供应商也可以直接添加。".into(),
            source: None,
            hint: labels::hint_layout(
                &[("↑↓", "选择"), ("enter/tab", "打开"), ("esc", "返回")],
                usize::from(body.width),
            ),
        };
    }
    let detail = if wide {
        Rect::new(body.x + 24, body.y, body.width - 24, body.height)
    } else {
        body
    };
    if panel.providers.draft.is_some() {
        return draw_provider_draft(frame, panel, theme, detail, hits);
    }
    let Some(id) = panel.providers.selected.as_ref() else {
        let area = Rect {
            height: 1,
            ..detail
        };
        label(
            frame,
            area,
            "+ 添加供应商：填写连接参数后获取或手动添加模型",
            Style::new().fg(theme.accent_model),
        );
        hits.push((area, Hit::ProviderAction(ProviderAction::New)));
        return Footer {
            description: "按 N 新建供应商。".into(),
            ..Footer::default()
        };
    };
    label(
        frame,
        Rect {
            height: 1,
            ..detail
        },
        &format!("{id} · 连接与模型"),
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    let mut content = Rect::new(
        detail.x,
        detail.y + 2,
        detail.width,
        detail.height.saturating_sub(2),
    );
    let status = panel.providers.discovery.as_ref().and_then(|discovery| {
        if let Some(error) = &discovery.error {
            Some((
                format!("获取失败：{error}；可重试或手动添加"),
                theme.accent_error,
            ))
        } else if discovery.pending.is_some() {
            Some((
                "正在获取模型，连接参数仍可编辑…".into(),
                theme.accent_running,
            ))
        } else if discovery.rows.is_empty() {
            Some(("供应商未返回模型，可手动添加模型 ID。".into(), theme.gray))
        } else if panel.visible_candidates().is_empty() {
            Some(("没有匹配模型，请修改搜索内容。".into(), theme.gray))
        } else {
            None
        }
    });
    if let Some((text, color)) = status {
        label(
            frame,
            Rect {
                height: 1,
                ..content
            },
            &text,
            Style::new().fg(color),
        );
        content.y += 1;
        content.height = content.height.saturating_sub(1);
    }
    let mut footer = draw_fields(frame, panel, theme, content, hits);
    if panel.edit.is_none() {
        footer.hint = labels::hint_layout(
            &[
                ("[]", "切页"),
                ("tab", "供应商/详情"),
                ("↑↓", "移动"),
                ("enter/space", "编辑/勾选"),
                ("esc", "返回"),
            ],
            usize::from(body.width),
        );
    }
    footer
}

pub(super) fn draw_provider_draft(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> Footer {
    let draft = panel.providers.draft.as_mut().expect("新建表单");
    label(
        frame,
        Rect { height: 1, ..body },
        "添加供应商",
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    let list = Rect::new(
        body.x,
        body.y + 2,
        body.width,
        body.height.saturating_sub(2),
    );
    let height = usize::from(list.height);
    const CONTROLS: usize = 6;
    draft.offset = if draft.scroll.reveal_selection {
        window_offset(draft.offset, draft.focus, height, CONTROLS)
    } else {
        draft.offset.min(CONTROLS.saturating_sub(height))
    };
    draft.scroll.max_offset = CONTROLS.saturating_sub(height);
    draft.scroll.reveal_selection = false;
    for index in (draft.offset..CONTROLS).take(height) {
        let area = Rect::new(list.x, list.y + cells(index - draft.offset), list.width, 1);
        let selected = draft.focus == index;
        let style = if selected {
            Style::new().bg(theme.bg_visual).fg(theme.text_primary)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        let title = [
            "标识",
            "API 格式",
            "服务地址",
            "API 密钥",
            "添加并获取模型",
            "仅添加供应商",
        ][index];
        label(
            frame,
            area,
            &format!("{} {title}", if selected { "❯" } else { " " }),
            style,
        );
        let start = 14.min(area.width);
        let input_area = Rect::new(area.x + start, area.y, area.width.saturating_sub(start), 1);
        if index == 1 {
            label(frame, input_area, APIS[draft.api].1, style);
        } else if let Some(input) = match index {
            0 => Some(&draft.id),
            2 => Some(&draft.url),
            3 => Some(&draft.key),
            _ => None,
        } {
            let (text, column) = edit_line(input, usize::from(input_area.width));
            label(frame, input_area, &text, style);
            if selected && input_area.width > 0 && draft.pending.is_none() {
                frame.set_cursor_position((
                    input_area.x + cells(column).min(input_area.width - 1),
                    area.y,
                ));
            }
        }
        hits.push((area, Hit::ProviderInput(index)));
    }
    Footer {
        description: if draft.pending.is_some() {
            "正在保存供应商…"
        } else {
            "地址示例：https://api.example.com/v1；本地服务无需密钥时可留空。"
        }
        .into(),
        source: None,
        hint: labels::hint_layout(
            &[
                ("tab/↑↓", "切换字段"),
                ("←→", "切换协议"),
                ("f6", "显示密钥"),
                ("ctrl+s", "添加并获取"),
                ("esc", "取消"),
            ],
            usize::from(body.width),
        ),
    }
}

/// 维护预览：摘要 + 可滚动的删除清单。
pub(super) fn draw_maintenance(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> Footer {
    let View::Maintenance { report, offset } = &panel.view else {
        return Footer::default();
    };
    let summary = format!(
        "{}清理 {} 个临时文件（{}），保留 {} 项。",
        if report.applied {
            "维护完成："
        } else {
            "预计"
        },
        report.removed.len(),
        short_bytes(report.bytes),
        report.protected
    );
    label(
        frame,
        Rect { height: 1, ..body },
        &summary,
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    let list = Rect::new(
        body.x,
        body.y + 2,
        body.width,
        body.height.saturating_sub(2),
    );
    let height = usize::from(list.height);
    let total = report.removed.len();
    let offset = (*offset).min(total.saturating_sub(height));
    for (row, path) in report.removed.iter().skip(offset).take(height).enumerate() {
        label(
            frame,
            Rect::new(list.x, list.y + cells(row), list.width, 1),
            &format!("删除 {path}"),
            Style::new().fg(theme.text_secondary),
        );
    }
    scrollbar(frame, list, offset, total, theme, Some((hits, Hit::Scroll)));
    if let View::Maintenance {
        offset: current, ..
    } = &mut panel.view
    {
        *current = offset;
    }
    // 执行与返回都在工具栏上；清单溢出时才提示可以滚动。
    Footer {
        description: "只清理过期且没有被会话引用的临时文件。".into(),
        source: None,
        hint: if total > height {
            labels::hint_layout(&[("↑↓", "浏览清单")], usize::from(body.width))
        } else {
            Default::default()
        },
    }
}

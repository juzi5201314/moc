//! 模型选择器：锚定触发标签的列表卡 + 详情卡 + 下拉卡三层不透明浮层。

use super::*;
use crate::modal::{
    DetailRow, FUSION_ENTRY_SELECTOR, MENU_PAGE, MenuItems, ModelEntry, filtered_entries,
};

/// 列表卡外宽（列）；窄屏夹到屏宽。
const LIST_WIDTH: u16 = 52;
/// 详情卡外宽（列）。
const DETAIL_WIDTH: u16 = 38;

/// 未过滤列表的自然正文行数：搜索行 + 分组标题行 + 条目行。
/// 按空 query 计算，输入搜索时卡片高度不跳。
fn list_natural_rows(app: &App, modal: &Modal) -> usize {
    let Modal::Model {
        entries, picker, ..
    } = modal
    else {
        return 1;
    };
    let filtered = filtered_entries(entries, "");
    let mut groups = 0usize;
    let mut last_group = "";
    for (_, entry) in &filtered {
        let group = entry_group(app, entry);
        if group != last_group {
            groups += 1;
            last_group = group;
        }
    }
    // 状态行（切换中/错误）最多再占一行。
    1 + groups + filtered.len() + usize::from(picker.loading || picker.error.is_some())
}

/// 条目所属分组名；与列表行的分组逻辑同一份。
fn entry_group<'a>(app: &App, entry: &'a ModelEntry) -> &'a str {
    if entry.selector == FUSION_ENTRY_SELECTOR {
        "模式"
    } else if entry.provider().is_empty() {
        "继承"
    } else if app.recent_models.rank(&entry.selector).is_some() {
        "最近使用"
    } else {
        entry.provider()
    }
}

/// 优先向上展开；空间不足时选择下方。无可见标签时贴输入框右上方。
fn popup_rect(screen: Rect, anchor: Rect, natural_rows: usize) -> Rect {
    let width = LIST_WIDTH.min(screen.width);
    let above = anchor.y.saturating_sub(screen.y).min(screen.height);
    let below = screen.bottom().saturating_sub(anchor.bottom());
    // 卡片高度按内容收缩：正文自然行数 + 两行边框 + 一行提示条，上限 18。
    let wanted = u16::try_from(natural_rows + 3).unwrap_or(u16::MAX).min(18);
    let up = above >= wanted || above >= below;
    let height = wanted.min(if up { above } else { below });
    Rect::new(
        anchor
            .right()
            .saturating_sub(width)
            .clamp(screen.x, screen.right().saturating_sub(width)),
        if up {
            anchor.y.saturating_sub(height)
        } else {
            anchor.bottom()
        },
        width,
        height,
    )
}

/// 卡片不透明底 + 圆角边框 + 标题；正文区矩形（边框 + 左右内边距之后）。
fn draw_panel(frame: &mut Frame, rect: Rect, title: &str, theme: &Theme) -> Rect {
    overlay::paint_opaque(
        frame.buffer_mut(),
        rect,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    let width = overlay::content_width(rect.width);
    let panel = overlay::panel(theme).title(overlay::title_span(title, width, theme));
    frame.render_widget(panel, rect);
    Rect::new(
        rect.x + 2,
        rect.y + 1,
        rect.width.saturating_sub(4),
        rect.height.saturating_sub(2),
    )
}

/// 详情卡的横向落位：优先列表卡左侧（留 1 列空隙），其次右侧，
/// 两侧都放不下（窄屏）时盖在列表卡左部——那种情况下详情卡只在
/// 焦点落在它上面或下拉打开时才绘制。返回 `(x, 是否覆盖在列表卡上)`。
fn detail_x(screen: Rect, list: Rect) -> (u16, bool) {
    let width = DETAIL_WIDTH.min(screen.width);
    if list.x > screen.x + width {
        (list.x - width - 1, false)
    } else if list.right() + 1 + width <= screen.right() {
        (list.right() + 1, false)
    } else {
        (list.x, true)
    }
}

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let screen = frame.area();
    let anchor = if app.model_tag_area.is_empty() {
        Rect::new(
            screen.right().saturating_sub(1),
            screen.bottom().saturating_sub(1),
            1,
            1,
        )
    } else {
        app.model_tag_area
    };
    let modal = app.modal.as_ref().expect("模型浮层已打开");
    let rect = popup_rect(screen, anchor, list_natural_rows(app, modal));
    if rect.height < 4 {
        return;
    }
    let theme = app.theme;
    let width = overlay::content_width(rect.width);
    let rows = usize::from(rect.height.saturating_sub(3));
    let body = body(app, modal, width, rows);
    let footer = body.footer.unwrap_or_default();
    let content = draw_panel(frame, rect, &modal.title(), theme);
    frame.render_widget(Paragraph::new(body.lines), content);
    let y = rect.bottom() - 2;
    app.key_hits = footer.hits(content.x, y).collect();
    frame.render_widget(
        Paragraph::new(footer.line),
        Rect::new(content.x, y, content.width, 1),
    );
    app.modal_item_hits = body
        .hits
        .into_iter()
        .filter(|(_, row)| *row < rows)
        .map(|(index, row)| {
            (
                index,
                Rect::new(content.x, content.y + row as u16, content.width, 1),
            )
        })
        .collect();
    app.modal_card_area = rect;

    let Modal::Model {
        entries,
        picker,
        level,
        detail,
        ..
    } = modal
    else {
        return;
    };
    // 详情卡：外宽 38，顶部与列表卡对齐；高度 = 内容行数 + 边框，
    // 超出屏幕底就整体上移，仍超高就截断内容。
    let (dx, overlaid) = detail_x(screen, rect);
    let focused = detail.focus.is_some() || detail.menu.is_some();
    if overlaid && !focused {
        return;
    }
    let inner = overlay::content_width(DETAIL_WIDTH.min(screen.width));
    let (lines, row_hits) = detail_lines(app, modal, inner);
    let height = (u16::try_from(lines.len() + 2).unwrap_or(u16::MAX)).min(screen.height);
    let dy = rect
        .y
        .min(screen.bottom().saturating_sub(height))
        .max(screen.y);
    let drect = Rect::new(dx, dy, DETAIL_WIDTH.min(screen.width), height);
    let dcontent = draw_panel(frame, drect, "", theme);
    let visible = usize::from(dcontent.height);
    frame.render_widget(
        Paragraph::new(lines.into_iter().take(visible).collect::<Vec<_>>()),
        dcontent,
    );
    app.modal_detail_area = drect;
    app.modal_detail_hits = row_hits
        .iter()
        .filter(|(_, row)| *row < visible)
        .map(|(row, line)| {
            (
                *row,
                Rect::new(dcontent.x, dcontent.y + *line as u16, dcontent.width, 1),
            )
        })
        .collect();

    // 下拉卡：挂在所属详情行下方（放不下放上方），右缘对齐详情卡内容右缘。
    let Some(menu) = detail.menu.as_ref() else {
        return;
    };
    let entry = filtered_entries(entries, &picker.query)
        .get(picker.selected)
        .map(|(_, entry)| *entry);
    let items =
        crate::modal::menu_items(menu.row, entry, entries, level, &detail.fusion, &menu.query);
    // 等级下拉外宽 = 最长等级 + 6（至少 14）；模型下拉与详情卡同宽。
    let mwidth = match &items {
        MenuItems::Models { .. } => DETAIL_WIDTH.min(screen.width),
        MenuItems::Levels { levels, .. } => {
            let longest = levels
                .iter()
                .map(|level| str_width(level))
                .max()
                .unwrap_or(0);
            u16::try_from(longest + 6)
                .unwrap_or(u16::MAX)
                .max(14)
                .min(screen.width)
        }
    };
    let minner = overlay::content_width(mwidth);
    let (mlines, hits) = menu_lines(&items, menu, minner, theme);
    let mheight = u16::try_from(mlines.len() + 2)
        .unwrap_or(u16::MAX)
        .min(screen.height);
    let row_y = row_hits
        .iter()
        .find(|(row, _)| *row == menu.row)
        .map(|(_, line)| dcontent.y + *line as u16)
        .unwrap_or(dcontent.y);
    let my = if row_y + 1 + mheight <= screen.bottom() {
        row_y + 1
    } else {
        row_y.saturating_sub(mheight)
    }
    .clamp(screen.y, screen.bottom().saturating_sub(mheight));
    let right = dcontent.right().min(screen.right());
    let mx = right.saturating_sub(mwidth).max(screen.x);
    let mrect = Rect::new(
        mx,
        my,
        mwidth.min(screen.right().saturating_sub(mx)),
        mheight,
    );
    let mcontent = draw_panel(frame, mrect, "", theme);
    let mvisible = usize::from(mcontent.height);
    frame.render_widget(
        Paragraph::new(mlines.into_iter().take(mvisible).collect::<Vec<_>>()),
        mcontent,
    );
    app.modal_menu_area = mrect;
    app.modal_menu_hits = hits
        .into_iter()
        .filter(|(_, row)| *row < mvisible)
        .map(|(index, row)| {
            (
                index,
                Rect::new(mcontent.x, mcontent.y + row as u16, mcontent.width, 1),
            )
        })
        .collect();
}

/// 下拉卡的正文行：模型下拉首行是搜索行，候选用 `model_row` 同样式；
/// 等级下拉竖排全部档位。返回 `(行, (过滤后候选下标, 行号))`。
fn menu_lines(
    items: &MenuItems,
    menu: &crate::modal::MenuState,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let mut lines = Vec::new();
    let mut hits = Vec::new();
    match items {
        MenuItems::Levels { levels, current } => {
            for (index, level) in levels.iter().enumerate() {
                hits.push((index, lines.len()));
                let mark = if *current == Some(index) { " ✓" } else { "" };
                lines.push(clip_line(
                    overlay::list_row(
                        &format!("{level}{mark}"),
                        index == menu.selected,
                        width,
                        theme,
                    ),
                    width,
                ));
            }
        }
        MenuItems::Models { entries, current } => {
            lines.push(clip_line(search_line(&menu.query, width, theme), width));
            let offset = window_offset(menu.offset, menu.selected, MENU_PAGE, entries.len());
            if entries.is_empty() {
                lines.push(Line::from(Span::styled(
                    fit("没有匹配的模型", width),
                    Style::new().fg(theme.gray),
                )));
            }
            for (position, (_, entry)) in entries.iter().enumerate().skip(offset).take(MENU_PAGE) {
                hits.push((position, lines.len()));
                lines.push(model_row(
                    entry,
                    position == menu.selected,
                    *current == Some(position),
                    &menu.query,
                    width,
                    theme,
                ));
            }
        }
    }
    (lines, hits)
}

pub(super) fn body(app: &App, modal: &Modal, width: usize, max_lines: usize) -> ModalBody {
    let Modal::Model {
        entries,
        current,
        picker,
        detail,
        target,
        ..
    } = modal
    else {
        unreachable!()
    };
    let theme = app.theme;
    let filtered = filtered_entries(entries, &picker.query);
    let highlighted = filtered.get(picker.selected).map(|(_, entry)| *entry);
    let rows = crate::modal::detail_rows(highlighted, entries, target, &detail.fusion);
    let current_id = if crate::modal::parse_fusion(current).is_some() {
        FUSION_ENTRY_SELECTOR
    } else {
        current
            .rsplit_once(':')
            .map_or(current.as_str(), |(id, _)| id)
    };
    let mut body = ModalBody::default();
    let count = format!(
        " {}/{}",
        if filtered.is_empty() {
            0
        } else {
            picker.selected + 1
        },
        filtered.len()
    );
    let mut search = search_line(
        &picker.query,
        width.saturating_sub(str_width(&count)),
        theme,
    );
    search = fit_line(search, width.saturating_sub(str_width(&count)));
    search
        .spans
        .push(Span::styled(count, Style::new().fg(theme.gray)));
    body.lines.push(search);
    let fusion_index = filtered
        .iter()
        .position(|(_, entry)| entry.selector == FUSION_ENTRY_SELECTOR);
    let pinned_fusion = fusion_index.filter(|_| max_lines >= 6);
    if let Some(index) = pinned_fusion {
        body.hits.push((index, body.lines.len()));
        body.lines.push(clip_line(
            overlay::list_row(
                if current_id == FUSION_ENTRY_SELECTOR {
                    "Fusion ✓"
                } else {
                    "Fusion"
                },
                picker.selected == index,
                width,
                theme,
            ),
            width,
        ));
    }
    let header_rows = body.lines.len();
    let fusion = highlighted.is_some_and(|entry| entry.selector == FUSION_ENTRY_SELECTOR);
    let status = if picker.loading {
        Some("正在切换模型…".to_owned())
    } else {
        picker.error.as_ref().map(|error| format!("错误：{error}"))
    };
    let available = max_lines.saturating_sub(header_rows);
    let status_rows = usize::from(status.is_some() && available >= 3);
    let visible = available.saturating_sub(status_rows).max(1);
    let mut list = Vec::<(Option<usize>, Line<'static>)>::new();
    let mut last_group = String::new();
    for (index, (_, entry)) in filtered.iter().enumerate() {
        if pinned_fusion == Some(index) {
            continue;
        }
        let group = entry_group(app, entry);
        if picker.query.trim().is_empty() && group != last_group && visible >= 4 {
            list.push((
                None,
                Line::from(Span::styled(
                    format!("─ {group}"),
                    Style::new().fg(theme.gray),
                )),
            ));
            last_group = group.to_owned();
        }
        list.push((
            Some(index),
            model_row(
                entry,
                index == picker.selected,
                entry.selector == current_id,
                &picker.query,
                width,
                theme,
            ),
        ));
    }
    let selected_row = list
        .iter()
        .position(|(index, _)| *index == Some(picker.selected))
        .unwrap_or(0);
    let initial_offset = list
        .iter()
        .position(|(index, _)| *index == Some(picker.offset))
        .unwrap_or(0)
        .saturating_sub(1);
    let offset = window_offset(initial_offset, selected_row, visible, list.len());
    for row in 0..visible {
        let line = if let Some((index, line)) = list.get(offset + row) {
            if let Some(index) = index {
                body.hits.push((*index, body.lines.len()));
            }
            fit_line(line.clone(), width)
        } else if row == 0 && filtered.is_empty() {
            Line::from(Span::styled(
                fit("没有匹配的模型", width),
                Style::new().fg(theme.gray),
            ))
        } else {
            Line::from(" ".repeat(width))
        };
        body.lines.push(line);
    }
    if status_rows > 0 {
        body.lines.push(Line::from(Span::styled(
            truncate(status.unwrap(), width).into_owned(),
            Style::new().fg(if picker.loading {
                theme.accent_running
            } else {
                theme.accent_error
            }),
        )));
    }
    // 底部提示按焦点分层：下拉 > 详情卡 > 列表。
    let items: Vec<(&str, &str)> = if let Some(menu) = detail.menu.as_ref() {
        let mut items = vec![("↑↓", "选择"), ("enter", "确定"), ("esc", "收起")];
        if matches!(menu.row, DetailRow::Lead | DetailRow::Sidekick) {
            items.push(("", "输入搜索"));
        }
        items
    } else if let Some(focus) = detail.focus {
        let enter = match focus {
            DetailRow::Fast => "切换",
            DetailRow::Apply => "应用",
            _ => "展开",
        };
        vec![("↑↓", "移动"), ("enter", enter), ("left", "返回")]
    } else {
        let mut items = vec![("↑↓", "选择")];
        if highlighted.is_some_and(|entry| {
            entry.selector != FUSION_ENTRY_SELECTOR && !entry.levels.is_empty()
        }) {
            items.push(("tab", "等级"));
        }
        if !rows.is_empty() {
            items.push(("right", "详情"));
        }
        let enter = if fusion {
            "应用"
        } else if picker.error.is_some() {
            "重试"
        } else {
            "确认"
        };
        items.push(("enter", enter));
        items.push(("esc", "关闭"));
        items
    };
    body.footer = Some(overlay::footer(&items, width, theme));
    body.lines.truncate(max_lines);
    body
}

/// 详情卡正文：返回行与每个控制行所在的行号（鼠标命中换算用）。
pub(super) fn detail_lines(
    app: &App,
    modal: &Modal,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(DetailRow, usize)>) {
    let Modal::Model {
        entries,
        picker,
        detail,
        target,
        ..
    } = modal
    else {
        unreachable!()
    };
    let theme = app.theme;
    let filtered = filtered_entries(entries, &picker.query);
    let Some(entry) = filtered.get(picker.selected).map(|(_, entry)| *entry) else {
        return (Vec::new(), Vec::new());
    };
    let rows = crate::modal::detail_rows(Some(entry), entries, target, &detail.fusion);
    let mut lines = Vec::new();
    let mut hits = Vec::new();
    let push_controls = |lines: &mut Vec<Line<'static>>, hits: &mut Vec<(DetailRow, usize)>| {
        for row in &rows {
            hits.push((*row, lines.len()));
            lines.push(control_line(
                app,
                modal,
                *row,
                entry,
                &detail.fusion,
                width,
                theme,
            ));
        }
    };
    if entry.selector == FUSION_ENTRY_SELECTOR {
        lines.push(Line::from(Span::styled(
            "Fusion",
            Style::new()
                .fg(theme.accent_model)
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(wrapped_lines(
            "前沿模型负责规划、调查与评审，高性价比模型执行实现与验证",
            width,
            Style::new().fg(theme.text_secondary),
        ));
        if !rows.is_empty() {
            lines.push(Line::default());
            push_controls(&mut lines, &mut hits);
        }
        if let Some(lead) = crate::modal::side_entry(entries, &detail.fusion.lead) {
            let specs = model_meta_lines(theme, lead, width);
            if !specs.is_empty() {
                lines.push(Line::default());
                lines.extend(specs);
            }
        }
    } else if entry.provider().is_empty() {
        // 角色伪值只有一行标签。
        lines.push(Line::from(Span::styled(
            entry.label.clone(),
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            entry.name().to_owned(),
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            entry.provider().to_owned(),
            Style::new().fg(theme.text_secondary),
        )));
        if !rows.is_empty() {
            lines.push(Line::default());
            push_controls(&mut lines, &mut hits);
        }
        let specs = model_meta_lines(theme, entry, width);
        if !specs.is_empty() {
            lines.push(Line::default());
            lines.extend(specs);
        }
    }
    (lines, hits)
}

/// 详情卡的一行控制行：左标签右值（值右对齐）；focus 行整行反白。
fn control_line(
    app: &App,
    modal: &Modal,
    row: DetailRow,
    entry: &ModelEntry,
    fusion: &crate::modal::FusionDraft,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let Modal::Model {
        entries,
        current,
        level,
        detail,
        ..
    } = modal
    else {
        unreachable!()
    };
    let focused = detail.focus == Some(row);
    let open = detail.menu.as_ref().is_some_and(|menu| menu.row == row);
    // 键盘聚焦或该行下拉展开时标记、标签与值都加粗，整行不铺底色。
    let active = focused || open;
    let active_style = |color: Color| {
        let style = Style::new().fg(if active { theme.accent_user } else { color });
        if active {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    };
    let fast = app
        .session
        .as_ref()
        .is_some_and(|session| session.fast_mode);
    // `›` 常驻在第 0 列，后接 1 个空格再接标签；值右对齐到正文右缘。
    let (label, value, value_color): (String, String, Color) = match row {
        DetailRow::Level => (
            "思考等级".to_owned(),
            level
                .resolve(&entry.levels)
                .unwrap_or_else(|| "—".to_owned()),
            theme.accent_model,
        ),
        DetailRow::Fast => (
            "Fast Mode".to_owned(),
            if fast {
                "● 开".to_owned()
            } else {
                "○ 关".to_owned()
            },
            if fast {
                theme.accent_success
            } else {
                theme.gray
            },
        ),
        DetailRow::Lead | DetailRow::Sidekick => {
            let side = if row == DetailRow::Lead {
                &fusion.lead
            } else {
                &fusion.sidekick
            };
            let (name, empty) = side_label(entries, side);
            (
                if row == DetailRow::Lead {
                    "Lead".to_owned()
                } else {
                    "Sidekick".to_owned()
                },
                name,
                if empty {
                    theme.gray
                } else {
                    theme.accent_model
                },
            )
        }
        DetailRow::LeadLevel | DetailRow::SidekickLevel => {
            let side = if row == DetailRow::LeadLevel {
                &fusion.lead
            } else {
                &fusion.sidekick
            };
            let value = crate::modal::side_level(entries, side).unwrap_or_else(|| "—".to_owned());
            ("  思考".to_owned(), value, theme.accent_model)
        }
        DetailRow::Apply => {
            let applied = !fusion.lead.is_empty()
                && !fusion.sidekick.is_empty()
                && crate::modal::fusion_selector(&fusion.lead, &fusion.sidekick) == *current;
            (
                "应用".to_owned(),
                if applied {
                    "✓".to_owned()
                } else {
                    String::new()
                },
                theme.accent_success,
            )
        }
    };
    // 值超宽时只截断值本身，标签与值之间至少 1 格空隙。
    let value_budget = width.saturating_sub(4 + str_width(&label)).max(1);
    let value = truncate(&value, value_budget).into_owned();
    let gap = width
        .saturating_sub(2 + str_width(&label) + str_width(&value))
        .max(1);
    fit_line(
        Line::from(vec![
            Span::styled(
                "› ",
                active_style(if active {
                    theme.accent_user
                } else {
                    theme.gray_dim
                }),
            ),
            Span::styled(label, active_style(theme.text_secondary)),
            Span::styled(" ".repeat(gap), Style::new()),
            Span::styled(value, active_style(value_color)),
        ]),
        width,
    )
}

/// Fusion 草稿一侧的展示名：找到条目用 `name()`，找不到给裸 id，空串给「未选择」。
fn side_label(entries: &[ModelEntry], selector: &str) -> (String, bool) {
    if selector.is_empty() {
        return ("未选择".to_owned(), true);
    }
    if let Some(entry) = crate::modal::side_entry(entries, selector) {
        return (entry.name().to_owned(), false);
    }
    (
        selector
            .rsplit_once(':')
            .map_or(selector, |(id, _)| id)
            .to_owned(),
        false,
    )
}

/// 规格键值行：context / output / input / price / cache，只出现有值的项；
/// 标签灰、值次要色，普通模型与 Fusion 的 lead 共用。
fn model_meta_lines(theme: &Theme, entry: &ModelEntry, width: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<(&'static str, String)> = Vec::with_capacity(5);
    if let Some(window) = entry.context_window {
        rows.push(("context", crate::render::format_tokens(window)));
    }
    if let Some(tokens) = entry.max_output_tokens {
        rows.push(("output", crate::render::format_tokens(u64::from(tokens))));
    }
    if !entry.inputs.is_empty() {
        let inputs: Vec<&str> = entry
            .inputs
            .iter()
            .map(|input| match input {
                moc_protocol::methods::ModelInput::Text => "text",
                moc_protocol::methods::ModelInput::Image => "image",
            })
            .collect();
        rows.push(("input", inputs.join(" · ")));
    }
    if let Some(pricing) = &entry.pricing {
        rows.push((
            "price",
            format!(
                "{} → {}/M",
                meta_price(pricing.input),
                meta_price(pricing.output)
            ),
        ));
        // 缓存读写各自选填，只把配置了的那一项画出来。
        let cache: Vec<String> = [pricing.cache_read, pricing.cache_write]
            .into_iter()
            .flatten()
            .map(meta_price)
            .collect();
        if !cache.is_empty() {
            rows.push(("cache", format!("{}/M", cache.join(" / "))));
        }
    }
    rows.into_iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(fit(label, 7), Style::new().fg(theme.gray)),
                Span::raw("  "),
                Span::styled(
                    fit(&value, width.saturating_sub(9)),
                    Style::new().fg(theme.text_secondary),
                ),
            ])
        })
        .collect()
}

/// 每百万 token 的单价：最多三位小数，去掉尾随零，`$1`、`$0.075`、`$1.25`。
fn meta_price(value: f64) -> String {
    let text = format!("{value:.3}");
    format!("${}", text.trim_end_matches('0').trim_end_matches('.'))
}

fn model_row(
    entry: &ModelEntry,
    selected: bool,
    current: bool,
    query: &str,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let style = if selected {
        Style::new()
            .fg(theme.accent_user)
            .bg(theme.bg_visual)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.text_primary)
    };
    let provider_width = str_width(entry.provider()).min(width / 3);
    let name_width = width.saturating_sub(5 + provider_width);
    let name = fit(entry.name(), name_width);
    let mut spans = vec![Span::styled(if selected { "❯ " } else { "  " }, style)];
    spans.extend(highlight(&name, query, style, theme));
    spans.push(Span::styled(
        format!(
            " {} {}",
            fit(entry.provider(), provider_width),
            if current { "✓" } else { " " }
        ),
        style.fg(theme.text_secondary),
    ));
    fit_line(Line::from(spans), width)
}

/// 多词在可见名称中高亮；连续命中优先，否则标出子序列命中的字符。
fn highlight(text: &str, query: &str, style: Style, theme: &Theme) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut marked = vec![false; chars.len()];
    for term in query.to_lowercase().split_whitespace() {
        let needle: Vec<char> = term.chars().collect();
        let lower: Vec<char> = chars
            .iter()
            .map(|ch| ch.to_lowercase().next().unwrap_or(*ch))
            .collect();
        if let Some(start) = lower
            .windows(needle.len())
            .position(|window| window == needle)
        {
            marked[start..start + needle.len()].fill(true);
        } else {
            let mut positions = Vec::new();
            let mut next = 0;
            for ch in &needle {
                if let Some(index) = lower[next..].iter().position(|candidate| candidate == ch) {
                    next += index;
                    positions.push(next);
                    next += 1;
                } else {
                    positions.clear();
                    break;
                }
            }
            for index in positions {
                marked[index] = true;
            }
        }
    }
    chars
        .into_iter()
        .zip(marked)
        .map(|(ch, marked)| {
            Span::styled(
                ch.to_string(),
                if marked {
                    style
                        .fg(theme.accent_model)
                        .add_modifier(Modifier::UNDERLINED)
                } else {
                    style
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn model_picker_anchor_stays_inside_terminal() {
        for (w, h) in [(120, 40), (80, 24), (40, 12), (20, 6)] {
            let screen = Rect::new(0, 0, w, h);
            for anchor in [
                Rect::new(1, h - 1, 4, 1),
                Rect::new(w - 5, h - 1, 4, 1),
                Rect::new(w - 5, 0, 4, 1),
            ] {
                let rect = popup_rect(screen, anchor, 20);
                assert_eq!(rect.intersection(screen), rect);
                assert!(rect.bottom() <= anchor.y || rect.y >= anchor.bottom());
                assert!(rect.height >= 4);
            }
        }
    }

    /// 背景不被压暗；列表卡底边贴模型标签行；详情卡与列表卡不相交；
    /// 等级下拉打开时 6 档全部落在屏内；元信息变化不移动列表卡。
    #[test]
    fn model_picker_preserves_background_and_separates_detail_hits() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let entry = ModelEntry {
            selector: "p/model".into(),
            label: "p/model".into(),
            display_name: "模型示例".into(),
            levels: crate::modal::DEFAULT_LEVELS
                .iter()
                .map(|level| (*level).into())
                .collect(),
            context_window: Some(128_000),
            max_output_tokens: None,
            inputs: Vec::new(),
            pricing: None,
            fast_mode: false,
        };
        app.modal = Some(Modal::Model {
            entries: vec![entry],
            current: "p/model:max".into(),
            picker: Default::default(),
            level: Default::default(),
            detail: Default::default(),
            target: crate::modal::ModelTarget::Session,
        });
        app.model_tag_area = Rect::new(90, 28, 8, 1);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_string(0, 0, "正文", Style::new().fg(Color::Red));
                super::super::draw_modal(frame, &mut app);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "正");
        assert_eq!(terminal.backend().buffer()[(0, 0)].fg, Color::Red);
        assert_eq!(app.modal_card_area.bottom(), app.model_tag_area.y);
        assert!(
            app.modal_detail_area
                .intersection(app.modal_card_area)
                .is_empty(),
            "详情卡与列表卡不相交"
        );

        // Tab 展开等级下拉的状态：6 档全部有命中区且都在屏内。
        if let Some(Modal::Model { detail, .. }) = app.modal.as_mut() {
            detail.menu = Some(crate::modal::MenuState {
                row: DetailRow::Level,
                selected: 5,
                offset: 0,
                query: String::new(),
            });
        }
        terminal
            .draw(|frame| super::super::draw_modal(frame, &mut app))
            .unwrap();
        let screen = Rect::new(0, 0, 110, 30);
        assert_eq!(
            app.modal_menu_hits.len(),
            crate::modal::DEFAULT_LEVELS.len(),
            "等级下拉给出全部档位"
        );
        for (_, area) in &app.modal_menu_hits {
            assert_eq!(area.intersection(screen), *area, "下拉候选都在屏内");
        }

        let before = app.modal_card_area;
        if let Some(Modal::Model { entries, .. }) = &mut app.modal {
            entries[0].context_window = None;
        }
        terminal
            .draw(|frame| super::super::draw_modal(frame, &mut app))
            .unwrap();
        assert_eq!(app.modal_card_area, before, "元信息变化不能移动浮层");
    }

    /// 详情卡 Level 行以 `›` 开头、完整显示最长的等级名，不被裁成省略号。
    #[test]
    fn detail_level_chip_shows_full_level_name() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(Theme::global(), events);
        let modal = Modal::Model {
            entries: vec![ModelEntry {
                selector: "p/model".into(),
                label: "p/model".into(),
                display_name: "Model".into(),
                levels: crate::modal::DEFAULT_LEVELS
                    .iter()
                    .map(|level| (*level).into())
                    .collect(),
                context_window: None,
                max_output_tokens: None,
                inputs: Vec::new(),
                pricing: None,
                fast_mode: false,
            }],
            current: "p/model:max".into(),
            picker: Default::default(),
            level: Default::default(),
            detail: Default::default(),
            target: crate::modal::ModelTarget::Session,
        };
        let (lines, hits) = detail_lines(&app, &modal, 34);
        let row = hits
            .iter()
            .find(|(row, _)| *row == DetailRow::Level)
            .map(|(_, line)| *line)
            .expect("思考等级控制行");
        let text: String = lines[row]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.starts_with("›"), "{text}");
        assert!(text.contains("max"), "{text}");
        assert!(!text.contains('…'), "{text}");
    }
}

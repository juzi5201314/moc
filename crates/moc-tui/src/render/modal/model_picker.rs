//! 模型选择器：锚定触发标签的列表卡 + 详情卡 + 下拉卡三层不透明浮层。

use super::*;
use crate::modal::{
    DetailRow, FUSION_ENTRY_SELECTOR, MENU_PAGE, MenuItems, ModelEntry, filtered_entries,
};
use rust_i18n::t;

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
    let mut last_group = String::new();
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
fn entry_group(app: &App, entry: &ModelEntry) -> String {
    if entry.selector == FUSION_ENTRY_SELECTOR {
        t!("tui.picker.group_mode").into_owned()
    } else if entry.provider().is_empty() {
        t!("tui.picker.group_inherit").into_owned()
    } else if app.recent_models.rank(&entry.selector).is_some() {
        t!("tui.picker.group_recent").into_owned()
    } else {
        entry.provider().to_owned()
    }
}

/// 优先向上展开；空间不足时选择下方。无可见标签时贴输入框右上方。
/// `align_left`（计划芯片锚点）：卡片左缘对齐锚点左缘；否则右缘对齐。
fn popup_rect(screen: Rect, anchor: Rect, natural_rows: usize, align_left: bool) -> Rect {
    let width = LIST_WIDTH.min(screen.width);
    let above = anchor.y.saturating_sub(screen.y).min(screen.height);
    let below = screen.bottom().saturating_sub(anchor.bottom());
    // 卡片高度按内容收缩：正文自然行数 + 两行边框 + 一行提示条，上限 18。
    let wanted = u16::try_from(natural_rows + 3).unwrap_or(u16::MAX).min(18);
    let up = above >= wanted || above >= below;
    let height = wanted.min(if up { above } else { below });
    let x = if align_left {
        anchor
            .x
            .clamp(screen.x, screen.right().saturating_sub(width))
    } else {
        anchor
            .right()
            .saturating_sub(width)
            .clamp(screen.x, screen.right().saturating_sub(width))
    };
    Rect::new(
        x,
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
    // 空标题不设 title，否则边框会画出 `╭  ───` 的双空格标题。
    let panel = overlay::panel(theme);
    let panel = if title.is_empty() {
        panel
    } else {
        panel.title(overlay::title_span(title, width, theme))
    };
    frame.render_widget(panel, rect);
    Rect::new(
        rect.x + 2,
        rect.y + 1,
        rect.width.saturating_sub(4),
        rect.height.saturating_sub(2),
    )
}

/// 详情卡的横向落位：默认优先列表卡左侧，计划芯片锚点（`prefer_right`）优先
/// 右侧；两侧留 1 列空隙，都放不下（窄屏）时盖在列表卡左部——那种情况下详情卡
/// 只在焦点落在它上面或下拉打开时才绘制。返回 `(x, 是否覆盖在列表卡上)`。
fn detail_x(screen: Rect, list: Rect, prefer_right: bool) -> (u16, bool) {
    let width = DETAIL_WIDTH.min(screen.width);
    let left = || list.x.checked_sub(width + 1).filter(|x| *x >= screen.x);
    let right = || (list.right() + 1 + width <= screen.right()).then_some(list.right() + 1);
    let spot = if prefer_right {
        right().or_else(left)
    } else {
        left().or_else(right)
    };
    match spot {
        Some(x) => (x, false),
        None => (list.x, true),
    }
}

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    draw_cards(frame, app);
    draw_hover(frame, app);
}

/// 悬停只改变背景；选中标记与详情仍由点击或键盘决定。
fn draw_hover(frame: &mut Frame, app: &App) {
    let Some(Modal::Model { picker, detail, .. }) = app.modal.as_ref() else {
        return;
    };
    let Some(pointer) = detail.pointer else {
        return;
    };
    let over_menu = app.modal_menu_area.contains(pointer);
    let over_detail = app.modal_detail_area.contains(pointer);
    let hovered = if over_menu {
        app.modal_menu_hits
            .iter()
            .find(|(index, area)| {
                area.contains(pointer)
                    && detail
                        .menu
                        .as_ref()
                        .is_some_and(|menu| menu.selected != *index)
            })
            .map(|(_, area)| area)
    } else if over_detail {
        app.modal_detail_hits
            .iter()
            .find(|(row, area)| area.contains(pointer) && detail.focus != Some(*row))
            .map(|(_, area)| area)
    } else {
        app.modal_item_hits
            .iter()
            .find(|(index, area)| area.contains(pointer) && picker.selected != *index)
            .map(|(_, area)| area)
    };
    if let Some(area) = hovered {
        for x in area.x..area.right() {
            let position = (x, area.y).into();
            // 窄屏浮层会覆盖列表的一部分，悬停底色不能盖到上层卡片。
            if !over_menu
                && (app.modal_menu_area.contains(position)
                    || (!over_detail && app.modal_detail_area.contains(position)))
            {
                continue;
            }
            frame.buffer_mut()[(x, area.y)].set_bg(app.theme.bg_hover);
        }
    }
}

fn draw_cards(frame: &mut Frame, app: &mut App) {
    let screen = frame.area();
    // 计划审阅的模型芯片锚点优先，其次才是输入框图例。
    let chip = app.model_picker_anchor.filter(|anchor| !anchor.is_empty());
    let anchor = chip.unwrap_or_else(|| {
        if app.model_tag_area.is_empty() {
            Rect::new(
                screen.right().saturating_sub(1),
                screen.bottom().saturating_sub(1),
                1,
                1,
            )
        } else {
            app.model_tag_area
        }
    });
    let modal = app.modal.as_ref().expect("模型浮层已打开");
    // 芯片锚点：列表卡左缘对齐芯片、详情卡在右；图例锚点维持右对齐原状。
    let rect = popup_rect(
        screen,
        anchor,
        list_natural_rows(app, modal),
        chip.is_some(),
    );
    if rect.height < 4 {
        return;
    }
    let theme = app.theme;
    let width = overlay::content_width(rect.width);
    let rows = usize::from(rect.height.saturating_sub(3));
    let (body, offset) = body(app, modal, width, rows);
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
    if let Some(Modal::Model { picker, detail, .. }) = app.modal.as_mut() {
        picker.offset = offset;
        detail.list_scroll.max_offset = body.scroll_max;
        detail.list_scroll.reveal_selection = false;
    }
    let modal = app.modal.as_ref().expect("模型浮层已打开");

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
    let (dx, overlaid) = detail_x(screen, rect, chip.is_some());
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
    // 列表卡与详情卡之间的 1 列空隙铺不透明底，免得漏出底下卡片的字符。
    if !overlaid {
        let top = rect.y.min(drect.y);
        let bottom = rect.bottom().max(drect.bottom());
        let gap_x = if drect.x > rect.x {
            rect.right()
        } else {
            drect.right()
        };
        overlay::paint_opaque(
            frame.buffer_mut(),
            Rect::new(gap_x, top, 1, bottom.saturating_sub(top)),
            Style::new().bg(theme.bg_base).fg(theme.text_primary),
        );
    }
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
    let (menu_body, menu_offset) = menu_lines(
        &items,
        menu,
        minner,
        usize::from(screen.height.saturating_sub(2)),
        theme,
    );
    let mheight = u16::try_from(menu_body.lines.len() + 2)
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
    frame.render_widget(Paragraph::new(menu_body.lines), mcontent);
    app.modal_menu_area = mrect;
    app.modal_menu_hits = menu_body
        .hits
        .into_iter()
        .filter(|(_, row)| *row < mvisible)
        .map(|(index, row)| {
            (
                index,
                Rect::new(mcontent.x, mcontent.y + row as u16, mcontent.width, 1),
            )
        })
        .collect();
    if let Some(Modal::Model { detail, .. }) = app.modal.as_mut()
        && let Some(menu) = detail.menu.as_mut()
    {
        menu.offset = menu_offset;
        menu.scroll.max_offset = menu_body.scroll_max;
        menu.scroll.reveal_selection = false;
    }
}

/// 下拉按实际高度分配可见窗口；模型下拉额外保留一行搜索框。
fn menu_lines(
    items: &MenuItems,
    menu: &crate::modal::MenuState,
    width: usize,
    max_lines: usize,
    theme: &Theme,
) -> (ModalBody, usize) {
    let models = matches!(items, MenuItems::Models { .. });
    let visible = max_lines
        .saturating_sub(usize::from(models))
        .min(if models { MENU_PAGE } else { items.len() })
        .max(1);
    let mut body = ModalBody {
        scroll_max: items.len().saturating_sub(visible),
        ..Default::default()
    };
    let offset = if menu.scroll.reveal_selection {
        window_offset(menu.offset, menu.selected, visible, items.len())
    } else {
        menu.offset.min(body.scroll_max)
    };
    match items {
        MenuItems::Levels { levels, current } => {
            for (index, level) in levels.iter().enumerate().skip(offset).take(visible) {
                body.hits.push((index, body.lines.len()));
                let mark = if *current == Some(index) { " ✓" } else { "" };
                body.lines.push(clip_line(
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
            body.lines
                .push(clip_line(search_line(&menu.query, width, theme), width));
            if entries.is_empty() {
                body.lines.push(Line::from(Span::styled(
                    fit(&t!("tui.modal.no_match_models"), width),
                    Style::new().fg(theme.gray),
                )));
            }
            for (position, (_, entry)) in entries.iter().enumerate().skip(offset).take(visible) {
                body.hits.push((position, body.lines.len()));
                body.lines.push(model_row(
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
    (body, offset)
}

pub(super) fn body(app: &App, modal: &Modal, width: usize, max_lines: usize) -> (ModalBody, usize) {
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
        Some(t!("tui.modal.switching_model").into_owned())
    } else {
        picker
            .error
            .as_ref()
            .map(|error| t!("tui.err.fmt", err = error.as_str()).into_owned())
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
    body.scroll_max = list.len().saturating_sub(visible);
    let selected_row = list
        .iter()
        .position(|(index, _)| *index == Some(picker.selected));
    let offset = match selected_row.filter(|_| detail.list_scroll.reveal_selection) {
        Some(row) => window_offset(picker.offset, row, visible, list.len()),
        None => picker.offset.min(body.scroll_max),
    };
    for row in 0..visible {
        let line = if let Some((index, line)) = list.get(offset + row) {
            if let Some(index) = index {
                body.hits.push((*index, body.lines.len()));
            }
            fit_line(line.clone(), width)
        } else if row == 0 && filtered.is_empty() {
            Line::from(Span::styled(
                fit(&t!("tui.modal.no_match_models"), width),
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
    let items: Vec<(&str, std::borrow::Cow<'_, str>)> = if let Some(menu) = detail.menu.as_ref() {
        let mut items = vec![
            ("↑↓", t!("tui.hint.choose")),
            ("enter", t!("tui.hint.ok")),
            ("esc", t!("tui.hint.collapse")),
        ];
        if matches!(menu.row, DetailRow::Lead | DetailRow::Sidekick) {
            items.push(("", t!("tui.hint.type_search")));
        }
        items
    } else if let Some(focus) = detail.focus {
        let enter = match focus {
            DetailRow::Fast => t!("tui.hint.switch"),
            DetailRow::Apply => t!("tui.hint.apply"),
            _ => t!("tui.hint.expand_row"),
        };
        vec![
            ("↑↓", t!("tui.hint.move")),
            ("enter", enter),
            ("left", t!("tui.hint.back")),
        ]
    } else {
        let mut items = vec![("↑↓", t!("tui.hint.choose"))];
        if highlighted.is_some_and(|entry| {
            entry.selector != FUSION_ENTRY_SELECTOR && !entry.levels.is_empty()
        }) {
            items.push(("tab", t!("tui.hint.level")));
        }
        if !rows.is_empty() {
            items.push(("right", t!("tui.hint.details")));
        }
        let enter = if fusion {
            t!("tui.hint.apply")
        } else if picker.error.is_some() {
            t!("tui.hint.retry")
        } else {
            t!("tui.hint.confirm")
        };
        items.push(("enter", enter));
        items.push(("esc", t!("tui.hint.close")));
        items
    };
    body.footer = Some(overlay::footer(&items, width, theme));
    body.lines.truncate(max_lines);
    (body, offset)
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
            &t!("tui.picker.fusion_desc"),
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
            t!("tui.picker.thinking_level").into_owned(),
            level
                .resolve(&entry.levels)
                .unwrap_or_else(|| "—".to_owned()),
            theme.accent_model,
        ),
        DetailRow::Fast => (
            "Fast Mode".to_owned(),
            if fast {
                t!("tui.picker.fast_on").into_owned()
            } else {
                t!("tui.picker.fast_off").into_owned()
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
            (
                t!("tui.picker.thinking_side").into_owned(),
                value,
                theme.accent_model,
            )
        }
        DetailRow::Apply => {
            let applied = !fusion.lead.is_empty()
                && !fusion.sidekick.is_empty()
                && crate::modal::fusion_selector(&fusion.lead, &fusion.sidekick) == *current;
            (
                t!("tui.hint.apply").into_owned(),
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
        return (t!("tui.picker.unselected").into_owned(), true);
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
mod tests;

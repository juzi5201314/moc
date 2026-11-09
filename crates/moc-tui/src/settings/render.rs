use super::controls::Entry;
use super::*;
use crate::render::MARGIN_X;
use crate::render::modal::window_offset;
use crate::render::labels::{self, SEP};
use crate::render::overlay;
use crate::render::wrap::{
    ELLIPSIS, clip_line, fit, fit_line, str_width, truncate, wrap_spans, wrap_text,
};
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};

/// 文件大小缩写（2411724 → 2.3 MB）。
fn short_bytes(value: u64) -> String {
    if value >= 1 << 20 {
        format!("{:.1} MB", value as f64 / f64::from(1 << 20))
    } else if value >= 1 << 10 {
        format!("{:.1} KB", value as f64 / f64::from(1 << 10))
    } else {
        format!("{value} B")
    }
}

fn cells(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// 单行文本的唯一出口：先铺 `style` 到整个 `area`，再写入超宽以 `…` 收尾的文本。
/// 设置页不把超宽内容交给 ratatui 静默裁掉。
fn label(frame: &mut Frame, area: Rect, text: &str, style: Style) {
    let area = area.intersection(frame.area());
    if area.is_empty() {
        return;
    }
    let buffer = frame.buffer_mut();
    buffer.set_style(area, style);
    let width = usize::from(area.width);
    buffer.set_stringn(area.x, area.y, truncate(text, width), width, Style::new());
}

/// 自上而下逐行写入已排好宽度的行。
fn paint_lines(frame: &mut Frame, area: Rect, lines: &[Line<'_>]) {
    let area = area.intersection(frame.area());
    for (row, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        frame
            .buffer_mut()
            .set_line(area.x, area.y + cells(row), line, area.width);
    }
}

/// 说明文字：按 `width` 折行，至多 `rows` 行，被截掉的部分在末行以 `…` 示意；
/// 反引号包住的片段按行内代码着色（目录描述自带 markdown 反引号）。
fn prose(text: &str, width: usize, rows: usize, style: Style, theme: &Theme) -> Vec<Line<'static>> {
    let spans: Vec<Span<'static>> = text
        .split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(index, part)| {
            let style = if index % 2 == 1 {
                style.fg(theme.path)
            } else {
                style
            };
            Span::styled(part.to_owned(), style)
        })
        .collect();
    let mut lines = wrap_spans(&spans, width);
    if lines.len() > rows {
        lines.truncate(rows);
        if let Some(last) = lines.pop() {
            let mut last = clip_line(last, width.saturating_sub(1));
            if !last.to_string().ends_with(ELLIPSIS) {
                last.spans.push(Span::styled(ELLIPSIS.to_string(), style));
            }
            lines.push(last);
        }
    }
    lines
}

/// 流式排一行芯片（选项卡、工具栏按钮）：放不下就换行，从不丢项。
/// 返回最后一行的 y。
fn flow(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    chips: impl IntoIterator<Item = (String, Style, Hit)>,
    hits: &mut Vec<(Rect, Hit)>,
) -> u16 {
    let mut x = area.x;
    let mut y = y;
    for (text, style, hit) in chips {
        let width = cells(str_width(&text)).min(area.width);
        if x > area.x && x.saturating_add(width) > area.right() {
            x = area.x;
            y += 1;
        }
        let chip = Rect::new(x, y, width, 1);
        label(frame, chip, &text, style);
        hits.push((chip, hit));
        x = x.saturating_add(width + 1);
    }
    y
}

/// 工具栏按钮：`(动作, 键位, 键码, 修饰键)`；键位写配置口径，经 [`labels::hints`] 规范。
type Button = (&'static str, &'static str, KeyCode, KeyModifiers);

fn toolbar(panel: &SettingsPanel) -> Vec<Button> {
    const NONE: KeyModifiers = KeyModifiers::NONE;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    if panel.provider_page() && matches!(panel.view, View::Fields) {
        if panel.providers.draft.is_some() {
            return vec![
                ("添加并获取", "ctrl+s", KeyCode::Char('s'), CTRL),
                ("取消", "esc", KeyCode::Esc, NONE),
            ];
        }
        return vec![
            ("新建", "n", KeyCode::Char('n'), NONE),
            ("获取模型", "f", KeyCode::Char('f'), NONE),
            ("手动添加", "a", KeyCode::Char('a'), NONE),
            ("高级参数", "f4", KeyCode::F(4), NONE),
            ("范围", "f2", KeyCode::F(2), NONE),
            ("删除供应商", "ctrl+x", KeyCode::Char('x'), CTRL),
        ];
    }
    match &panel.view {
        View::Fields => {
            let mut buttons = vec![
                ("撤回", "ctrl+z", KeyCode::Char('z'), CTRL),
                ("重做", "ctrl+y", KeyCode::Char('y'), CTRL),
                ("范围", "f2/s", KeyCode::F(2), NONE),
                ("高级", "f4/v", KeyCode::F(4), NONE),
                ("搜索", "/", KeyCode::Char('/'), NONE),
            ];
            if !panel.filter.is_empty() {
                buttons.push(("清空过滤", "esc", KeyCode::Esc, NONE));
            }
            buttons.extend([
                ("展开全部", "e", KeyCode::Char('e'), NONE),
                ("折叠全部", "c", KeyCode::Char('c'), NONE),
            ]);
            match TABS[panel.tab] {
                "模型角色" => buttons.push(("默认规则", "ctrl+d", KeyCode::Char('d'), CTRL)),
                "MCP" => buttons.push(("添加服务", "a", KeyCode::Char('a'), NONE)),
                "会话" => buttons.push(("维护预览", "m", KeyCode::Char('m'), NONE)),
                _ => {}
            }
            buttons
        }

        View::Maintenance { report, .. } => {
            let mut buttons = Vec::new();
            if !report.applied {
                buttons.push(("执行维护", "enter", KeyCode::Enter, NONE));
            }
            buttons.push(("返回", "esc", KeyCode::Esc, NONE));
            buttons
        }
        View::Confirm { .. } => vec![
            ("确认", "y/enter", KeyCode::Char('y'), NONE),
            ("取消", "n/esc", KeyCode::Esc, NONE),
        ],
    }
}

/// 底部说明区：描述原文 + 来源状态（单独一行）+ 操作提示。
#[derive(Default)]
struct Footer {
    description: String,
    source: Option<&'static str>,
    /// 提示条文字与其中可点击的键位（列区间相对提示条起点）。
    hint: (String, Vec<labels::HintKey>),
}

pub fn draw(frame: &mut Frame, panel: &mut SettingsPanel, saving: bool, theme: &Theme) {
    let outer = frame.area();
    overlay::paint_opaque(
        frame.buffer_mut(),
        outer,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    // 与主界面同一套左右边距；右边距里的最右一列留给滚动条。
    let area = Rect::new(
        outer.x.saturating_add(MARGIN_X),
        outer.y,
        outer.width.saturating_sub(MARGIN_X * 2),
        outer.height,
    );
    if area.width < 20 || area.height < 12 {
        label(
            frame,
            area,
            &labels::hints(&[("", "终端过小；请放大窗口"), ("esc", "返回")], usize::MAX),
            Style::new().fg(theme.text_primary),
        );
        return;
    }
    let width = usize::from(area.width);
    let mut hits = Vec::new();
    let scope = match panel.scope {
        ConfigScope::Global => "全局配置",
        ConfigScope::Project => "项目配置",
    };
    let mut title = format!("设置{SEP}{scope}");
    if !panel.rows.is_empty() {
        title.push_str(&format!("  {}/{}", panel.selected + 1, panel.rows.len()));
    }
    // 保存优先：同一时刻只报一件事。
    if saving {
        title.push_str(&format!("{SEP}正在保存…"));
    } else if panel.busy {
        title.push_str(&format!("{SEP}正在处理…"));
    }
    if !panel.filter.is_empty() {
        let filter = truncate(panel.filter.as_str(), 24);
        title.push_str(&format!("{SEP}过滤：{filter}"));
    }
    label(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        &title,
        Style::new().add_modifier(Modifier::BOLD),
    );
    let current = panel.tab;
    let tabs = TABS.iter().enumerate().map(|(index, tab)| {
        let key = match index {
            0..=8 => format!("{}·", index + 1),
            9 => "0·".into(),
            10 => "-·".into(),
            _ => String::new(),
        };
        let style = if index == current {
            Style::new()
                .bg(theme.bg_visual)
                .fg(theme.accent_model)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.gray)
        };
        (format!(" {key}{tab} "), style, Hit::Tab(index))
    });
    let compact = panel.provider_page() && (area.width < 72 || area.height < 24);
    let gap = if compact { 1 } else { 2 };
    let tabs_end = if compact {
        let previous = current.checked_sub(1).unwrap_or(TABS.len() - 1);
        let next = (current + 1) % TABS.len();
        let style = Style::new().fg(theme.accent_model);
        flow(
            frame,
            area,
            area.y + gap,
            [
                (" ‹ ".into(), style, Hit::Tab(previous)),
                (TABS[current].into(), style, Hit::Tab(current)),
                (" › ".into(), style, Hit::Tab(next)),
            ],
            &mut hits,
        )
    } else {
        flow(frame, area, area.y + gap, tabs, &mut hits)
    };
    let advanced = panel.advanced;
    let buttons = toolbar(panel)
        .into_iter()
        .map(|(action, key, code, modifiers)| {
            // 「高级」开着时反白，与选中的选项卡同一种强调。
            let style = if advanced && code == KeyCode::F(4) {
                Style::new()
                    .fg(theme.accent_model)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text_primary).bg(theme.bg_visual)
            };
            let text = format!(" {} ", labels::hints(&[(key, action)], usize::MAX));
            (text, style, Hit::Key(code, modifiers))
        });
    let toolbar_end = flow(frame, area, tabs_end + gap, buttons, &mut hits);
    // 自下而上：消息行、提示行、空行、说明槽（固定高、内容贴底）、空行、正文。
    let bottom = area.bottom();
    let message_y = bottom - 1;
    let hint_y = bottom - 2;
    let desc_rows: u16 = if compact {
        1
    } else if area.height >= 24 {
        3
    } else {
        2
    };
    let desc_area = Rect::new(area.x, hint_y - 1 - desc_rows, area.width, desc_rows);
    let body_y = toolbar_end + gap;
    let body = Rect::new(
        area.x,
        body_y,
        area.width,
        desc_area.y.saturating_sub(1).saturating_sub(body_y),
    );
    let footer = match &panel.view {
        View::Fields if panel.provider_page() => {
            draw_providers(frame, panel, theme, body, &mut hits)
        }
        View::Fields => draw_fields(frame, panel, theme, body, &mut hits),
        View::Maintenance { .. } => draw_maintenance(frame, panel, theme, body, &mut hits),
        View::Confirm { text, .. } => {
            let lines = prose(
                text,
                width,
                usize::from(body.height),
                Style::new()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
                theme,
            );
            paint_lines(frame, body, &lines);
            Footer::default()
        }
    };
    let rows = usize::from(desc_rows);
    let text_rows = rows - usize::from(footer.source.is_some());
    let mut lines = prose(
        &footer.description,
        width,
        text_rows,
        Style::new().fg(theme.text_secondary),
        theme,
    );
    if let Some(source) = footer.source {
        lines.push(Line::styled(
            truncate(source, width).into_owned(),
            Style::new().fg(theme.gray),
        ));
    }
    let top = desc_area.y + desc_rows - cells(lines.len());
    let shown = Rect {
        y: top,
        ..desc_area
    };
    paint_lines(frame, shown, &lines);
    let hint_area = Rect::new(area.x, hint_y, area.width, 1);
    label(
        frame,
        hint_area,
        &footer.hint.0,
        Style::new().fg(theme.gray),
    );
    hits.extend(footer.hint.1.iter().map(|key| {
        let hit = key.at(hint_area.x, hint_area.y);
        (hit.area, Hit::Hint(hit.code, hit.modifiers))
    }));
    draw_edit_popup(frame, panel, theme, outer, &mut hits);
    paint_hover(frame, panel, &hits, theme);
    // 失败红字、普通反馈中性色，与主界面 notice 同一套配色；都不铺底。
    let message_area = Rect::new(area.x, message_y, area.width, 1);
    if let Some(error) = &panel.error {
        label(
            frame,
            message_area,
            &format!("✗ {error}"),
            Style::new()
                .fg(theme.accent_error)
                .add_modifier(Modifier::BOLD),
        );
    } else if let Some(message) = &panel.message {
        label(
            frame,
            message_area,
            message,
            Style::new().fg(theme.accent_tool),
        );
    }
    panel.hits = hits;
}

/// 滚动条命中登记：行 → 目标偏移 的换算器，配上收集矩形与 Hit 的表。
type HitSink<'a> = (&'a mut Vec<(Rect, Hit)>, fn(usize) -> Hit);

/// 竖向滚动条：贴终端右缘（与主界面同一列、同一字形），可选登记点击跳转。
fn scrollbar(
    frame: &mut Frame,
    rows: Rect,
    offset: usize,
    total: usize,
    theme: &Theme,
    hit: Option<HitSink<'_>>,
) {
    let height = usize::from(rows.height);
    if total <= height || height == 0 {
        return;
    }
    let track = Rect::new(frame.area().right() - 1, rows.y, 1, rows.height);
    let thumb = (height * height / total).clamp(1, height);
    let max_offset = total - height;
    let thumb_top = offset.min(max_offset) * (height - thumb) / max_offset;
    overlay::paint_scrollbar(
        frame.buffer_mut(),
        track,
        cells(thumb_top),
        cells(thumb),
        theme,
    );
    if let Some((hits, make)) = hit {
        for row in 0..height {
            // 轨道上的行号线性映射到目标偏移：点击只滚视口，不动光标。
            let target = (row * max_offset / height).min(max_offset);
            hits.push((Rect::new(track.x, track.y + cells(row), 1, 1), make(target)));
        }
    }
}

/// 悬停只改变背景；选中标记与编辑焦点仍由点击或键盘决定。
/// 与模型选择器同一做法：按指针坐标在刚登记的最终命中区上重算，滚动后不会沿用旧行号。
fn paint_hover(frame: &mut Frame, panel: &SettingsPanel, hits: &[(Rect, Hit)], theme: &Theme) {
    let Some(pointer) = panel.pointer else {
        return;
    };
    let Some((area, hit)) = hits.iter().find(|(area, _)| area.contains(pointer)) else {
        return;
    };
    let hoverable = match hit {
        Hit::Row(index) => *index != panel.selected,
        Hit::Provider(index) => !panel.provider_selected(&panel.provider_ids(), *index),
        Hit::ProviderInput(index) => panel
            .providers
            .draft
            .as_ref()
            .is_some_and(|draft| draft.focus != *index),
        Hit::Item(index) => panel
            .edit
            .as_ref()
            .is_some_and(|edit| edit.editor.cursor() != *index),
        _ => false,
    };
    if !hoverable {
        return;
    }
    for x in area.x..area.right() {
        frame.buffer_mut()[(x, area.y)].set_bg(theme.bg_hover);
    }
}

/// 标签列宽：取整页最宽标签（含光标、缩进与自定义标记），夹在 24–44 列，
/// 且不超过一半宽度；按整页而非可见行计算，滚动时列线不跳。
fn label_column(panel: &SettingsPanel, width: usize) -> usize {
    let widest = panel
        .rows
        .iter()
        .map(|row| {
            let text = match &row.kind {
                RowKind::Group(group) => str_width(&group.title),
                RowKind::Action(_) => 0,
                RowKind::Field(index) => {
                    let field = &panel.fields[*index];
                    let badge = if panel.filter.is_empty() {
                        0
                    } else {
                        str_width(&field.tab) + 3
                    };
                    str_width(&field.label) + badge
                }
            };
            4 + usize::from(row.depth) * 2 + text
        })
        .max()
        .unwrap_or(0);
    (widest + 2).clamp(24, 44).min(width / 2)
}

/// 字段树：分组头可折叠，字段行可原地编辑。
fn draw_fields(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> Footer {
    let width = usize::from(body.width);
    let hints = |items: &[(&str, &str)]| labels::hint_layout(items, width);
    if panel.data.is_none() {
        label(
            frame,
            Rect { height: 1, ..body },
            "正在读取设置…",
            Style::new().fg(theme.accent_running),
        );
        return Footer {
            hint: hints(&[("esc", "返回")]),
            ..Footer::default()
        };
    }
    if panel.rows.is_empty() {
        let text = if !panel.filter.is_empty() {
            "没有匹配的设置项；按 / 修改搜索内容，或按 Esc 清空过滤。"
        } else {
            match TABS[panel.tab] {
                "供应商" => "还没有供应商。按 A 添加，填写服务地址与密钥后按 F 获取模型。",
                "模型" => "还没有模型。按 A 填写模型 ID，或到“供应商”页按 F 获取。",
                "MCP" => "还没有 MCP 服务。按 A 添加。",
                "插件" => "当前没有可配置的插件项。",
                _ => "这一页暂时没有可配置项。",
            }
        };
        let lines = prose(
            text,
            width,
            usize::from(body.height),
            Style::new().fg(theme.gray),
            theme,
        );
        paint_lines(frame, body, &lines);
        let hint = if !panel.filter.is_empty() {
            hints(&[("/", "重新搜索"), ("esc", "清空过滤")])
        } else {
            hints(&[("1-9/[]", "切页"), ("esc", "返回")])
        };
        return Footer {
            hint,
            ..Footer::default()
        };
    }
    let height = usize::from(body.height);
    let total = panel.rows.len();
    // 键盘/点击选中了才显露选中项；滚轮手动滚到的位置普通重绘不拉回。
    panel.offset = if panel.scroll.reveal_selection {
        window_offset(panel.offset, panel.selected, height, total)
    } else {
        panel.offset.min(total.saturating_sub(height))
    };
    panel.scroll.max_offset = total.saturating_sub(height);
    panel.scroll.reveal_selection = false;
    let label_width = label_column(panel, width);
    let value_x = body.x + cells(label_width);
    let value_width = body.width.saturating_sub(cells(label_width));
    for (row_index, row) in panel
        .rows
        .iter()
        .enumerate()
        .skip(panel.offset)
        .take(height)
    {
        let y = body.y + cells(row_index - panel.offset);
        let row_area = Rect::new(body.x, y, body.width, 1);
        let label_area = Rect::new(body.x, y, cells(label_width.saturating_sub(1)), 1);
        let value_area = Rect::new(value_x, y, value_width, 1);
        let selected = row_index == panel.selected;
        let style = if selected {
            Style::new().bg(theme.bg_visual).fg(theme.text_primary)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        frame.buffer_mut().set_style(row_area, style);
        // 选中标记与浮层列表同一个 `❯`。
        let lead = format!(
            "{} {}",
            if selected { "❯" } else { " " },
            " ".repeat(usize::from(row.depth) * 2)
        );
        match &row.kind {
            RowKind::Action(action) => {
                let inline = panel
                    .edit
                    .as_ref()
                    .filter(|edit| !edit.popup)
                    .and_then(|edit| {
                        let matches = matches!(
                            (&edit.target, action),
                            (EditTarget::Model(_), ProviderAction::Manual)
                                | (EditTarget::ModelFilter, ProviderAction::Search)
                        );
                        if matches && let Editor::Text(editor) = &edit.editor {
                            Some(editor)
                        } else {
                            None
                        }
                    });
                if let Some(editor) = inline {
                    let prompt = if *action == ProviderAction::Manual {
                        "模型 ID："
                    } else {
                        "搜索："
                    };
                    let prefix = format!("{lead}{prompt}");
                    let start = cells(str_width(&prefix)).min(row_area.width);
                    label(frame, row_area, &prefix, style);
                    let input_area = Rect::new(
                        row_area.x + start,
                        y,
                        row_area.width.saturating_sub(start),
                        1,
                    );
                    let (text, column) = edit_line(editor, usize::from(input_area.width));
                    label(frame, input_area, &text, style.fg(theme.accent_running));
                    if input_area.width > 0 {
                        frame.set_cursor_position((
                            input_area.x + cells(column).min(input_area.width - 1),
                            y,
                        ));
                    }
                } else {
                    label(
                        frame,
                        row_area,
                        &format!("{lead}{}", panel.provider_action_text(action)),
                        style.fg(theme.accent_model),
                    );
                }
            }
            RowKind::Group(group) => {
                let marker = if panel.is_expanded(&group.key, usize::from(row.depth)) {
                    "▾"
                } else {
                    "▸"
                };
                label(
                    frame,
                    label_area,
                    &format!("{lead}{marker} {}", group.title),
                    style.add_modifier(Modifier::BOLD),
                );
                label(frame, value_area, &group.detail, style.fg(theme.gray));
            }
            RowKind::Field(index) => {
                let field = &panel.fields[*index];
                let is_explicit = panel.explicit(&field.path);
                let label_style = if is_explicit && !selected {
                    style.fg(theme.text_primary).add_modifier(Modifier::BOLD)
                } else {
                    style
                };
                let mut spans = vec![Span::styled(
                    format!(
                        "{lead}{}{}",
                        if is_explicit { "● " } else { "  " },
                        field.label
                    ),
                    label_style,
                )];
                // 搜索结果标出所在选项卡：独立的暗色徽标，不和标签同色。
                if !panel.filter.is_empty() {
                    spans.push(Span::styled(
                        format!("  [{}]", field.tab),
                        style.fg(theme.gray),
                    ));
                }
                let line = clip_line(Line::from(spans), usize::from(label_area.width));
                frame
                    .buffer_mut()
                    .set_line(label_area.x, y, &line, label_area.width);
                let inline =
                    panel
                        .edit
                        .as_ref()
                        .and_then(|edit| match (&edit.target, &edit.editor) {
                            (EditTarget::Field(path), Editor::Text(text))
                                if !edit.popup && *path == field.path =>
                            {
                                Some(text)
                            }
                            _ => None,
                        });
                match inline {
                    Some(editor) => {
                        let (text, column) = edit_line(editor, usize::from(value_width));
                        label(frame, value_area, &text, style.fg(theme.accent_running));
                        frame.set_cursor_position((
                            value_x + cells(column).min(value_width.saturating_sub(1)),
                            y,
                        ));
                    }
                    None => {
                        let value = panel.value(&field.path).unwrap_or(&Value::Null);
                        let control = &panel.controls[*index];
                        let (text, value_style) = match (control, value) {
                            (Control::Toggle, Value::Bool(true)) => (
                                "● 开启".to_owned(),
                                style.fg(theme.accent_success).add_modifier(Modifier::BOLD),
                            ),
                            (Control::Toggle, Value::Bool(false)) => {
                                ("○ 关闭".to_owned(), style.fg(theme.gray))
                            }
                            _ => {
                                let text = controls::summary(control, value)
                                    .unwrap_or_else(|| display_value(value, field));
                                let dim = if value.is_null() {
                                    style.fg(theme.gray)
                                } else {
                                    style
                                };
                                (text, dim)
                            }
                        };
                        label(frame, value_area, &text, value_style);
                    }
                }
            }
        }
        hits.push((row_area, Hit::Row(row_index)));
    }
    scrollbar(
        frame,
        body,
        panel.offset,
        panel.rows.len(),
        theme,
        Some((hits, Hit::Scroll)),
    );
    if let Some(edit) = panel.edit.as_ref().filter(|edit| !edit.popup) {
        let note = match &edit.target {
            EditTarget::Model(_) => "模型 ID 原样保存，可包含 /",
            EditTarget::ModelFilter => "实时过滤，已有勾选会保留",
            _ => "留空恢复默认",
        };
        let mut items = vec![("", note), ("tab", "下一项")];
        items.extend(edit.editor.hints());
        return Footer {
            hint: hints(&items),
            ..Footer::default()
        };
    }
    // 提示条从左丢项：最可省的写在前面，Esc 恒在末尾。
    match panel.row() {
        Some(RowKind::Action(_)) => Footer {
            description: "模型操作只作用于当前供应商；新增模型后可直接展开编辑参数。".into(),
            source: None,
            hint: hints(&[
                ("tab", "切换焦点"),
                ("↑↓", "移动"),
                ("enter/space", "操作"),
                ("esc", "返回"),
            ]),
        },
        Some(RowKind::Group(group)) => {
            let mut items = vec![
                ("1-9/[]", "切页"),
                ("e/c", "展收全部"),
                ("←→/hl", "层级导航"),
            ];
            if group.path.is_some() {
                items.push(("del", "删除"));
            }
            items.extend([("enter/space", "展开收起"), ("esc", "返回")]);
            Footer {
                description: group_description(group),
                source: None,
                hint: hints(&items),
            }
        }
        Some(RowKind::Field(index)) => {
            let field = &panel.fields[*index];
            let source = if field.path.first().is_some_and(|part| part == "tui") {
                "界面偏好设置"
            } else if panel.explicit(&field.path) {
                "● 已在本文件自定义"
            } else if matches!(panel.scope, ConfigScope::Project) {
                "○ 沿用全局配置或默认值"
            } else {
                "○ 使用系统默认值"
            };
            let items = [
                ("1-9/[]", "切页"),
                ("←/h", "跳至分组"),
                ("r", "恢复默认"),
                ("", controls::action_hint(&panel.controls[*index])),
                ("esc", "返回"),
            ];
            Footer {
                description: field.description.clone(),
                source: Some(source),
                hint: hints(&items),
            }
        }
        None => Footer {
            hint: hints(&[("1-9/[]", "切页"), ("esc", "返回")]),
            ..Footer::default()
        },
    }
}

fn group_description(group: &Group) -> String {
    let path = group.grouping.as_slice();
    match path.first().map(String::as_str) {
        Some("providers") if path.len() == 2 => format!("供应商“{}”的设置。", group.title),
        Some("providers") if group.detail.is_empty() => format!("模型“{}”的设置。", group.title),
        Some("providers") => format!("模型“{}”：{}。", group.title, group.detail),
        Some("models") if path.get(1).is_some_and(|part| part == "roles") => {
            "会话使用的各类模型；候选来自已添加的模型。".into()
        }
        Some("models") => {
            "主导模型负责规划与评审，执行模型负责实现与测试；两者都配置才生效。".into()
        }
        Some("defaults") => format!(
            "名称匹配“{}”的模型默认设置。",
            path.get(2).map_or("", String::as_str)
        ),
        Some("mcp") => format!("MCP 服务“{}”。", group.title),
        _ => format!("“{}”相关的设置。", group.title),
    }
}

/// 供应商列表和详情共用一个页面，窄终端用可切换的单栏布局。
fn draw_providers(
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

fn draw_provider_draft(
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
            label(frame, input_area, providers::APIS[draft.api].1, style);
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
fn draw_maintenance(
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
    if let View::Maintenance { offset: current, .. } = &mut panel.view {
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

/// 弹窗编辑器：单选、清单、键值表与多行文本共用一张居中卡片；单行文本在字段行内编辑。
fn draw_edit_popup(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let Some(edit) = panel.edit.as_mut().filter(|edit| edit.popup) else {
        return;
    };
    // 弹窗是模态的：底下的行与按钮不再响应点击。
    hits.clear();
    let wide = matches!(&edit.editor, Editor::Text(editor) if editor.multiline());
    let card_width = overlay::card_width(area, if wide { overlay::CARD_MAX_WIDTH } else { 72 });
    let width = overlay::content_width(card_width);
    let rows = overlay::max_body_rows(area, true).max(1);
    let mut lines: Vec<Line<'static>> = if edit.note.is_empty() {
        Vec::new()
    } else {
        wrap_text(&edit.note, width)
            .into_iter()
            .map(|line| Line::styled(line, Style::new().fg(theme.gray)))
            .chain([Line::default()])
            .collect()
    };
    // 说明挤占编辑区时让位：至少给候选或输入留 3 行。
    if lines.len() + 3 > rows {
        lines.clear();
    }
    let offset = lines.len();
    let body = popup_body(&mut edit.editor, width, rows - offset, theme);
    lines.extend(body.lines);
    frame
        .buffer_mut()
        .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
    let layout = overlay::Card {
        title: edit.title.clone(),
        body: lines,
        footer: Some(overlay::footer(&edit.editor.hints(), width, theme)),
    }
    .render(frame, area, card_width, theme);
    hits.extend(
        layout
            .keys
            .iter()
            .map(|key| (key.area, Hit::Hint(key.code, key.modifiers))),
    );
    for (row, item) in body.items.iter().enumerate() {
        let y = layout.body.y + cells(offset + row);
        if let Some(index) = item
            && y < layout.body.bottom()
        {
            hits.push((
                Rect::new(layout.body.x, y, layout.body.width, 1),
                Hit::Item(*index),
            ));
        }
    }
    dismiss_around(layout.area, area, hits);
    if let Some((row, column)) = body.cursor {
        frame.set_cursor_position((
            layout.body.x + cells(column).min(layout.body.width.saturating_sub(1)),
            layout.body.y + cells(offset + row),
        ));
    }
}

/// 卡片四周的空白：点击即关闭弹窗。
fn dismiss_around(card: Rect, area: Rect, hits: &mut Vec<(Rect, Hit)>) {
    let sides = [
        Rect::new(area.x, area.y, area.width, card.y - area.y),
        Rect::new(
            area.x,
            card.bottom(),
            area.width,
            area.bottom() - card.bottom(),
        ),
        Rect::new(area.x, card.y, card.x - area.x, card.height),
        Rect::new(
            card.right(),
            card.y,
            area.right() - card.right(),
            card.height,
        ),
    ];
    hits.extend(
        sides
            .into_iter()
            .filter(|side| !side.is_empty())
            .map(|side| (side, Hit::DismissPopup)),
    );
}

/// 弹窗正文：已按宽度截好、按可见行数截窗。
struct PopupBody {
    lines: Vec<Line<'static>>,
    /// 每行对应编辑器里的第几行；`None` 不可点击。
    items: Vec<Option<usize>>,
    /// 光标位置：（正文行, 列）。
    cursor: Option<(usize, usize)>,
}

fn popup_body(editor: &mut Editor, width: usize, rows: usize, theme: &Theme) -> PopupBody {
    match editor {
        Editor::Text(editor) => text_body(editor, width, rows),
        Editor::Select(editor) => {
            let lines: Vec<_> = editor
                .picks
                .iter()
                .enumerate()
                .map(|(index, pick)| {
                    let current = editor.current == Some(index);
                    let (mark, style) = if current {
                        ("● ", Style::new().fg(theme.accent_model))
                    } else {
                        ("○ ", Style::new().fg(theme.gray))
                    };
                    let spans = vec![Span::styled(mark, style), Span::raw(pick.label.clone())];
                    (
                        option_row(index == editor.selected, spans, width, theme),
                        Some(index),
                    )
                })
                .collect();
            let start = viewport(&mut editor.scroll, &mut editor.offset, editor.selected, rows, lines.len());
            window(lines, start, rows, None)
        }
        Editor::List(editor) => list_body(editor, width, rows, theme),
        Editor::Record(editor) => record_body(editor, width, rows, theme),
    }
}

/// 弹窗视口起点：光标移动过才显露，滚轮手动滚到的位置普通重绘不拉回；
/// 顺带回写 `max_offset`、记消费过的 `reveal_selection`，与模型选择器同一套。
fn viewport(
    scroll: &mut ModelScroll,
    offset: &mut usize,
    focus: usize,
    rows: usize,
    total: usize,
) -> usize {
    scroll.max_offset = total.saturating_sub(rows);
    let start = if scroll.reveal_selection {
        window_offset(*offset, focus, rows, total)
    } else {
        (*offset).min(scroll.max_offset)
    };
    *offset = start;
    scroll.reveal_selection = false;
    start
}

/// 截出从 `start` 起的 `rows` 行，光标行号随之平移。
fn window(
    lines: Vec<(Line<'static>, Option<usize>)>,
    start: usize,
    rows: usize,
    cursor: Option<(usize, usize)>,
) -> PopupBody {
    let end = (start + rows).min(lines.len());
    let (lines, items) = lines.into_iter().skip(start).take(rows).unzip();
    PopupBody {
        lines,
        items,
        cursor: cursor
            .filter(|(row, _)| (start..end).contains(row))
            .map(|(row, column)| (row - start, column)),
    }
}

/// 候选行：与浮层列表同一套 `❯` 标记与选中色带；自带颜色的 span 保留前景色。
fn option_row(
    focused: bool,
    spans: Vec<Span<'static>>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let (marker, style) = if focused {
        (
            "❯ ",
            Style::new()
                .fg(theme.accent_user)
                .bg(theme.bg_visual)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("  ", Style::new().fg(theme.text_secondary))
    };
    let mut line = Vec::with_capacity(spans.len() + 1);
    line.push(Span::raw(marker));
    line.extend(spans);
    fit_line(Line::from(line).style(style), width)
}

/// 行内草稿：前缀 + 跟随光标横向滚动的输入，返回行与光标列。
fn draft_row(
    prefix: &str,
    input: &PromptState,
    width: usize,
    theme: &Theme,
) -> (Line<'static>, usize) {
    let lead = 2 + str_width(prefix);
    let (text, column) = tail(&input.text, input.cursor, width.saturating_sub(lead));
    let line = Line::from(vec![
        Span::raw("❯ "),
        Span::raw(prefix.to_owned()),
        Span::raw(text),
    ])
    .style(Style::new().fg(theme.accent_running).bg(theme.bg_visual));
    (fit_line(line, width), lead + column)
}

/// 清单的草稿行；录入按键时只提示按键，不显示光标。
fn list_draft(
    capture: bool,
    mark: &str,
    input: &PromptState,
    width: usize,
    theme: &Theme,
) -> (Line<'static>, Option<usize>) {
    if capture {
        let line = Line::from(format!("❯ {mark}请按下要绑定的键…")).style(
            Style::new()
                .fg(theme.accent_running)
                .bg(theme.bg_visual)
                .add_modifier(Modifier::BOLD),
        );
        return (fit_line(line, width), None);
    }
    let (line, column) = draft_row(mark, input, width, theme);
    (line, Some(column))
}

fn list_body(editor: &mut ListEditor, width: usize, rows: usize, theme: &Theme) -> PopupBody {
    let entry = editor.spec.custom;
    let capture = matches!(entry, Some(Entry::Key));
    let checklist = editor.checklist();
    let shown: Vec<String> = editor
        .items
        .iter()
        .map(|item| {
            if capture {
                labels::key(&item.value)
            } else {
                item.value.clone()
            }
        })
        .collect();
    let column = shown
        .iter()
        .map(|text| str_width(text))
        .max()
        .unwrap_or(0)
        .min(width / 2);
    // 思考等级：第一个勾选项就是默认等级。
    let default = if matches!(entry, Some(Entry::Level)) {
        editor.items.iter().position(|item| item.checked)
    } else {
        None
    };
    let mut lines = Vec::with_capacity(editor.rows());
    let mut cursor = None;
    for (index, (item, text)) in editor.items.iter().zip(&shown).enumerate() {
        let mark = match (checklist, item.checked) {
            (true, true) => "☑ ",
            (true, false) => "☐ ",
            (false, _) => "• ",
        };
        if let Some(draft) = editor
            .draft
            .as_ref()
            .filter(|draft| draft.index == Some(index))
        {
            let (line, column) = list_draft(capture, mark, &draft.input, width, theme);
            cursor = column.map(|column| (index, column));
            lines.push((line, None));
            continue;
        }
        let mut note = if item.note.is_empty() && checklist && !item.preset {
            "自定义".to_owned()
        } else {
            item.note.clone()
        };
        if default == Some(index) {
            note.push_str("（默认）");
        }
        let mark_style = if item.checked {
            Style::new().fg(theme.accent_model)
        } else {
            Style::new().fg(theme.gray)
        };
        let mut spans = vec![Span::styled(mark, mark_style), Span::raw(fit(text, column))];
        if !note.is_empty() {
            spans.push(Span::styled(
                format!("  {note}"),
                Style::new().fg(theme.gray),
            ));
        }
        lines.push((
            option_row(editor.cursor == index, spans, width, theme),
            Some(index),
        ));
    }
    if let Some(entry) = entry {
        let index = editor.items.len();
        match editor.draft.as_ref().filter(|draft| draft.index.is_none()) {
            Some(draft) => {
                let (line, column) = list_draft(capture, "+ ", &draft.input, width, theme);
                cursor = column.map(|column| (index, column));
                lines.push((line, None));
            }
            None => {
                let text = match entry {
                    Entry::Level => "+ 添加自定义等级",
                    Entry::Key => "+ 添加按键",
                    Entry::Text => "+ 添加一项",
                };
                let spans = vec![Span::raw(text)];
                lines.push((
                    option_row(editor.cursor == index, spans, width, theme),
                    Some(index),
                ));
            }
        }
    }
    if lines.is_empty() {
        lines.push((
            Line::styled("  没有可选项", Style::new().fg(theme.gray)),
            None,
        ));
    }
    let total = lines.len();
    let focus = cursor.map_or(editor.cursor, |(row, _)| row);
    let start = viewport(&mut editor.scroll, &mut editor.offset, focus, rows, total);
    window(lines, start, rows, cursor)
}

fn record_body(editor: &mut RecordEditor, width: usize, rows: usize, theme: &Theme) -> PopupBody {
    let column = editor
        .rows
        .iter()
        .map(|row| str_width(&row.key))
        .max()
        .unwrap_or(0)
        .min(width / 2);
    // 填了任何一项后，留空的必填键要标出来。
    let partial = editor.rows.iter().any(|row| !row.value.trim().is_empty());
    let mut lines = Vec::with_capacity(editor.rows());
    let mut cursor = None;
    for (index, row) in editor.rows.iter().enumerate() {
        let key = fit(&row.key, column);
        if let Some(draft) = editor
            .draft
            .as_ref()
            .filter(|draft| draft.index == Some(index))
        {
            let (line, column) = draft_row(&format!("{key}  "), &draft.input, width, theme);
            cursor = Some((index, column));
            lines.push((line, None));
            continue;
        }
        let value = if !row.value.trim().is_empty() {
            Span::raw(row.value.clone())
        } else if partial && editor.spec.required.iter().any(|name| *name == row.key) {
            Span::styled("必填", Style::new().fg(theme.accent_warning))
        } else {
            Span::styled("未设置", Style::new().fg(theme.gray))
        };
        let mut spans = vec![Span::raw(key), Span::raw("  "), value];
        if !row.note.is_empty() {
            spans.push(Span::styled(
                format!("  {}", row.note),
                Style::new().fg(theme.gray),
            ));
        }
        lines.push((
            option_row(editor.cursor == index, spans, width, theme),
            Some(index),
        ));
    }
    if editor.spec.custom {
        let index = editor.rows.len();
        match editor.draft.as_ref().filter(|draft| draft.index.is_none()) {
            Some(draft) => {
                let (line, column) = draft_row("键名：", &draft.input, width, theme);
                cursor = Some((index, column));
                lines.push((line, None));
            }
            None => {
                let spans = vec![Span::raw("+ 添加键")];
                lines.push((
                    option_row(editor.cursor == index, spans, width, theme),
                    Some(index),
                ));
            }
        }
    }
    let total = lines.len();
    let focus = cursor.map_or(editor.cursor, |(row, _)| row);
    let start = viewport(&mut editor.scroll, &mut editor.offset, focus, rows, total);
    window(lines, start, rows, cursor)
}

/// 弹窗里的文本框：单行横向滚动，多行按宽度折行并让光标行可见。
fn text_body(editor: &TextEditor, width: usize, rows: usize) -> PopupBody {
    if !editor.multiline() {
        let (text, column) = edit_line(editor, width);
        return window(
            vec![(clip_line(Line::from(text), width), None)],
            0,
            rows,
            Some((0, column)),
        );
    }
    let wrapped = wrap_editor(&editor.input, width);
    let lines: Vec<_> = wrapped
        .lines
        .into_iter()
        .map(|line| (Line::from(line), None))
        .collect();
    // 文本框没有独立视口状态：光标行仍按居中窗口显露。
    let start = wrapped
        .cursor_row
        .saturating_sub(rows / 2)
        .min(lines.len().saturating_sub(rows));
    window(
        lines,
        start,
        rows,
        Some((wrapped.cursor_row, wrapped.cursor_column)),
    )
}

/// 单行输入：横向滚动到光标可见；未显示的密钥逐字符打码。
fn edit_line(editor: &TextEditor, width: usize) -> (String, usize) {
    let input = &editor.input;
    if matches!(editor.kind, FieldKind::Secret) && !editor.reveal {
        let masked = "•".repeat(input.text.chars().count());
        let cursor = input.text[..input.cursor].chars().count() * '•'.len_utf8();
        return tail(&masked, cursor, width);
    }
    tail(&input.text, input.cursor, width)
}

/// 光标处可见的文本尾巴：起点尽量靠后，保证光标不超出宽度。
fn tail(text: &str, cursor: usize, width: usize) -> (String, usize) {
    let width = width.max(1);
    let cursor = cursor.min(text.len());
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(index, _)| index)
        .filter(|index| *index <= cursor)
        .collect();
    let mut start = 0;
    for boundary in boundaries.iter().rev() {
        if str_width(&text[*boundary..cursor]) + 1 > width {
            break;
        }
        start = *boundary;
    }
    (text[start..].to_owned(), str_width(&text[start..cursor]))
}

struct Wrapped {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_column: usize,
}

/// 多行输入按宽度折行，并给出光标所在行与列。
fn wrap_editor(input: &PromptState, width: usize) -> Wrapped {
    let width = width.max(1);
    let text = input.text.as_str();
    let prefix = &input.text[..input.cursor];
    let cursor_line = prefix.matches('\n').count();
    let mut lines: Vec<String> = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_column = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index == cursor_line {
            let head = prefix.rsplit('\n').next().unwrap_or_default();
            let wrapped = crate::render::wrap::wrap_text(head, width);
            cursor_row = lines.len() + wrapped.len().max(1) - 1;
            cursor_column = wrapped.last().map_or(0, |line| str_width(line));
        }
        let wrapped = crate::render::wrap::wrap_text(line, width);
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrapped);
        }
    }
    Wrapped {
        lines,
        cursor_row,
        cursor_column,
    }
}

/// 数字千分位格式化（200000 -> 200,000）。
pub(crate) fn format_number(value: u64) -> String {
    let s = value.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len + len / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }
    result
}

/// 没有专属控件摘要的值（文本、数值、模型、对象）的展示形式。
pub(super) fn display_value(value: &Value, field: &ConfigField) -> String {
    if value.is_null() {
        return "未设置".into();
    }
    if matches!(field.kind, FieldKind::Secret) {
        return if value.as_str().is_some_and(|text| !text.is_empty()) {
            "••••••"
        } else {
            "未设置"
        }
        .into();
    }
    // 字节预算按 KB/MB 显示，避免一串数字看不出量级。
    if let Some(bytes) = value.as_u64()
        && field.path.last().is_some_and(|key| key.ends_with("_bytes"))
    {
        return short_bytes(bytes);
    }
    // 上下文与回复上限等 Token 数显示千分位与缩写（200,000 (200k)）。
    if let Some(num) = value.as_u64()
        && field.path.last().is_some_and(|key| {
            matches!(
                key.as_str(),
                "context_window" | "max_output_tokens" | "max_tokens"
            )
        })
    {
        let compact = crate::render::format_tokens(num);
        let plain = format_number(num);
        // 不足 1000 时两者相同，没必要写成「512 (512)」。
        return if compact == plain {
            plain
        } else {
            format!("{plain} ({compact})")
        };
    }
    // 大于等于一万的整数加上千分位。
    if let Some(num) = value.as_u64()
        && num >= 10_000
    {
        return format_number(num);
    }
    match value {
        Value::Bool(true) => "● 开启".into(),
        Value::Bool(false) => "○ 关闭".into(),
        Value::String(text) if text.is_empty() => "未设置".into(),
        // 多行文本只露首行，并注明总行数，免得被当成值就这一行。
        Value::String(text) => match text.lines().count() {
            0 | 1 => text.trim_end().to_owned(),
            count => format!(
                "{}{ELLIPSIS}（共 {count} 行）",
                text.lines().next().unwrap_or_default()
            ),
        },
        Value::Object(values) if values.is_empty() => "无".into(),
        Value::Object(values) => format!("{{ {} 项 }}", values.len()),
        Value::Array(values) if values.is_empty() => "无".into(),
        Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("、"),
        // 整数值的浮点（20.0）与整数同样显示，免得同一页混排 `20.0` 与 `10`。
        Value::Number(number)
            if number.as_f64().is_some_and(|value| value.fract() == 0.0)
                && number.as_i64().is_none()
                && number.as_u64().is_none() =>
        {
            format!("{:.0}", number.as_f64().unwrap_or_default())
        }
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{EditState, EditTarget, SettingsPanel, View};
    use moc_protocol::ConfigScope;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use serde_json::json;
    use std::collections::BTreeSet;

    /// 单选弹窗：卡片不透底，候选行可点击，卡片外点击关闭，底下的行不再响应。
    #[test]
    fn select_popup_is_opaque_and_modal() {
        let theme = Theme::global();
        let mut panel = SettingsPanel {
            id: 1,
            cwd: "/tmp".into(),
            scope: ConfigScope::Global,
            data: None,
            tui: json!({}),
            effective: json!({}),
            fields: Vec::new(),
            controls: Vec::new(),
            rows: Vec::new(),
            collapsed: BTreeSet::new(),
            expanded: BTreeSet::new(),
            tab: 0,
            selected: 0,
            offset: 0,
            scroll: Default::default(),
            pointer: None,
            advanced: false,
            filter: String::new(),
            edit: Some(EditState {
                target: EditTarget::Filter,
                editor: Editor::Select(SelectEditor::new(
                    vec![controls::Pick {
                        label: "选项A".into(),
                        action: PickAction::Set(json!("a")),
                    }],
                    &Value::Null,
                )),
                popup: true,
                title: "选择选项".into(),
                note: String::new(),
            }),
            error: None,
            message: None,
            busy: false,
            hits: Vec::new(),
            pending: None,
            view: View::Fields,
            providers: Providers::default(),
            history: Default::default(),
            outcome: None,
            restore: None,
        };

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_style(Rect::new(0, 0, 80, 24), Style::new().bg(Color::Red));
                draw(frame, &mut panel, false, theme);
            })
            .expect("绘制成功");

        let buffer = terminal.backend().buffer();
        // 验证中心弹窗卡片位置不漏底
        let center = buffer.cell((40, 12)).expect("中心格");
        assert_ne!(center.bg, Color::Reset, "弹窗不应为 Reset");
        assert_ne!(center.bg, Color::Red, "弹窗不应漏出底层红色");
        assert!(center.bg == theme.bg_base || center.bg == theme.bg_visual);

        let item = panel
            .hits
            .iter()
            .find(|(_, hit)| matches!(hit, Hit::Item(0)))
            .map(|(area, _)| *area)
            .expect("候选行可点击");
        let row: String = (item.x..item.right())
            .map(|x| buffer.cell((x, item.y)).expect("候选格").symbol())
            .collect();
        // 宽字符后的占位格是空格，比较前去掉
        assert!(
            row.replace(' ', "").contains("选项A"),
            "点击区域落在候选行上：{row}"
        );
        // 只剩弹窗自己的候选、底部键位与卡片外关闭；底下页面的命中区全部清掉。
        assert!(
            panel
                .hits
                .iter()
                .all(|(_, hit)| matches!(hit, Hit::Item(_) | Hit::Hint(..) | Hit::DismissPopup))
        );
        assert!(panel.hits.iter().any(|(area, hit)| {
            matches!(hit, Hit::DismissPopup) && area.contains((0, 0).into())
        }));
    }

    /// 读入 `document` 并停在 `tab` 页的面板（字段目录由 core 补齐）。
    fn loaded(tab: &str, document: Value, fields: Vec<ConfigField>) -> SettingsPanel {
        let mut panel = SettingsPanel {
            id: 1,
            cwd: "/tmp".into(),
            scope: ConfigScope::Global,
            data: Some(SettingsDocument {
                path: String::new(),
                revision: String::new(),
                document: json!({}),
                effective: document.clone(),
                inherited: document,
                defaults: Value::Null,
                fields,
                provider_template: Value::Null,
                model_template: Value::Null,
            }),
            tui: json!({}),
            effective: Value::Null,
            fields: Vec::new(),
            controls: Vec::new(),
            rows: Vec::new(),
            collapsed: BTreeSet::new(),
            expanded: BTreeSet::new(),
            tab: TABS
                .iter()
                .position(|name| *name == tab)
                .expect("选项卡存在"),
            selected: 0,
            offset: 0,
            scroll: Default::default(),
            pointer: None,
            advanced: false,
            filter: String::new(),
            edit: None,
            error: None,
            message: None,
            busy: false,
            hits: Vec::new(),
            pending: None,
            view: View::Fields,
            providers: Providers::default(),
            history: Default::default(),
            outcome: None,
            restore: None,
        };
        panel.rebuild();
        panel
    }

    /// 绘制一帧，返回每行的可见文本与每个字符的起始列（宽字符尾格不计）。
    fn screen(panel: &mut SettingsPanel, width: u16, height: u16) -> Vec<(String, Vec<u16>)> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
        terminal
            .draw(|frame| draw(frame, panel, false, Theme::global()))
            .expect("绘制成功");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let mut text = String::new();
                let mut columns = Vec::new();
                let mut x = 0;
                while x < width {
                    let symbol = buffer.cell((x, y)).expect("格").symbol();
                    for _ in symbol.chars() {
                        columns.push(x);
                    }
                    text.push_str(symbol);
                    x += u16::try_from(str_width(symbol).max(1)).expect("单格宽度");
                }
                (text, columns)
            })
            .collect()
    }

    /// `needle` 在第几行、从第几列开始。
    fn locate(rows: &[(String, Vec<u16>)], needle: &str) -> Option<(usize, u16)> {
        rows.iter().enumerate().find_map(|(y, (text, columns))| {
            let index = text.find(needle)?;
            Some((y, columns[text[..index].chars().count()]))
        })
    }

    /// 「模型」页常见的配置：模型角色 + Fusion 组合。
    fn models_config() -> Value {
        json!({"models": {
            "roles": {"default": "p/m", "explore": "inherit", "plan": "none"},
            "fusion": {"lead": "p/m", "sidekick": "p/m"}
        }})
    }

    fn select_group(panel: &mut SettingsPanel, key: &str) {
        panel.selected = panel
            .rows
            .iter()
            .position(|row| matches!(&row.kind, RowKind::Group(group) if group.key == key))
            .expect("分组行存在");
    }

    /// 不可删除的分组也要按自己的分组路径给说明，不能统统落到模型角色那句。
    #[test]
    fn group_description_follows_grouping_path() {
        let mut panel = loaded("模型角色", models_config(), Vec::new());
        select_group(&mut panel, "models/fusion");
        let rows = screen(&mut panel, 120, 40);
        assert!(
            locate(&rows, "会话使用的各类模型").is_none(),
            "Fusion 分组不得显示模型角色说明"
        );
        assert!(locate(&rows, "执行模型负责实现与测试").is_some());

        select_group(&mut panel, "models/roles");
        let rows = screen(&mut panel, 120, 40);
        assert!(locate(&rows, "会话使用的各类模型").is_some());
    }

    /// 工具栏放不下时换行，按钮一个都不能丢。
    #[test]
    fn toolbar_wraps_instead_of_dropping_buttons() {
        let mut panel = loaded("模型角色", models_config(), Vec::new());
        screen(&mut panel, 80, 30);
        let keys: Vec<(KeyCode, KeyModifiers)> = panel
            .hits
            .iter()
            .filter_map(|(_, hit)| match hit {
                Hit::Key(code, modifiers) => Some((*code, *modifiers)),
                _ => None,
            })
            .collect();
        for expected in [
            (KeyCode::Char('c'), KeyModifiers::NONE),
            (KeyCode::Char('d'), KeyModifiers::CONTROL),
        ] {
            assert!(
                keys.contains(&expected),
                "工具栏丢了 {expected:?}：{keys:?}"
            );
        }
        panel.select_tab(providers::TAB);
        screen(&mut panel, 80, 30);
        for expected in ['n', 'a', 'f'] {
            assert!(
                panel.hits.iter().any(|(_, hit)| matches!(hit,
                    Hit::Key(KeyCode::Char(key), KeyModifiers::NONE) if *key == expected
                )),
                "统一页面丢了 {expected} 操作"
            );
        }
    }

    /// 底部提示条的单键项可点：命中区落在画出来的那段文字上，且是编辑态也生效的 `Hint`。
    #[test]
    fn hint_bar_keys_are_clickable_where_drawn() {
        let mut panel = loaded("模型角色", models_config(), Vec::new());
        select_group(&mut panel, "models/roles");
        let rows = screen(&mut panel, 120, 40);
        let (y, x) = locate(&rows, "Esc 返回").expect("提示条有 Esc 返回");
        let hit = panel
            .hits
            .iter()
            .find_map(|(area, hit)| match hit {
                Hit::Hint(KeyCode::Esc, KeyModifiers::NONE) => Some(*area),
                _ => None,
            })
            .expect("Esc 返回 登记为提示条键位");
        let y = u16::try_from(y).expect("行号");
        assert_eq!((hit.y, hit.x, hit.width), (y, x, 8));
    }

    /// 超宽的值以 `…` 收尾，且不越过右边距。
    #[test]
    fn long_values_end_with_ellipsis_inside_margin() {
        let path = vec!["zz".to_owned(), "long".to_owned()];
        let field = ConfigField {
            path: path.clone(),
            tab: "模型".into(),
            label: "超长取值".into(),
            description: "测试用".into(),
            kind: FieldKind::Text,
            advanced: false,
        };
        let document = json!({"zz": {"long": "x".repeat(200)}});
        let mut panel = loaded("模型角色", document, vec![field]);
        panel.select_path(&path);
        let rows = screen(&mut panel, 80, 30);
        let (y, _) = locate(&rows, "超长取值").expect("字段行可见");
        let (text, columns) = &rows[y];
        let end = text.trim_end();
        assert!(end.ends_with('…'), "超宽值应以 … 收尾：{text:?}");
        let last = columns[end.chars().count() - 1];
        assert!(
            last < 80 - crate::render::MARGIN_X,
            "值越过右边距：止于第 {last} 列"
        );
    }

    /// 模型候选三种状态的前缀等宽，模型 ID 落在同一列。
    #[test]
    fn model_candidates_align_ids() {
        let mut panel = loaded(
            "供应商与模型",
            json!({"providers": {"p": {"models": {"a1": {}}}}}),
            Vec::new(),
        );
        panel.providers.discovery = Some(providers::Discovery {
            generation: 1,
            rows: ["a1", "b2", "c3"]
                .into_iter()
                .map(|id| moc_protocol::CatalogModel {
                    id: id.into(),
                    name: id.into(),
                })
                .collect(),
            checked: BTreeSet::from(["b2".to_owned()]),
            query: String::new(),
            error: None,
            pending: None,
        });
        panel.rebuild_rows();
        panel.select_action(ProviderAction::Candidate(0));
        let rows = screen(&mut panel, 100, 40);
        let column = |id| locate(&rows, id).expect("候选行可见").1;
        assert_eq!(column("a1"), column("b2"));
        assert_eq!(column("b2"), column("c3"));
    }

    /// MCP 服务分组行右侧显示启动方式。
    #[test]
    fn mcp_group_shows_server_address() {
        let mut panel = loaded(
            "MCP",
            json!({"mcp": {"servers": {"srv": {"command": "npx"}}}}),
            Vec::new(),
        );
        let rows = screen(&mut panel, 120, 40);
        assert!(locate(&rows, "srv").is_some(), "MCP 分组可见");
        assert!(locate(&rows, "本地程序：npx").is_some());
    }

    #[test]
    fn provider_workspace_and_draft_fit_narrow_terminals() {
        let mut panel = loaded(
            "供应商与模型",
            json!({"providers": {"empty": {"models": {}}}}),
            Vec::new(),
        );
        for (width, height) in [(120, 40), (80, 24), (60, 24), (40, 20)] {
            let rows = screen(&mut panel, width, height);
            assert!(
                locate(&rows, "empty").is_some(),
                "当前供应商必须可见：{width}x{height}"
            );
            assert!(
                panel
                    .hits
                    .iter()
                    .any(|(area, hit)| area.y < height - 3 && matches!(hit, Hit::Row(_))),
                "详情必须留有操作空间"
            );
            panel.new_provider();
            panel.providers.draft.as_mut().unwrap().key.input =
                PromptState::from_text("secret-must-stay-hidden".into());
            let rows = screen(&mut panel, width, height);
            assert!(
                !rows
                    .iter()
                    .any(|(text, _)| text.contains("secret-must-stay-hidden"))
            );
            for index in 0..6 {
                panel.providers.draft.as_mut().unwrap().focus = index;
                screen(&mut panel, width, height);
                assert!(
                    panel.hits.iter().any(|(area, hit)| area.y < height - 3
                        && matches!(hit, Hit::ProviderInput(i) if *i == index)),
                    "表单控件 {index} 在 {width}x{height} 必须可见"
                );
            }
            panel.providers.draft = None;
        }
    }

    /// 字段树的滚动条与主界面同一字形，贴终端右缘。
    #[test]
    fn field_scrollbar_matches_main_screen() {
        let mut panel = loaded("模型角色", models_config(), Vec::new());
        let rows = screen(&mut panel, 80, 16);
        assert!(
            rows.iter().any(|(text, _)| text.ends_with('▐')),
            "滚动条应在最右列：{rows:?}"
        );
    }

    /// 一帧的真实缓冲区（`screen` 只给文本，这里要按格看底色）。
    fn buffer_of(panel: &mut SettingsPanel, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
        terminal
            .draw(|frame| draw(frame, panel, false, Theme::global()))
            .expect("绘制成功");
        terminal.backend().buffer().clone()
    }

    /// 「界面」页的一组平铺字段，行数足以溢出正文区。
    fn flat_fields(count: usize) -> Vec<ConfigField> {
        (0..count)
            .map(|index| ConfigField {
                path: vec!["ui".into(), format!("f{index}")],
                tab: "界面".into(),
                label: format!("字段{index}"),
                description: "测试用".into(),
                kind: FieldKind::Text,
                advanced: false,
            })
            .collect()
    }

    /// 滚轮只滚视口：选中项不动，普通重绘也不把手动滚到的位置拉回。
    #[test]
    fn wheel_scrolls_fields_viewport_not_selection() {
        let mut panel = loaded("界面", json!({}), flat_fields(20));
        screen(&mut panel, 80, 16);
        assert!(panel.scroll.max_offset > 0, "行数应溢出正文区");

        crate::settings::scroll(&mut panel, (10, 10).into(), 6);
        assert_eq!(panel.selected, 0, "滚轮不得移动选中项");
        assert_eq!(panel.offset, 6.min(panel.scroll.max_offset));
        assert!(!panel.scroll.reveal_selection);

        let offset = panel.offset;
        screen(&mut panel, 80, 16);
        assert_eq!(panel.offset, offset, "普通重绘不得拉回手动滚到的位置");
    }

    /// 悬停只铺底色：不改选中行，选中行保留自己的色带。
    #[test]
    fn hover_paints_background_without_moving_selection() {
        let theme = Theme::global();
        let mut panel = loaded("界面", json!({}), flat_fields(20));
        screen(&mut panel, 80, 24);
        let (row_y, row_x) = {
            let (area, _) = panel
                .hits
                .iter()
                .find(|(area, hit)| {
                    matches!(hit, Hit::Row(index) if *index != panel.selected)
                        && area.height == 1
                })
                .expect("有未选中的行");
            (area.y, area.x)
        };
        let selected_y = panel
            .hits
            .iter()
            .find_map(|(area, hit)| match hit {
                Hit::Row(index) if *index == panel.selected => Some(area.y),
                _ => None,
            })
            .expect("选中行可见");
        panel.pointer = Some((row_x, row_y).into());

        let buffer = buffer_of(&mut panel, 80, 24);
        assert_eq!(panel.selected, 0, "悬停不得移动选中项");
        assert_eq!(buffer.cell((row_x + 2, row_y)).unwrap().bg, theme.bg_hover);
        let selected = buffer.cell((row_x + 2, selected_y)).unwrap();
        assert_eq!(selected.bg, theme.bg_visual, "选中行保留选中色带");
        assert_ne!(selected.bg, theme.bg_hover);
    }

    /// 供应商页滚轮按指针所在子区分流：侧栏滚动不动详情列表与侧栏光标。
    #[test]
    fn wheel_routes_to_provider_sidebar_not_detail() {
        let providers: serde_json::Map<String, Value> = (0..30)
            .map(|index| (format!("p{index:02}"), json!({"models": {}})))
            .collect();
        let mut panel = loaded(
            "供应商与模型",
            json!({ "providers": Value::Object(providers) }),
            Vec::new(),
        );
        screen(&mut panel, 100, 16);
        let point = panel
            .hits
            .iter()
            .find_map(|(area, hit)| match hit {
                Hit::Provider(_) => Some(*area),
                _ => None,
            })
            .map(|area| ratatui::layout::Position::new(area.x, area.y))
            .expect("侧栏行存在");
        assert!(panel.providers.scroll.max_offset > 0, "侧栏应溢出");

        crate::settings::scroll(&mut panel, point, 6);
        assert_eq!(panel.providers.offset, 6.min(panel.providers.scroll.max_offset));
        assert_eq!(panel.providers.cursor, 0, "滚轮不得移动侧栏光标");
        assert_eq!(panel.offset, 0, "详情列表的视口不得跟着滚动");
        assert!(!panel.providers.scroll.reveal_selection);

        let offset = panel.providers.offset;
        screen(&mut panel, 100, 16);
        assert_eq!(panel.providers.offset, offset, "普通重绘不得拉回侧栏");
    }

    /// 弹窗列表滚轮只滚自己的视口，光标行不动。
    #[test]
    fn popup_wheel_scrolls_viewport_not_cursor() {
        let mut panel = loaded("界面", json!({}), flat_fields(1));
        panel.edit = Some(EditState {
            target: EditTarget::Filter,
            editor: Editor::Select(SelectEditor::new(
                (0..30)
                    .map(|index| controls::Pick {
                        label: format!("候选{index}"),
                        action: PickAction::Set(json!(index)),
                    })
                    .collect(),
                &Value::Null,
            )),
            popup: true,
            title: "选择".into(),
            note: String::new(),
        });
        screen(&mut panel, 80, 24);
        let Editor::Select(editor) = &panel.edit.as_ref().unwrap().editor else {
            panic!("是单选编辑器");
        };
        assert!(editor.scroll.max_offset > 0, "候选应溢出弹窗");

        crate::settings::scroll(&mut panel, (40, 12).into(), 3);
        let Editor::Select(editor) = &panel.edit.as_ref().unwrap().editor else {
            panic!("是单选编辑器");
        };
        assert_eq!(editor.offset, 3.min(editor.scroll.max_offset));
        assert_eq!(editor.selected, 0, "滚轮不得移动弹窗光标");
        assert!(!editor.scroll.reveal_selection);

        let offset = editor.offset;
        screen(&mut panel, 80, 24);
        let Editor::Select(editor) = &panel.edit.as_ref().unwrap().editor else {
            panic!("是单选编辑器");
        };
        assert_eq!(editor.offset, offset, "普通重绘不得拉回弹窗视口");
    }
}

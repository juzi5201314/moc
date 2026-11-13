use super::*;
use crate::render::MARGIN_X;
use crate::render::labels::{self, SEP};
use crate::render::overlay;
use crate::render::wrap::{ELLIPSIS, clip_line, str_width, truncate, wrap_spans};
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

mod providers;
use providers::draw_maintenance;
use providers::draw_providers;
mod fields;
use fields::draw_fields;
mod editor;
use editor::draw_edit_popup;
mod values;
pub(super) use values::display_value;

/// 弹窗正文：已按宽度截好、按可见行数截窗。
struct PopupBody {
    lines: Vec<Line<'static>>,
    /// 每行对应编辑器里的第几行；`None` 不可点击。
    items: Vec<Option<usize>>,
    /// 光标位置：（正文行, 列）。
    cursor: Option<(usize, usize)>,
}

struct Wrapped {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_column: usize,
}

#[cfg(test)]
mod tests;

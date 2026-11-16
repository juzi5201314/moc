use super::super::Hit;
use super::super::SettingsPanel;
use super::super::controls::Entry;
use super::super::editors::Editor;
use super::super::editors::ListEditor;
use super::super::editors::RecordEditor;
use super::super::editors::TextEditor;
use super::PopupBody;
use super::Wrapped;
use super::cells;
use crate::modal::ModelScroll;
use crate::render::labels;
use crate::render::modal::window_offset;
use crate::render::overlay;
use crate::render::wrap::clip_line;
use crate::render::wrap::fit;
use crate::render::wrap::fit_line;
use crate::render::wrap::str_width;
use crate::render::wrap::wrap_text;
use crate::theme::Theme;
use crate::widgets::prompt::PromptState;
use moc_protocol::FieldKind;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use rust_i18n::t;

/// 弹窗编辑器：单选、清单、键值表与多行文本共用一张居中卡片；单行文本在字段行内编辑。
pub(super) fn draw_edit_popup(
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
pub(super) fn dismiss_around(card: Rect, area: Rect, hits: &mut Vec<(Rect, Hit)>) {
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

pub(super) fn popup_body(
    editor: &mut Editor,
    width: usize,
    rows: usize,
    theme: &Theme,
) -> PopupBody {
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
            let start = viewport(
                &mut editor.scroll,
                &mut editor.offset,
                editor.selected,
                rows,
                lines.len(),
            );
            window(lines, start, rows, None)
        }
        Editor::List(editor) => list_body(editor, width, rows, theme),
        Editor::Record(editor) => record_body(editor, width, rows, theme),
    }
}

/// 弹窗视口起点：光标移动过才显露，滚轮手动滚到的位置普通重绘不拉回；
/// 顺带回写 `max_offset`、记消费过的 `reveal_selection`，与模型选择器同一套。
pub(super) fn viewport(
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
pub(super) fn window(
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
pub(super) fn option_row(
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
pub(super) fn draft_row(
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
pub(super) fn list_draft(
    capture: bool,
    mark: &str,
    input: &PromptState,
    width: usize,
    theme: &Theme,
) -> (Line<'static>, Option<usize>) {
    if capture {
        let line = Line::from(t!("tui.editor.capture_fmt", mark = mark).into_owned()).style(
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

pub(super) fn list_body(
    editor: &mut ListEditor,
    width: usize,
    rows: usize,
    theme: &Theme,
) -> PopupBody {
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
            t!("tui.editor.custom").into_owned()
        } else {
            item.note.clone()
        };
        if default == Some(index) {
            note.push_str(&t!("tui.editor.default_mark"));
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
                    Entry::Level => t!("tui.editor.add_level"),
                    Entry::Key => t!("tui.editor.add_key_item"),
                    Entry::Text => t!("tui.editor.add_item"),
                };
                let spans = vec![Span::raw(text.into_owned())];
                lines.push((
                    option_row(editor.cursor == index, spans, width, theme),
                    Some(index),
                ));
            }
        }
    }
    if lines.is_empty() {
        lines.push((
            Line::styled(
                t!("tui.editor.no_options").into_owned(),
                Style::new().fg(theme.gray),
            ),
            None,
        ));
    }
    let total = lines.len();
    let focus = cursor.map_or(editor.cursor, |(row, _)| row);
    let start = viewport(&mut editor.scroll, &mut editor.offset, focus, rows, total);
    window(lines, start, rows, cursor)
}

pub(super) fn record_body(
    editor: &mut RecordEditor,
    width: usize,
    rows: usize,
    theme: &Theme,
) -> PopupBody {
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
            Span::styled(
                t!("tui.editor.required").into_owned(),
                Style::new().fg(theme.accent_warning),
            )
        } else {
            Span::styled(
                t!("tui.values.unset").into_owned(),
                Style::new().fg(theme.gray),
            )
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
                let (line, column) =
                    draft_row(&t!("tui.editor.key_name"), &draft.input, width, theme);
                cursor = Some((index, column));
                lines.push((line, None));
            }
            None => {
                let spans = vec![Span::raw(t!("tui.editor.add_key").into_owned())];
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
pub(super) fn text_body(editor: &TextEditor, width: usize, rows: usize) -> PopupBody {
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
pub(super) fn edit_line(editor: &TextEditor, width: usize) -> (String, usize) {
    let input = &editor.input;
    if matches!(editor.kind, FieldKind::Secret) && !editor.reveal {
        let masked = "•".repeat(input.text.chars().count());
        let cursor = input.text[..input.cursor].chars().count() * '•'.len_utf8();
        return tail(&masked, cursor, width);
    }
    tail(&input.text, input.cursor, width)
}

/// 光标处可见的文本尾巴：起点尽量靠后，保证光标不超出宽度。
pub(super) fn tail(text: &str, cursor: usize, width: usize) -> (String, usize) {
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

/// 多行输入按宽度折行，并给出光标所在行与列。
pub(super) fn wrap_editor(input: &PromptState, width: usize) -> Wrapped {
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

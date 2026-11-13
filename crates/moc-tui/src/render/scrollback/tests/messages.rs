use super::*;

/// 用户消息：圆角边框取 `accent_user`，首行 `❯` 提示符；时间戳放在末行
/// 框内右缘，余量不足时在框下另起一行。
#[test]
fn user_message_in_accent_frame_with_clock() {
    let theme = Theme::resolved(ColorLevel::TrueColor);
    let at_ms = 1_700_000_000_000;
    let clock = format_clock(at_ms).expect("演示时间戳应可格式化");

    let short = user_message("hi", Some(at_ms), 40, &theme);
    assert_eq!(short.len(), 4, "短消息 = 上框 + 正文 + 下框 + 留白");
    let top: String = short[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(top.contains('╭') && top.contains('╮'));
    let body = &short[1];
    assert!(
        body.spans
            .iter()
            .any(|span| span.content == "❯ " && span.style.fg == Some(theme.accent_user))
    );
    assert!(body.spans.iter().any(|span| span.content == clock));
    assert!(
        body.spans
            .iter()
            .any(|span| span.content == "│" && span.style.fg == Some(theme.accent_user))
    );
    let bottom: String = short[2]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(bottom.contains('╰') && bottom.contains('╯'));

    let narrow = user_message("一二三四五六七八九十", Some(at_ms), 20, &theme);
    for line in &narrow {
        assert!(line.width() <= 20, "行宽 {} 超出消息区域", line.width());
    }
    let clock_rows: Vec<_> = narrow
        .iter()
        .filter(|line| line.spans.iter().any(|span| span.content == clock))
        .collect();
    assert_eq!(clock_rows.len(), 1, "时间戳应恰好出现一次");
    assert!(
        clock_rows[0]
            .spans
            .iter()
            .all(|span| !span.content.contains('│')),
        "余量不足时时间戳应在框下另起一行"
    );
}

#[test]
fn thinking_block_and_rows_apply_thinking_and_grayscale_palette() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let app = App::new(crate::theme::Theme::global(), events);
    let lines = thinking_block(
        "test line",
        Some(10),
        Some(1_000),
        &app,
        80,
        DetailLevel::Full,
    );

    // Title line: spans[1] is '◆ ' in accent_thinking, spans[2] is 'Thought' in gray_bright
    let title_spans = &lines[0].spans;
    assert_eq!(title_spans[1].content, "◆ ");
    assert_eq!(title_spans[1].style.fg, Some(app.theme.accent_thinking));
    assert_eq!(title_spans[2].content, "Thought");
    assert_eq!(title_spans[2].style.fg, Some(app.theme.gray_bright));

    // Thinking rows: first content row starts with '▏ ' in accent_thinking, content in gray
    assert!(lines.len() >= 2);
    let row_spans = &lines[1].spans;
    assert_eq!(row_spans[1].content, "▏ ");
    assert_eq!(row_spans[1].style.fg, Some(app.theme.accent_thinking));
    assert_eq!(row_spans[2].style.fg, Some(app.theme.gray));
}

/// 回归：思考简略档的预览预算不被段间空行吃掉，并提示还藏了多少行。
#[test]
fn brief_thinking_skips_blank_rows_and_marks_omission() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let app = App::new(crate::theme::Theme::global(), events);
    let text = (1..=8)
        .map(|index| format!("段落 {index}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let lines = thinking_block(&text, None, None, &app, 80, DetailLevel::Brief);
    let body: Vec<String> = lines[1..lines.len() - 1].iter().map(plain).collect();
    for row in &body {
        assert_ne!(row.trim(), "▏", "预览里不应出现空引文行");
    }
    assert!(
        body.iter().any(|row| row.contains("段落 8")),
        "保留最新内容"
    );
    assert!(
        body.last()
            .is_some_and(|row| row.contains(&crate::render::labels::more_lines(4))),
        "末行提示省略：{body:?}"
    );
}

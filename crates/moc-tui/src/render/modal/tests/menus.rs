use super::*;

#[test]
fn completion_menu_renders_solid_opaque_background() {
    use crate::widgets::completion::CompletionState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    use ratatui::layout::Rect;

    let mut app_for_completion = App::new(Theme::global(), unbounded_channel().0);
    app_for_completion.prompt.insert_str_raw("/he");
    let mut completion = CompletionState::derive(&app_for_completion).expect("构造补全状态");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
    let prompt_area = Rect::new(0, 10, 80, 3);
    terminal
        .draw(|frame| {
            // 用醒目底色填满缓冲，模拟底层聊天记录或透明终端
            frame.buffer_mut().set_style(
                Rect::new(0, 0, 80, 24),
                ratatui::style::Style::new().bg(Color::Red),
            );
            super::super::draw_completion_menu(frame, &mut completion, prompt_area);
        })
        .expect("绘制成功");

    let theme = Theme::global();
    let area = completion.area;
    assert!(area.width > 0 && area.height > 0);
    let buffer = terminal.backend().buffer();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if x > area.x && buffer.cell((x - 1, y)).is_some_and(|c| c.cell_width() > 1) {
                continue;
            }
            let cell = buffer.cell((x, y)).expect("单元格存在");
            assert_ne!(
                cell.bg,
                Color::Reset,
                "浮层区域不应出现 Reset 穿透：({x}, {y})"
            );
            assert_ne!(cell.bg, Color::Red, "浮层区域不应泄露底层颜色：({x}, {y})");
            assert!(
                cell.bg == theme.bg_base || cell.bg == theme.bg_visual,
                "单元格背景必须为实色 base 或 visual 高亮：({x}, {y}) 实际为 {:?}",
                cell.bg
            );
        }
    }
}

#[test]
fn engine_menu_renders_solid_opaque_background() {
    use crate::app::EngineMenu;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    let (events, _rx) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.engine_menu = Some(EngineMenu {
        selected: 0,
        items: crate::app::ENGINES.to_vec(),
        area: Rect::default(),
    });
    app.engine_area = Rect::new(50, 20, 10, 1);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
    terminal
        .draw(|frame| {
            frame.buffer_mut().set_style(
                Rect::new(0, 0, 80, 24),
                ratatui::style::Style::new().bg(Color::Red),
            );
            super::super::draw_engine_menu(frame, &mut app);
        })
        .expect("绘制成功");

    let theme = app.theme;
    let menu_area = app.engine_menu.as_ref().unwrap().area;
    assert!(menu_area.width > 0 && menu_area.height > 0);
    let buffer = terminal.backend().buffer();
    for y in menu_area.y..menu_area.y + menu_area.height {
        for x in menu_area.x..menu_area.x + menu_area.width {
            let cell = buffer.cell((x, y)).expect("单元格存在");
            // 宽字符的尾格：渲染时被引导格盖住，diff 不冲刷，backend 里留着
            // 初始 EMPTY（Reset）；上屏时显示的是引导格的右半，不算穿透。
            if x > menu_area.x
                && crate::render::str_width(buffer.cell((x - 1, y)).expect("单元格存在").symbol())
                    > 1
            {
                continue;
            }
            assert_ne!(
                cell.bg,
                Color::Reset,
                "引擎菜单不应出现 Reset 穿透：({x}, {y})"
            );
            assert_ne!(cell.bg, Color::Red, "引擎菜单不应泄露底层颜色：({x}, {y})");
            assert!(
                cell.bg == theme.bg_base || cell.bg == theme.bg_visual,
                "单元格背景必须为实色 base 或 visual：({x}, {y}) 实际为 {:?}",
                cell.bg
            );
        }
    }
}

/// 快捷键一览排版：行不越宽、无省略号、不再列斜杠命令；末尾是指路行。
#[test]
fn help_layout_has_no_gaps_or_fake_ellipsis() {
    let (events, _rx) = unbounded_channel();
    let app = App::new(Theme::global(), events);

    for width in [92usize, 40] {
        let (lines, _) = super::super::help_lines(&app, width);
        for line in &lines {
            let text = line_text(line);
            assert!(line_width(&text) <= width, "行宽应不超过 {width}：{text:?}");
            assert!(!text.contains('…'), "{width} 宽下不应有截断：{text:?}");
            assert!(
                !text.starts_with("  /"),
                "{width} 宽下不应列出命令行：{text:?}"
            );
        }
        // 末尾指路斜杠命令：输入 `/` 即见全表。
        let last = lines
            .iter()
            .map(line_text)
            .rev()
            .find(|text| !text.trim().is_empty())
            .expect("末尾非空行");
        assert_eq!(last, "输入 / 查看全部斜杠命令");
    }

    // 「选择模型」出现在键位列上限之内。
    let (lines, _) = super::super::help_lines(&app, 92);
    let row = lines
        .iter()
        .map(line_text)
        .find(|text| text.contains("选择模型"))
        .expect("选择模型行");
    let offset = row.find("选择模型").expect("描述在行内");
    let column = line_width(&row[..offset]);
    assert!(
        column
            <= super::super::HELP_INDENT + super::super::HELP_KEY_MAX + super::super::HELP_KEY_GAP,
        "描述列起点 {column} 超过键位列上限"
    );
}

/// 事件层只加不减的滚动偏移由渲染写回收紧：滚到底后往上滚一格立即生效。
#[test]
fn help_scroll_is_clamped_on_draw() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _rx) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.modal = Some(Modal::Help { scroll: usize::MAX });
    // 矮终端：帮助正文（约 20 行）超过正文可见行数才会出现滚动。
    let mut terminal = Terminal::new(TestBackend::new(100, 14)).expect("构造测试终端");
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");
    let Some(Modal::Help { scroll: max }) = app.modal else {
        unreachable!()
    };
    assert!((4..1000).contains(&max), "写回应把偏移收紧到底部：{max}");

    app.modal = Some(Modal::Help { scroll: max - 3 });
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");
    // 状态行格式「⋯ a-b/total 行」：偏移减一，窗口起点立即跟着减一。
    let marker = format!(" {}-", max - 2);
    let screen = terminal.backend().buffer();
    let found = (0..14u16).any(|y| {
        (0..100u16)
            .filter_map(|x| screen.cell((x, y)).map(|cell| cell.symbol()))
            .collect::<String>()
            .contains(&marker)
    });
    assert!(found, "状态行应包含 {marker:?}");
}

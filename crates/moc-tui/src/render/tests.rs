use super::*;
use crate::render::prompt::prompt_lines;
use crate::theme::Theme;
use crate::transcript::Block as TranscriptBlock;

#[test]
fn prompt_crlf_and_variation_selector_widths() {
    assert_eq!(wrap::str_width(&wrap::fit("❤️abc", 2)), 2);
    for text in ["❤️abc", "abc❤️", "👩‍💻abc", "", "a\r\nb"] {
        for width in 0..=6 {
            assert!(wrap::str_width(&wrap::truncate(text, width)) <= width);
            assert!(wrap::str_width(&wrap::truncate_head(text, width)) <= width);
            assert_eq!(wrap::str_width(&wrap::fit(text, width)), width);
            let line = wrap::fit_line(ratatui::text::Line::raw(text.to_owned()), width);
            assert_eq!(wrap::line_width(&line), width);
            assert!(wrap::str_width(&wrap::display_window(text, 0, width)) <= width);
            if width >= 2 {
                assert!(
                    wrap::wrap_text(text, width)
                        .iter()
                        .all(|row| wrap::str_width(row) <= width)
                );
            }
        }
    }
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.prompt.insert_str_raw("a\r\nb");
    app.prompt.up();
    let area = Rect::new(0, 0, 80, 4);
    assert_eq!(prompt_cursor_position(&app, area).1, 1);
    for cursor in 0..=app.prompt.text.len() {
        app.prompt.cursor = cursor;
        let _ = prompt_cursor_position(&app, area);
    }
}

#[test]
fn prompt_preserves_spaces_after_deleting_arguments() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let area = Rect::new(2, 3, 30, 3);
    app.prompt.insert_text("/goal  xyz");
    for _ in 0..3 {
        app.prompt.backspace();
    }
    assert_eq!(app.prompt.text, "/goal  ");
    // `/goal` 已注册 → 渲染成「图标 名称」控件，光标位置经显示映射换算。
    assert_eq!(
        prompt_lines(&app, area)[0].to_string(),
        " ❯ \u{a0}/\u{a0}goal\u{a0}  "
    );
    assert_eq!(prompt_cursor_position(&app, area), (16, 4));
    assert_eq!(prompt_cursor_from_point(&app, area, 16, 4), Some(7));
    app.prompt.insert_char('x');
    assert_eq!(prompt_cursor_position(&app, area), (17, 4));
}

#[test]
fn prompt_only_highlights_registered_command_tokens() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.commands
        .set_plugin_commands(vec![moc_protocol::commands::PluginCommand {
            name: "deploy".into(),
            description: String::new(),
            usage: String::new(),
            params: Vec::new(),
            plugin: "demo".into(),
            origin: String::new(),
        }]);
    for width in [18, 80] {
        for (text, highlighted) in [
            ("  /help 参数 /help", "\u{a0}/\u{a0}help\u{a0}"),
            ("/exit 参数", "\u{a0}/\u{a0}exit\u{a0}"),
            ("/deploy 参数", "\u{a0}/\u{a0}deploy\u{a0}"),
            ("/unknown-command 参数", ""),
            ("普通文本 /help", ""),
            ("", ""),
        ] {
            app.prompt.text = text.into();
            let lines = prompt_lines(&app, Rect::new(0, 0, width, 20));
            let body: Vec<_> = lines
                .iter()
                .flat_map(|line| line.spans.iter().skip(2))
                .collect();
            let colored: String = body
                .iter()
                .filter(|span| span.style.fg == Some(app.theme.accent_warning))
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(colored, highlighted, "{text} @ {width}");
            assert!(
                body.iter()
                    .all(|span| span.style.fg == Some(app.theme.text_primary)
                        || span.style.fg == Some(app.theme.accent_warning))
            );
        }
    }
}

#[test]
fn prompt_wrap_keeps_spaces_and_cursor_mapping() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let area = Rect::new(0, 0, 14, 5);
    // 正文宽度 8 格；涵盖宽字符、连续空格、硬换行与滚动。
    app.prompt.insert_text("中文ab    cd\n123456789");
    assert_eq!(prompt_content_rows(&app, area.width), 4);
    for cursor in app
        .prompt
        .text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(app.prompt.text.len()))
        .collect::<Vec<_>>()
    {
        app.prompt.cursor = cursor;
        let (x, y) = prompt_cursor_position(&app, area);
        assert_eq!(prompt_cursor_from_point(&app, area, x, y), Some(cursor));
    }
    app.prompt.cursor = "中文ab  ".len();
    assert_eq!(prompt_cursor_position(&app, area), (4, 1));
    assert_eq!(prompt_lines(&app, area)[0].to_string(), "     cd");
}

/// 经 `resource/inspect` 确认的资源 token 渲染成控件：显示文本与光标、点击都按
/// 控件边界换算，落点不进入控件内部。
#[test]
fn prompt_resource_refs_render_and_map_atomically() {
    use moc_protocol::methods::SessionInfo;

    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.session = Some(SessionInfo {
        id: "ses_test".into(),
        title: "t".into(),
        model: "m".into(),
        permission: moc_protocol::SessionPermission::Full,
        engine: moc_protocol::SessionEngine::Standard,
        autonomy: moc_protocol::SessionAutonomy::Default,
        plan_active: false,
        fast_mode: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: "/tmp".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        lock: None,
    });
    app.refs.take_missing("ses_test", "@src/main.rs");
    app.refs.store(
        "ses_test",
        vec!["@src/main.rs".to_owned()],
        vec![Some(moc_protocol::ResourceRefInfo {
            kind: "file".into(),
            label: "main.rs".into(),
            detail: None,
        })],
    );
    app.prompt.text = "open @src/main.rs".into();
    app.prompt.cursor = app.prompt.text.len();
    let area = Rect::new(0, 0, 40, 5);

    let line = prompt_lines(&app, area)[0].to_string();
    assert_eq!(line, " ❯ open \u{a0}▤\u{a0}main.rs\u{a0}", "{line:?}");
    // 光标本在 token 末尾 → 显示在控件右缘之外（open ␣ = 5 列 + 控件 11 列）。
    assert_eq!(prompt_cursor_position(&app, area), (20, 1));
    // 点控件中部 → 光标落在控件首边界；点靠右缘 → 落在尾边界。
    assert_eq!(prompt_cursor_from_point(&app, area, 10, 1), Some(5));
    assert_eq!(prompt_cursor_from_point(&app, area, 18, 1), Some(17));
    // 拖选只覆盖控件前半截时，控件依然整块进入选区原文。
    app.prompt_area = area;
    app.selection = Some(((4, 1), (11, 1)));
    assert_eq!(prompt_selection_text(&app), "open @src/main.rs");
    app.selection = Some(((10, 1), (11, 1)));
    assert_eq!(prompt_selection_text(&app), "@src/main.rs");
    app.selection = None;
    // token 无效则不成形：会话串不符时缓存视为空。
    app.refs.take_missing("other", "x");
    assert_eq!(
        prompt_lines(&app, area)[0].to_string(),
        " ❯ open @src/main.rs"
    );
}

/// 窗口标题的状态选择：空闲留项目名，运行中上 spinner，等待人工置顶提示。
#[test]
fn terminal_title_reflects_app_state() {
    use moc_protocol::methods::SessionInfo;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.session = Some(SessionInfo {
        id: "ses_test".into(),
        title: "测试会话".into(),
        model: "test/model".into(),
        permission: moc_protocol::SessionPermission::Full,
        engine: moc_protocol::SessionEngine::Standard,
        autonomy: moc_protocol::SessionAutonomy::Default,
        plan_active: false,
        fast_mode: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: "/home/user/myproject".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        lock: None,
    });
    assert_eq!(terminal_title(&app), "moc — myproject");

    app.prompt_running = true;
    app.prompt_started_at = Some(std::time::Instant::now());
    let running = terminal_title(&app);
    assert!(running.ends_with("moc — myproject"), "{running}");
    assert!(
        running.starts_with(['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏']),
        "{running}"
    );

    app.modal = Some(Modal::Permission {
        params: moc_protocol::methods::PermissionRequestParams {
            session_id: "ses_test".into(),
            prompt_id: "prm_001".into(),
            request_id: "req_001".into(),
            tool_call_id: "call_001".into(),
            tool_name: "bash".into(),
            title: "执行命令".into(),
            body: None,
            diff: None,
            options: vec![],
        },
        selected: 0,
        scroll_offset: 0,
        reply: None,
    });
    assert!(terminal_title(&app).contains("needs input"));
}

/// 拖拽换算：轨道顶端对应历史顶端，轨道末端对应跟随尾部；抓取点保持。
#[test]
fn scrollbar_drag_maps_rows_to_offsets() {
    let bar = ScrollbackScrollbar {
        track: Rect::new(79, 4, 1, 10),
        thumb: 3,
        max_offset: 20,
    };
    // 轨道 10 行、滑块 3 行 → 可移动 7 行
    assert_eq!(bar.offset_at(4, 0), 20, "轨道顶端 = 历史顶端");
    assert_eq!(bar.offset_at(11, 0), 0, "轨道末端 = 跟随尾部");
    assert_eq!(bar.offset_at(8, 0), 9, "轨道中线落在中间偏移附近");
    // 抓取点：按在滑块第 2 行时，该行保持指向原偏移。
    assert_eq!(bar.offset_at(9, 1), 9);
    // 轨道之外的行夹到两端，不越界。
    assert_eq!(bar.offset_at(0, 0), 20);
    assert_eq!(bar.offset_at(30, 0), 0);
    // 滑块与轨道等高：永远回到底部（无溢出空间）。
    let flat = ScrollbackScrollbar {
        track: Rect::new(79, 4, 1, 10),
        thumb: 10,
        max_offset: 0,
    };
    assert_eq!(flat.offset_at(8, 4), 0);
}

/// 「回到底部」按钮压在滚动区底缘一行正文上（不占行），贴尾时消失。
#[test]
fn jump_bottom_pill_floats_over_scrollback_bottom() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let text = (0..200)
        .map(|index| format!("line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_block(TranscriptBlock::Assistant { text });
    app.scroll_offset = 8;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
    let mut view = ScrollbackView::new();
    terminal
        .draw(|frame| draw(frame, &mut app, &mut view))
        .expect("绘制成功");

    let theme = Theme::global();
    let pill = app.jump_bottom_area;
    let buffer = terminal.backend().buffer();
    assert_eq!((pill.width, pill.height), (1, 1));
    assert_eq!(
        pill.bottom(),
        app.scrollback_area.bottom(),
        "箭头压在滚动区底缘、输入框上方"
    );
    let center = app.scrollback_area.x + app.scrollback_area.width / 2;
    assert!(
        pill.x <= center && center <= pill.right(),
        "单格箭头落在中间两格之一即居中"
    );
    let cell = &buffer[(pill.x, pill.y)];
    assert_eq!(cell.symbol(), "▼", "实心三角箭头");
    assert_eq!(cell.fg, theme.text_primary, "白字");
    assert!(cell.modifier.contains(Modifier::BOLD), "加粗");
    assert_eq!(cell.bg, theme.bg_base, "无底色、浮在正文上");

    app.scroll_offset = 0;
    terminal
        .draw(|frame| draw(frame, &mut app, &mut view))
        .expect("绘制成功");
    assert!(app.jump_bottom_area.is_empty(), "贴尾时胶囊消失");
}

/// 状态图例的命中区正好盖住引擎名；就地菜单右上对齐图例、整体贴在其上方。
#[test]
fn status_legend_hit_area_carries_the_menu() {
    use moc_protocol::methods::SessionInfo;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.session = Some(SessionInfo {
        id: "ses_test".into(),
        title: "测试会话".into(),
        model: "test/model".into(),
        permission: moc_protocol::SessionPermission::Full,
        engine: moc_protocol::SessionEngine::Standard,
        autonomy: moc_protocol::SessionAutonomy::Default,
        plan_active: true,
        fast_mode: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: "/tmp".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        lock: None,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
    let mut view = ScrollbackView::new();
    terminal
        .draw(|frame| draw(frame, &mut app, &mut view))
        .expect("绘制成功");

    let row_text = |terminal: &Terminal<TestBackend>, area: Rect| {
        (area.x..area.x.saturating_add(area.width))
            .filter_map(|x| terminal.backend().buffer().cell((x, area.y)))
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    let legend = app.engine_area;
    assert_eq!(legend.height, 1);
    assert_eq!(
        row_text(&terminal, legend),
        "standard+plan",
        "命中区正好是状态图例"
    );

    app.open_engine_menu();
    assert_eq!(
        app.engine_menu.as_ref().map(|menu| menu.selected),
        Some(0),
        "默认高亮当前引擎"
    );
    terminal
        .draw(|frame| draw(frame, &mut app, &mut view))
        .expect("绘制成功");
    let menu = app.engine_menu.as_ref().expect("菜单保持打开");
    assert_eq!(menu.area.right(), legend.right(), "右缘对齐状态图例");
    assert_eq!(menu.area.bottom(), legend.y, "整体贴在图例上方");
    assert_eq!(menu.entry_at(menu.area.x + 4, menu.area.y + 1), Some(0));
    // 当前引擎 standard 是菜单第一项，位于图例上方第 4 行（边框 + 3 条目）；
    // 正文从「边框 1 + 卡片内边距 CARD_PAD_X」之后开始。
    let current_y = legend.y - 4;
    assert!(
        row_text(
            &terminal,
            Rect {
                x: menu.area.x + 2,
                y: current_y,
                width: 12,
                height: 1,
            },
        )
        .starts_with("❯ standard"),
        "当前引擎行同时是菜单高亮行"
    );
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((menu.area.x + 4, current_y))
            .expect("表头格存在")
            .fg,
        Theme::global().accent_model
    );
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((menu.area.x + 3, menu.area.y + 2))
            .expect("次行格存在")
            .fg,
        Theme::global().text_primary
    );

    // 本次提示临时问答：图例追加 `+ask`，会话引擎与 plan 叠加态不变；清除后即恢复。
    app.engine_menu = None;
    app.transient_mode = Some(moc_protocol::methods::PromptMode::Ask);
    terminal
        .draw(|frame| draw(frame, &mut app, &mut view))
        .expect("绘制成功");
    assert_eq!(row_text(&terminal, app.engine_area), "standard+plan+ask");
    assert_eq!(
        app.session
            .as_ref()
            .map(|session| { (session.engine, session.plan_active) }),
        Some((moc_protocol::SessionEngine::Standard, true)),
        "临时模式不得改写会话引擎与 plan 态"
    );
    app.transient_mode = None;
    terminal
        .draw(|frame| draw(frame, &mut app, &mut view))
        .expect("绘制成功");
    assert_eq!(row_text(&terminal, app.engine_area), "standard+plan");
    app.session.as_mut().unwrap().model = crate::modal::fusion_selector(
        "deepseek-ai/DeepSeek-V4.1-Flash-1017",
        "zai-org/GLM-5.3-Flash",
    );
    for width in [60, 86, 120] {
        for running in [false, true] {
            app.prompt_running = running;
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| draw(frame, &mut app, &mut view))
                .unwrap();
            assert!(!app.legend_tip_area.is_empty(), "保留帮助或停止入口");
            assert_eq!(row_text(&terminal, app.engine_area), "standard+plan");
            assert_eq!(row_text(&terminal, app.permission_tag_area), "full");
            let model = row_text(&terminal, app.model_tag_area);
            assert!(model.starts_with("Fusion"));
            assert!(app.model_tag_area.right() < app.engine_area.x);
            if width <= 86 {
                assert!(!model.contains("deepseek-ai/"));
                assert!(model.ends_with('…'));
            }
        }
    }
}

#[test]
fn manual_shell_full_access_hint_preserves_prompt_legends() {
    moc_i18n::set_locale("zh");
    use moc_protocol::SessionPermission;
    use ratatui::{Terminal, backend::TestBackend};
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.session = Some(serde_json::from_value(serde_json::json!({
        "id":"shell", "title":"shell", "model":"test/model", "permission":"workspace", "engine":"standard", "autonomy":"default", "plan_active":false, "fast_mode":false, "engine_locked":false, "cwd":"/tmp", "created_at_ms":0, "updated_at_ms":0
    })).unwrap());
    for width in [60, 240] {
        for permission in [
            SessionPermission::Full,
            SessionPermission::Workspace,
            SessionPermission::WorkspaceOnly,
        ] {
            for text in ["!echo ok", "!!echo ok", "ordinary", ""] {
                app.session.as_mut().unwrap().permission = permission;
                app.prompt.text = text.into();
                let mut terminal = Terminal::new(TestBackend::new(width, 5)).unwrap();
                terminal
                    .draw(|frame| super::prompt::draw_prompt(frame, &mut app, frame.area()))
                    .unwrap();
                let mut row = String::new();
                let mut x = 0;
                while x < width {
                    let symbol = terminal.backend().buffer()[(x, 4)].symbol();
                    row.push_str(symbol);
                    x += u16::try_from(super::wrap::str_width(symbol).max(1)).unwrap();
                }
                let hint = moc_i18n::t!("tui.prompt.manual_full_access");
                assert_eq!(
                    row.contains(hint.as_ref()),
                    width == 240 && permission != SessionPermission::Full && text.starts_with('!')
                );
                assert!(!app.legend_tip_area.is_empty());
                assert!(!app.engine_area.is_empty());
                assert!(!app.permission_tag_area.is_empty());
                assert!(row.contains(permission.as_str()));
            }
        }
    }
}

/// 告警贴滚动区右上角、notice 贴右下角；两者同屏共存且各自只占一行。
///
/// 同时校验强调色：告警用 warning、普通 notice 用中性 tool 色、失败 notice 用 error 色。
#[test]
fn warning_chip_sits_top_right_and_notice_bottom_right() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let area = Rect {
        x: 2,
        y: 1,
        width: 40,
        height: 10,
    };
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("构造测试终端");

    let row_text = |terminal: &Terminal<TestBackend>, row: u16| {
        (0..60)
            .filter_map(|x| terminal.backend().buffer().cell((x, row)))
            .map(|cell| cell.symbol())
            .collect::<String>()
    };

    app.set_warning("model model → model-lite");
    app.set_notice("clipboard image attached");
    terminal
        .draw(|frame| {
            draw_warning(frame, &app, area);
            draw_notice(frame, &app, area);
        })
        .expect("绘制成功");

    let top = row_text(&terminal, area.y);
    assert!(top.contains("▌ ◆ model model → model-lite"), "{top:?}");
    let bottom = row_text(&terminal, area.y + area.height - 1);
    assert!(
        bottom.contains("▌ ◆ clipboard image attached"),
        "{bottom:?}"
    );
    assert!(
        row_text(&terminal, area.y + 1).trim().is_empty(),
        "告警只占一行"
    );

    // 右对齐：胶囊右端贴 area 右边；图标格配色区分告警与提示
    let chip_width = u16::try_from(str_width("model model → model-lite") + 7).expect("宽度有界");
    let icon_x = area.x + area.width - chip_width + 3;
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((icon_x, area.y))
            .expect("图标格存在")
            .fg,
        Theme::global().accent_warning
    );
    let notice_width = u16::try_from(str_width("clipboard image attached") + 7).expect("宽度有界");
    let notice_icon_x = area.x + area.width - notice_width + 3;
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((notice_icon_x, area.y + area.height - 1))
            .expect("图标格存在")
            .fg,
        Theme::global().accent_tool
    );

    app.set_error_notice("clipboard image attached");
    terminal
        .draw(|frame| draw_notice(frame, &app, area))
        .expect("绘制成功");
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((notice_icon_x, area.y + area.height - 1))
            .expect("图标格存在")
            .fg,
        Theme::global().accent_error,
        "失败提示用错误色"
    );
}

/// 拖选高亮按格子收边：同一行只亮选区内的列，跨行时首尾行才铺到区域边缘。
#[test]
fn selection_highlight_covers_selected_columns_only() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // 测试环境没有终端配色能力，先给主题塞两个可区分的颜色再交给 App。
    let mut custom = *Theme::global();
    custom.bg_selection = ratatui::style::Color::Rgb(10, 20, 30);
    custom.text_primary = ratatui::style::Color::Rgb(200, 200, 200);
    let theme: &'static Theme = Box::leak(Box::new(custom));

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(theme, events);
    let area = Rect::new(2, 1, 20, 5);
    let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("构造测试终端");
    let bg = |terminal: &Terminal<TestBackend>, column: u16, row: u16| {
        terminal
            .backend()
            .buffer()
            .cell((column, row))
            .expect("格存在")
            .bg
    };

    app.selection = Some(((5, 1), (10, 1)));
    terminal
        .draw(|frame| highlight_selection(frame, &app, area))
        .expect("绘制成功");
    assert_eq!(bg(&terminal, 5, 1), theme.bg_selection, "起点格子高亮");
    assert_eq!(
        bg(&terminal, 10, 1),
        theme.bg_selection,
        "终点格子包含在选区内"
    );
    assert_ne!(bg(&terminal, 4, 1), theme.bg_selection, "选区左侧不铺色");
    assert_ne!(bg(&terminal, 11, 1), theme.bg_selection, "选区右侧不铺色");
    assert_ne!(bg(&terminal, 5, 2), theme.bg_selection, "选区外的行不铺色");

    // 反向跨行拖选：首行铺到右缘、末行从左缘铺到终点，中间整行。
    app.selection = Some(((10, 3), (5, 1)));
    terminal
        .draw(|frame| highlight_selection(frame, &app, area))
        .expect("绘制成功");
    assert_ne!(bg(&terminal, 4, 1), theme.bg_selection);
    assert_eq!(bg(&terminal, area.right() - 1, 1), theme.bg_selection);
    assert_eq!(
        bg(&terminal, area.x, 2),
        theme.bg_selection,
        "中间行整行高亮"
    );
    assert_eq!(bg(&terminal, area.x, 3), theme.bg_selection);
    assert_eq!(bg(&terminal, 10, 3), theme.bg_selection);
    assert_ne!(bg(&terminal, 11, 3), theme.bg_selection);
}

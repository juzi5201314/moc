use super::*;

/// 推荐项在标签后显示星号；星号与用户当前勾选状态相互独立，多选可显示多个星号。
#[test]
fn ask_recommendations_render_after_option_labels() {
    let (events, _receiver) = unbounded_channel();
    let app = App::new(Theme::global(), events);
    let mut modal = ask_modal(0, 0);
    let Modal::Ask {
        params, selected, ..
    } = &mut modal
    else {
        unreachable!("测试模态应为 ask");
    };
    // 多星号只可能出现在多选问题；多选不支持预览，先把 fixture 的预览清掉。
    params.questions[0].multi = true;
    params.questions[0].recommended = vec![0, 1];
    for option in &mut params.questions[0].options {
        option.preview = None;
    }
    selected[0] = vec!["上中下".to_owned()];

    let texts: Vec<String> = modal_lines(&app, &modal, 80, 24)
        .iter()
        .map(line_text)
        .collect();
    let first = texts
        .iter()
        .find(|text| text.contains("上中下 (Recommended)"))
        .expect("第一个推荐项应带推荐后缀");
    assert!(
        first.contains("☑ 上中下 (Recommended)"),
        "已选推荐项仍应保留推荐后缀：{first}"
    );
    let second = texts
        .iter()
        .find(|text| text.contains("左右 (Recommended)"))
        .expect("第二个推荐项应带推荐后缀");
    assert!(
        second.contains("☐ 左右 (Recommended)"),
        "未选推荐项也应显示推荐后缀：{second}"
    );
}

/// 问题正文进限高窗口；聚焦项 description 进底部详情区（非聚焦项不铺满）；
/// 标题栏只放 header；矮终端详情区先让位，问题正文与选项行保留。
#[test]
fn ask_body_renders_question_and_descriptions() {
    let (events, _receiver) = unbounded_channel();
    let app = App::new(Theme::global(), events);
    let mut modal = ask_modal(0, 0);
    if let Modal::Ask { params, .. } = &mut modal {
        params.questions[0].header = Some("方案".into());
        params.questions[0].question =
            "这是一个很长很长的问题正文，应当折行展示，而不是挤在标题栏里被截断。".into();
        for option in &mut params.questions[0].options {
            option.preview = None;
        }
        params.questions[0].options[0].description = Some("第一个选项的含义说明".into());
        params.questions[0].options[1].description = Some("第二个选项的含义说明".into());
    }
    assert_eq!(modal.title(), "方案", "标题栏只放 header");

    let texts: Vec<String> = modal_lines(&app, &modal, 40, 24)
        .iter()
        .map(line_text)
        .collect();
    assert!(
        texts
            .iter()
            .any(|text| text.contains("这是一个很长很长的问题正文")),
        "问题正文应出现在正文区：{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("第一个选项的含义说明")),
        "聚焦项 description 应渲染进详情区：{texts:?}"
    );
    assert!(
        !texts
            .iter()
            .any(|text| text.contains("第二个选项的含义说明")),
        "非聚焦项 description 不铺满列表（渐进披露）：{texts:?}"
    );

    let tight: Vec<String> = modal_lines(&app, &modal, 40, 8)
        .iter()
        .map(line_text)
        .collect();
    assert!(
        tight.iter().any(|text| text.contains("问题正文")),
        "矮终端仍显示问题正文：{tight:?}"
    );
    assert!(
        tight.iter().any(|text| text.contains("上中下")),
        "矮终端仍显示选项行：{tight:?}"
    );
    assert!(
        !tight.iter().any(|text| text.contains("含义说明")),
        "矮终端详情区先让位给选项列表：{tight:?}"
    );
}

/// 折叠态：卡片收成占据输入框位的细条，左侧进度与当前问、右侧返回/取消键，
/// 不再绘制选项行，命中几何只剩细条自身。
#[test]
fn ask_parked_collapses_to_strip_over_prompt() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let mut modal = ask_modal(0, 0);
    if let Modal::Ask { params, parked, .. } = &mut modal {
        params.questions[0].header = Some("布局偏好".into());
        *parked = true;
    }
    app.modal = Some(modal);
    app.prompt_area = ratatui::layout::Rect::new(3, 20, 114, 3);
    app.ask_option_hits = vec![(0, ratatui::layout::Rect::new(10, 5, 40, 1))];
    let (width, height) = (120u16, 30u16);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");

    assert_eq!(app.ask_strip_area, app.prompt_area, "细条应占住输入框位");
    assert!(app.ask_option_hits.is_empty(), "折叠态没有可点选项");
    let text: String = (0..width)
        .filter_map(|x| terminal.backend().buffer().cell((x, 21)))
        .map(|cell| cell.symbol())
        .collect();
    let compact = text.replace(' ', "");
    assert!(compact.contains("布局偏好"), "细条应显示当前问：{text:?}");
    assert!(
        compact.contains("TabExpand") && compact.contains("EscCancel"),
        "细条应给出返回与取消的路：{text:?}"
    );
}

/// 提问预览：宽内容宽度并排（左选项、右聚焦项详情），窄内容宽度详情进底部署名区；
/// 预览/描述内容都跟随聚焦项。
#[test]
fn ask_preview_joins_columns_when_wide_and_stacks_when_narrow() {
    let (events, _receiver) = unbounded_channel();
    let app = App::new(Theme::global(), events);
    let modal = ask_modal(0, 0);

    // 并排：聚焦项与其预览同行，分隔线左缘对齐。
    let wide: Vec<String> = modal_lines(&app, &modal, 100, 24)
        .iter()
        .map(line_text)
        .collect();
    let row = wide
        .iter()
        .find(|text| text.contains("❯ ○ 上中下"))
        .expect("选项行");
    assert!(row.contains("+--+"), "聚焦项预览应与选项同行：{row:?}");
    let offsets: Vec<usize> = wide
        .iter()
        .filter_map(|text| text.split_once(" │ "))
        .map(|(left, _)| crate::render::wrap::str_width(left))
        .collect();
    assert!(offsets.len() >= 3, "选项与预览应共行：{wide:?}");
    assert!(
        offsets.iter().all(|offset| *offset == offsets[0]),
        "左栏宽度应逐行一致：{offsets:?}"
    );
    assert!(
        wide.iter()
            .all(|text| crate::render::wrap::str_width(text) <= 100),
        "并排行不得超出内容宽度：{wide:?}"
    );

    // 窄内容宽度：预览随聚焦项进底部详情区（与描述同一去处），不分栏。
    let narrow: Vec<String> = modal_lines(&app, &modal, 60, 24)
        .iter()
        .map(line_text)
        .collect();
    assert!(!narrow.iter().any(|text| text.contains(" │ ")));
    let divider = narrow
        .iter()
        .position(|text| text.starts_with("───"))
        .expect("窄布局的详情区分隔线");
    assert!(
        narrow[divider..].iter().any(|text| text.contains("+--+")),
        "预览应渲染进分隔线下的详情区：{narrow:?}"
    );

    // 聚焦到无预览的选项：右栏空出但双栏布局保持（预览判定是问题级的）。
    let plain: Vec<String> = modal_lines(&app, &ask_modal(1, 0), 100, 24)
        .iter()
        .map(line_text)
        .collect();
    assert!(!plain.iter().any(|text| text.contains("+--+")));
}

/// 提问预览：内容高于窗口时按滚动偏移开窗，末行给出位置与按键，
/// 偏移越界夹到尾部。
#[test]
fn ask_preview_scrolls_and_reports_position() {
    let (events, _receiver) = unbounded_channel();
    let app = App::new(Theme::global(), events);
    let mut modal = ask_modal(0, 0);
    let rows = 18;
    let code = (1..=30)
        .map(|line| format!("L{line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    if let Modal::Ask { params, .. } = &mut modal {
        params.questions[0].options[0].preview = Some(format!("```\n{code}\n```"));
    }

    let texts = |modal: &Modal| -> Vec<String> {
        modal_lines(&app, modal, 60, rows)
            .iter()
            .map(line_text)
            .collect()
    };
    let head = texts(&modal);
    assert!(head.iter().any(|text| text.contains("L01")), "{head:?}");
    let status = head
        .iter()
        .position(|text| text.contains("PgUp/PgDn Scroll"))
        .unwrap_or_else(|| panic!("超窗详情区应给滚动提示：{head:?}"));
    assert_eq!(
        status,
        head.len() - 2,
        "滚动提示应在详情区末行（其后只有提示条）：{head:?}"
    );

    if let Modal::Ask { preview_scroll, .. } = &mut modal {
        *preview_scroll = 8;
    }
    let scrolled = texts(&modal);
    assert!(
        !scrolled.iter().any(|text| text.contains("L01")),
        "滚过首行后不应再显示 L01：{scrolled:?}"
    );
    assert!(
        scrolled.iter().any(|text| text.contains("L09")),
        "{scrolled:?}"
    );

    if let Modal::Ask { preview_scroll, .. } = &mut modal {
        *preview_scroll = usize::MAX;
    }
    let tail = texts(&modal);
    assert!(tail.iter().any(|text| text.contains("L30")), "{tail:?}");
    assert!(
        tail.iter()
            .all(|text| !text.contains("L00") && !text.contains("L31")),
        "{tail:?}"
    );
}

/// 长问题进限高窗口（滚动状态行 + question_area 命中区）；选项超预算时
/// 窗口跟随光标、两端给出 ▲/▼ 隐藏项数提示；命中矩形只记可见选项。
#[test]
fn ask_windows_overflowing_question_and_options() {
    let (events, _receiver) = unbounded_channel();
    let _app = App::new(Theme::global(), events);
    let theme = Theme::global();
    let mut modal = ask_modal(0, 0);
    if let Modal::Ask { params, .. } = &mut modal {
        params.questions[0].question = "很长的问".repeat(30);
        params.questions[0].options = (1..=12)
            .map(|index| AskOption {
                label: format!("选项{index}"),
                description: None,
                preview: None,
            })
            .collect();
    }

    // 首屏：问题窗限高 + 状态行；选项窗口从头开始，尾部 ▼ 提示。
    // 15 行正文：问题窗 6 + 空行 2 + 选项窗口 6 + 自定义行 1。
    let layout = ask_layout(theme, &modal, 40, 15);
    let texts: Vec<String> = layout.lines.iter().map(line_text).collect();
    assert!(
        texts
            .iter()
            .any(|text| text.contains("lines ") && text.contains("PgUp/PgDn Scroll")),
        "超窗问题应给滚动状态行：{texts:?}"
    );
    assert!(layout.question_area.is_some(), "溢出时问题区登记命中");
    assert!(
        texts.iter().any(|text| text.contains("▼ 6 more items")),
        "选项窗口尾部应提示隐藏项数：{texts:?}"
    );
    assert_eq!(
        layout.option_hits.len(),
        7,
        "命中矩形只覆盖可见选项 + 自定义行"
    );

    // 光标到底：窗口跟随，头部 ▲ 提示上方隐藏项数。
    if let Modal::Ask { highlight, .. } = &mut modal {
        highlight[0] = 11;
    }
    let layout = ask_layout(theme, &modal, 40, 15);
    let texts: Vec<String> = layout.lines.iter().map(line_text).collect();
    assert!(
        texts.iter().any(|text| text.contains("▲ 7 more items")),
        "滚到底后头部应提示隐藏项数：{texts:?}"
    );

    // 问题正文窗口按偏移推进（与光标移动互不相干）；偏移越界时钳到末窗。
    if let Modal::Ask {
        highlight,
        question_scroll,
        ..
    } = &mut modal
    {
        highlight[0] = 0;
        *question_scroll = 3;
    }
    let layout = ask_layout(theme, &modal, 40, 15);
    let texts: Vec<String> = layout.lines.iter().map(line_text).collect();
    assert!(
        texts.iter().any(|text| text.contains("⋯ lines 3-6/6")),
        "问题窗应显示钳取后的行区间：{texts:?}"
    );
}

/// 卡片级渲染：带预览的提问在真实终端缓冲里并排出现，卡片高度容得下
/// 选项、预览与底部按键行（不会因换行把提示挤出边框）。
#[test]
fn ask_preview_draws_side_by_side_in_terminal_buffer() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.modal = Some(ask_modal(0, 0));
    let (width, height) = (120u16, 30u16);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");

    let rows: Vec<String> = (0..height)
        .map(|row| {
            (0..width)
                .filter_map(|x| terminal.backend().buffer().cell((x, row)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        })
        .collect();
    // TestBackend 给宽字符的尾部补空格格位：比较时先去掉所有空格。
    let compact: Vec<String> = rows.iter().map(|text| text.replace(' ', "")).collect();
    let option_row = compact
        .iter()
        .find(|text| text.contains("❯○上中下"))
        .unwrap_or_else(|| {
            let shown: Vec<&String> = rows.iter().filter(|text| !text.trim().is_empty()).collect();
            panic!("选项行应出现在卡片内：{shown:#?}")
        });
    assert!(
        option_row.contains("+--+"),
        "聚焦项预览应与选项同行：{option_row:?}"
    );
    assert!(
        compact.iter().any(|text| text.contains("EscCollapsecard")),
        "底部按键行应留在卡片内：{compact:?}"
    );
}

/// 渲染时把选项与自定义行的命中矩形记到 `ask_option_hits`：
/// 下标顺序对应选项（自定义行在末位），行号自上而下递增且落在卡片内。
#[test]
fn ask_render_records_option_hit_areas() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.modal = Some(ask_modal(0, 0));
    let (width, height) = (120u16, 30u16);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");

    let hits = &app.ask_option_hits;
    assert_eq!(hits.len(), 3, "两个选项 + 自定义行：{hits:?}");
    assert_eq!(
        hits.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "下标按选项顺序排列，末位是自定义行"
    );
    assert!(hits[0].1.y < hits[1].1.y && hits[1].1.y < hits[2].1.y);
    for (index, area) in hits {
        assert!(area.y > 0 && area.y < height, "选项 {index} 命中行在终端内");
        assert_eq!(area.height, 1);
    }

    // 命中行确实渲染了对应选项：下标 0 的行里能看到聚焦标记与标签。
    let (_, first) = &hits[0];
    let row: String = (first.x..first.right())
        .filter_map(|x| terminal.backend().buffer().cell((x, first.y)))
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        row.replace(' ', "").contains("上中下"),
        "命中行渲染选项文本：{row:?}"
    );
}

/// 多问导航条：各问 `n. header` 逐项排开，已答带 ✓；
/// 每项记下列范围命中矩形，供鼠标跳问。
#[test]
fn ask_nav_strip_marks_answered_and_records_hits() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let question = |id: &str, header: &str| AskQuestion {
        id: id.into(),
        question: "选哪个？".into(),
        header: Some(header.into()),
        multi: false,
        recommended: Vec::new(),
        options: vec![AskOption {
            label: "甲".into(),
            description: None,
            preview: None,
        }],
    };
    app.modal = Some(Modal::Ask {
        params: AskParams {
            session_id: "ses_1".into(),
            prompt_id: "prompt_1".into(),
            request_id: "ask_1".into(),
            questions: vec![
                question("q1", "布局"),
                question("q2", "配色"),
                question("q3", "交互"),
            ],
        },
        question: 1,
        highlight: vec![0; 3],
        selected: vec![vec!["甲".to_owned()], Vec::new(), Vec::new()],
        custom: vec![crate::modal::CustomAnswer::default(); 3],
        preview_scroll: 0,
        question_scroll: 0,
        parked: false,
        reply: None,
    });
    let (width, height) = (120u16, 30u16);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");

    assert_eq!(app.ask_nav_hits.len(), 4, "三问各一个命中矩形 + 确认项");
    let row = app.ask_nav_hits[0].1.y;
    assert!(app.ask_nav_hits.iter().all(|(_, area)| area.y == row));
    let texts: Vec<String> = app
        .ask_nav_hits
        .iter()
        .map(|(_, area)| {
            (area.x..area.right())
                .filter_map(|x| terminal.backend().buffer().cell((x, area.y)))
                .map(|cell| cell.symbol())
                .collect::<String>()
                .replace(' ', "")
        })
        .collect();
    assert!(texts[0].contains("✓1.布局"), "已答带 ✓：{texts:?}");
    assert!(texts[1].contains("2.配色"), "第二问 header：{texts:?}");
    assert!(texts[2].contains("3.交互"), "第三问 header：{texts:?}");
    assert_eq!(texts[3], "Confirm", "导航条末尾是确认页：{texts:?}");
    assert_eq!(app.ask_nav_hits[3].0, 3, "确认项的下标是 questions.len()");
    assert!(
        app.ask_nav_hits[0].1.x < app.ask_nav_hits[1].1.x
            && app.ask_nav_hits[1].1.x < app.ask_nav_hits[2].1.x,
        "命中矩形按列递增"
    );
}

/// 确认页：越过最后一问强制经过，逐问一行列出全部回答，未回答的醒目标出。
#[test]
fn ask_confirm_page_lists_answers_and_flags_unanswered() {
    let (events, _receiver) = unbounded_channel();
    let app = App::new(Theme::global(), events);
    let question = |id: &str, header: &str| AskQuestion {
        id: id.into(),
        question: "选哪个？".into(),
        header: Some(header.into()),
        multi: false,
        recommended: Vec::new(),
        options: vec![AskOption {
            label: "甲".into(),
            description: None,
            preview: None,
        }],
    };
    let modal = Modal::Ask {
        params: AskParams {
            session_id: "ses_1".into(),
            prompt_id: "prompt_1".into(),
            request_id: "ask_1".into(),
            questions: vec![
                question("theme", "主题"),
                question("features", "features"),
                question("lang", "lang"),
            ],
        },
        question: 3, // 确认页 = questions.len()
        highlight: vec![0; 3],
        selected: vec![
            vec!["纸张白".to_owned()],
            Vec::new(),
            vec!["Rust".to_owned()],
        ],
        custom: vec![crate::modal::CustomAnswer::default(); 3],
        preview_scroll: 0,
        question_scroll: 0,
        parked: false,
        reply: None,
    };
    let body: String = modal_lines(&app, &modal, 100, 24)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("Confirm Answers"), "确认页标题：{body}");
    assert!(body.contains("主题    : 纸张白"), "key 列补齐对齐：{body}");
    assert!(body.contains("features: Unanswered"), "未回答醒目标记：{body}");
    assert!(body.contains("lang    : Rust"), "第三问回答：{body}");
    assert!(body.contains("Enter Submit"), "确认页页脚：{body}");
}

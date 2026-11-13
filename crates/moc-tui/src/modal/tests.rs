use super::{
    CustomAnswer, LevelState, Modal, ModelEntry, PickerState, RewindState, filtered_entries,
    fusion_selector, parse_fusion,
};

use moc_protocol::methods::{AskOption, AskParams, AskQuestion, TranscriptEntry};
use moc_protocol::{Message, Role};

/// Fusion selector 的解析与拼装：规范形式拆两侧，非 Fusion 返回 None。
#[test]
fn fusion_selector_round_trip() {
    let selector = fusion_selector("test/lead:off", "test/side:high");
    assert_eq!(selector, "fusion:test/lead:off+test/side:high");
    assert_eq!(
        parse_fusion(&selector),
        Some(("test/lead:off", "test/side:high"))
    );
    assert_eq!(parse_fusion("test/model:off"), None);
    assert_eq!(parse_fusion("fusion"), None);
    assert_eq!(parse_fusion("fusion:test/lead"), None, "没有 + 不算组合");
}

/// 等级默认用候选里最高的那个；手动值仍在候选内时优先，掉了就回退最高。
#[test]
fn level_state_defaults_to_highest_and_keeps_supported_manual_choice() {
    let levels = |list: &[&str]| {
        list.iter()
            .map(|level| (*level).to_owned())
            .collect::<Vec<_>>()
    };
    let full = levels(&["off", "low", "medium", "high", "xhigh", "max"]);
    let narrow = levels(&["off", "low"]);

    let mut state = LevelState::default();
    assert_eq!(state.resolve(&full).as_deref(), Some("max"));
    assert!(state.resolve(&[]).is_none(), "没有候选就没有等级");

    // 手动选的等级在候选内就沿用：切到同样支持 max 的模型不会被降级。
    state.manual = Some("max".into());
    assert_eq!(state.resolve(&full).as_deref(), Some("max"));
    // 目标模型装不下手动值时回退到它自己的最高等级。
    assert_eq!(state.resolve(&narrow).as_deref(), Some("low"));

    // 尚未调整的模型仍取最高档，不改变默认策略。
    state.manual = None;
    assert_eq!(state.resolve(&narrow).as_deref(), Some("low"));
    // 不认识的名字排在 off 之后，不会成为默认等级。
    assert_eq!(
        super::default_level(&["strange".into(), "high".into()]).as_deref(),
        Some("high")
    );
}

/// 写回的 selector：模型带等级后缀，角色伪值原样。
#[test]
fn model_entry_appends_level_only_for_models() {
    let model = ModelEntry {
        selector: "p/m".into(),
        label: "p/m".into(),
        display_name: "模型".into(),
        levels: vec!["off".into(), "high".into()],
        context_window: Some(1000),
        max_output_tokens: Some(512),
        inputs: vec![moc_protocol::methods::ModelInput::Text],
        pricing: None,
        fast_mode: false,
    };
    assert_eq!(model.selector_with(Some("high")), "p/m:high");
    assert_eq!(model.selector_with(None), "p/m");

    let inherit = ModelEntry {
        selector: "inherit".into(),
        label: "沿用父会话当前模型".into(),
        display_name: String::new(),
        levels: Vec::new(),
        context_window: None,
        max_output_tokens: None,
        inputs: Vec::new(),
        pricing: None,
        fast_mode: false,
    };
    assert_eq!(
        inherit.selector_with(Some("max")),
        "inherit",
        "伪值不接等级"
    );
}

/// 展示名：等于 selector（目录未配名称）时去掉供应商前缀，不同则原样。
#[test]
fn model_entry_name_strips_provider_when_display_name_is_selector() {
    let entry = |display_name: &str| ModelEntry {
        selector: "p/m".into(),
        label: "p/m".into(),
        display_name: display_name.into(),
        levels: Vec::new(),
        context_window: None,
        max_output_tokens: None,
        inputs: Vec::new(),
        pricing: None,
        fast_mode: false,
    };
    assert_eq!(
        entry("p/m").name(),
        "m",
        "display_name 等于 selector 去前缀"
    );
    assert_eq!(entry("").name(), "m", "display_name 为空去前缀");
    assert_eq!(entry("DeepSeek Flash").name(), "DeepSeek Flash");
}

/// 过滤同时匹配选择串与展示名，伪值也能被中文标签搜到。
#[test]
fn filtering_matches_selector_display_name_and_label() {
    let entries = vec![
        ModelEntry {
            selector: "inherit".into(),
            label: "沿用父会话当前模型".into(),
            display_name: String::new(),
            levels: Vec::new(),
            context_window: None,
            max_output_tokens: None,
            inputs: Vec::new(),
            pricing: None,
            fast_mode: false,
        },
        ModelEntry {
            selector: "p/model".into(),
            label: "p/model".into(),
            display_name: "DeepSeek Flash".into(),
            levels: vec!["off".into()],
            context_window: None,
            max_output_tokens: None,
            inputs: Vec::new(),
            pricing: None,
            fast_mode: false,
        },
    ];
    let found = |query: &str| {
        filtered_entries(&entries, query)
            .into_iter()
            .map(|(_, entry)| entry.selector.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(found(""), vec!["inherit", "p/model"], "空查询不过滤");
    assert_eq!(found("model"), vec!["p/model"]);
    assert_eq!(found("deepseek"), vec!["p/model"], "展示名参与匹配");
    assert_eq!(found("父会话"), vec!["inherit"], "伪值按标签匹配");
    assert_eq!(found("p FLSH"), vec!["p/model"], "多词跨供应商与展示名匹配");
    assert!(found("deepseek missing").is_empty(), "每个词都必须匹配");
    let mut ranked = vec![entries[1].clone(); 3];
    ranked[0].display_name = "Distant Space".into();
    ranked[1].display_name = "DS Flash".into();
    ranked[2].display_name = "ds".into();
    assert_eq!(
        filtered_entries(&ranked, "ds")
            .iter()
            .map(|(i, _)| *i)
            .collect::<Vec<_>>(),
        [2, 1, 0]
    );
}

/// 预览判定：只认非空预览，且只作用于提问模态的当前问题。
#[test]
fn option_preview_detection_follows_current_question() {
    let question = |id: &str, previews: [Option<&str>; 2]| AskQuestion {
        id: id.into(),
        question: "选哪个？".into(),
        header: None,
        multi: false,
        recommended: Vec::new(),
        options: previews
            .into_iter()
            .enumerate()
            .map(|(index, preview)| AskOption {
                label: format!("选项{index}"),
                description: None,
                preview: preview.map(str::to_owned),
            })
            .collect(),
    };
    let modal = |questions: Vec<AskQuestion>, question: usize| Modal::Ask {
        params: AskParams {
            session_id: "ses".into(),
            prompt_id: "prompt".into(),
            request_id: "ask".into(),
            questions,
        },
        question,
        highlight: vec![0; 2],
        selected: vec![Vec::new(); 2],
        custom: vec![CustomAnswer::default(); 2],
        preview_scroll: 0,
        question_scroll: 0,
        parked: false,
        reply: None,
    };

    let questions = vec![
        question("first", [None, Some("   ")]),
        question("second", [None, Some("```\n+--+\n```")]),
    ];
    assert!(!modal(questions.clone(), 0).has_option_preview());
    assert!(modal(questions, 1).has_option_preview());
}

#[test]
fn picker_keeps_selected_item_inside_viewport() {
    let mut picker = PickerState {
        selected: 0,
        offset: 0,
        ..Default::default()
    };
    picker.move_selection(9, 20, 8);
    assert_eq!(picker.selected, 9);
    assert_eq!(picker.offset, 2);
    picker.move_selection(-9, 20, 8);
    assert_eq!(picker.selected, 0);
    assert_eq!(picker.offset, 0);
}
#[test]
fn rewind_navigation_moves_within_turns_and_between_users() {
    let entry = |id: &str, message: Message| TranscriptEntry {
        id: id.into(),
        at_ms: 0,
        message,
        tool: None,
        bash: None,
        agent: None,
        goal_round: None,
        reminder: None,
        agent_result: None,
    };
    let mut state = RewindState::default();
    state.set_entries(
        vec![
            entry("u1", Message::user_text("第一条")),
            entry("a1", Message::assistant_text("回复")),
            entry("t1", Message::tool_result("call", "结果", false)),
            entry("u2", Message::user_text("第二条")),
        ],
        vec![],
    );
    assert_eq!(state.prompts.len(), 2);
    assert_eq!(state.prompts[0].len(), 3);
    assert_eq!(state.selected_prompt, 1);
    assert_eq!(state.selected_item, 0);

    state.move_horizontal(-1);
    state.move_vertical(2);
    assert_eq!((state.selected_prompt, state.selected_item), (0, 2));
    state.move_horizontal(1);
    assert_eq!((state.selected_prompt, state.selected_item), (1, 0));
    assert_eq!(state.selected().expect("选中条目").message.role, Role::User);
}

#[test]
fn test_rewind_omp_style_navigation_and_filtering() {
    let entry = |id: &str, message: Message| TranscriptEntry {
        id: id.into(),
        at_ms: 1700000000000,
        message,
        tool: None,
        bash: None,
        agent: None,
        goal_round: None,
        reminder: None,
        agent_result: None,
    };
    let mut state = RewindState::default();
    state.set_entries(
        vec![
            entry("u1", Message::user_text("帮我实现一个快排")),
            entry(
                "a1",
                Message::assistant_text("好的，这是快速排序（快排）实现"),
            ),
            entry("u2", Message::user_text("加几个单元测试")),
            entry("a2", Message::assistant_text("测试已增加并全部通过")),
            entry("u3", Message::user_text("改成双轴快排")),
        ],
        vec![
            entry("u4", Message::user_text("这条已经被回退")),
            entry("a4", Message::assistant_text("被回退的回复")),
        ],
    );

    // 时间线共 7 项：5 条历史 + 2 条已回退；默认光标停在最后一条历史上。
    assert_eq!(state.items.len(), 7);
    assert_eq!(state.future_start, 5);
    assert_eq!(state.selected, 4);
    assert_eq!(state.selected_item_ref().unwrap().preview, "改成双轴快排");
    assert!(!state.selected_item_ref().unwrap().future);
    assert!(state.items[5].future);
    assert!(state.items[6].future);
    // 已回退的用户消息自成一组；其回复挂在同一组。
    assert_eq!(state.items[5].prompt_index, 3);
    assert_eq!(state.items[6].prompt_index, 3);

    // 0. 向下走进已回退区：选中即代表前滚目标；再走回来继续原有导航断言
    state.step(1);
    assert_eq!(state.selected, 5);
    assert!(state.selected_item_ref().unwrap().future);
    state.step(-1);
    assert_eq!(state.selected, 4);

    // 1. Up / Down linear stepping (↑↓ / jk)
    state.step(-1);
    assert_eq!(state.selected, 3);
    assert!(!state.selected_item_ref().unwrap().is_user);

    state.step(-1);
    assert_eq!(state.selected, 2);
    assert!(state.selected_item_ref().unwrap().is_user);

    // 2. Jump between User turns (←→ / hl)
    state.jump_user(-1);
    assert_eq!(state.selected, 0); // Jumps to u1
    assert_eq!(state.selected_item_ref().unwrap().entry_id, "u1");

    state.jump_user(1);
    assert_eq!(state.selected, 2); // Jumps to u2
    assert_eq!(state.selected_item_ref().unwrap().entry_id, "u2");

    state.jump_user(1);
    assert_eq!(state.selected, 4); // Jumps to u3

    // 3. Search and filtering mode
    state.start_filter();
    state.input_char('快');
    state.input_char('排');
    let matches = state.filtered_indices();
    // u1 ("快排"), a1 ("快速排序"), u3 ("双轴快排")
    assert_eq!(matches.len(), 3);
    assert_eq!(matches, vec![0, 1, 4]);

    // Stepping within filtered matches
    state.selected = 4;
    state.step(-1);
    assert_eq!(state.selected, 1);
    state.step(-1);
    assert_eq!(state.selected, 0);

    // Clear filter
    state.clear_filter();
    assert!(!state.filtering);
    assert!(state.query.is_empty());
    assert_eq!(state.filtered_indices().len(), 7);

    // 4. Detail scroll
    assert_eq!(state.detail_scroll, 0);
    state.scroll_detail(3);
    assert_eq!(state.detail_scroll, 3);
    state.scroll_detail(-2);
    assert_eq!(state.detail_scroll, 1);
}

/// 错误只能落在有显示位置的模态上；其余模态返回 `false` 由调用方回落到通知。
#[test]
fn set_error_reports_when_modal_cannot_show_errors() {
    let overflow = moc_protocol::methods::ModelSwitchOverflow {
        tokens: 3_000,
        context_window: 1_000,
        threshold: 850,
        target_tokens: 680,
    };
    let mut card = Modal::SwitchModel {
        session_id: "ses".into(),
        selector: "p/tiny".into(),
        overflow,
        option: 0,
        loading: true,
        error: None,
    };
    assert!(card.set_error("切换模型失败：断开"));
    assert!(matches!(
        &card,
        Modal::SwitchModel {
            loading: false,
            error: Some(error),
            ..
        } if error == "切换模型失败：断开"
    ));

    let mut sessions = Modal::Sessions {
        sessions: Vec::new(),
        cwd: "/tmp".into(),
        scope: super::SessionScope::Folder,
        picker: PickerState {
            loading: true,
            ..Default::default()
        },
    };
    assert!(sessions.set_error("读取失败"));
    assert!(matches!(
        &sessions,
        Modal::Sessions { picker: PickerState { loading: false, error: Some(error), .. }, .. }
            if error == "读取失败"
    ));

    assert!(!Modal::Help { scroll: 0 }.set_error("没有位置显示"));
}

/// 正文窗的滚动、焦点与命中：都按上一帧缓存的几何换算。
#[test]
fn context_content_navigation_follows_cached_geometry() {
    use moc_protocol::methods::{ContextContentResult, ContextContentSection};
    use ratatui::layout::Rect;

    let mut state = super::ContextContentState::default();
    state.set_content(ContextContentResult {
        context_window: 500_000,
        total_tokens: 4_000,
        sections: ["intro", "tool policy", "read"]
            .into_iter()
            .map(|name| ContextContentSection {
                id: name.into(),
                name: name.into(),
                tokens: 10,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    assert_eq!(state.count(), 3);
    // 视口 10 行、全文 30 行：滚动夹在 [0, 20]。
    state.viewport = 10;
    state.layout = Some(super::ContextLayout {
        width: 40,
        spans: vec![0..10, 10..20, 20..30],
        lines: vec![ratatui::text::Line::default(); 30],
    });
    state.scroll_by(100);
    assert_eq!(state.scroll, 20, "滚到底部");
    state.scroll_by(-5);
    assert_eq!(state.scroll, 15);
    state.scroll_to_edge(false);
    assert_eq!(state.scroll, 0);

    // 焦点从首块开始逐块推进，并把目标块滚进视口。
    state.focus_step(1);
    assert_eq!(state.focus, Some(0));
    state.focus_step(1);
    assert_eq!(state.focus, Some(1));
    state.focus_step(10);
    assert_eq!(state.focus, Some(2), "越界夹在末块");
    assert_eq!(state.scroll, 20, "末块整块滚进视口");
    state.focus_step(-10);
    assert_eq!(state.focus, Some(0));
    assert_eq!(state.scroll, 0);

    // 命中：轨道覆盖正文区，行号换算成行表里的行。
    state.scrollbar.track = Rect::new(30, 4, 1, 10);
    state.scroll = 5;
    state.layout = Some(super::ContextLayout {
        width: 40,
        spans: vec![0..6, 6..14, 14..20],
        lines: vec![ratatui::text::Line::default(); 20],
    });
    assert_eq!(state.block_at(4), Some(0), "轨道首行 = 视口首行");
    assert_eq!(state.block_at(5), Some(1));
    assert_eq!(state.block_at(3), None, "轨道上方不是正文");
    assert_eq!(state.block_at(14), None, "轨道下方不是正文");
}

/// 滚动条几何：滑块位置与拖拽换算互为逆运算。
#[test]
fn context_scrollbar_maps_rows_to_offsets() {
    use ratatui::layout::Rect;

    let scrollbar = super::ContextScrollbar {
        track: Rect::new(10, 2, 1, 20),
        thumb: 4,
        max_offset: 100,
    };
    assert_eq!(scrollbar.thumb_top(0), 0, "顶部偏移贴轨道顶端");
    assert_eq!(scrollbar.thumb_top(100), 16, "底部偏移贴轨道末端");
    // 抓取滑块顶端后拖到轨道末端：偏移走满。
    assert_eq!(scrollbar.offset_at(2 + 16, 0), 100);
    assert_eq!(scrollbar.offset_at(2, 0), 0);
    assert_eq!(scrollbar.offset_at(2 + 8, 0), 50);
    // 抓取点偏下时同样的行号落在更小的偏移上。
    assert!(scrollbar.offset_at(2 + 8, 2) < scrollbar.offset_at(2 + 8, 0));
    // 内容不溢出：任何行号都回到顶端。
    let flat = super::ContextScrollbar {
        track: Rect::new(10, 2, 1, 20),
        thumb: 20,
        max_offset: 0,
    };
    assert_eq!(flat.offset_at(20, 3), 0);
    assert_eq!(flat.thumb_top(0), 0);
}

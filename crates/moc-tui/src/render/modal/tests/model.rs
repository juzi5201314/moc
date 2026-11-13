use super::*;

#[test]
fn model_picker_separates_provider_and_context_colors() {
    let (events, _receiver) = unbounded_channel();
    let app = App::new(Theme::global(), events);
    let theme = app.theme;
    let modal = Modal::Model {
        entries: vec![crate::modal::ModelEntry {
            selector: "p/model".into(),
            label: "p/model".into(),
            display_name: "Model".into(),
            levels: vec!["off".into(), "high".into()],
            context_window: Some(500_000),
            max_output_tokens: Some(64_000),
            inputs: vec![
                moc_protocol::methods::ModelInput::Text,
                moc_protocol::methods::ModelInput::Image,
            ],
            pricing: Some(moc_protocol::methods::ModelPricing {
                input: 0.15,
                output: 0.6,
                cache_read: Some(0.015),
                cache_write: None,
            }),
            fast_mode: false,
        }],
        current: "p/model:high".into(),
        picker: PickerState::default(),
        level: crate::modal::LevelState::default(),
        detail: Default::default(),
        target: crate::modal::ModelTarget::Session,
    };

    // 列表卡：选中行、供应商次要色与当前模型 ✓。
    let lines = modal_lines(&app, &modal, 80, 20);
    let row = lines
        .iter()
        .find(|line| line_text(line).starts_with("❯ "))
        .expect("模型行");
    let text = line_text(row);
    assert!(text.starts_with("❯ Model"), "{text}");
    assert!(text.contains('✓'), "{text}");
    let provider = row
        .spans
        .iter()
        .find(|span| span.content.contains("p ✓"))
        .expect("供应商次要列");
    assert_eq!(provider.style.fg, Some(theme.text_secondary));

    // 详情卡：规格行带上下文窗口，思考等级控制行带当前值。
    let (detail, hits) = super::super::model_picker::detail_lines(&app, &modal, 30);
    let texts: Vec<String> = detail.iter().map(line_text).collect();
    assert!(texts.iter().any(|text| text.contains("500k")), "{texts:?}");
    let level_row = hits
        .iter()
        .find(|(row, _)| *row == crate::modal::DetailRow::Level)
        .map(|(_, line)| *line)
        .expect("思考等级控制行");
    assert!(texts[level_row].contains("high"), "{texts:?}");
}

/// 模型选择器在任何高度都要保住页脚与至少一条候选：留白随行预算递减，
/// 页脚永远单行收尾，且每行都不超过正文宽度。
#[test]
fn model_picker_fits_short_terminals() {
    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.modal = Some(Modal::Model {
        entries: vec![
            crate::modal::ModelEntry {
                selector: "p/model".into(),
                label: "p/model".into(),
                display_name: "Model".into(),
                levels: vec!["off".into(), "low".into(), "high".into()],
                context_window: Some(500_000),
                max_output_tokens: Some(64_000),
                inputs: vec![moc_protocol::methods::ModelInput::Text],
                pricing: None,
                fast_mode: false,
            },
            crate::modal::ModelEntry {
                selector: "p/other".into(),
                label: "p/other".into(),
                display_name: "Other".into(),
                levels: vec!["off".into(), "high".into()],
                context_window: Some(128_000),
                max_output_tokens: None,
                inputs: Vec::new(),
                pricing: None,
                fast_mode: false,
            },
        ],
        current: "p/model:high".into(),
        picker: PickerState::default(),
        level: crate::modal::LevelState {
            manual: Some("high".into()),
            ..Default::default()
        },
        detail: Default::default(),
        target: crate::modal::ModelTarget::Session,
    });
    let modal = app.modal.as_ref().expect("模型选择器");

    // 提示条在正文预算之外；包括极短窗口的紧凑行。
    for (width, budget) in [(76usize, 18usize), (56, 8), (36, 6), (24, 4)] {
        let body = modal_body(&app, modal, width, budget);
        let texts: Vec<String> = body.lines.iter().map(line_text).collect();
        assert!(
            body.lines.len() <= budget,
            "{width}/{budget} 超出预算：{texts:?}"
        );
        let footer = line_text(&body.footer.as_ref().expect("缺少提示条").line);
        for text in texts.iter().chain([&footer]) {
            assert!(
                crate::render::wrap::str_width(text) <= width,
                "{text:?} 超过 {width} 列"
            );
        }
        assert!(texts[0].starts_with("搜索："), "搜索置顶：{texts:?}");
        assert!(
            texts.iter().any(|text| text.starts_with("❯ Model")),
            "{texts:?}"
        );
        assert!(
            footer.contains("Enter 确认") && footer.contains("Esc 关闭"),
            "{footer:?}"
        );
    }
}

/// 恢复会话列表：默认只看当前目录且行尾不带目录；分叉来源始终标注，
/// 来源已不在列表时退回短 id。切到全部目录后行尾才显示 cwd。
#[test]
fn session_picker_scopes_to_current_folder_and_marks_fork_provenance() {
    let (events, _receiver) = unbounded_channel();
    let app = App::new(Theme::global(), events);
    let session = |id: &str, title: &str, cwd: &str, forked_from: Option<&str>| SessionInfo {
        id: id.into(),
        title: title.into(),
        model: "p/model".into(),
        permission: SessionPermission::Full,
        engine: SessionEngine::Standard,
        autonomy: moc_protocol::SessionAutonomy::Default,
        plan_active: false,
        fast_mode: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: forked_from.map(str::to_owned),
        agent_preset: None,
        delegation_depth: 0,
        cwd: cwd.into(),
        created_at_ms: 1,
        updated_at_ms: 2,
        lock: None,
    };
    let rows = |scope| -> Vec<String> {
        let modal = Modal::Sessions {
            sessions: vec![
                session("ses_plain0000001", "原会话", "/tmp/project", None),
                session(
                    "ses_fork00000001",
                    "回退：原会话",
                    "/tmp/project",
                    Some("ses_plain0000001"),
                ),
                session(
                    "ses_orphan0000001",
                    "孤儿分叉",
                    "/tmp/other",
                    Some("ses_gone00000001"),
                ),
            ],
            cwd: "/tmp/project".into(),
            scope,
            picker: PickerState::default(),
        };
        modal_lines(&app, &modal, 80, 20)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    };
    // 列表行补满正文宽度（选中色带右缘整齐），比较前去掉尾部补白。
    let row = |rows: &[String], needle: &str| {
        rows.iter()
            .find(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("缺少 {needle} 行：{rows:?}"))
            .trim_end()
            .to_owned()
    };

    let folder = rows(SessionScope::Folder);
    assert_eq!(row(&folder, "回退：原会话"), "  回退：原会话  ← 原会话");
    assert!(
        !folder.iter().any(|row| row.contains("孤儿分叉")),
        "当前目录范围不得出现其它目录的会话：{folder:?}"
    );
    assert_eq!(row(&folder, "原会话"), "❯ 原会话");

    let all = rows(SessionScope::All);
    assert_eq!(
        row(&all, "回退：原会话"),
        "  回退：原会话  ← 原会话  /tmp/project"
    );
    assert_eq!(
        row(&all, "孤儿分叉"),
        "  孤儿分叉  ← ses_gone00000001  /tmp/other"
    );
    assert_eq!(row(&all, "原会话"), "❯ 原会话  /tmp/project");
}

/// 当前目录没有会话时给出切到全部目录的提示，而不是笼统的「没有匹配」。
#[test]
fn session_picker_hints_tab_when_current_folder_is_empty() {
    let (events, _receiver) = unbounded_channel();
    let app = App::new(Theme::global(), events);
    let modal = Modal::Sessions {
        sessions: Vec::new(),
        cwd: "/tmp/project".into(),
        scope: SessionScope::Folder,
        picker: PickerState::default(),
    };
    let rows: Vec<String> = modal_lines(&app, &modal, 80, 20)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    assert_eq!(modal.title(), "恢复会话（当前目录）");
    assert!(
        rows.iter()
            .any(|row| row.contains("当前目录没有会话 · Tab 查看全部")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Tab 切换范围")),
        "页脚应提示 Tab 的范围切换：{rows:?}"
    );
}

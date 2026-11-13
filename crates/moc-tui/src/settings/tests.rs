use super::*;
use crate::settings::editors::ListEditor;
use crate::settings::editors::RecordEditor;

/// 小6：放弃未应用的撤回只把快照放回原栈，重做历史必须留着。
///
/// 走 `History::put` 会按「新改动」语义清空重做栈——撤回没执行，重做却没了。
#[test]
fn restore_keeps_the_other_stack() {
    let snapshot = |label: &str| Snapshot {
        document: Value::Null,
        tui: Value::Null,
        label: label.into(),
    };
    let mut history = History {
        undo: vec![snapshot("改 A")],
        redo: vec![snapshot("被撤回的 B")],
    };

    // 撤回被保存阻塞：快照放回撤回栈，重做栈不动
    let entry = history.undo.pop().expect("撤回栈有快照");
    history.restore(entry, true);
    assert_eq!(history.undo.len(), 1, "快照回到原栈");
    assert_eq!(history.redo.len(), 1, "重做历史不受影响");
    assert_eq!(history.undo[0].label, "改 A");

    // 对照：put 的语义是「发生新改动」，会清空重做栈
    let entry = history.undo.pop().expect("撤回栈有快照");
    history.put(entry, true);
    assert!(history.redo.is_empty(), "put 作为新改动清空重做栈");
}

/// nit7：鼠标点已添加的模型不产生空改动，也不提示「已添加 0 个模型」。
///
/// 旧的鼠标路径直接翻转 `checked`，绕过了「已添加即不可勾选」的判定：勾不出
/// 任何新增，Enter 却照常写入一条空改动。
#[tokio::test]
async fn clicking_an_added_model_keeps_the_selection_empty() {
    use crate::settings::{Hit, View};
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use moc_core::{Core, CoreConfig};
    use moc_protocol::methods::CatalogModel;
    use ratatui::layout::Rect;
    use std::collections::BTreeSet;

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(crate::theme::Theme::global(), events);

    app.settings = Some(SettingsPanel {
        id: 1,
        cwd: "/tmp".into(),
        data: None,
        tui: Value::Null,
        effective: serde_json::json!({
            "providers": { "p": { "models": { "m1": {} } } }
        }),
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
        filter: String::new(),
        edit: None,
        error: None,
        message: None,
        busy: false,
        hits: vec![(Rect::new(10, 5, 20, 1), Hit::DiscoveryItem(0))],
        pending: None,
        view: View::Fields,
        providers: Providers {
            selected: Some("p".into()),
            discovery: Some(providers::Discovery {
                generation: 1,
                rows: vec![CatalogModel {
                    id: "m1".into(),
                    name: "m1".into(),
                }],
                checked: BTreeSet::new(),
                query: String::new(),
                cursor: 0,
                offset: 0,
                scroll: Default::default(),
                error: None,
                pending: None,
            }),
            ..Default::default()
        },
        history: History::default(),
        outcome: None,
    });

    let panel = app.settings.as_mut().expect("设置页");
    panel.tab = providers::TAB;
    panel.data = Some(SettingsDocument {
        path: String::new(),
        revision: String::new(),
        document: panel.effective.clone(),
        effective: panel.effective.clone(),
        inherited: json!({}),
        defaults: json!({}),
        fields: Vec::new(),
        provider_template: json!({}),
        model_template: json!({}),
    });
    mouse(
        &mut app,
        &connection,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
    );

    let panel = app.settings.as_ref().expect("面板仍在");
    assert!(
        panel
            .providers
            .discovery
            .as_ref()
            .expect("页内候选仍在")
            .checked
            .is_empty(),
        "已添加的模型不得进入勾选集合"
    );
    assert!(panel.message.is_none(), "不得产生任何提示");

    connection.shutdown().await;
    core.shutdown().await;
}

#[test]
fn test_matches_filter_multidimensional() {
    let field = ConfigField {
        path: vec!["providers".into(), "openai".into(), "api_key".into()],
        tab: "供应商".into(),
        label: "API 密钥".into(),
        description: "访问供应商的鉴权密钥".into(),
        kind: FieldKind::Secret,
        advanced: false,
    };
    assert!(matches_filter(&field, "api_key"));
    assert!(matches_filter(&field, "密钥"));
    assert!(matches_filter(&field, "鉴权"));
    assert!(matches_filter(&field, "供应商"));
    assert!(matches_filter(&field, "API"));
    assert!(!matches_filter(&field, "notfound"));
}

#[test]
fn test_parse_friendly_inputs() {
    let parse = |text: &str, kind: &FieldKind| forms::parse(text, kind).unwrap();
    assert_eq!(parse("123", &FieldKind::Integer), Some(json!(123)));
    assert_eq!(parse("200k", &FieldKind::Integer), Some(json!(200_000)));
    assert_eq!(parse("200K", &FieldKind::Integer), Some(json!(200_000)));
    assert_eq!(parse("1m", &FieldKind::Integer), Some(json!(1_000_000)));
    assert_eq!(parse("200,000", &FieldKind::Integer), Some(json!(200_000)));
    assert!(forms::parse("invalid", &FieldKind::Integer).is_err());

    assert_eq!(parse("1.5k", &FieldKind::Number), Some(json!(1500.0)));
    assert_eq!(parse("10,000.5", &FieldKind::Number), Some(json!(10000.5)));

    // 留空：数值删除该项回到默认，文本写空串。
    assert_eq!(parse("   ", &FieldKind::Integer), None);
    assert_eq!(parse("", &FieldKind::Text), Some(json!("")));
}

fn model_field(key: &str, kind: FieldKind) -> ConfigField {
    ConfigField {
        path: ["providers", "openai", "models", "o1", key]
            .map(str::to_owned)
            .to_vec(),
        tab: "模型".into(),
        label: key.into(),
        description: String::new(),
        kind,
        advanced: false,
    }
}

fn list_editor(field: &ConfigField, value: Value) -> ListEditor {
    match controls::control(field) {
        Control::List(spec) => ListEditor::new(spec, &value),
        _ => panic!("{} 应是清单控件", field.label),
    }
}

fn press(editor: &mut Editor, keys: &[KeyCode]) -> Outcome {
    let mut outcome = Outcome::Stay;
    for code in keys {
        outcome = editor.key(KeyModifiers::NONE, *code);
    }
    outcome
}

fn typed(text: &str) -> Vec<KeyCode> {
    text.chars().map(KeyCode::Char).collect()
}

/// 思考等级清单：首项即默认等级，可调整顺序、追加自定义等级；全不选回到默认集合。
#[test]
fn thinking_levels_checklist_orders_and_adds_custom_levels() {
    let field = model_field("thinking_levels", FieldKind::Strings);
    let mut editor = Editor::List(list_editor(&field, json!(["high", "low"])));
    // 已选项按配置顺序排前；high 移到 low 之后，再加一个自定义等级。
    editor.key(KeyModifiers::SHIFT, KeyCode::Down);
    press(&mut editor, &[KeyCode::Char('a')]);
    press(&mut editor, &typed("ultra-2"));
    press(&mut editor, &[KeyCode::Enter]);
    let Outcome::Value(value) = press(&mut editor, &[KeyCode::Up, KeyCode::Enter]) else {
        panic!("应用清单应写回值");
    };
    assert_eq!(value, Some(json!(["low", "high", "ultra-2"])));

    // 自定义等级按 core 规则校验：大写与 off 都拒绝。
    for bad in ["Ultra", "off"] {
        let mut editor = Editor::List(list_editor(&field, Value::Null));
        press(&mut editor, &[KeyCode::Char('a')]);
        press(&mut editor, &typed(bad));
        assert!(
            matches!(press(&mut editor, &[KeyCode::Enter]), Outcome::Error(_)),
            "{bad} 不是合法的自定义等级"
        );
    }

    // 未配置时默认全选内置的 5 个等级；全部取消即删除该项，回到默认集合。
    let mut editor = Editor::List(list_editor(&field, Value::Null));
    for _ in 0..5 {
        press(&mut editor, &[KeyCode::Char(' '), KeyCode::Down]);
    }
    assert!(matches!(
        press(&mut editor, &[KeyCode::Up, KeyCode::Enter]),
        Outcome::Value(None)
    ));
}

/// 输入模态至少保留一种。
#[test]
fn input_modalities_reject_an_empty_selection() {
    let field = model_field("input", FieldKind::Strings);
    let mut editor = Editor::List(list_editor(&field, json!(["text"])));
    assert!(matches!(
        press(&mut editor, &[KeyCode::Char(' '), KeyCode::Enter]),
        Outcome::Error(_)
    ));
    let Outcome::Value(value) = press(
        &mut editor,
        &[KeyCode::Down, KeyCode::Char(' '), KeyCode::Enter],
    ) else {
        panic!("勾选图片后应写回值");
    };
    assert_eq!(value, Some(json!(["image"])));
}

/// 价格表：数值解析，填了任何一项时输入与输出价格必填。
#[test]
fn pricing_record_requires_input_and_output() {
    let field = model_field("pricing", FieldKind::Object);
    let Control::Record(spec) = controls::control(&field) else {
        panic!("价格应是键值表");
    };
    let mut editor = Editor::Record(RecordEditor::new(spec, &Value::Null));
    let mut keys = vec![KeyCode::Enter];
    keys.extend(typed("2.5"));
    keys.push(KeyCode::Enter);
    press(&mut editor, &keys);
    let save = |editor: &mut Editor| editor.key(KeyModifiers::CONTROL, KeyCode::Char('s'));
    assert!(matches!(save(&mut editor), Outcome::Error(message) if message.contains("output")));

    let mut keys = vec![KeyCode::Down, KeyCode::Enter];
    keys.extend(typed("10"));
    keys.push(KeyCode::Enter);
    press(&mut editor, &keys);
    let Outcome::Value(value) = save(&mut editor) else {
        panic!("填齐后应写回值");
    };
    assert_eq!(value, Some(json!({"input": 2.5, "output": 10.0})));
}

/// 设置只保留探索角色的快捷模型选择器，Fusion 不在设置页提供入口。
#[test]
fn model_role_fields_carry_their_own_specials() {
    let path = |segments: [&str; 3]| segments.map(str::to_owned).to_vec();
    assert_eq!(model_role(&path(["models", "roles", "plan"])), None);
    assert_eq!(model_role(&path(["models", "roles", "default"])), None);
    assert_eq!(
        model_role(&path(["models", "roles", "explore"])),
        Some("explore")
    );
    assert_eq!(model_role(&path(["models", "fusion", "lead"])), None);
    assert_eq!(model_role(&path(["providers", "test", "models"])), None);
    assert!(role_specials("default").is_empty());
    assert!(role_specials("plan").is_empty());
    assert!(!TABS.contains(&"高级"));
}

#[test]
fn server_plugin_fields_survive_rebuild_with_defaults() {
    let path = vec![
        "plugins".into(),
        "foo".into(),
        "config".into(),
        "api_key".into(),
    ];
    let field = ConfigField {
        path: path.clone(),
        tab: "插件".into(),
        label: "服务密钥".into(),
        description: "插件访问服务所需的密钥".into(),
        kind: FieldKind::Choice(vec![
            moc_protocol::ChoiceOption::new("safe", "安全"),
            moc_protocol::ChoiceOption::new("fast", "快速"),
        ]),
        advanced: false,
    };
    let mut panel = SettingsPanel {
        id: 1,
        cwd: String::new(),
        data: Some(SettingsDocument {
            path: String::new(),
            revision: String::new(),
            document: json!({}),
            effective: json!({
                "plugins": {"foo": {"config": {"api_key": "safe"}}}
            }),
            inherited: json!({
                "plugins": {"foo": {"config": {"api_key": "safe"}}}
            }),
            defaults: Value::Null,
            fields: vec![field.clone()],
            provider_template: Value::Null,
            model_template: Value::Null,
        }),
        tui: Value::Null,
        effective: Value::Null,
        fields: Vec::new(),
        controls: Vec::new(),
        rows: Vec::new(),
        collapsed: BTreeSet::new(),
        expanded: BTreeSet::new(),
        tab: TABS.iter().position(|tab| *tab == "插件").unwrap(),
        selected: 0,
        offset: 0,
        scroll: Default::default(),
        pointer: None,
        filter: String::new(),
        edit: None,
        error: None,
        message: None,
        busy: false,
        hits: Vec::new(),
        pending: None,
        view: View::Fields,
        providers: Providers::default(),
        history: History::default(),
        outcome: None,
    };
    panel.rebuild();

    let actual = panel
        .fields
        .iter()
        .find(|candidate| candidate.path == path)
        .expect("服务端插件字段保留");
    assert_eq!(actual.label, "服务密钥");
    assert_eq!(actual.description, "插件访问服务所需的密钥");
    assert!(matches!(actual.kind, FieldKind::Choice(_)));
    assert_eq!(panel.value(&actual.path), Some(&json!("safe")));
    assert!(panel.rows.iter().any(|row| {
        matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == actual.path)
    }));
}

#[test]
fn rebuilding_does_not_infer_plugin_field_metadata() {
    let path = vec![
        "plugins".into(),
        "foo".into(),
        "config".into(),
        "api_key".into(),
    ];
    let field = ConfigField {
        path: path.clone(),
        tab: "插件".into(),
        label: "自定义标签".into(),
        description: "自定义说明".into(),
        kind: FieldKind::Text,
        advanced: false,
    };
    let mut panel = SettingsPanel {
        id: 1,
        cwd: String::new(),
        data: Some(SettingsDocument {
            path: String::new(),
            revision: String::new(),
            document: json!({}),
            effective: json!({"plugins": {"foo": {"config": {"api_key": "x"}}}}),
            inherited: json!({}),
            defaults: Value::Null,
            fields: vec![field],
            provider_template: Value::Null,
            model_template: Value::Null,
        }),
        tui: Value::Null,
        effective: Value::Null,
        fields: Vec::new(),
        controls: Vec::new(),
        rows: Vec::new(),
        collapsed: BTreeSet::new(),
        expanded: BTreeSet::new(),
        tab: TABS.iter().position(|tab| *tab == "插件").unwrap(),
        selected: 0,
        offset: 0,
        scroll: Default::default(),
        pointer: None,
        filter: String::new(),
        edit: None,
        error: None,
        message: None,
        busy: false,
        hits: Vec::new(),
        pending: None,
        view: View::Fields,
        providers: Providers::default(),
        history: History::default(),
        outcome: None,
    };
    panel.rebuild();

    let actual = panel
        .fields
        .iter()
        .find(|candidate| candidate.path == path)
        .expect("服务端插件字段保留");
    assert_eq!(actual.label, "自定义标签");
    assert_eq!(actual.description, "自定义说明");
}

#[test]
fn list_summaries_show_defaults_and_choices() {
    let field = model_field("thinking_levels", FieldKind::Strings);
    let control = controls::control(&field);
    assert_eq!(
        controls::summary(&control, &Value::Null).as_deref(),
        Some("默认（low、medium、high、xhigh、max）")
    );
    assert_eq!(
        controls::summary(&control, &json!(["low", "ultra-2"])).as_deref(),
        Some("low、ultra-2")
    );
    let field = model_field("input", FieldKind::Strings);
    assert_eq!(
        controls::summary(&controls::control(&field), &json!(["text", "image"])).as_deref(),
        Some("text、image")
    );
}

#[test]
fn advanced_group_collapses_by_default_and_expands_on_filter() {
    let basic = ConfigField {
        path: vec!["limits".into(), "max_turns".into()],
        tab: "会话".into(),
        label: "最大轮数".into(),
        description: "基础".into(),
        kind: FieldKind::Number,
        advanced: false,
    };
    let advanced = ConfigField {
        path: vec!["limits".into(), "max_concurrency".into()],
        tab: "会话".into(),
        label: "最大并发".into(),
        description: "高级".into(),
        kind: FieldKind::Number,
        advanced: true,
    };
    let mut panel = SettingsPanel {
        id: 1,
        cwd: String::new(),
        data: Some(SettingsDocument {
            path: String::new(),
            revision: String::new(),
            document: json!({}),
            effective: json!({}),
            inherited: json!({}),
            defaults: Value::Null,
            fields: vec![basic, advanced],
            provider_template: Value::Null,
            model_template: Value::Null,
        }),
        tui: Value::Null,
        effective: Value::Null,
        fields: Vec::new(),
        controls: Vec::new(),
        rows: Vec::new(),
        collapsed: BTreeSet::new(),
        expanded: BTreeSet::new(),
        tab: TABS.iter().position(|tab| *tab == "会话").unwrap(),
        selected: 0,
        offset: 0,
        scroll: Default::default(),
        pointer: None,
        filter: String::new(),
        edit: None,
        error: None,
        message: None,
        busy: false,
        hits: Vec::new(),
        pending: None,
        view: View::Fields,
        providers: Providers::default(),
        history: History::default(),
        outcome: None,
    };
    panel.rebuild();

    let (adv_index, adv_depth, adv_key, adv_title) = panel
        .rows
        .iter()
        .enumerate()
        .find_map(|(index, row)| match &row.kind {
            RowKind::Group(group) if group.key.ends_with("/__advanced") => {
                Some((index, row.depth, group.key.clone(), group.title.clone()))
            }
            _ => None,
        })
        .expect("存在高级参数折叠行");
    assert_eq!(adv_title, "高级参数");
    assert!(!panel.expanded.contains(&adv_key), "高级参数分组默认折叠");
    assert!(
        !panel.rows.iter().any(|row| matches!(
            &row.kind,
            RowKind::Field(index) if panel.fields[*index].label == "最大并发"
        )),
        "折叠时隐藏高级字段"
    );

    panel.selected = adv_index;
    panel.toggle_group(&adv_key, adv_depth);
    assert!(panel.rows.iter().any(|row| matches!(
        &row.kind,
        RowKind::Field(index) if panel.fields[*index].label == "最大并发"
    )));
    panel.toggle_group(&adv_key, adv_depth);

    panel.filter = "最大并发".into();
    panel.rebuild();
    assert!(panel.rows.iter().any(|row| matches!(
        &row.kind,
        RowKind::Field(index) if panel.fields[*index].label == "最大并发"
    )));
}

use super::*;
use crate::settings::persistence::adopt;
use moc_core::{Core, CoreConfig};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn panel(document: Value) -> SettingsPanel {
    let mut panel = SettingsPanel {
        id: 1,
        scope: ConfigScope::Global,
        cwd: "/tmp".into(),
        data: Some(SettingsDocument {
            path: String::new(),
            revision: String::new(),
            document: document.clone(),
            effective: document,
            inherited: json!({}),
            defaults: json!({}),
            fields: Vec::new(),
            provider_template: json!({}),
            model_template: json!({}),
        }),
        tui: json!({}),
        effective: json!({}),
        fields: Vec::new(),
        controls: Vec::new(),
        rows: Vec::new(),
        collapsed: BTreeSet::new(),
        expanded: BTreeSet::new(),
        tab: TAB,
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
    panel
}

fn discovery(generation: u64, ids: &[&str]) -> Discovery {
    Discovery {
        generation,
        rows: ids
            .iter()
            .map(|id| CatalogModel {
                id: (*id).into(),
                name: (*id).into(),
                ..Default::default()
            })
            .collect(),
        checked: BTreeSet::new(),
        query: String::new(),
        cursor: 0,
        offset: 0,
        scroll: Default::default(),
        error: None,
        status: None,
        pending: None,
    }
}

#[test]
fn provider_and_model_subtabs_separate_connection_compat_and_on_demand_params() {
    let mut panel = panel(json!({"providers":{"p":{"api":"openai_responses","models":{"m":{}}}}}));
    // 默认停在 Models 子页：只展示拉取/手动添加动作和模型行，不混杂连接字段与几十个参数。
    assert_eq!(panel.providers.subtab, ProviderSubTab::Models);
    assert!(
        panel
            .rows
            .iter()
            .any(|row| matches!(row.kind, RowKind::Action(ProviderAction::Fetch)))
    );
    assert!(
        !panel.rows.iter().any(|row| {
            matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "timeout_secs"])
        }),
        "Models 子页不应混入连接字段"
    );

    // 切到 Connection 子页：显示测试连接动作与连接配置，不显示 extra_body 参数。
    panel.set_provider_subtab(ProviderSubTab::Connection);
    assert!(
        panel
            .rows
            .iter()
            .any(|row| matches!(row.kind, RowKind::Action(ProviderAction::Test)))
    );
    assert!(panel.rows.iter().any(|row| {
        matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "timeout_secs"])
    }));
    assert!(
        !panel.rows.iter().any(|row| {
            matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "extra_body", "reasoning", "summary"])
        }),
        "Connection 子页不应平铺 API 参数"
    );

    // 切到 Params 子页：未配置的参数默认隐藏，按需固定（pinned_params）后才展开。
    panel.set_provider_subtab(ProviderSubTab::Params);
    assert!(
        panel
            .rows
            .iter()
            .any(|row| matches!(row.kind, RowKind::Action(ProviderAction::AddParam)))
    );
    assert!(
        !panel.rows.iter().any(|row| {
            matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "extra_body", "reasoning", "summary"])
        }),
        "未配置的请求参数默认按需隐藏"
    );
    panel
        .providers
        .pinned_params
        .insert("providers/p/extra_body/reasoning/summary".into());
    panel.rebuild_rows();
    assert!(panel.rows.iter().any(|row| {
        matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "extra_body", "reasoning", "summary"])
    }));

    // 钻取模型编辑：General 放基础能力，Compat 放兼容与高级限制，Params 按需添加模型请求参数。
    panel.open_model("p", "m");
    assert_eq!(panel.providers.model_subtab, ModelSubTab::General);
    assert!(panel.rows.iter().any(|row| {
        matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "models", "m", "context_window"])
    }));
    assert!(
        !panel.rows.iter().any(|row| {
            matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "models", "m", "max_request_bytes"])
        }),
        "模型请求体上限应归入 Compat 子页"
    );

    panel.set_model_subtab(ModelSubTab::Compat);
    assert!(panel.rows.iter().any(|row| {
        matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "models", "m", "max_request_bytes"])
    }));

    panel.set_model_subtab(ModelSubTab::Params);
    assert!(
        !panel.rows.iter().any(|row| {
            matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "models", "m", "compat", "extra_body", "reasoning", "summary"])
        }),
        "模型未配置参数默认隐藏"
    );
    panel
        .providers
        .pinned_params
        .insert("providers/p/models/m/compat/extra_body/reasoning/summary".into());
    panel.rebuild_rows();
    assert!(panel.rows.iter().any(|row| {
        matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == ["providers", "p", "models", "m", "compat", "extra_body", "reasoning", "summary"])
    }));
}

#[test]
fn api_switch_replaces_stale_provider_fields_before_save_reply() {
    let mut panel = panel(json!({"providers":{
        "p":{"api":"openai_chat","models":{"m":{"compat":{
            "thinking":{"effort_field":"vendor_effort"}
        }}}},
        "q":{"api":"openai_chat","models":{"m":{}}}
    }}));
    let data = panel.data.as_mut().unwrap();
    data.fields = panel.fields.clone();
    data.inherited = json!({"providers":{"p":{"api":"openai_chat"}}});
    let api_path = ["providers", "p", "api"].map(str::to_owned);
    for (api, effort) in [
        (Some("openai_responses"), vec!["reasoning", "effort"]),
        (Some("anthropic_messages"), vec!["output_config", "effort"]),
        (None, vec!["reasoning_effort"]),
    ] {
        if let Some(api) = api {
            panel.set_value(&api_path, json!(api));
        } else {
            panel.remove_value(&api_path);
        }
        panel.filter = "effort".into();
        panel.rebuild();
        for provider in ["p", "q"] {
            let path = [
                "providers",
                provider,
                "models",
                "m",
                "compat",
                "thinking",
                "effort_field",
            ];
            assert_eq!(
                panel.fields.iter().any(|field| field.path == path),
                provider == "q" || api.is_none(),
                "仅更新切换 API 的供应商兼容项"
            );
        }
        for scope in [
            vec!["providers", "p", "extra_body"],
            vec!["providers", "p", "models", "m", "compat", "extra_body"],
        ] {
            let path: Vec<String> = scope
                .into_iter()
                .chain(effort.iter().copied())
                .map(str::to_owned)
                .collect();
            assert!(
                panel.rows.iter().any(|row| {
                    matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == path)
                }),
                "搜索结果应展示当前 API 的思考参数"
            );
        }
        let old_path = ["providers", "p", "extra_body", "reasoning_effort"];
        assert_eq!(
            panel.fields.iter().any(|field| field.path == old_path),
            api.is_none()
        );
    }
    assert_eq!(
        panel.data.as_ref().unwrap().document["providers"]["p"]["models"]["m"]["compat"]["thinking"]
            ["effort_field"],
        "vendor_effort",
        "切换后恢复 API 不得丢失隐藏的显式配置"
    );
}

#[test]
fn empty_provider_keeps_context_across_tabs_and_model_ids_are_literal() {
    let mut panel = panel(
        json!({"models": {"roles": {"default": "a/old"}}, "providers": {"a": {"models": {"old": {}}}, "z": {"models": {}}}}),
    );
    panel.choose_provider(1);
    panel.select_tab(0);
    panel.select_tab(TAB);
    assert_eq!(panel.providers.selected.as_deref(), Some("z"));
    assert!(
        panel
            .rows
            .iter()
            .any(|row| matches!(row.kind, RowKind::Action(ProviderAction::Manual)))
    );
    panel.insert_model("z", "a/new").unwrap();
    panel.rebuild();
    let doc = &panel.data.as_ref().unwrap().document;
    assert!(doc["providers"]["z"]["models"].get("a/new").is_some());
    assert!(doc["providers"]["a"]["models"].get("new").is_none());
    assert_eq!(
        doc["models"]["roles"]["default"], "a/old",
        "添加模型不改变已有角色"
    );
    assert!(panel.insert_model("z", "a/new").is_err());
}

#[test]
fn stale_discovery_does_not_cross_providers_or_replace_a_retry() {
    let mut panel = panel(json!({"providers": {"a": {}, "b": {}}}));
    panel.providers.discovery = Some(discovery(1, &["old"]));
    panel.choose_provider(1);
    panel.models_response(
        1,
        Ok(ConfigReply::Models(vec![CatalogModel {
            id: "late".into(),
            name: "late".into(),
            ..Default::default()
        }])),
    );
    assert!(panel.providers.discovery.is_none());
    panel.providers.discovery = Some(discovery(2, &["current"]));
    panel.models_response(1, Err(anyhow::anyhow!("旧请求失败")));
    let current = panel.providers.discovery.as_ref().unwrap();
    assert_eq!(current.rows[0].id, "current");
    assert!(current.error.is_none());
    panel.models_response(2, Err(anyhow::anyhow!("认证失败")));
    assert_eq!(
        panel.providers.discovery.as_ref().unwrap().error.as_deref(),
        Some("认证失败")
    );
    assert_eq!(panel.providers.selected.as_deref(), Some("b"));
}

#[test]
fn provider_form_validates_complete_connection_without_mutating_config() {
    let panel = panel(json!({"providers": {"taken": {}}}));
    let mut draft = ProviderDraft::default();
    assert!(draft.value(&panel).is_err());
    draft.id.input = PromptState::from_text("taken".into());
    draft.url.input = PromptState::from_text("https://example.com/v1".into());
    assert!(draft.value(&panel).is_err());
    draft.id.input = PromptState::from_text("new/provider".into());
    assert!(draft.value(&panel).is_err());
    draft.id.input = PromptState::from_text("local".into());
    draft.api = 2;
    let (id, value) = draft.value(&panel).unwrap();
    assert_eq!(id, "local");
    assert_eq!(value["api"], "anthropic_messages");
    assert_eq!(value["api_key"], "", "本地服务可留空密钥");
    assert!(panel.effective["providers"].get("local").is_none());
}

async fn response(
    app: &mut App,
    connection: &EmbeddedConnection,
    events: &mut UnboundedReceiver<AppEvent>,
    models: bool,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.expect("事件通道");
            if let AppEvent::SettingsResponse { panel, job, result } = event {
                let matched = if models {
                    matches!(job, Job::Models(_))
                } else {
                    matches!(job, Job::Save)
                };
                assert!(result.is_ok(), "配置请求失败：{result:?}");
                super::super::response(app, connection, panel, job, result);
                if matched {
                    break;
                }
            }
        }
    })
    .await
    .expect("设置请求及时完成");
}

/// 只使用临时配置和本地 mock 服务，覆盖真实 RPC 保存/发现与键鼠操作链。
#[tokio::test]
async fn create_discover_filter_add_and_edit_in_one_page() {
    moc_i18n::set_locale("zh");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "a/fast"}, {"id": "b/reason"}, {"id": "c/other"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let boot = CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    };
    let core = Core::new(boot.clone()).unwrap();
    let (events, mut receiver) = unbounded_channel();
    let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let mut settings = panel(json!({}));
    settings.cwd = root.path().to_string_lossy().into_owned();
    settings.data =
        Some(moc_core::config::editor::inspect(&boot, ConfigScope::Global, root.path()).unwrap());
    settings.rebuild();
    settings.new_provider();
    app.settings = Some(settings);
    let press = |app: &mut App, mods, code| super::super::handle_key(app, &connection, mods, code);
    // 0 是 Preset 行，先 Tab 到 1 (ID)
    press(&mut app, KeyModifiers::NONE, KeyCode::Tab);
    super::super::paste(&mut app, "new-provider");
    press(&mut app, KeyModifiers::NONE, KeyCode::Tab);
    press(&mut app, KeyModifiers::NONE, KeyCode::Tab);
    super::super::paste(&mut app, &format!("{}/v1", server.uri()));
    press(&mut app, KeyModifiers::NONE, KeyCode::Tab);
    super::super::paste(&mut app, "test-secret");
    press(&mut app, KeyModifiers::CONTROL, KeyCode::Char('s'));
    response(&mut app, &connection, &mut receiver, false).await;
    assert!(app.settings.as_ref().unwrap().providers.draft.is_none());
    response(&mut app, &connection, &mut receiver, true).await;
    {
        let panel = app.settings.as_ref().unwrap();
        assert_eq!(panel.providers.selected.as_deref(), Some("new-provider"));
        let discovery = panel.providers.discovery.as_ref().unwrap();
        assert!(discovery.error.is_none(), "{:?}", discovery.error);
        assert_eq!(discovery.rows.len(), 3);
        assert_eq!(panel.tab, TAB);
    }
    // 弹窗打开后直接键入过滤，多个搜索间保留按 ID 勾选的状态。
    for c in "fast".chars() {
        press(&mut app, KeyModifiers::NONE, KeyCode::Char(c));
    }
    assert_eq!(app.settings.as_ref().unwrap().visible_candidates(), vec![0]);
    press(&mut app, KeyModifiers::CONTROL, KeyCode::Char('a'));
    let mut panel = app.settings.take().unwrap();
    let d = panel.providers.discovery.as_mut().unwrap();
    d.query = "reason".into();
    d.cursor = 0;
    panel.toggle_select_all_discovery();
    panel.confirm_discovery(&mut app, &connection);
    assert_eq!(panel.tab, TAB);
    assert_eq!(panel.providers.selected.as_deref(), Some("new-provider"));
    let doc = &panel.data.as_ref().unwrap().document;
    assert!(
        doc["providers"]["new-provider"]["models"]
            .get("a/fast")
            .is_some()
    );
    assert!(
        doc["providers"]["new-provider"]["models"]
            .get("b/reason")
            .is_some()
    );
    assert!(
        doc["providers"]["new-provider"]["models"]
            .get("c/other")
            .is_none()
    );
    assert!(
        doc.pointer("/models/roles/default").is_none(),
        "导入候选不替用户选择模型"
    );
    // 批量添加后保持发现弹窗打开并显示成功反馈，允许继续筛选添加，Esc 关闭。
    let d = panel.providers.discovery.as_ref().expect("添加后弹窗保持打开");
    assert!(d.checked.is_empty());
    assert!(d.status.is_some());
    app.settings = Some(panel);
    response(&mut app, &connection, &mut receiver, false).await;
    press(&mut app, KeyModifiers::NONE, KeyCode::Esc);
    assert!(app.settings.as_ref().unwrap().providers.discovery.is_none());
    // 重复添加保留输入及错误；Esc 后可以继续添加，不跳去角色页。
    press(&mut app, KeyModifiers::NONE, KeyCode::Char('a'));
    super::super::paste(&mut app, "a/fast");
    press(&mut app, KeyModifiers::NONE, KeyCode::Enter);
    assert!(
        app.settings
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("已存在")
    );
    assert!(app.settings.as_ref().unwrap().edit.is_some());
    press(&mut app, KeyModifiers::NONE, KeyCode::Esc);
    press(&mut app, KeyModifiers::NONE, KeyCode::Char('a'));
    super::super::paste(&mut app, "manual/model");
    press(&mut app, KeyModifiers::NONE, KeyCode::Enter);
    response(&mut app, &connection, &mut receiver, false).await;
    let panel = app.settings.as_mut().unwrap();
    assert_eq!(
        panel.providers.editing_model.as_deref(),
        Some("manual/model"),
        "手动添加模型后直接钻取至模型编辑视图"
    );
    let path: Vec<String> = [
        "providers",
        "new-provider",
        "models",
        "manual/model",
        "context_window",
    ]
    .map(str::to_owned)
    .to_vec();
    panel.select_path(&path);
    assert_eq!(
        panel.field().map(|field| &field.path),
        Some(&path),
        "新模型参数必须在当前页可选"
    );
    press(&mut app, KeyModifiers::NONE, KeyCode::Enter);
    assert!(
        app.settings.as_ref().unwrap().edit.is_some(),
        "参数输入已打开"
    );
    press(&mut app, KeyModifiers::CONTROL, KeyCode::Char('u'));
    super::super::paste(&mut app, "128k");
    press(&mut app, KeyModifiers::NONE, KeyCode::Enter);
    response(&mut app, &connection, &mut receiver, false).await;
    let saved = moc_core::config::editor::inspect(&boot, ConfigScope::Global, root.path()).unwrap();
    assert_eq!(get(&saved.document, &path), Some(&json!(128000)));
    assert_eq!(app.settings.as_ref().unwrap().tab, TAB);
    super::super::response(
        &mut app,
        &connection,
        999,
        Job::Models(1),
        Ok(ConfigReply::Models(Vec::new())),
    );
    assert!(app.settings.is_some(), "旧面板回执不得关闭当前面板");
    let panel = app.settings.as_mut().unwrap();
    panel.new_provider();
    let snapshot = panel.snapshot("测试保存失败");
    let draft = panel.providers.draft.as_mut().unwrap();
    draft.id.input = PromptState::from_text("retry-provider".into());
    draft.pending = Some(snapshot);
    super::super::finish_save(&mut app, &connection, Err(anyhow::anyhow!("写入失败")));
    let panel = app.settings.as_ref().unwrap();
    let draft = panel.providers.draft.as_ref().unwrap();
    assert_eq!(draft.id.input.text, "retry-provider");
    assert!(draft.pending.is_none(), "失败后允许重试");
    assert_eq!(panel.error.as_deref(), Some("写入失败"));
    let panel = app.settings.as_mut().unwrap();
    panel.providers.draft = None;
    let pending_path = ["providers".into(), "new-provider".into(), "api_key".into()];
    panel.set_value(&pending_path, json!("newer-key"));
    adopt(&mut app, &connection, saved, true);
    assert_eq!(
        app.settings.as_ref().unwrap().value(&pending_path),
        Some(&json!("newer-key")),
        "旧保存回执不覆盖新输入"
    );
    connection.shutdown().await;
    core.shutdown().await;
}

#[test]
fn draft_preset_autofills_and_sidebar_preview_and_image_summary_work() {
    let mut panel = panel(json!({
        "providers": {
            "a": {
                "api": "openai_responses",
                "models": {
                    "img": { "image_gen": true },
                    "off-m": { "enabled": false }
                }
            },
            "b": { "api": "anthropic_messages", "models": {} }
        }
    }));
    // 侧栏实时预览：移动光标即切换右侧详情，且保留侧栏焦点。
    panel.providers.sidebar = true;
    panel.preview_provider(1);
    assert_eq!(panel.providers.selected.as_deref(), Some("b"));
    assert!(panel.providers.sidebar);

    // image_gen 模型摘要不误显上下文/输出上限，停用模型带 Disabled 标记。
    assert_eq!(panel.model_summary("a", "img"), "Image Gen");
    assert!(panel.model_summary("a", "off-m").contains("Disabled"));

    // 新建表单预设自动填充 ID / API / URL / Key 提示。
    let mut draft = ProviderDraft::default();
    draft.apply_preset(1, &panel.provider_ids()); // OpenAI
    assert_eq!(draft.id.input.text, "openai");
    assert_eq!(APIS[draft.api].0, "openai_responses");
    assert_eq!(draft.url.input.text, "https://api.openai.com/v1");
    assert_eq!(draft.key.input.text, "env:OPENAI_API_KEY");
    // 地址缺少协议头时预览校验报错
    draft.url.input = PromptState::from_text("api.openai.com/v1".into());
    assert!(draft.preview_payload().is_err());
}

#[test]
fn discovery_popup_cursor_aligns_with_locale_search_prefix_width() {
    use ratatui::{Terminal, backend::TestBackend};

    let render_cursor = |locale: &str| {
        moc_i18n::set_locale(locale);
        let mut p = panel(json!({"providers": {"p": {"api": "openai_chat"}}}));
        p.providers.discovery = Some(discovery(1, &["gpt-5"]));
        let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let theme = crate::theme::Theme::global();
        term.draw(|frame| {
            super::super::render::draw(frame, &mut p, false, &theme);
        })
        .unwrap();
        let pos = term.get_cursor_position().unwrap();
        let prefix = t!("tui.providers.search_prefix").into_owned();
        (pos, crate::render::wrap::str_width(&prefix))
    };

    let (zh_pos, zh_w) = render_cursor("zh");
    let (en_pos, en_w) = render_cursor("en");
    assert_eq!(en_pos.x.saturating_sub(zh_pos.x) as usize, en_w.saturating_sub(zh_w));
    moc_i18n::set_locale("zh");
}

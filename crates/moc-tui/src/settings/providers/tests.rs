use super::*;
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
        cwd: "/tmp".into(),
        scope: ConfigScope::Global,
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
        advanced: false,
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
        restore: None,
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
            })
            .collect(),
        checked: BTreeSet::new(),
        query: String::new(),
        error: None,
        pending: None,
    }
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
    // 搜索实时过滤，多个搜索间保留按 ID 勾选的状态。
    press(&mut app, KeyModifiers::NONE, KeyCode::Char('/'));
    for c in "fast".chars() {
        press(&mut app, KeyModifiers::NONE, KeyCode::Char(c));
    }
    assert_eq!(app.settings.as_ref().unwrap().visible_candidates(), vec![0]);
    press(&mut app, KeyModifiers::NONE, KeyCode::Enter);
    let mut panel = app.settings.take().unwrap();
    panel.provider_action(ProviderAction::SelectAll, &mut app, &connection);
    panel.providers.discovery.as_mut().unwrap().query = "reason".into();
    panel.provider_action(ProviderAction::SelectAll, &mut app, &connection);
    panel.provider_action(ProviderAction::AddPicked, &mut app, &connection);
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
    assert!(
        panel
            .expanded
            .contains("providers/new-provider/models/a/fast")
    );
    app.settings = Some(panel);
    response(&mut app, &connection, &mut receiver, false).await;
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
    super::super::adopt(&mut app, &connection, saved, true);
    assert_eq!(
        app.settings.as_ref().unwrap().value(&pending_path),
        Some(&json!("newer-key")),
        "旧保存回执不覆盖新输入"
    );
    connection.shutdown().await;
    core.shutdown().await;
}

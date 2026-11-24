use super::*;
use crate::event::modal_key::handle_modal_key;
use crate::event::usage::usage_table;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use moc_protocol::ConfigGet;
use moc_protocol::ConfigGetParams;
use moc_protocol::SessionNew;
use moc_protocol::SessionNewParams;
use moc_protocol::SessionUpdate;

#[tokio::test]
async fn startup_selects_then_remembers_model_and_resume_ignores_missing_preference() {
    moc_i18n::set_locale("zh");
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"id\":\"reply\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"已记录\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        format!("language='zh'\n[providers.p]\napi='openai_chat'\nbase_url='{}/v1'\n[providers.p.models.m]\n[plugins]\nenabled=false\n", upstream.uri()),
    )
    .unwrap();
    let core = Core::new(moc_core::CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        session_dir: Some(root.path().join("sessions")),
        ..Default::default()
    })
    .unwrap();
    let (events, mut receiver) = unbounded_channel();
    let connection = connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events.clone());
    app.startup_plan = true;
    bootstrap(
        &connection.client,
        &mut app,
        &TuiOptions {
            plan: true,
            ..Default::default()
        },
    )
    .await;
    assert!(app.session.is_none());
    let Some(Modal::Model {
        entries, picker, ..
    }) = app.modal.as_mut()
    else {
        panic!("没有记忆时应打开模型选择器");
    };
    picker.selected = entries
        .iter()
        .position(|entry| entry.selector.starts_with("p/m"))
        .unwrap();
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let info = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        if let AppEvent::Loaded(info, ..) = event {
            break info;
        }
    };
    assert_eq!(info.model, "p/m:off");
    assert!(info.plan_active);
    // 空会话刻意不落盘；先完成一轮真实 RPC 提示，再验证卸载后的冷恢复。
    connection
        .client
        .call::<moc_protocol::SessionPrompt>(moc_protocol::PromptParams {
            session_id: info.id.clone(),
            input: vec![moc_protocol::InputPart::Text {
                text: "恢复前的历史".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        })
        .await
        .unwrap();
    let mut next = App::new(Theme::global(), events.clone());
    bootstrap(&connection.client, &mut next, &TuiOptions::default()).await;
    assert_eq!(next.session.as_ref().unwrap().model, info.model);
    moc_core::config::selection::save(root.path(), "p/deleted:off", 0, str::to_owned).unwrap();
    connection
        .client
        .call::<moc_protocol::SessionUnload>(moc_protocol::SessionUnloadParams {
            session_id: info.id.clone(),
        })
        .await
        .unwrap();
    let mut restored = App::new(Theme::global(), events);
    bootstrap(
        &connection.client,
        &mut restored,
        &TuiOptions {
            resume: Some(Some(info.id.clone())),
            ..Default::default()
        },
    )
    .await;
    let session = restored
        .session
        .as_ref()
        .unwrap_or_else(|| panic!("恢复会话失败：{:?}", restored.notice));
    assert_eq!(session.id, info.id);
    assert_eq!(session.model, "p/m:off");
    connection.shutdown().await;
    core.shutdown().await;
}

/// `/config` 的模型角色字段与 `Ctrl+P` 共用选择器：候选取自 core（带思考等级），
/// 选中后写回该字段的配置值，不切换会话模型。

#[tokio::test]
async fn config_model_role_picker_shares_the_searchable_picker() {
    moc_i18n::set_locale("zh");
    use crate::settings::{History, RowKind, SettingsPanel, View};
    use moc_core::{Core, CoreConfig};
    use moc_protocol::methods::SettingsDocument;
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "language='zh'\n[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, _receiver) = unbounded_channel();
    let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events);
    app.models = connection
        .client
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .unwrap()
        .models;

    // 面板只放探索模型字段：行由文档重建，选中该行后按 Enter。
    let role = vec![
        "models".to_owned(),
        "roles".to_owned(),
        "explore".to_owned(),
    ];
    app.settings = Some(SettingsPanel {
        id: 1,
        cwd: "/tmp".into(),
        data: Some(SettingsDocument {
            path: "config.toml".into(),
            revision: "0".into(),
            document: json!({}),
            effective: json!({}),
            inherited: json!({
                "models": {"roles": {"explore": "test/big:low"}},
                "providers": {"test": {"models": {"big": {}, "small": {}}}},
            }),
            defaults: json!({}),
            fields: Vec::new(),
            provider_template: json!({}),
            model_template: json!({}),
        }),
        tui: Value::Null,
        effective: Value::Null,
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
        hits: Vec::new(),
        pending: None,
        view: View::Fields,
        providers: Default::default(),
        history: History::default(),
        outcome: None,
    });
    {
        let panel = app.settings.as_mut().expect("设置面板");
        panel.rebuild();
        let index = panel
            .rows
            .iter()
            .position(|row| {
                matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == role)
            })
            .expect("探索模型行");
        panel.selected = index;
    }

    crate::settings::handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert!(
        matches!(
            &app.modal,
            Some(Modal::Model {
                target: crate::modal::ModelTarget::Config { path, .. },
                ..
            }) if *path == role
        ),
        "探索模型字段按 Enter 打开共用选择器"
    );
    let choice = |app: &App| {
        app.modal
            .as_ref()
            .and_then(Modal::model_choice)
            .expect("选择器有高亮条目")
            .1
    };
    assert_eq!(choice(&app), "test/big:low", "沿用配置里写明的等级");

    // 选择器就是搜索框：j／h／l 这类字符必须进查询串，而不是被当成方向键。
    let query = |app: &App| match app.modal.as_ref() {
        Some(Modal::Model { picker, .. }) => picker.query.clone(),
        _ => panic!("选择器应当打开"),
    };
    for key in ['g', 'j', 'h', 'l'] {
        handle_modal_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char(key),
        );
    }
    assert_eq!(query(&app), "gjhl", "字符键全部进搜索框");
    handle_modal_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('u'),
    );
    assert_eq!(query(&app), "", "Ctrl+U 清空查询");

    // 按模型名筛选，不依赖继承/禁用选项在列表中的数量。
    for key in "small".chars() {
        handle_modal_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char(key),
        );
    }
    assert_eq!(choice(&app), "test/small:medium");

    // 切回 big 恢复原 low；Tab 展开等级下拉，选中 max 后确认。
    handle_modal_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('u'),
    );
    for key in "big".chars() {
        handle_modal_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char(key),
        );
    }
    assert_eq!(choice(&app), "test/big:low");
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert_eq!(choice(&app), "test/big:max", "下拉里选 max 生效");
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);

    assert!(app.modal.is_none(), "确认后关闭选择器");
    let panel = app.settings.as_ref().expect("设置面板");
    let document = &panel.data.as_ref().expect("设置文档").document;
    assert_eq!(
        document
            .pointer("/models/roles/explore")
            .and_then(Value::as_str),
        Some("test/big:max"),
        "选中的 selector 写回配置"
    );
    connection.shutdown().await;
    core.shutdown().await;
}

/// 会话模型选择器的 Fusion 组合复用两侧槽位选择器。
#[tokio::test]
async fn session_fusion_pair_flows_through_slot_pickers() {
    moc_i18n::set_locale("zh");
    use moc_core::{Core, CoreConfig};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "language='zh'\n[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, _receiver) = unbounded_channel();
    let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events);
    app.models = connection
        .client
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .unwrap()
        .models;

    app.open_model_picker_for(
        "test/big:low".into(),
        &[],
        crate::modal::ModelTarget::Session,
    );

    // Fusion 特殊项排在候选首位；→ 进入详情卡，焦点落在 Lead 行。
    let Some(Modal::Model {
        entries, picker, ..
    }) = app.modal.as_mut()
    else {
        panic!("模型选择器应打开");
    };
    assert_eq!(entries[0].selector, crate::modal::FUSION_ENTRY_SELECTOR);
    picker.selected = 0;
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
    let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert_eq!(detail.focus, Some(crate::modal::DetailRow::Lead));
    assert_eq!(
        detail.fusion.lead, "test/big:low",
        "无默认组合时沿用当前执行模型"
    );
    assert!(detail.fusion.sidekick.is_empty());

    // 移到 Sidekick 行，Enter 展开模型下拉；输入 "small" 后 Enter 回填。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    for ch in "small".chars() {
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
    }
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert_eq!(detail.fusion.sidekick, "test/small:medium");

    // 回到 LeadLevel 行，下拉里把等级从 low 改成 max。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Up);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert_eq!(detail.fusion.lead, "test/big:max");
    assert_eq!(
        crate::modal::fusion_selector(&detail.fusion.lead, &detail.fusion.sidekick),
        "fusion:test/big:max+test/small:medium"
    );

    // Esc 回列表焦点，再 Esc 关闭。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert_eq!(detail.focus, None);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert!(app.modal.is_none());
    connection.shutdown().await;
    core.shutdown().await;
}

/// 会话模型同样可应用 Fusion 组合：应用回执落地后配置卡关闭，
/// 会话模型经 SessionUpdate 推送变成规范组合串。
#[tokio::test]
async fn session_fusion_pair_applies_and_closes() {
    moc_i18n::set_locale("zh");
    use moc_core::{Core, CoreConfig};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "language='zh'\n[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, mut receiver) = unbounded_channel();
    let mut connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events);
    app.models = connection
        .client
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .unwrap()
        .models;
    let session = connection
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    app.session = Some(session);

    // 选择器首位是 Fusion 特殊项 → → 进入详情卡（Lead 沿用当前模型）。
    app.open_model_picker();
    let Some(Modal::Model { picker, .. }) = app.modal.as_mut() else {
        panic!("模型选择器应打开");
    };
    picker.selected = 0;
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
    let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert_eq!(detail.fusion.lead, "test/big:low");

    // Sidekick 行：下拉里选第二个模型。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    for ch in "small".chars() {
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
    }
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert_eq!(detail.fusion.sidekick, "test/small:medium");

    // 移到 Apply 行应用：发起 session/set_model，收尾回执落地后选择器关闭。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(event @ AppEvent::SessionControlSettled { .. }) = receiver.recv().await {
                break event;
            }
        }
    })
    .await
    .expect("应收到切换收尾回执");
    crate::interaction::handle_event(&mut app, &connection, event);
    assert!(app.modal.is_none(), "收尾后模型选择器应关闭");

    // 会话模型经 SessionUpdate 推送更新为规范组合串。
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let update = connection.updates.recv().await.unwrap();
            let model_changed = matches!(
                &update,
                crate::protocol::UiUpdate::Session(params)
                    if matches!(&params.update, SessionUpdate::ModelChanged { .. })
            );
            apply_ui_update(&mut app, &connection, update);
            if model_changed {
                break;
            }
        }
    })
    .await
    .expect("应收到模型变更推送");
    assert_eq!(
        app.session.as_ref().map(|session| session.model.as_str()),
        Some("fusion:test/big:low+test/small:medium")
    );
    connection.shutdown().await;
    core.shutdown().await;
}

/// 会话目标下在等级下拉里选档：立即发起切换，回执后选择器保持打开、
/// `current` 写回新 selector；换到别的模型同样生效。
#[tokio::test]
async fn session_level_pick_applies_inline_and_stays_open() {
    moc_i18n::set_locale("zh");
    use moc_core::{Core, CoreConfig};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "language='zh'\n[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, mut receiver) = unbounded_channel();
    let mut connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events);
    app.models = connection
        .client
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .unwrap()
        .models;
    let session = connection
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    app.session = Some(session);

    // 收掉一次切换的收尾回执与 ModelChanged 推送，返回会话模型。
    async fn settle(
        app: &mut App,
        connection: &mut crate::protocol::EmbeddedConnection,
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) -> Option<String> {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(event @ AppEvent::SessionControlSettled { .. }) = receiver.recv().await
                {
                    break event;
                }
            }
        })
        .await
        .expect("应收到切换收尾回执");
        crate::interaction::handle_event(app, connection, event);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let update = connection.updates.recv().await.unwrap();
                let changed = matches!(
                    &update,
                    crate::protocol::UiUpdate::Session(params)
                        if matches!(&params.update, SessionUpdate::ModelChanged { .. })
                );
                apply_ui_update(app, connection, update);
                if changed {
                    break;
                }
            }
        })
        .await
        .expect("应收到模型变更推送");
        app.session.as_ref().map(|session| session.model.clone())
    }

    // a) 高亮 big（选择器打开时默认落第一行真实模型），Tab 开等级下拉，
    //    移到 max 提交：立即切换，选择器不关。
    app.open_model_picker();
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let model = settle(&mut app, &mut connection, &mut receiver).await;
    assert_eq!(model.as_deref(), Some("test/big:max"));
    let Some(Modal::Model {
        current, picker, ..
    }) = app.modal.as_ref()
    else {
        panic!("内联切换后选择器应保持打开");
    };
    assert_eq!(current, "test/big:max");
    assert!(!picker.loading);

    // b) 高亮移到 small，Tab 后唯一档位 medium 直接提交：同样立即生效。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let model = settle(&mut app, &mut connection, &mut receiver).await;
    assert_eq!(model.as_deref(), Some("test/small:medium"));
    let Some(Modal::Model {
        current, picker, ..
    }) = app.modal.as_ref()
    else {
        panic!("第二次内联切换后选择器仍应保持打开");
    };
    assert_eq!(current, "test/small:medium");
    assert!(!picker.loading);

    connection.shutdown().await;
    core.shutdown().await;
}

/// Fast Mode 行只出现在支持该能力的模型详情卡里，且只限会话目标；
/// Enter 发起 `session/set_fast_mode`，推送落地后写回会话状态。
#[tokio::test]
async fn fast_mode_row_only_for_fast_models_and_session_target() {
    moc_i18n::set_locale("zh");
    use moc_core::{Core, CoreConfig};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "language='zh'\n[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\nfast_mode=true\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, _receiver) = unbounded_channel();
    let mut connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events);
    app.models = connection
        .client
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .unwrap()
        .models;
    let session = connection
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    app.session = Some(session);

    // Session 目标：fast 模型的详情行含 Fast；不支持 fast 的模型不含。
    app.open_model_picker();
    let Some(Modal::Model {
        entries,
        picker,
        detail,
        target,
        ..
    }) = app.modal.as_ref()
    else {
        panic!("模型选择器应打开");
    };
    let rows_of = |index: usize| {
        crate::modal::detail_rows(entries.get(index), entries, target, &detail.fusion)
    };
    assert!(
        rows_of(picker.selected).contains(&crate::modal::DetailRow::Fast),
        "fast 模型的会话目标详情行含 Fast"
    );
    let small = entries
        .iter()
        .position(|entry| entry.selector == "test/small")
        .expect("small 条目");
    assert!(
        !rows_of(small).contains(&crate::modal::DetailRow::Fast),
        "非 fast 模型不显示 Fast 行"
    );

    // Config 目标下同一模型也不显示 Fast 行。
    app.open_config_model_picker(
        vec!["models".into(), "roles".into(), "explore".into()],
        "探索模型".into(),
        "test/big:low".into(),
        &[],
    );
    let Some(Modal::Model {
        entries,
        picker,
        detail,
        target,
        ..
    }) = app.modal.as_ref()
    else {
        panic!("配置选择器应打开");
    };
    assert!(
        !crate::modal::detail_rows(
            entries.get(picker.selected),
            entries,
            target,
            &detail.fusion,
        )
        .contains(&crate::modal::DetailRow::Fast),
        "配置目标不显示 Fast 行"
    );

    // 回到会话选择器：聚焦 Fast 行按 Enter 发起切换。
    app.open_model_picker();
    let Some(Modal::Model { detail, .. }) = app.modal.as_mut() else {
        panic!("模型选择器应打开");
    };
    detail.focus = Some(crate::modal::DetailRow::Fast);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let update = connection.updates.recv().await.unwrap();
            let fast_changed = matches!(
                &update,
                crate::protocol::UiUpdate::Session(params)
                    if matches!(&params.update, SessionUpdate::FastModeChanged { enabled: true })
            );
            apply_ui_update(&mut app, &connection, update);
            if fast_changed {
                break;
            }
        }
    })
    .await
    .expect("应收到 Fast Mode 推送");
    assert!(
        app.session
            .as_ref()
            .is_some_and(|session| session.fast_mode),
        "推送落地后会话 fast_mode 变 true"
    );
    connection.shutdown().await;
    core.shutdown().await;
}
/// 后代会话的用量行在 /usage 表上标出来源。
#[test]
fn usage_table_marks_descendant_sources() {
    moc_i18n::set_locale("zh");
    use moc_protocol::methods::UsageSource;
    use moc_protocol::{ModelUsageSummary, SessionUsageResult};

    let summary = |model: &str, source| ModelUsageSummary {
        model: model.into(),
        source,
        calls: 1,
        input_tokens: 100,
        output_tokens: 10,
        cached_input_tokens: None,
        cache_creation_input_tokens: None,
        reasoning_tokens: None,
        active_ms: 10,
        ttft_ms: 0,
        ttft_calls: 0,
        gen_ms: 0,
    };
    let lines = usage_table(
        &SessionUsageResult {
            models: vec![
                summary("p/lead:off", UsageSource::Session),
                summary("p/cheap:off", UsageSource::Sidekick),
                summary("p/lead:off", UsageSource::Subagent),
            ],
            total: summary("total", UsageSource::Session),
            tool_ms: 0,
        },
        100,
    );
    let text = lines.join("\n");
    assert!(text.contains("p/lead (subagent)"), "{text}");
    assert!(text.contains("p/cheap (sidekick)"), "{text}");
}

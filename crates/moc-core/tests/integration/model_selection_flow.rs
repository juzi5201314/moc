//! 跨 core 实例模拟 Web/TUI 进程，验证模型记忆与会话、模式相互独立。

use moc_core::{Core, CoreConfig};
use moc_protocol::{methods::*, *};
use std::sync::Arc;

async fn connect(core: &Arc<Core>, frontend: &str) -> (ClientHandle, ServerConnection) {
    let (server_side, client_side) = transport::duplex(65536);
    let server = Server::new((**core).clone());
    let connection = server.spawn(server_side);
    let (client, _) = ClientBuilder::new(ClientInfo {
        name: frontend.into(),
        version: "test".into(),
        frontend: frontend.into(),
    })
    .connect(client_side)
    .await
    .unwrap();
    (client, connection)
}

async fn create(client: &ClientHandle, model: Option<&str>, plan_active: bool) -> SessionInfo {
    client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: model.map(str::to_owned),
            title: None,
            engine: SessionEngine::Standard,
            plan_active,
            fast_mode: false,
        })
        .await
        .unwrap()
}

async fn remembered(client: &ClientHandle) -> Option<String> {
    client
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .unwrap()
        .default_model
}

#[tokio::test]
async fn model_selection_is_shared_without_mode_resume_or_print_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "\
[providers.p]\napi='openai_chat'\nbase_url='http://127.0.0.1:1'\n\
[providers.p.models.a]\nreasoning=true\nthinking_levels=['low','high']\n\
[providers.p.models.b]\n\
[workspace]\nwatch=false\n[compaction]\nenabled=false\n\
[plugins]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let boot = || CoreConfig {
        home: home.clone(),
        cwd: cwd.clone(),
        session_dir: Some(root.path().join("sessions")),
        ..Default::default()
    };
    let web_core = Core::new(boot()).unwrap();
    let tui_core = Core::new(boot()).unwrap();
    let (web, web_connection) = connect(&web_core, "webui").await;
    let (tui, tui_connection) = connect(&tui_core, "tui").await;
    let (print, print_connection) = connect(&tui_core, "print").await;
    assert_eq!(remembered(&web).await, None);
    assert!(
        web.call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .is_err()
    );

    // 草稿选择立即持久化，另一个已启动的 core 无需重启就能读取。
    web.call::<ConfigManage>(ConfigAction::SelectModel {
        cwd: cwd.to_string_lossy().into_owned(),
        selector: "p/a:high".into(),
    })
    .await
    .unwrap();
    assert_eq!(remembered(&tui).await.as_deref(), Some("p/a:high"));
    let session = create(&tui, None, true).await;
    assert_eq!(session.model, "p/a:high");
    let selected = tui
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.id.clone(),
            selector: "p/b".into(),
            overflow: SessionModelOverflowAction::Switch,
        })
        .await
        .unwrap();
    assert_eq!(selected.session.model, "p/b:off");
    assert_eq!(remembered(&web).await.as_deref(), Some("p/b:off"));
    for active in [false, true, false] {
        let info = tui
            .call::<SessionPlanMode>(SessionPlanModeParams {
                session_id: session.id.clone(),
                active,
            })
            .await
            .unwrap();
        assert_eq!(info.model, "p/b:off");
        assert_eq!(remembered(&web).await.as_deref(), Some("p/b:off"));
    }

    let latest = create(&web, Some("fusion:p/a:high+p/b:off"), true).await;
    assert_eq!(
        remembered(&tui).await.as_deref(),
        Some(latest.model.as_str())
    );
    let printed = create(&print, Some("p/a:low"), true).await;
    assert_eq!(printed.model, "p/a:low");
    print
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: printed.id,
            selector: "p/a:high".into(),
            overflow: SessionModelOverflowAction::Switch,
        })
        .await
        .unwrap();
    assert_eq!(
        remembered(&web).await.as_deref(),
        Some(latest.model.as_str())
    );

    // 卸载后由另一进程恢复旧会话，不覆盖最近模型记忆。
    tui.call::<SessionUnload>(SessionUnloadParams {
        session_id: session.id.clone(),
    })
    .await
    .unwrap();
    let resumed = web
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id,
            max_entries: None,
            omit_auxiliary: false,
            read_only: false,
        })
        .await
        .unwrap();
    assert_eq!(resumed.session.model, "p/b:off");
    assert_eq!(
        remembered(&tui).await.as_deref(),
        Some(latest.model.as_str())
    );
    assert_eq!(create(&tui, None, false).await.model, latest.model);

    // 子代理持久会话通过普通模型接口切换，也不能污染主会话偏好。
    let table = moc_core::session::SessionTable::new(root.path().join("sessions"), true);
    let child = table
        .create(
            &cwd,
            "p/a:low",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            moc_core::session::entries::SessionOrigin::spawned(latest.id.clone(), 1, None),
            false,
        )
        .await
        .unwrap();
    table
        .append_message(
            &child.id,
            moc_protocol::Message::user_text("探索"),
            None,
            None,
        )
        .await
        .unwrap();
    drop(table);
    tui.call::<SessionLoad>(SessionLoadParams {
        session_id: child.id.clone(),
        max_entries: None,
        omit_auxiliary: false,
        read_only: false,
    })
    .await
    .unwrap();
    tui.call::<SessionSetModel>(SessionSetModelParams {
        session_id: child.id,
        selector: "p/b:off".into(),
        overflow: SessionModelOverflowAction::Switch,
    })
    .await
    .unwrap();
    assert_eq!(
        remembered(&web).await.as_deref(),
        Some(latest.model.as_str())
    );

    // 无效主动选择不得覆盖记忆。
    assert!(
        web.call::<ConfigManage>(ConfigAction::SelectModel {
            cwd: cwd.to_string_lossy().into_owned(),
            selector: "p/missing".into(),
        })
        .await
        .is_err()
    );
    assert_eq!(
        remembered(&tui).await.as_deref(),
        Some(latest.model.as_str())
    );
    let restarted = Core::new(boot()).unwrap();
    let (fresh, fresh_connection) = connect(&restarted, "tui").await;
    assert_eq!(
        remembered(&fresh).await.as_deref(),
        Some(latest.model.as_str())
    );

    drop((web, tui, print, fresh));
    drop((
        web_connection,
        tui_connection,
        print_connection,
        fresh_connection,
    ));
    web_core.shutdown().await;
    tui_core.shutdown().await;
    restarted.shutdown().await;
}

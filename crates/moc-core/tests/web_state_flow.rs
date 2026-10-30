mod support;

use moc_protocol::{
    ClientBuilder, ClientCapabilities, ClientInfo, RpcErrorCode, WebState, WebStateDocument,
    WebStateGet, WebStateGetParams, WebStateSave, WebStateSaveParams, WebStateUpdated,
    WebSubmitAction, WebTheme, transport,
};
use std::sync::Arc;

#[tokio::test]
async fn web_state_round_trips_through_rpc() {
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;

    let doc = harness
        .client
        .call::<WebStateGet>(WebStateGetParams {})
        .await
        .expect("web/state 成功");
    assert!(!doc.exists);
    assert_eq!(doc.state, WebState::default());

    let state = WebState {
        theme: WebTheme::Dark,
        active_submit: WebSubmitAction::Interrupt,
        workspaces: vec!["/repo".into()],
        ..WebState::default()
    };
    let saved = harness
        .client
        .call::<WebStateSave>(WebStateSaveParams {
            revision: doc.revision.clone(),
            state,
        })
        .await
        .expect("web/state_save 成功");
    assert!(saved.exists);

    // 旧 revision 再写被拒；重读后拿新 revision 才能写。
    let error = harness
        .client
        .call::<WebStateSave>(WebStateSaveParams {
            revision: doc.revision,
            state: WebState::default(),
        })
        .await
        .expect_err("过期 revision 必须失败");
    assert_eq!(error.code(), Some(RpcErrorCode::InvalidParams.code()));

    let fresh = harness
        .client
        .call::<WebStateGet>(WebStateGetParams {})
        .await
        .unwrap();
    assert_eq!(fresh.state.theme, WebTheme::Dark);
    assert_eq!(fresh.state.active_submit, WebSubmitAction::Interrupt);
    assert_eq!(fresh.state.workspaces, ["/repo".to_owned()]);

    // web.toml 落在 config_home，且不碰 config.toml。
    let home = harness.root.path().join("home");
    assert!(home.join("web.toml").exists());
    let config_toml = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(config_toml.contains("[providers.test]"));

    harness.close().await;
}

/// 另一条连接保存后，core 向其余读过 `web/state` 的连接广播 `web/state_updated`。
#[tokio::test]
async fn web_state_save_broadcasts_to_other_connections() {
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;

    // 第二条连接模拟另一个浏览器：注册通知处理器并读一次状态进扇出表。
    let received = Arc::new(parking_lot::Mutex::new(Vec::<WebStateDocument>::new()));
    let inbox = Arc::clone(&received);
    let (server_side, client_side) = transport::duplex(65536);
    let watcher_connection = harness.server.spawn(server_side);
    let (watcher, _) = ClientBuilder::new(ClientInfo {
        name: "旁观".into(),
        version: "1".into(),
        frontend: "test".into(),
    })
    .capabilities(ClientCapabilities {
        permission_requests: false,
        ask_requests: false,
    })
    .on_notification::<WebStateUpdated, _, _>(move |doc, _ctx| {
        inbox.lock().push(doc);
        std::future::ready(())
    })
    .connect(client_side)
    .await
    .expect("第二客户端连接");
    let doc = watcher
        .call::<WebStateGet>(WebStateGetParams {})
        .await
        .expect("旁观连接读 web/state");

    let saved = harness
        .client
        .call::<WebStateSave>(WebStateSaveParams {
            revision: doc.revision,
            state: WebState {
                theme: WebTheme::Dark,
                ..WebState::default()
            },
        })
        .await
        .expect("保存成功");

    // 扇出在独立任务里发，等通知到达即可断言收到了同一份文档。
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(doc) = received.lock().last() {
            assert_eq!(*doc, saved);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "等待 web/state_updated 超时"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    harness.close().await;
    watcher_connection.wait().await;
}

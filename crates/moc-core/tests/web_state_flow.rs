mod support;

use moc_protocol::{
    RpcErrorCode, WebState, WebStateGet, WebStateGetParams, WebStateSave, WebStateSaveParams,
    WebSubmitAction, WebTheme,
};

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

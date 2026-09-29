use moc_core::{
    CoreConfig, ResolvedConfig,
    config::{discovery, editor},
};
use moc_protocol::ConfigScope;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

fn workspace() -> (tempfile::TempDir, CoreConfig) {
    let root = tempfile::tempdir().unwrap();
    let boot = CoreConfig {
        home: root.path().join("home"),
        cwd: root.path().join("project"),
        ..Default::default()
    };
    std::fs::create_dir_all(&boot.home).unwrap();
    std::fs::create_dir_all(&boot.cwd).unwrap();
    (root, boot)
}

#[test]
fn first_configuration_requires_a_primary_and_inherits_other_roles() {
    let (_root, boot) = workspace();
    let empty = ResolvedConfig::load(&boot).unwrap();
    assert!(empty.resolve_role("default").is_err());
    let view = editor::inspect(&boot, ConfigScope::Global, &boot.cwd).unwrap();
    let document = json!({"models":{"roles":{"default":"p/demo:high"}},
        "providers":{"p":{"api":"openai_chat","base_url":"http://127.0.0.1:1", "models":{"demo":{"reasoning":true,"max_output_tokens":190000,"context_window":200000}}}}});
    let resolved = editor::save(
        &boot,
        ConfigScope::Global,
        &boot.cwd,
        document,
        &view.revision,
    )
    .unwrap();
    for role in ["default", "smol", "slow", "plan"] {
        assert_eq!(resolved.resolve_role(role).unwrap().selector, "p/demo:high");
    }
    assert_eq!(moc_core::compact::threshold(&resolved, 200000), 170000);
    assert_eq!(
        resolved.resolve_role("default").unwrap().llm.timeout_secs,
        None
    );
    let saved: toml::Value =
        toml::from_str(&std::fs::read_to_string(boot.home.join("config.toml")).unwrap()).unwrap();
    assert!(
        saved.get("tools").is_none(),
        "没有编辑的默认项不能被固化到用户文件"
    );
}

#[test]
fn stale_editor_cannot_overwrite_another_writer() {
    let (_root, boot) = workspace();
    let view = editor::inspect(&boot, ConfigScope::Global, &boot.cwd).unwrap();
    let changed = "[tools.job]\nmax_running=3\n";
    std::fs::write(boot.home.join("config.toml"), changed).unwrap();
    let result = editor::save(
        &boot,
        ConfigScope::Global,
        &boot.cwd,
        json!({"tools":{"job":{"max_running":7}}}),
        &view.revision,
    );
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(boot.home.join("config.toml")).unwrap(),
        changed
    );
}

#[test]
fn disabled_project_source_cannot_override_global_settings() {
    let (_root, boot) = workspace();
    std::fs::create_dir_all(boot.cwd.join(".moc")).unwrap();
    std::fs::write(
        boot.home.join("config.toml"),
        "[sources]\nproject_config=false\n[tools.job]\nmax_running=3",
    )
    .unwrap();
    std::fs::write(
        boot.cwd.join(".moc/config.toml"),
        "[tools.job]\nmax_running=9",
    )
    .unwrap();
    let resolved = ResolvedConfig::load(&boot)
        .unwrap()
        .for_project(&boot.cwd)
        .unwrap();
    assert_eq!(resolved.tools.job.max_running, 3);
    let view = editor::inspect(&boot, ConfigScope::Global, &boot.cwd).unwrap();
    editor::save(
        &boot,
        ConfigScope::Global,
        &boot.cwd,
        json!({"sources":{"project_config":true},"tools":{"job":{"max_running":3}}}),
        &view.revision,
    )
    .unwrap();
    assert_eq!(
        ResolvedConfig::load(&boot)
            .unwrap()
            .for_project(&boot.cwd)
            .unwrap()
            .tools
            .job
            .max_running,
        9
    );
}

#[tokio::test]
async fn anthropic_discovery_uses_auth_and_follows_opaque_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/v1/models")).and(header("x-api-key","test-key"))
        .and(header("anthropic-version","2023-06-01"))
        .respond_with(|request:&wiremock::Request|{
            if request.url.query_pairs().any(|(key,_)|key=="after_id"){
                ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"b/slash.model","display_name":"模型 B"}],"has_more":false}))
            }else{ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"a.1"}],"last_id":"a.1","has_more":true}))}
        }).expect(2).mount(&server).await;
    let models = discovery::models(
        "demo",
        json!({"api":"anthropic_messages","base_url":server.uri(),"api_key":"test-key"}),
    )
    .await
    .unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a.1", "b/slash.model"]
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[1]
            .url
            .query_pairs()
            .find(|(key, _)| key == "after_id")
            .unwrap()
            .1,
        "a.1"
    );
}

#[tokio::test]
async fn workspace_rename_preserves_identity_and_manual_cleanup_is_scoped() {
    let (_root, boot) = workspace();
    let server = MockServer::start().await;
    std::fs::write(
        boot.home.join("config.toml"),
        format!("[openviking]\nurl='{}'\n", server.uri()),
    )
    .unwrap();
    let config = ResolvedConfig::load(&boot).unwrap();
    let original = moc_core::workspace::registry::resolve(&config, &boot.cwd).unwrap();
    let renamed = boot.cwd.with_file_name("renamed");
    std::fs::rename(&boot.cwd, &renamed).unwrap();
    let before = moc_core::workspace::registry::list(&boot.home).unwrap();
    assert!(!before[0].exists);
    moc_core::workspace::registry::rebind(
        &boot.home,
        &server.uri(),
        &before[0].path,
        renamed.to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(
        moc_core::workspace::registry::resolve(&config, &renamed)
            .unwrap()
            .peer_id,
        original.peer_id
    );
    let uri = format!("viking://~/peers/{}", original.peer_id);
    Mock::given(method("DELETE"))
        .and(path("/api/v1/fs"))
        .and(query_param("uri", uri.clone()))
        .and(query_param("recursive", "true"))
        .and(header("x-openviking-actor-peer", original.peer_id.clone()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"status":"ok","result":{"uri":uri}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let current = moc_core::workspace::registry::list(&boot.home).unwrap();
    moc_core::workspace::registry::delete(
        &config,
        &server.uri(),
        &current[0].path,
        &original.peer_id,
    )
    .await
    .unwrap();
    assert!(
        moc_core::workspace::registry::list(&boot.home)
            .unwrap()
            .is_empty()
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "读取及重绑不应修改远端知识");
}

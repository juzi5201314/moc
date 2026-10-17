use moc_core::{
    CoreConfig, ResolvedConfig,
    config::{discovery, editor},
};
use moc_protocol::{ConfigScope, FieldKind};
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
    for role in ["default", "explore", "plan"] {
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
fn project_mcp_json_is_opt_in() {
    let (_root, boot) = workspace();
    std::fs::write(
        boot.cwd.join(".mcp.json"),
        r#"{"mcpServers":{"project":{"command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();
    let resolved = ResolvedConfig::load(&boot)
        .unwrap()
        .for_project(&boot.cwd)
        .unwrap();
    assert!(!resolved.mcp.project_config, "项目级 MCP 配置默认关闭");
    assert!(
        resolved.mcp_servers().unwrap().is_empty(),
        "默认不得启动项目 .mcp.json 里的命令"
    );
    std::fs::write(
        boot.home.join("config.toml"),
        "[mcp]\nproject_config=true\n",
    )
    .unwrap();
    let resolved = ResolvedConfig::load(&boot)
        .unwrap()
        .for_project(&boot.cwd)
        .unwrap();
    let servers = resolved.mcp_servers().unwrap();
    assert_eq!(servers.len(), 1, "显式开启后才加载项目配置");
    assert_eq!(servers[0].name, "project");
}

#[test]
fn defaults_section_is_a_directive_and_may_be_empty() {
    let (_root, boot) = workspace();
    std::fs::write(
        boot.home.join("config.toml"),
        "[defaults]\n[models.roles]\ndefault='p/m'\n[providers.p]\napi='openai_chat'\n\
         base_url='http://127.0.0.1:1'\n[providers.p.models.m]\nreasoning=true\n",
    )
    .unwrap();
    let loaded = ResolvedConfig::load(&boot);
    assert!(
        loaded.is_ok(),
        "空的 [defaults] 表与没有 defaults 段等价：{:?}",
        loaded.err()
    );
    assert!(editor::inspect(&boot, ConfigScope::Global, &boot.cwd).is_ok());
    std::fs::write(
        boot.home.join("config.toml"),
        "[defaults.models.m]\ncontext_window=123456\n[models.roles]\ndefault='p/m'\n\
         [providers.p]\napi='openai_chat'\nbase_url='http://127.0.0.1:1'\n\
         [providers.p.models.m]\nreasoning=true\n",
    )
    .unwrap();
    let resolved = ResolvedConfig::load(&boot).unwrap();
    let models = resolved.available_models();
    assert_eq!(models.len(), 1);
    assert_eq!(
        models[0].context_window, 123456,
        "defaults.models 应给匹配的模型补缺省字段"
    );
    std::fs::write(boot.home.join("config.toml"), "[defaults]\nmodels = 3\n").unwrap();
    assert!(
        ResolvedConfig::load(&boot).is_err(),
        "defaults.models 不是表时仍要报错"
    );
}

#[test]
fn project_config_sources_stays_opt_in() {
    let (_root, boot) = workspace();
    std::fs::create_dir_all(boot.cwd.join(".moc")).unwrap();
    let marker = boot.cwd.join("project-command-ran");
    std::fs::write(
        boot.cwd.join(".moc/config.toml"),
        format!(
            "[tools.job]\nmax_running=9\n[web_search]\napi_key='!echo project-key; touch {}'\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::write(
        boot.home.join("config.toml"),
        "[tools.job]\nmax_running=3\n",
    )
    .unwrap();
    let resolved = ResolvedConfig::load(&boot)
        .unwrap()
        .for_project(&boot.cwd)
        .unwrap();
    assert!(!resolved.sources.project_config, "项目级配置默认关闭");
    assert_eq!(resolved.tools.job.max_running, 3, "默认不得合并项目配置");
    assert!(!marker.exists(), "默认不得执行项目配置里的 !命令");
    std::fs::write(
        boot.home.join("config.toml"),
        "[sources]\nproject_config=true\n[tools.job]\nmax_running=3\n",
    )
    .unwrap();
    let resolved = ResolvedConfig::load(&boot)
        .unwrap()
        .for_project(&boot.cwd)
        .unwrap();
    assert_eq!(resolved.tools.job.max_running, 9, "显式开启后项目配置生效");
    assert!(marker.exists(), "显式开启后项目配置里的 !命令才执行");
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
    let server = MockServer::builder().start().await;
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
    let server = MockServer::builder().start().await;
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

#[test]
fn soft_method_defaults_to_moc_and_switches_through_the_editor() {
    let (_root, boot) = workspace();
    std::fs::write(
        boot.home.join("config.toml"),
        "[models.roles]\ndefault='p/demo'\nexplore='p/demo'\nplan='p/demo'\n\
         [providers.p]\napi='openai_chat'\nbase_url='http://127.0.0.1:1'\n\
         [providers.p.models.demo]\n",
    )
    .unwrap();
    let resolved = ResolvedConfig::load(&boot).unwrap();
    assert_eq!(
        resolved.compaction.soft_method,
        moc_core::config::SoftMethod::Moc,
        "默认仍是 moc 自有 soft 实现"
    );
    assert_eq!(
        resolved.compaction.method_order.len(),
        3,
        "soft_method 不进入 method_order"
    );

    let view = editor::inspect(&boot, ConfigScope::Global, &boot.cwd).unwrap();
    let field = view
        .fields
        .iter()
        .find(|field| field.path == ["compaction", "soft_method"])
        .expect("设置目录出现 soft_method");
    assert_eq!(field.tab, "上下文");
    assert_eq!(field.label, "Soft 摘要方式");
    let FieldKind::Choice(options) = &field.kind else {
        panic!("soft_method 应为候选列表");
    };
    assert!(
        options
            .iter()
            .any(|option| option.value == "grok_build" && option.label.contains("Grok Build")),
        "{options:?}"
    );
    assert!(
        options.iter().any(|option| option.value == "moc"),
        "{options:?}"
    );

    let resolved = editor::save(
        &boot,
        ConfigScope::Global,
        &boot.cwd,
        json!({"compaction": {"soft_method": "grok_build"}}),
        &view.revision,
    )
    .unwrap();
    assert_eq!(
        resolved.compaction.soft_method,
        moc_core::config::SoftMethod::GrokBuild
    );
    let saved: toml::Value =
        toml::from_str(&std::fs::read_to_string(boot.home.join("config.toml")).unwrap()).unwrap();
    assert_eq!(
        saved["compaction"]["soft_method"].as_str(),
        Some("grok_build")
    );
}

#[test]
fn unknown_soft_method_is_rejected() {
    let (_root, boot) = workspace();
    std::fs::write(
        boot.home.join("config.toml"),
        "[models.roles]\ndefault='p/demo'\nexplore='p/demo'\nplan='p/demo'\n\
         [providers.p]\napi='openai_chat'\nbase_url='http://127.0.0.1:1'\n\
         [providers.p.models.demo]\n[compaction]\nsoft_method = \"caveman\"\n",
    )
    .unwrap();
    let Err(error) = ResolvedConfig::load(&boot) else {
        panic!("未知的 soft 实现必须报错");
    };
    assert!(error.to_string().contains("soft_method"), "{error}");
}

#[test]
fn web_search_base_url_round_trips_through_editor() {
    let (_root, boot) = workspace();
    let view = editor::inspect(&boot, ConfigScope::Global, &boot.cwd).unwrap();
    let web_search_field = |name: &str| {
        view.fields.iter().find(|field| {
            field.path.len() == 2 && field.path[0] == "web_search" && field.path[1] == name
        })
    };
    let provider_field = web_search_field("provider").expect("provider 字段出现在设置目录");
    assert_eq!(provider_field.tab, "工具");
    assert_eq!(provider_field.label, "供应商");
    let FieldKind::Choice(options) = &provider_field.kind else {
        panic!("provider 应为候选列表");
    };
    assert!(
        options
            .iter()
            .any(|option| option.value == "tavily" && option.label == "Tavily"),
        "provider 候选应包含 Tavily: {options:?}"
    );
    let api_key_field = web_search_field("api_key").expect("api_key 字段出现在设置目录");
    assert!(matches!(api_key_field.kind, FieldKind::Secret));
    let resolved = editor::save(
        &boot,
        ConfigScope::Global,
        &boot.cwd,
        json!({"web_search": {"base_url": "https://proxy.example.com"}}),
        &view.revision,
    )
    .unwrap();
    assert_eq!(resolved.web_search.base_url, "https://proxy.example.com");
    let saved: toml::Value =
        toml::from_str(&std::fs::read_to_string(boot.home.join("config.toml")).unwrap()).unwrap();
    assert_eq!(
        saved["web_search"]["base_url"].as_str(),
        Some("https://proxy.example.com")
    );
    assert!(
        saved["web_search"].get("provider").is_none(),
        "未编辑的默认项不落盘"
    );
}

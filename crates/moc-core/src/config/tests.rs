use std::fs;

use super::*;

fn core_config(home: &Path, cwd: &Path, overlays: Vec<PathBuf>) -> CoreConfig {
    CoreConfig {
        home: home.to_owned(),
        cwd: cwd.to_owned(),
        overlays,
        session_dir: None,
        client: moc_llm::Client::new(),
        ptc_worker: None,
    }
}

fn provider_config(api_key: &str) -> String {
    format!(
        "[models.roles]\ndefault='p/model:high'\nexplore='p/model:low'\nplan='p/model:max'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\napi_key='{api_key}'\n[providers.p.models.model]\nreasoning=true\ninput=['text','image']\n"
    )
}

#[test]
fn session_compression_defaults_off_and_can_be_enabled() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    let cwd = dir.path().join("work");
    fs::create_dir_all(&home).expect("创建 home");
    fs::create_dir_all(&cwd).expect("创建 cwd");
    let config_path = home.join("config.toml");
    fs::write(&config_path, provider_config("literal")).expect("写配置");
    let config = ResolvedConfig::load(&core_config(&home, &cwd, vec![])).expect("默认配置可解析");
    assert!(!config.session.compress, "会话压缩默认关闭");

    fs::write(
        &config_path,
        format!("{}\n[session]\ncompress=true\n", provider_config("literal")),
    )
    .expect("写压缩配置");
    let config = ResolvedConfig::load(&core_config(&home, &cwd, vec![])).expect("压缩配置可解析");
    assert!(config.session.compress);
}

#[test]
fn sandbox_paths_are_expanded_and_allow_paths_must_exist() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    let cwd = dir.path().join("work");
    let extra = dir.path().join("extra");
    fs::create_dir_all(&home).expect("创建 home");
    fs::create_dir_all(&cwd).expect("创建 cwd");
    fs::create_dir_all(&extra).expect("创建 extra");
    let config_path = home.join("config.toml");
    fs::write(
            &config_path,
            format!(
                "{}\n[sandbox]\ndefault_mode='workspace_only'\nnetwork=false\non_denied='deny'\nextra_read_paths=[{:?}]\ndeny_read_paths=['/definitely-not-existing']\n",
                provider_config("literal"),
                extra.display().to_string()
            ),
        )
        .expect("写配置");
    let config = ResolvedConfig::load(&core_config(&home, &cwd, vec![])).expect("沙盒配置可解析");
    assert_eq!(
        config.sandbox.default_mode,
        SessionPermission::WorkspaceOnly
    );
    assert!(!config.sandbox.network);
    assert_eq!(config.sandbox.on_denied, OnDenied::Deny);
    assert_eq!(
        config.sandbox.extra_read_paths,
        vec![extra.canonicalize().unwrap()]
    );
    assert_eq!(
        config.sandbox.deny_read_paths,
        vec![PathBuf::from("/definitely-not-existing")]
    );

    fs::write(
        &config_path,
        format!(
            "{}\n[sandbox]\nextra_read_paths=['relative/path']\n",
            provider_config("literal")
        ),
    )
    .expect("写相对路径配置");
    assert!(ResolvedConfig::load(&core_config(&home, &cwd, vec![])).is_err());

    fs::write(
        &config_path,
        format!(
            "{}\n[sandbox]\nextra_read_paths=['/definitely/not/existing']\n",
            provider_config("literal")
        ),
    )
    .expect("写不存在路径配置");
    assert!(ResolvedConfig::load(&core_config(&home, &cwd, vec![])).is_err());
}

#[test]
fn deny_paths_cannot_cover_the_workspace() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    let cwd = dir.path().join("work");
    fs::create_dir_all(&home).expect("创建 home");
    fs::create_dir_all(&cwd).expect("创建 cwd");
    let config_path = home.join("config.toml");
    for denied in [cwd.clone(), dir.path().to_path_buf()] {
        fs::write(
            &config_path,
            format!(
                "{}\n[sandbox]\ndeny_write_paths=[{:?}]\n",
                provider_config("literal"),
                denied.display().to_string()
            ),
        )
        .expect("写配置");
        assert!(
            ResolvedConfig::load(&core_config(&home, &cwd, vec![])).is_err(),
            "deny 路径 {denied:?} 覆盖工作区时必须报错"
        );
    }
}

#[test]
fn merges_global_project_and_cli_tables_in_precedence_order() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    let overlay = dir.path().join("cli.toml");
    fs::create_dir_all(&home).expect("创建 home");
    fs::create_dir_all(project.join(".moc")).expect("创建项目配置目录");
    fs::write(
            home.join("config.toml"),
            format!(
                "{}\n[sandbox]\nnetwork=false\non_denied=\"deny\"\n[tools.read]\ndefault_limit=400\nmax_bytes=40000\n",
                provider_config("literal")
            ),
        )
        .expect("写全局配置");
    fs::write(
        project.join(".moc/config.toml"),
        "[tools.read]\ndefault_limit=600\nline_max_bytes=12345\n\n[sandbox]\nnetwork=true\n",
    )
    .expect("写项目配置");
    fs::write(
        &overlay,
        "[tools.read]\ndefault_limit=700\nmax_bytes=8192\n\n[sandbox]\nallow_unsandboxed=false\n",
    )
    .expect("写 CLI 覆盖层");

    let config = ResolvedConfig::load(&core_config(&home, &project, vec![overlay]))
        .expect("全局与 CLI 配置可解析")
        .for_project(&project)
        .expect("项目配置可合并");

    assert!(config.sandbox.network);
    assert_eq!(config.sandbox.on_denied, OnDenied::Deny);
    assert!(!config.sandbox.allow_unsandboxed);
    // 项目配置覆盖全局、CLI 覆盖层再覆盖项目：三层的值都能看出来。
    assert_eq!(config.tools.read.default_limit, 700);
    assert_eq!(config.tools.read.line_max_bytes, 12345);
    assert_eq!(config.tools.read.max_bytes, 8192);
    assert_eq!(
        config
            .require_last_model()
            .expect("default 模型")
            .thinking_level,
        ThinkingLevel::High
    );
    assert!(config.resolve_model("plan").is_err());
}

#[test]
fn resolves_literal_environment_and_command_secrets_without_leaking_them() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    let (home_variable, home_secret) = std::env::var("HOME")
        .map(|value| ("HOME", value))
        .or_else(|_| std::env::var("USERPROFILE").map(|value| ("USERPROFILE", value)))
        .expect("测试环境提供用户主目录变量");
    let environment_secret = format!("env:{home_variable}");
    for (secret, expected) in [
        ("literal-token", "literal-token".to_owned()),
        (environment_secret.as_str(), home_secret),
        ("!echo command-token", "command-token".to_owned()),
    ] {
        fs::write(home.join("config.toml"), provider_config(secret)).expect("写配置");
        let config =
            ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("密钥可解析");
        let model = config.require_last_model().expect("模型可解析");
        assert_eq!(model.llm.auth, moc_llm::Auth::Bearer(expected.clone()));
        assert!(!format!("{:?}", model.llm.auth).contains(&expected));
    }
}

/// 插件配置的 0 值必须拒绝：它会让「永远超时」「永远加载失败」变成静默行为。
#[test]
fn rejects_zero_plugin_limits_and_unknown_plugin_keys() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    let base = provider_config("k");
    fs::write(
        home.join("config.toml"),
        format!("{base}\n[plugins]\ncall_timeout_ms=0\n"),
    )
    .expect("写配置");
    let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
        Ok(_) => panic!("0 超时必须报错"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("call_timeout_ms"), "{error}");

    fs::write(
        home.join("config.toml"),
        format!("{base}\n[plugins]\nmax_plugns=3\n"),
    )
    .expect("写配置");
    assert!(
        ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).is_err(),
        "未知字段必须报错"
    );

    // 默认值：全开、64 MiB、5 秒、4k 提示词预算。
    fs::write(home.join("config.toml"), &base).expect("写配置");
    let config =
        ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("默认可用");
    assert!(config.plugins.enabled && config.plugins.project && config.plugins.user);
    assert_eq!(config.plugins.memory_limit_bytes, 64 * 1024 * 1024);
    assert_eq!(config.plugins.call_timeout_ms, 5000);
    assert_eq!(config.plugins.max_prompt_chars, 4000);
}

#[test]
fn rejects_unknown_levels_missing_environment_and_invalid_toml() {
    assert!("Ultra".parse::<ThinkingLevel>().is_err());
    assert!("x:y".parse::<ThinkingLevel>().is_err());
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    fs::write(
        home.join("config.toml"),
        provider_config("env:MOC_TEST_MISSING_KEY_7261"),
    )
    .expect("写配置");
    assert!(ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).is_err());
    fs::write(home.join("config.toml"), "[providers.p\ninvalid").expect("写非法 TOML");
    assert!(ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).is_err());
}

/// Fusion 双模型配置：lead 是带推理的 model，sidekick 是普通 model2。
fn fusion_provider_config(roles: &str, fusion: &str) -> String {
    format!(
        "[models.roles]\n{roles}\nexplore='inherit'\nplan='inherit'\n{fusion}\
             [providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\napi_key='k'\n\
             [providers.p.models.model]\nreasoning=true\ninput=['text','image']\n\
             [providers.p.models.model2]\n"
    )
}

fn load_fusion_config(home: &Path, cwd: &Path, source: &str) -> ResolvedConfig {
    fs::create_dir_all(home).expect("创建 home");
    fs::write(home.join("config.toml"), source).expect("写配置");
    ResolvedConfig::load(&core_config(home, cwd, vec![])).expect("配置可解析")
}

#[test]
fn fusion_selector_canonicalizes_both_sides_and_exposes_the_pair() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    let cwd = dir.path().join("work");
    let config = load_fusion_config(
        &home,
        &cwd,
        &fusion_provider_config(
            "default='p/model:high'",
            "[models.fusion]\nlead='p/model'\nsidekick='p/model2'\n",
        ),
    );
    // 显式写法两侧各自规范化，等级补全。
    let model = config
        .resolve_model("fusion:p/model:high+p/model2")
        .expect("显式 Fusion 可解析");
    assert_eq!(model.selector, "fusion:p/model:high+p/model2:off");
    assert_eq!(model.display_name, "Fusion");
    let fusion = model.fusion.as_ref().expect("Fusion 组合");
    assert_eq!(fusion.lead, "p/model:high");
    assert_eq!(fusion.sidekick, "p/model2:off");
    assert_eq!(model.actual_selector(), "p/model:high");
    // 普通模型 actual_selector 即自身。
    let plain = config.require_last_model().expect("普通模型");
    assert!(plain.fusion.is_none());
    assert_eq!(plain.actual_selector(), "p/model:high");
    // 别名展开 [models.fusion]，等级按模型默认补全。
    let alias = config.resolve_model("fusion").expect("别名可解析");
    assert_eq!(alias.selector, "fusion:p/model:low+p/model2:off");
    assert_eq!(alias.actual_selector(), "p/model:low");
    assert!(crate::config::model::is_fusion_selector(&alias.selector));
    // 角色映射后同样走 Fusion 语法。
    let dir = tempfile::tempdir().expect("临时目录");
    let config = load_fusion_config(
        &dir.path().join("home"),
        &dir.path().join("work"),
        &fusion_provider_config(
            "default='fusion'",
            "[models.fusion]\nlead='p/model'\nsidekick='p/model2'\n",
        ),
    );
    assert!(
        config
            .require_last_model()
            .expect("default=fusion 可解析")
            .fusion
            .is_some()
    );
}

#[test]
fn fusion_alias_and_nested_or_half_configured_pairs_are_rejected() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    let cwd = dir.path().join("work");
    // 未配置 [models.fusion] 时别名明确报错。
    let config = load_fusion_config(&home, &cwd, &provider_config("literal"));
    let Err(error) = config.resolve_model("fusion") else {
        panic!("别名未配置必须报错");
    };
    assert!(
        error.to_string().contains("尚未配置 Fusion 组合"),
        "{error}"
    );
    // 任一侧仍是 Fusion 选择器：拒绝嵌套。
    for selector in ["fusion:fusion+p/model", "fusion:p/model+fusion:a/b"] {
        let Err(error) = config.resolve_model(selector) else {
            panic!("嵌套 Fusion 必须报错");
        };
        assert!(error.to_string().contains("不能嵌套 Fusion"), "{error}");
    }
    // [models.fusion] 只配一侧：合法且视为未配置，但非空侧必须可解析。
    fs::write(
        home.join("config.toml"),
        fusion_provider_config("default='p/model'", "[models.fusion]\nlead='p/model'\n"),
    )
    .expect("写半配置");
    let config = ResolvedConfig::load(&core_config(&home, &cwd, vec![])).expect("半配置可加载");
    let Err(error) = config.resolve_model("fusion") else {
        panic!("半配置下别名仍按未配置报错");
    };
    assert!(
        error.to_string().contains("尚未配置 Fusion 组合"),
        "{error}"
    );
    fs::write(
        home.join("config.toml"),
        fusion_provider_config("default='p/model'", "[models.fusion]\nsidekick='p/nope'\n"),
    )
    .expect("写坏配置");
    let Err(error) = ResolvedConfig::load(&core_config(&home, &cwd, vec![])) else {
        panic!("非空侧不可解析必须报错");
    };
    assert!(
        error.to_string().contains("models.fusion.sidekick"),
        "{error}"
    );
}

#[test]
fn wildcard_model_defaults_merge_before_provider_overrides() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    fs::write(
        home.join("config.toml"),
        r#"
[models.roles]
default = "p/ag/gpt-5.6-sol:high"
explore = "p/ag/gpt-5.6-sol:high"
plan = "p/ag/gpt-5.6-sol:max"

[defaults.models."*gpt-5.6-sol"]
context_window = 500000
max_output_tokens = 384000
reasoning = true
thinking_levels = ["high", "max"]
input = ["text", "image"]

[defaults.models."*gpt-5.6-sol".compat]
developer_role = true

[providers.p]
api = "openai_responses"
base_url = "https://example.invalid"

[providers.p.models."ag/gpt-5.6-sol"]
max_output_tokens = 128000
"#,
    )
    .expect("写配置");

    let config = ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new()))
        .expect("通配模型默认值可解析");
    let model = config.require_last_model().expect("解析模型");
    assert_eq!(model.context_window, 500_000);
    assert_eq!(model.max_output_tokens, 128_000);
    assert!(model.capabilities.reasoning);
    assert!(model.capabilities.image);
    assert_eq!(
        model.thinking_levels,
        vec![ThinkingLevel::High, ThinkingLevel::Max]
    );
    assert!(model.llm.wire.developer_role);
}

#[test]
fn reasoning_defaults_to_standard_effort_levels() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    fs::write(
            home.join("config.toml"),
            "[models.roles]\ndefault='p/m:max'\nexplore='p/m:low'\nplan='p/m:max'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\nreasoning=true\n",
        )
        .expect("写配置");
    let config = ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new()))
        .expect("默认思考等级可解析");
    assert_eq!(
        config
            .require_last_model()
            .expect("解析模型")
            .thinking_levels,
        vec![
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ]
    );
}

#[test]
fn validates_standard_tool_concurrency_limit() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    fs::write(
        home.join("config.toml"),
        format!(
            "{}\n[tools]\nmax_concurrent_calls = 0\n",
            provider_config("literal")
        ),
    )
    .expect("写配置");
    assert!(ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).is_err());
}

#[test]
fn legacy_roles_migrate_without_restoring_plan_model_behavior() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let provider = "[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\n[providers.p.models.other]\n";
    fs::write(
        home.join("config.toml"),
        format!("[models.roles]\ndefault='p/m'\nplan='p/missing'\n{provider}"),
    )
    .unwrap();
    let boot = core_config(&home, dir.path(), Vec::new());
    let config = ResolvedConfig::load(&boot).unwrap();
    assert_eq!(config.require_last_model().unwrap().selector, "p/m:off");
    assert!(config.resolve_model("default").is_err());
    assert!(config.resolve_model("plan").is_err());
    selection::save(&home, "p/other:off").unwrap();
    let reloaded = ResolvedConfig::load(&boot).unwrap();
    assert_eq!(
        reloaded.require_last_model().unwrap().selector,
        "p/other:off"
    );
    let view = editor::inspect(&boot, moc_protocol::ConfigScope::Global, dir.path()).unwrap();
    assert!(view.document.pointer("/models/roles/default").is_none());
    assert!(view.effective.pointer("/models/roles/plan").is_none());
    fs::write(home.join("config.toml"), "[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\n").unwrap();
    let removed = ResolvedConfig::load(&boot).unwrap();
    assert!(removed.last_model().unwrap().is_none());
    assert!(removed.require_last_model().is_err());
    fs::write(
        home.join("config.toml"),
        format!("[models.roles]\nexplore='none'\n{provider}"),
    )
    .unwrap();
    assert_eq!(ResolvedConfig::load(&boot).unwrap().roles.explore, "none");
}

#[test]
fn web_tools_follow_their_own_switches() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    fs::write(home.join("config.toml"), provider_config("literal")).expect("写配置");
    let config =
        ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("配置可解析");
    let registry = crate::tools::ToolRegistry::new();
    let visible = |config: &ResolvedConfig, name: &str| {
        registry.get(name).is_some_and(|tool| {
            crate::service::mode::tool_enabled(name, tool.origin(), false, config)
        })
    };
    assert!(!visible(&config, "web_search"), "无 key 时搜索工具隐藏");
    assert!(visible(&config, "web_fetch"), "抓取不依赖密钥");

    fs::write(
        home.join("config.toml"),
        format!(
            "{}\n[web_search]\napi_key='tvly-test'\n",
            provider_config("literal")
        ),
    )
    .expect("写配置");
    let config =
        ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("配置可解析");
    assert_eq!(config.web_search.api_key.as_deref(), Some("tvly-test"));
    assert!(visible(&config, "web_search"));
    assert!(visible(&config, "web_fetch"));

    fs::write(
        home.join("config.toml"),
        format!(
            "{}\n[web_search]\napi_key='tvly-test'\n[web_fetch]\nenabled=false\n",
            provider_config("literal")
        ),
    )
    .expect("写配置");
    let config =
        ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("配置可解析");
    assert!(visible(&config, "web_search"));
    assert!(!visible(&config, "web_fetch"), "独立开关只隐藏抓取");
}

#[test]
fn limits_reject_removed_knobs_and_invalid_deadlines() {
    // 已移除的轮数／字节／token／调用数上限必须被拒绝，避免旧配置静默失效。
    for (snippet, expected) in [
        ("max_turns_per_prompt=1\n", "max_turns_per_prompt"),
        ("max_response_bytes=1\n", "max_response_bytes"),
        (
            "max_response_duration_secs=1\n",
            "max_response_duration_secs",
        ),
        ("max_prompt_bytes=1\n", "max_prompt_bytes"),
        ("max_prompt_output_tokens=1\n", "max_prompt_output_tokens"),
        ("max_tool_calls_per_prompt=1\n", "max_tool_calls_per_prompt"),
        ("max_prompt_duration_secs=1\n", "max_prompt_duration_secs"),
        ("connect_deadline_secs=0\n", "必须大于 0"),
        ("request_warn_secs=0\n", "必须大于 0"),
        ("connect_deadline_secs=604801\n", "不得超过 7 天"),
    ] {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(
            home.join("config.toml"),
            format!("{}\n[limits]\n{snippet}", provider_config("literal")),
        )
        .expect("写配置");
        let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
            Ok(_) => panic!("{snippet} 应被拒绝"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains(expected),
            "unexpected error for {snippet}: {error}"
        );
    }
}

#[test]
fn limits_defaults_cover_loop_detection_and_deadlines() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    fs::write(home.join("config.toml"), provider_config("literal")).expect("写配置");
    let config =
        ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("配置可解析");
    assert_eq!(config.limits.connect_deadline_secs, 900);
    assert_eq!(config.limits.request_warn_secs, 900);
    assert!(config.limits.loop_detection);
    assert_eq!(config.limits.max_length_continues, 2);
}

#[test]
fn rejects_invalid_web_search_settings() {
    for (snippet, expected) in [
        ("provider='google'\n", "web_search.provider"),
        (
            "base_url='ftp://proxy.example.com'\n",
            "web_search.base_url",
        ),
        ("timeout_secs=0\n", "web_search.timeout_secs"),
    ] {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(
            home.join("config.toml"),
            format!("{}\n[web_search]\n{snippet}", provider_config("literal")),
        )
        .expect("写配置");
        let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
            Ok(_) => panic!("{snippet} 应被拒绝"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains(expected),
            "unexpected error for {snippet}: {error}"
        );
    }
}

#[test]
fn rejects_invalid_web_fetch_settings() {
    for (snippet, expected) in [
        ("timeout_secs=0\n", "web_fetch.timeout_secs"),
        ("max_bytes=0\n", "web_fetch.max_bytes"),
        ("max_chars=0\n", "web_fetch.max_chars"),
    ] {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(
            home.join("config.toml"),
            format!("{}\n[web_fetch]\n{snippet}", provider_config("literal")),
        )
        .expect("写配置");
        let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
            Ok(_) => panic!("{snippet} 应被拒绝"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains(expected),
            "unexpected error for {snippet}: {error}"
        );
    }
}

#[test]
fn normalizes_web_search_provider_case() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    fs::write(
        home.join("config.toml"),
        format!(
            "{}\n[web_search]\nprovider=' Tavily '\n",
            provider_config("literal")
        ),
    )
    .expect("写配置");
    let config = ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new()))
        .expect("大小写与空白被归一化");
    assert_eq!(config.web_search.provider, "tavily");
}

#[test]
fn web_search_env_secret_reports_missing_variable() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    fs::write(
        home.join("config.toml"),
        format!(
            "{}\n[web_search]\napi_key='env:MOC_TEST_MISSING_WEB_KEY_7261'\n",
            provider_config("literal")
        ),
    )
    .expect("写配置");
    let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
        Ok(_) => panic!("缺失环境变量应报错"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("web_search.api_key"));
}

#[test]
fn feature_flags_follow_overrides_and_existing_sections() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    let cwd = dir.path().join("work");
    fs::create_dir_all(&home).expect("创建 home");
    fs::create_dir_all(&cwd).expect("创建 cwd");
    let config_path = home.join("config.toml");
    fs::write(&config_path, provider_config("literal")).expect("写配置");
    let config = ResolvedConfig::load(&core_config(&home, &cwd, vec![])).expect("默认配置可解析");
    // 默认全开：自有键走注册表默认，已有段读各自 enabled。
    assert!(config.feature_enabled(FeatureFlag::Ptc));
    assert!(config.feature_enabled(FeatureFlag::Workflow));
    assert!(
        config
            .features
            .engine_available(moc_protocol::SessionEngine::Ptc)
    );

    fs::write(
        &config_path,
        format!(
            "{}\n[features]\nptc=false\n[workflow]\nenabled=false\n",
            provider_config("literal")
        ),
    )
    .expect("写覆盖配置");
    let config = ResolvedConfig::load(&core_config(&home, &cwd, vec![])).expect("覆盖配置可解析");
    assert!(!config.feature_enabled(FeatureFlag::Ptc));
    assert!(
        !config
            .features
            .engine_available(moc_protocol::SessionEngine::Ptc)
    );
    assert!(
        config
            .features
            .engine_available(moc_protocol::SessionEngine::Standard)
    );
    assert!(!config.feature_enabled(FeatureFlag::Workflow));
    assert!(
        config.feature_enabled(FeatureFlag::Clm),
        "未覆盖的键保持默认"
    );
}

#[test]
fn feature_flags_reject_unknown_and_foreign_keys() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("创建 home");
    fs::write(
        home.join("config.toml"),
        format!("{}\n[features]\nbogus=true\n", provider_config("literal")),
    )
    .expect("写配置");
    let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
        Ok(_) => panic!("未知 features 键应报错"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("features.bogus"));

    // 挂在已有配置段上的键在 [features] 里拒绝并指向正确路径。
    fs::write(
        home.join("config.toml"),
        format!(
            "{}\n[features]\nworkflow=false\n",
            provider_config("literal")
        ),
    )
    .expect("写配置");
    let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
        Ok(_) => panic!("已有段的 features 键应报错"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("workflow.enabled"));
}

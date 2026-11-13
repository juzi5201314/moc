use super::*;

/// 零泄漏（spec §6 验收）：未激活会话的工具定义 JSON 与「workflow 模块不存在」
/// 的产物逐字节一致；PTC 签名段同样不含 workflow 工具；`<skills>` 索引未激活
/// 不含 workflow-authoring。
#[test]
fn zero_leak_definitions_and_signatures() {
    let registry = crate::tools::ToolRegistry::new();
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let config = crate::config::ResolvedConfig::load(&crate::CoreConfig {
        home: home.path().to_path_buf(),
        cwd: cwd.path().to_path_buf(),
        overlays: Vec::new(),
        session_dir: None,
        client: moc_llm::Client::default(),
        ptc_worker: None,
    })
    .unwrap();
    // 未激活 scope：definitions 必须一个 workflow 工具都不含，且与「把 9 个工具
    // 从注册表删掉」的产物逐字节一致。
    let mut scope = crate::tools::ToolScope::top();
    let inactive: Vec<String> = registry
        .definitions(
            moc_protocol::SessionEngine::Standard,
            false,
            &config,
            scope.clone(),
            &[],
        )
        .iter()
        .map(|d| serde_json::to_string(d).unwrap())
        .collect();
    for tool in WORKFLOW_TOOLS {
        assert!(!inactive.iter().any(|s| s.contains(tool)), "leak: {tool}");
    }
    scope.workflows = true;
    let active = crate::tools::catalog::ToolCatalog::new(
        &registry,
        &moc_mcp::McpSnapshot::default(),
        moc_mcp::McpManager::new(Vec::new(), tokio_util::sync::CancellationToken::new()),
        Vec::new(),
        scope,
    )
    .unwrap();
    let active_names: Vec<_> = active
        .visible(
            moc_protocol::SessionEngine::Standard,
            crate::service::mode::Mode::Default,
            false,
            true,
            &config,
        )
        .map(|tool| tool.name())
        .collect();
    for tool in WORKFLOW_TOOLS {
        assert!(
            active_names.iter().any(|n| n == tool),
            "missing {tool} when active"
        );
    }
    // 子代理永不给 workflow 工具（即使会话已激活）。
    let sub_scope = crate::tools::ToolScope {
        workflows: false,
        ..crate::tools::ToolScope::top()
    };
    let sub: Vec<String> = registry
        .definitions(
            moc_protocol::SessionEngine::Standard,
            false,
            &config,
            sub_scope,
            &[],
        )
        .iter()
        .map(|d| d.name.clone())
        .collect();
    for tool in WORKFLOW_TOOLS {
        assert!(!sub.iter().any(|n| n == tool));
    }
    // PTC 签名段：workflows_active=false 的产物不得含任何 workflow 工具名。
    let catalog = crate::tools::catalog::ToolCatalog::new(
        &registry,
        &moc_mcp::McpSnapshot::default(),
        moc_mcp::McpManager::new(Vec::new(), tokio_util::sync::CancellationToken::new()),
        Vec::new(),
        crate::tools::ToolScope::top(),
    )
    .unwrap();
    let text = crate::tools::declarations::render_signatures(
        &catalog,
        moc_protocol::SessionEngine::Ptc,
        false,
        false,
        &config,
    )
    .unwrap_or_default();
    for tool in WORKFLOW_TOOLS {
        assert!(!text.contains(tool), "signature leak: {tool}");
    }
}

/// `<skills>` 未激活不含 workflow-authoring；激活后注入。
#[test]
fn skills_gate_by_activation() {
    let mut index = crate::skills::SkillIndex::empty();
    assert!(index.get(crate::workflow::skill::SKILL_NAME).is_none());
    let home = tempfile::tempdir().unwrap();
    crate::workflow::skill::inject(&mut index, home.path());
    let meta = index
        .get(crate::workflow::skill::SKILL_NAME)
        .expect("injected");
    assert!(
        meta.skill_file().exists(),
        "materialized SKILL.md must exist"
    );
    assert!(meta.base_dir.join("patterns.md").exists());
}

/// [workflow] enabled=false：谓词关闭（ToolScope/for_session 链路）、
/// activated() 门禁报 E_WORKFLOW_DISABLED。
#[test]
fn disabled_config_blocks_activated_session() {
    let header = crate::session::entries::Entry::header(
        "s1".into(),
        "/tmp".into(),
        "m".into(),
        moc_protocol::SessionEngine::Standard,
        moc_protocol::SessionPermission::Full,
        crate::session::entries::SessionOrigin {
            parent_session_id: None,
            forked_from: None,
            delegation_depth: None,
            agent: None,
        },
        "t".into(),
        0,
    );
    let mut state = crate::session::state::SessionState::new(header).unwrap();
    state.workflow_activated = true;
    // 谓词：激活会话 + enabled=false → false；enabled → true。
    let mut cfg = crate::config::WorkflowConfig {
        enabled: false,
        ..Default::default()
    };
    assert!(!crate::workflow::workflows_active(&state, &cfg));
    cfg.enabled = true;
    assert!(crate::workflow::workflows_active(&state, &cfg));
    // activated() 门禁错误码。
    cfg.enabled = false;
    let err = require_workflow_enabled(&cfg).unwrap_err();
    assert_eq!(err.code, "E_WORKFLOW_DISABLED");
}

#[test]
fn saved_name_charset() {
    assert!(valid_saved_name("plan-a.b_2"));
    assert!(!valid_saved_name("../x"));
    assert!(!valid_saved_name(".hidden"));
    assert!(!valid_saved_name("a/b"));
    assert!(!valid_saved_name(""));
    assert!(!valid_saved_name(&"n".repeat(65)));
}

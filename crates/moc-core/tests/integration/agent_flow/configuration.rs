use super::*;

/// Explorer 保留调查工具，包括 shell，同时禁止直接编辑与再次委托。
#[tokio::test]
async fn explorer_role_preserves_reconnaissance_tools() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if is_subagent_request(body) {
            yield_call("call_yield", "EXPLORE_DONE")
        } else {
            text_response("PARENT_OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: parent_id.clone(),
            task: "EXPLORE_TASK".into(),
            title: None,
            role: "explorer".into(),
        })
        .await
        .expect("explore preset spawn 成功")
        .agent
        .id;
    wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(10)).await;

    let requests = upstream.received_requests().await.expect("上游请求");
    let child: Value = requests
        .iter()
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .find(|body| is_subagent_request(body) && body.to_string().contains("EXPLORE_TASK"))
        .expect("子代理请求已记录");
    let tools = tool_names(&child);
    for denied in [
        "edit",
        "write",
        "spawn_agent",
        "stop_agent",
        "wait_agent",
        "plan",
        "exit_plan",
        "clm_context",
        "ask",
    ] {
        assert!(
            !tools.contains(&denied),
            "explore 不得拿到 {denied}：{tools:?}"
        );
    }
    // `web_search` 依赖 provider 能力，harness 未配置——不列入保留面断言。
    for kept in [
        "read", "grep", "find", "bash", "job", "todo", "skill", "execute", "yield",
    ] {
        assert!(tools.contains(&kept), "explore 必须保留 {kept}：{tools:?}");
    }
    for name in ["web_fetch", "list_agents", "message_agent"] {
        assert!(!tools.contains(&name), "低频工具不再常驻：{name}");
        let doc = harness
            .client
            .call::<moc_protocol::ResourceRead>(moc_protocol::ResourceReadParams {
                session_id: child_id.clone(),
                uri: format!("tool://{name}"),
                max_bytes: None,
            })
            .await;
        assert!(doc.is_ok(), "Explorer 的获准工具仍可发现：{name}");
    }

    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id,
        })
        .await
        .expect("list 成功");
    assert_eq!(listed.agents[0].preset.as_deref(), Some("explorer"));

    let info = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: child_id,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("child transcript 可加载")
        .session;
    assert_eq!(info.agent_preset.as_deref(), Some("explorer"));
    harness.close().await;
}

/// 未知 preset 名必须报出候选清单。
#[tokio::test]
async fn unknown_preset_is_rejected_with_available_names() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |_| text_response("PARENT_OK")).await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;

    let error = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: parent_id,
            task: "X".into(),
            title: None,
            role: "no_such_preset".into(),
        })
        .await
        .expect_err("未知 preset 必须失败");
    let message = error.to_string();
    assert!(
        message.contains("explore"),
        "错误必须列出可用 preset：{message}"
    );
    harness.close().await;
}

/// 用户注册的固定 custom 角色仍可按工具策略委托，并受 max_depth 限制。
#[tokio::test]
async fn nested_delegation_respects_max_depth() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        let text = body.to_string();
        let is_child = is_subagent_request(body);
        if is_child && text.contains("GRANDCHILD_TASK") {
            return yield_call("call_yield", "GRAND_DONE");
        }
        if is_child {
            let has_tool_result = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
            if has_tool_result {
                return yield_call("call_yield", "CHILD_DONE");
            }
            return ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![(
                    "call_spawn",
                    "spawn_agent",
                    json!({"task": "GRANDCHILD_TASK", "role": "explorer"}),
                )]),
                "text/event-stream",
            );
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "test/gpt-test:off",
        "[agents]\nmax_depth=2\n",
    )
    .await;
    crate::plugin_flow::install_plugin(
        &harness,
        "delegator",
        r#"
        import { agents } from 'moc';
        agents.registerPreset({ name: 'delegator', description: '用户配置的委托角色',
          systemPrompt: '完成指定委托后交付。', tools: { allow: ['spawn_agent', 'yield'] } });
        export default {};
    "#,
    );
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: parent_id.clone(),
            task: "CHILD_TASK".into(),
            title: None,
            role: "delegator".into(),
        })
        .await
        .unwrap()
        .agent
        .id;

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(15)).await;
    assert!(waited.settled, "子代理完成委托并交付");
    assert_eq!(waited.agents[0].status, AgentStatus::Completed);

    // depth-1 交付时孙代理可能还在跑；等到孙代理终态，保证它的请求已发出。
    let child_listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: child_id.clone(),
        })
        .await
        .expect("子代理可列出自己的后代");
    assert_eq!(child_listed.agents.len(), 1, "子代理名下必须有孙代理");
    if !matches!(
        child_listed.agents[0].status,
        AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Stopped
    ) {
        let grand = wait_children(
            &harness,
            &child_id,
            Some(vec![child_listed.agents[0].id.clone()]),
            Some(10),
        )
        .await;
        assert!(grand.settled, "孙代理必须到达终态");
    }

    let requests = upstream.received_requests().await.expect("上游请求");
    let bodies: Vec<Value> = requests
        .iter()
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .collect();
    // 只看首条普通任务消息，避免把后续 spawn 参数误认成孙代理任务。
    fn task(body: &Value) -> &str {
        body["messages"]
            .as_array()
            .and_then(|messages| messages.iter().find(|message| message["role"] == "user"))
            .and_then(|message| message["content"].as_str())
            .unwrap_or("")
    }
    let depth_one = bodies
        .iter()
        .find(|body| is_subagent_request(body) && task(body) == "CHILD_TASK")
        .expect("depth-1 请求已记录");
    assert!(
        tool_names(depth_one).contains(&"spawn_agent"),
        "max_depth=2 时 depth-1 必须拿到 spawn_agent"
    );
    let depth_two = bodies
        .iter()
        .find(|body| is_subagent_request(body) && task(body) == "GRANDCHILD_TASK")
        .expect("depth-2 请求已记录");
    assert!(
        !tool_names(depth_two).contains(&"spawn_agent"),
        "depth-2 已到委托上限，不得再派发"
    );
    harness.close().await;
}

/// sidekick `mode="retry"`：上游失败进入 Failed 后，用户在模型选择器里把
/// sidekick 换成别的模型，retry 必须先把会话切到新模型再重放——重跑请求落在
/// 新模型上、会话头已切换、不追加新的 handoff 消息，结果 delivery=retried。
#[tokio::test]
async fn fusion_sidekick_retry_runs_on_the_new_sidekick_model() {
    let upstream = MockServer::builder().start().await;
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    mount_router(&upstream, move |body| {
        recorded.lock().push(body.clone());
        // gpt-cheap 上游直接 500；换型后重跑走 gpt-alt。
        if body["model"] == "gpt-cheap" {
            return ResponseTemplate::new(500).set_body_string("upstream boom");
        }
        if body["model"] == "gpt-alt" {
            return sidekick_yield_call("RETRIED_REPORT");
        }
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last());
        if last.is_some_and(|message| message["role"] == "tool") {
            // 失败的 handoff 结果回来就收尾；retry 结果回来也收尾。
            return text_response("LEAD_DONE");
        }
        // 首个用户提示发 handoff，第二个提示发 retry。
        let calls = recorded
            .lock()
            .iter()
            .filter(|request| request["model"] == "gpt-test")
            .count();
        if calls <= 2 {
            ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![(
                    "call_sk",
                    "sidekick",
                    json!({"message": "HANDOFF_ONE"}),
                )]),
                "text/event-stream",
            )
        } else {
            ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![("call_retry", "sidekick", json!({"mode": "retry"}))]),
                "text/event-stream",
            )
        }
    })
    .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "fusion",
        "fusion",
        "[providers.test.models.gpt-cheap]\ncontext_window=1000000\nmax_output_tokens=4096\n\
         [providers.test.models.gpt-alt]\ncontext_window=1000000\nmax_output_tokens=4096\n\
         [models.fusion]\nlead='test/gpt-test'\nsidekick='test/gpt-cheap'\n",
    )
    .await;
    let lead_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    // 首轮 handoff：sidekick 上游 500 → 注册表 Failed，lead 拿到 failed。
    assert_eq!(
        harness.prompt(&lead_id, "开工").await.message.text(),
        "LEAD_DONE"
    );
    let agents = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: lead_id.clone(),
        })
        .await
        .expect("代理列表");
    let sidekick = agents
        .agents
        .iter()
        .find(|agent| agent.preset.as_deref() == Some("sidekick"))
        .expect("sidekick 会话存在");
    assert_eq!(sidekick.status, AgentStatus::Failed, "{:?}", agents.agents);

    // 用户在模型选择器里换 sidekick 模型。
    harness
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: lead_id.clone(),
            selector: "fusion:test/gpt-test+test/gpt-alt".into(),
            overflow: SessionModelOverflowAction::Switch,
        })
        .await
        .expect("set_model 成功");
    assert_eq!(
        harness.prompt(&lead_id, "再试一次").await.message.text(),
        "LEAD_DONE"
    );

    // retry 请求打到新模型；sidekick 会话头已切换。
    let bodies = bodies.lock().clone();
    assert!(
        bodies.iter().any(|body| body["model"] == "gpt-alt"),
        "retry 应落在新模型上：{bodies:?}"
    );
    let entries = transcript_entries(&harness, &sidekick.id).await;
    assert!(
        entries.iter().any(|entry| matches!(
            entry,
            moc_core::session::entries::Entry::ModelChange { model, .. }
                if model == "test/gpt-alt:off"
        )),
        "sidekick 会话应记录模型切换"
    );
    let handoff_users = entries
        .iter()
        .filter_map(|entry| match entry {
            moc_core::session::entries::Entry::Message { message, .. } => Some(message),
            _ => None,
        })
        .filter(|message| message.role == Role::User && message.text().contains("HANDOFF_ONE"))
        .count();
    assert_eq!(handoff_users, 1, "retry 不追加重复的 handoff 消息");

    let loaded = harness.load(&lead_id).await;
    let results: Vec<Value> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .filter_map(|result| serde_json::from_str::<Value>(&result.content).ok())
        .filter(|value| value["delivery"].is_string())
        .collect();
    let retry = results
        .iter()
        .find(|value| value["delivery"] == "retried")
        .expect("应有一条 retried 结果");
    assert_eq!(retry["status"], "completed");
    assert_eq!(retry["report"], "RETRIED_REPORT");
    assert_eq!(retry["model"], "test/gpt-alt:off");
    assert_eq!(retry["result"]["status"], "done");
    assert_eq!(results[0]["status"], "failed", "首轮 handoff 是 failed");
    harness.close().await;
}

/// 后续消息中仍可用 shell 调查，直接和 PTC 嵌套 write 则保持禁用。
#[tokio::test]
async fn explorer_followup_preserves_shell_and_denies_write_tools() {
    assert!(support::require_moc_bin("Explorer 工具隔离"));
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if !is_subagent_request(body) {
            return text_response("PARENT_OK");
        }
        let text = body.to_string();
        if !text.contains("FOLLOWUP_INSPECT") || text.contains("attempt_write") {
            return yield_call("yield_done", "READ_ONLY_REPORT");
        }
        ResponseTemplate::new(200).set_body_raw(support::tool_stream(vec![
            ("attempt_write", "write", json!({"path":"protected.txt", "content":"direct"})),
            ("inspect_shell", "bash", json!({"description":"验证 shell 调查能力", "command":"printf 'shell-inspection-ok'"})),
            ("attempt_nested", "execute", json!({"description":"尝试嵌套写入", "code":"await tools.write({path:'protected.txt',content:'nested'});"})),
        ]), "text/event-stream")
    }).await;
    let harness = harness(&upstream).await;
    let path = harness.root.path().join("work/protected.txt");
    std::fs::write(&path, "original").unwrap();
    let parent = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child = spawn_child(&harness, &parent, "只读调查").await;
    wait_children(&harness, &parent, Some(vec![child.clone()]), Some(10)).await;
    message_agent(
        &harness,
        &parent,
        &child,
        "FOLLOWUP_INSPECT：继续调查 protected.txt",
    )
    .await
    .unwrap();
    let completed = wait_children(&harness, &parent, Some(vec![child.clone()]), Some(10)).await;
    assert!(completed.settled);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    let loaded = harness.load(&child).await;
    let shell = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|result| result.tool_call_id == "inspect_shell")
        .expect("shell 调查结果");
    assert!(!shell.is_error, "{}", shell.content);
    assert!(shell.content.contains("shell-inspection-ok"));
    for id in ["attempt_write", "attempt_nested"] {
        let result = loaded
            .entries
            .iter()
            .flat_map(|entry| entry.message.tool_results())
            .find(|result| result.tool_call_id == id)
            .expect("越权调用必须返回失败结果");
        assert!(result.is_error, "{id} 不得执行成功：{}", result.content);
    }
    harness.close().await;
}

/// Explorer 关闭不影响用户启用的 custom；创建模型只能来自角色定义。
#[tokio::test]
async fn custom_role_uses_configured_model_when_explorer_disabled() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if is_subagent_request(body) {
            yield_call("custom_done", "CUSTOM_REPORT")
        } else {
            text_response("PARENT_OK")
        }
    })
    .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[providers.test.models.gpt-alt]\ncontext_window=1000000\nmax_output_tokens=4096\n",
    )
    .await;
    let config_dir = harness.root.path().join("work/.moc");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[models.roles]\nexplore='none'\n",
    )
    .unwrap();
    crate::plugin_flow::install_plugin(
        &harness,
        "fixed-custom",
        r#"
        import { agents } from 'moc';
        agents.registerPreset({ name: 'reader', description: '固定模型的只读角色', model: 'test/gpt-alt',
          systemPrompt: '只读调查，完成后交付。', tools: {allow: ['read']} });
        agents.registerPreset({ name: 'disabled', description: '停用的角色', model: 'none', systemPrompt: '停用' });
        agents.registerPreset({ name: 'explorer', description: '不能覆盖内置角色', systemPrompt: '不得生效' });
        export default {};
    "#,
    );
    let parent = harness.session(moc_protocol::SessionEngine::Standard).await;
    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        listed
            .roles
            .iter()
            .map(|role| role.name.as_str())
            .collect::<Vec<_>>(),
        ["reader"]
    );
    let child = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: parent.clone(),
            task: "调查入口".into(),
            title: None,
            role: "reader".into(),
        })
        .await
        .unwrap()
        .agent
        .id;
    let completed = wait_children(&harness, &parent, Some(vec![child]), Some(10)).await;
    assert_eq!(
        completed.agents[0].message.as_ref().unwrap().text(),
        "CUSTOM_REPORT"
    );
    let bodies: Vec<Value> = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    let body = bodies
        .iter()
        .find(|body| is_subagent_request(body))
        .expect("custom 请求");
    assert_eq!(body["model"], "gpt-alt");
    let names: Vec<_> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["read", "yield"]);
    harness.close().await;
}

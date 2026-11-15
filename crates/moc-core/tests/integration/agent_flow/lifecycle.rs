use super::*;

#[tokio::test]
async fn spawn_wait_returns_yield_report() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("CHILD_TASK") {
            yield_call("call_yield", "CHILD_REPORT")
        } else {
            text_response("PARENT_OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;

    let waited = wait_children(&harness, &parent_id, None, Some(5)).await;
    assert!(waited.settled, "子代理已交付，等待必须立即回报");
    assert!(!waited.timed_out);
    assert_eq!(waited.agents.len(), 1);
    assert_eq!(waited.agents[0].id, child_id);
    assert_eq!(waited.agents[0].status, AgentStatus::Completed);
    assert_eq!(
        waited.agents[0].message.as_ref().expect("交付物").text(),
        "CHILD_REPORT",
        "yield 报告即权威结果"
    );

    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id.clone(),
        })
        .await
        .expect("list 成功");
    assert_eq!(listed.agents.len(), 1);
    assert_eq!(listed.agents[0].status, AgentStatus::Completed);

    let child = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: child_id,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("child transcript 可加载");
    assert_eq!(child.entries[0].message.text(), "CHILD_TASK");
    assert!(
        child
            .entries
            .iter()
            .any(|entry| entry.message.tool_calls().any(|call| call.name == "yield")),
        "子代理必须通过 yield 交付"
    );
    harness.close().await;
}

#[tokio::test]
async fn failed_child_agent_reports_its_error_to_the_parent() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("CHILD_TASK") {
            return ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": {"message": "上游拒绝子代理", "code": "boom"}
            }));
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id]), Some(10)).await;
    assert!(waited.settled);
    assert_eq!(waited.agents[0].status, AgentStatus::Failed);
    let message = waited.agents[0]
        .message
        .as_ref()
        .expect("失败消息必须对父会话可见");
    assert!(
        message.text().contains("Subagent prompt failed"),
        "失败消息应说明原因：{}",
        message.text()
    );

    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id,
        })
        .await
        .expect("list 成功");
    assert!(
        listed.agents[0]
            .last_message
            .as_deref()
            .is_some_and(|text| text.contains("Subagent prompt failed")),
        "列表必须带出失败原因：{:?}",
        listed.agents[0].last_message
    );
    harness.close().await;
}

#[tokio::test]
async fn completed_child_result_lands_in_parent_notices() {
    let upstream = MockServer::builder().start().await;
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    mount_router(&upstream, move |body| {
        if body.to_string().contains("CHILD_TASK") {
            return yield_call("call_yield", "CHILD_REPORT");
        }
        recorded.lock().push(body.clone());
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;
    let waited = wait_children(&harness, &parent_id, Some(vec![child_id]), Some(10)).await;
    assert!(waited.settled);
    assert!(bodies.lock().is_empty(), "子代理结果不得进入父会话请求");

    // 子代理完成后父会话再跑一轮：结果必须以 notice 形式进入父的上下文。
    harness.prompt(&parent_id, "PARENT_AFTER").await;
    let bodies = bodies.lock().clone();
    let body = bodies.last().expect("父会话至少发出一次请求").to_string();
    assert!(
        body.contains("Subagent ") && body.contains("Result preview"),
        "父上下文必须包含子代理完成提示：{body}"
    );
    assert!(body.contains("CHILD_REPORT"), "提示必须带结果预览");
    harness.close().await;
}

/// `tasks[]` 批量 spawn：共享 `context` 前缀拼到每个任务上；单项失败不拖垮整批。
#[tokio::test]
async fn batch_spawn_creates_agents_and_reports_per_item_errors() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if is_subagent_request(body) {
            return yield_call("call_yield", "BATCH_DONE");
        }
        let has_tool_result = body["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
        if has_tool_result {
            return text_response("PARENT_DONE");
        }
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![(
                "call_spawn",
                "spawn_agent",
                json!({
                    "context": "SHARED_CONTEXT",
                    "tasks": [
                        {"task": "TASK_ONE", "role": "explorer"},
                        {"task": "TASK_TWO", "title": "second", "role": "explorer"},
                        {"task": "", "role": "explorer"}
                    ]
                }),
            )]),
            "text/event-stream",
        )
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    harness.prompt(&parent_id, "开批量子代理").await;

    let loaded = harness.load(&parent_id).await;
    let spawn_result = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|result| result.content.contains("agents"))
        .expect("批量 spawn 结果已落盘");
    let result_json: Value = serde_json::from_str(&spawn_result.content).expect("批量结果为 JSON");
    assert_eq!(
        result_json["agents"].as_array().map(Vec::len),
        Some(2),
        "两条有效任务必须各建一个代理：{result_json}"
    );
    let errors = result_json["errors"].as_array().expect("errors 数组");
    assert_eq!(errors.len(), 1, "空任务必须记为单项错误：{result_json}");
    assert_eq!(errors[0]["index"], 2);

    // wait 在首个 settle 即返回；list 与 wait 交替到两个都终态，保证第二个
    // 子代理的请求已发出。已全部终态时 AgentWait 无目标会报参数错，直接看名册。
    let listed = {
        let mut listed = harness
            .client
            .call::<AgentList>(AgentListParams {
                session_id: parent_id.clone(),
            })
            .await
            .expect("list 成功");
        for _ in 0..8 {
            if listed.agents.iter().all(|agent| {
                matches!(
                    agent.status,
                    AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Stopped
                )
            }) {
                break;
            }
            let _ = wait_children(&harness, &parent_id, None, Some(10)).await;
            listed = harness
                .client
                .call::<AgentList>(AgentListParams {
                    session_id: parent_id.clone(),
                })
                .await
                .expect("list 成功");
        }
        listed
    };
    assert_eq!(listed.agents.len(), 2, "批量必须建起两个代理");
    assert!(
        listed.agents.iter().all(|agent| matches!(
            agent.status,
            AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Stopped
        )),
        "批量子代理必须全部到达终态：{:?}",
        listed.agents
    );

    let requests = upstream.received_requests().await.expect("上游请求");
    let shared_count = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| is_subagent_request(body) && body.to_string().contains("SHARED_CONTEXT"))
        .count();
    assert_eq!(shared_count, 2, "共享 context 必须拼进每个子任务");
    harness.close().await;
}

/// `task` 与 `tasks` 互斥，批量不得超过 16 项。
#[test]
fn spawn_schema_rejects_task_task_conflict_and_batch_bounds() {
    use moc_core::tools::{Tool, agent::SpawnAgentTool};
    let validator = jsonschema::validator_for(&SpawnAgentTool.def().parameters)
        .expect("spawn_agent schema 必须是合法 JSON Schema");
    for args in [
        json!({}),
        json!({"task": "a"}),
        json!({"task": "a", "role": "explorer", "model": "inherit"}),
        json!({"task": "a", "role": "explorer", "tasks": [{"task": "b", "role": "explorer"}]}),
        json!({"tasks": []}),
        json!({"tasks": vec![json!({"task":"a", "role":"explorer"}); 17]}),
        json!({"tasks": [{"title": "no task"}]}),
    ] {
        let errors: Vec<String> = validator
            .iter_errors(&args)
            .map(|error| error.to_string())
            .collect();
        assert!(!errors.is_empty(), "{args} 必须被拒：{errors:?}");
    }
    for args in [
        json!({"task": "a", "role": "explorer"}),
        json!({"tasks": [{"task": "a", "role": "explorer"}]}),
        json!({"tasks": [{"task": "a", "role": "explorer"}, {"task": "b", "role": "explorer"}],
               "context": "shared"}),
    ] {
        assert!(
            validator.is_valid(&args),
            "{args} 必须合法：{:?}",
            validator.iter_errors(&args).collect::<Vec<_>>()
        );
    }
}

/// 回退截断 spawn 记录后子代理成孤儿：运行中被停杀、从名册与 wait 目标消失、
/// 标记落盘持久化；redo 接回 spawn 引用后重新可见，但已停的代理不复活。
#[tokio::test]
async fn rewinding_past_a_spawn_orphans_stops_and_hides_the_child() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        // 子代理判定必须在任务名匹配之前：父会话第二轮请求的工具结果里也回显
        // 任务文本，先匹配任务名会把父请求错路由到延迟分支。
        if is_subagent_request(body) && body.to_string().contains("SLOW_CHILD_TASK") {
            // 子代理挂在 bash 长任务上：回退发生时它仍在运行，孤儿停杀由
            // 同一关停路径回收进程。
            return ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![(
                    "call_hold",
                    "bash",
                    json!({"description":"hold","command":"sleep 300","timeout_secs":3600}),
                )]),
                "text/event-stream",
            );
        }
        if is_subagent_request(body) {
            return yield_call("call_yield", "CHILD_DONE");
        }
        let has_tool_result = body["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
        if has_tool_result {
            return text_response("PARENT_DONE");
        }
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![(
                "call_spawn",
                "spawn_agent",
                json!({"tasks": [{"task": "SLOW_CHILD_TASK", "role": "explorer"}]}),
            )]),
            "text/event-stream",
        )
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    harness.prompt(&parent_id, "开一个子代理").await;

    let loaded = harness.load(&parent_id).await;
    let spawn_result = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|result| result.content.contains("agent_id"))
        .expect("spawn 结果已落盘");
    let result_json: Value =
        serde_json::from_str(&spawn_result.content).expect("spawn 结果为 JSON");
    let child_id = result_json["agents"][0]["agent_id"]
        .as_str()
        .expect("结果带 agent_id")
        .to_owned();
    let user_entry_id = loaded
        .entries
        .iter()
        .find(|entry| entry.message.role == moc_protocol::Role::User && entry.agent.is_none())
        .expect("发起 spawn 的用户提示")
        .id
        .clone();
    let tip_entry_id = loaded.entries.last().expect("转录非空").id.clone();

    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id.clone(),
        })
        .await
        .expect("list 成功");
    assert_eq!(listed.agents.len(), 1, "回退前子代理在册");
    assert_eq!(listed.agents[0].id, child_id);

    // 回退到发起 spawn 的那条用户消息：消息本身回到输入框，其后整段被截走。
    harness
        .client
        .call::<SessionRewind>(SessionRewindParams {
            omit_auxiliary: false,
            session_id: parent_id.clone(),
            entry_id: user_entry_id,
            max_entries: None,
        })
        .await
        .expect("回退成功");

    // 名册与 wait 都不再认这个子代理；停杀状态即时推送。
    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id.clone(),
        })
        .await
        .expect("list 成功");
    assert!(listed.agents.is_empty(), "孤儿子代理必须退出名册");
    let waited = harness
        .client
        .call::<AgentWait>(AgentWaitParams {
            session_id: parent_id.clone(),
            agent_ids: Some(vec![child_id.clone()]),
            timeout_secs: None,
        })
        .await;
    assert!(waited.is_err(), "孤儿必须不是可等待目标：{waited:?}");
    let stopped_update = harness.updates.lock().iter().any(|update| {
        matches!(
            update,
            moc_protocol::methods::SessionUpdate::AgentStatusChanged {
                agent_id,
                status,
                ..
            } if *agent_id == child_id && *status == AgentStatus::Stopped
        )
    });
    assert!(stopped_update, "孤儿停杀必须推送状态变化");

    // 孤儿标记随截断落盘到父会话 sidecar。
    let store = moc_core::session::store::SessionStore::new(harness.root.path().join("sessions"));
    let paths = store
        .find(&parent_id)
        .await
        .expect("查父会话")
        .expect("父会话已落盘");
    let marks_file = paths.sidecar.join("orphaned_agents");
    let marks = std::fs::read_to_string(&marks_file).expect("孤儿标记文件已写盘");
    assert!(
        marks.lines().any(|line| line == child_id),
        "标记文件必须含孤儿 id：{marks}"
    );

    // redo 把 spawn 引用接回：名册恢复可见、标记清空；已停的代理不复活。
    harness
        .client
        .call::<SessionRedo>(SessionRedoParams {
            omit_auxiliary: false,
            session_id: parent_id.clone(),
            entry_id: tip_entry_id,
            max_entries: None,
        })
        .await
        .expect("前滚成功");
    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent_id.clone(),
        })
        .await
        .expect("list 成功");
    assert_eq!(listed.agents.len(), 1, "redo 后子代理回到名册");
    assert_eq!(listed.agents[0].id, child_id);
    assert_eq!(
        listed.agents[0].status,
        AgentStatus::Stopped,
        "停杀不随前滚复活"
    );
    assert!(!marks_file.exists(), "redo 必须清空孤儿标记文件");
    harness.close().await;
}

/// 用户在后台（block=false）handoff 进行中取消 lead 的提示：sidekick 必须被
/// 一并停掉，注册表状态落定 stopped。
#[tokio::test]
async fn fusion_cancel_stops_a_background_sidekick() {
    let upstream = MockServer::builder().start().await;
    // 门控掐住 sidekick 的上游响应：wiremock 服务端被冻住的期间 lead 的后续
    // 请求排不进处理，提示天然保持在飞，恰好覆盖「进行中取消」。
    let gate = support::Gate::new();
    let held = gate.clone();
    mount_router(&upstream, move |body| {
        if body["model"] == "gpt-cheap" {
            held.wait();
            return sidekick_yield_call("SK_REPORT");
        }
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last());
        if last.is_some_and(|message| message["role"] == "tool") {
            return text_response("LEAD_DONE");
        }
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![(
                "call_sk",
                "sidekick",
                json!({"message": "HANDOFF", "block": false}),
            )]),
            "text/event-stream",
        )
    })
    .await;
    let harness = fusion_harness(&upstream).await;
    let lead_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let mut events = harness.events.subscribe();
    let client = harness.client.clone();
    let session = lead_id.clone();
    let prompt = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: session,
                input: vec![InputPart::Text {
                    text: "开工".into(),
                }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await
    });
    // 等 sidekick 注册出来再取消；AgentStarted 事件是确定信号。
    let sidekick_id = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let moc_protocol::methods::SessionUpdate::AgentStarted { agent } =
                events.recv().await.expect("事件流开启")
            {
                if agent.preset.as_deref() == Some("sidekick") {
                    break agent.id;
                }
            }
        }
    })
    .await
    .expect("sidekick 必须注册");
    let cancelled = harness
        .client
        .call::<SessionCancel>(SessionCancelParams {
            session_id: lead_id.clone(),
        })
        .await
        .expect("取消成功");
    assert!(cancelled.cancelled, "提示应确实被取消");
    let _ = prompt.await;

    // 注册表里的 sidekick 状态必须落定 stopped，状态推送是确定信号。
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let moc_protocol::methods::SessionUpdate::AgentStatusChanged {
                agent_id,
                status,
                ..
            } = events.recv().await.expect("事件流开启")
            {
                if agent_id == sidekick_id && status == AgentStatus::Stopped {
                    break;
                }
            }
        }
    })
    .await
    .expect("后台 sidekick 必须被停掉");
    gate.open();
    harness.close().await;
}

/// Explorer 禁用后，工具目录、角色目录和直接 RPC 必须同时关闭创建入口。
#[tokio::test]
async fn disabled_explorer_has_no_spawnable_roles() {
    use moc_protocol::methods::{SessionTools, SessionToolsParams};
    let upstream = MockServer::builder().start().await;
    let harness = harness(&upstream).await;
    let config_dir = harness.root.path().join("work/.moc");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[models.roles]\nexplore='none'\n",
    )
    .unwrap();
    for engine in [
        moc_protocol::SessionEngine::Standard,
        moc_protocol::SessionEngine::Ptc,
    ] {
        let parent = harness.session(engine).await;
        let listed = harness
            .client
            .call::<AgentList>(AgentListParams {
                session_id: parent.clone(),
            })
            .await
            .unwrap();
        assert!(listed.roles.is_empty());
        let tools = harness
            .client
            .call::<SessionTools>(SessionToolsParams {
                session_id: parent.clone(),
            })
            .await
            .unwrap();
        assert!(!tools.iter().any(|tool| tool.name == "spawn_agent"));
        for role in ["explorer", "sidekick", "default"] {
            let error = harness
                .client
                .call::<AgentSpawn>(AgentSpawnParams {
                    session_id: parent.clone(),
                    task: "调查".into(),
                    title: None,
                    role: role.into(),
                })
                .await
                .expect_err("未启用角色不可绕过工具目录直接创建");
            assert!(error.to_string().contains("INVALID_PARAMS"));
        }
        assert!(
            harness
                .client
                .call::<AgentList>(AgentListParams { session_id: parent })
                .await
                .unwrap()
                .agents
                .is_empty()
        );
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
    harness.close().await;
}

use super::*;

#[tokio::test]
async fn unknown_target_and_self_target_are_rejected() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |_| text_response("PARENT_OK")).await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;

    let unknown = message_agent(&harness, &parent_id, "ses_unknown0001", "hi")
        .await
        .expect_err("未知目标必须失败");
    assert!(
        unknown.to_string().contains("INVALID_PARAMS"),
        "未知目标错误：{unknown}"
    );
    let itself = message_agent(&harness, &parent_id, &parent_id, "hi")
        .await
        .expect_err("自投递必须失败");
    assert!(
        itself.to_string().contains("INVALID_PARAMS"),
        "自投递错误：{itself}"
    );
    let no_parent = message_agent(&harness, &parent_id, "parent", "hi")
        .await
        .expect_err("顶层会话没有父代理");
    assert!(
        no_parent.to_string().contains("INVALID_PARAMS"),
        "顶层 parent 目标错误：{no_parent}"
    );
    harness.close().await;
}

/// 回退是原地截断：不产生新会话、不建立父链接，会话身份与普通顶层会话完全一致。
///
/// 回退后继续提问，系统提示与工具表仍是顶层会话那一套——没有子代理契约、没有
/// `yield`、没有代理名册。
#[tokio::test]
async fn rewind_stays_in_place_and_keeps_plain_session_shape() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |_body| text_response("收到")).await;
    let harness = harness(&upstream).await;
    let source = harness.session(moc_protocol::SessionEngine::Standard).await;
    harness.prompt(&source, "第一轮").await;

    let entries = harness.load(&source).await.entries;
    let target = entries
        .iter()
        .find(|entry| entry.message.role == moc_protocol::Role::User && entry.agent.is_none())
        .expect("回退目标为首条用户消息");
    let rewound = harness
        .client
        .call::<SessionRewind>(SessionRewindParams {
            omit_auxiliary: false,
            session_id: source.clone(),
            entry_id: target.id.clone(),
            max_entries: None,
        })
        .await
        .expect("回退成功")
        .session;
    assert_eq!(rewound.id, source, "回退必须留在原会话");
    assert!(rewound.forked_from.is_none());
    assert!(
        !rewound.is_subagent(),
        "回退不得建立父链接：{:?}",
        rewound.parent_session_id
    );

    harness.prompt(&source, "回退后的第一轮").await;
    let requests = upstream.received_requests().await.expect("上游请求");
    assert_eq!(requests.len(), 2, "两轮提示各一条请求");
    let plain: Value = serde_json::from_slice(&requests[0].body).expect("普通会话请求");
    let rewound_req: Value = serde_json::from_slice(&requests[1].body).expect("回退后请求");
    let tools = tool_names(&rewound_req);
    assert_eq!(
        rewound_req["messages"][0], plain["messages"][0],
        "回退后的系统提示必须与普通会话一致"
    );
    assert_eq!(
        tools,
        tool_names(&plain),
        "回退后的工具表必须与普通会话一致"
    );
    assert!(
        !tools.contains(&"yield"),
        "回退会话不得拿到 yield：{tools:?}"
    );
    let rendered = rewound_req.to_string();
    assert!(
        !rendered.contains("<subagent>") && !rendered.contains("<agent-roster>"),
        "回退会话不得携带子代理契约或名册：{rewound_req}"
    );
    harness.close().await;
}

/// 会话身份只由会话头决定，与请求来自谁无关：用户聚焦子代理会话直接提问时，
/// 系统提示与工具表仍必须是子代理那一套——有 `yield`、没有派发工具。
#[tokio::test]
async fn focused_child_session_keeps_subagent_contract_for_user_prompts() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("CHILD_TASK") {
            yield_call("call_yield", "CHILD_REPORT")
        } else {
            text_response("OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;
    wait_children(&harness, &parent_id, None, Some(5)).await;

    harness.prompt(&child_id, "用户直接跟进").await;
    let requests = upstream.received_requests().await.expect("上游请求");
    let body: Value =
        serde_json::from_slice(&requests.last().expect("最后一次请求").body).expect("请求 JSON");
    let system = body["messages"][0]["content"].as_str().expect("系统提示");
    assert!(system.contains("<subagent>"), "{system}");
    let tools = tool_names(&body);
    assert!(
        tools.contains(&"yield"),
        "子代理会话必须拿到 yield：{tools:?}"
    );
    assert!(
        !tools.contains(&"spawn_agent"),
        "子代理不得拿到派发工具：{tools:?}"
    );
    harness.close().await;
}

/// 压缩摘要请求复用子代理的固定角色系统前缀，避免缓存前缀分叉。
#[tokio::test]
async fn subagent_compaction_reuses_the_subagent_system_prefix() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body["stream"] != true {
            // 摘要内容不合格时 soft 会重试并最终走确定性兜底，压缩仍然成立。
            return ResponseTemplate::new(200).set_body_json(support::complete("压缩小结"));
        }
        if body.to_string().contains("子代理继续") {
            text_response("好的")
        } else {
            yield_call("call_yield", "CHILD_REPORT")
        }
    })
    .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "test/gpt-test:off",
        "[compaction]\nmethod='moc'\nauto=false\nkeep_recent_tokens=0\n",
    )
    .await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "CHILD_TASK").await;
    wait_children(&harness, &parent_id, None, Some(5)).await;
    harness.prompt(&child_id, "子代理继续").await;

    harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: child_id.clone(),
            instructions: Some("保留子代理任务状态".into()),
        })
        .await
        .expect("子代理会话必须能压缩");

    let requests = upstream.received_requests().await.expect("上游请求");
    let bodies: Vec<Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("请求 JSON"))
        .collect();
    let plain = bodies
        .iter()
        .rev()
        .find(|body| body["stream"] == true)
        .expect("子代理普通请求");
    let compaction = bodies
        .iter()
        .find(|body| body["stream"] != true)
        .expect("压缩摘要请求");

    let system = plain["messages"][0]["content"]
        .as_str()
        .expect("系统提示为字符串");
    assert!(system.contains("<subagent>"), "{system}");
    assert_eq!(
        compaction["messages"][0], plain["messages"][0],
        "压缩请求必须逐字节复用子代理系统提示"
    );
    assert_eq!(
        compaction["tools"], plain["tools"],
        "压缩请求必须携带同一份工具表"
    );
    harness.close().await;
}

/// Fusion lead：系统提示带 `<fusion>` 契约与 `sidekick` 工具；两次 handoff 走
/// spawned/woken 且复用同一个执行侧会话（上下文延续）；阻塞报告直达工具结果、
/// 不再回投完成通知；普通子代理继承的是 lead 模型而非组合 selector。
#[tokio::test]
async fn fusion_lead_hands_off_to_a_persistent_sidekick() {
    let upstream = MockServer::builder().start().await;
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    let handoffs = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&handoffs);
    mount_router(&upstream, move |body| {
        recorded.lock().push(body.clone());
        if body["model"] == "gpt-cheap" {
            // sidekick：唤醒后上下文里已带着首次 handoff。
            let report = if body.to_string().contains("HANDOFF_TWO") {
                "SIDEKICK_REPORT_2"
            } else {
                "SIDEKICK_REPORT"
            };
            return delayed(sidekick_yield_call(report), 50);
        }
        if is_subagent_request(body) {
            return yield_call("call_yield", "CHILD_DONE");
        }
        // lead：末条是工具结果就收尾，否则发起第 n 次 handoff。
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last());
        if last.is_some_and(|message| message["role"] == "tool") {
            return text_response("LEAD_DONE");
        }
        let index = counter.fetch_add(1, Ordering::SeqCst);
        let call = format!("call_sk{index}");
        let message = if index == 0 {
            "HANDOFF_ONE"
        } else {
            "HANDOFF_TWO"
        };
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![(
                call.as_str(),
                "sidekick",
                json!({"message": message}),
            )]),
            "text/event-stream",
        )
    })
    .await;
    let harness = fusion_harness(&upstream).await;
    let lead_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    assert_eq!(
        harness.prompt(&lead_id, "开工").await.message.text(),
        "LEAD_DONE"
    );
    assert_eq!(
        harness.prompt(&lead_id, "继续").await.message.text(),
        "LEAD_DONE"
    );

    let bodies = bodies.lock().clone();
    let lead_reqs: Vec<&Value> = bodies
        .iter()
        .filter(|body| body["model"] == "gpt-test" && !is_subagent_request(body))
        .collect();
    assert!(lead_reqs.len() >= 4, "两轮提示各两条 lead 请求");
    let system = lead_reqs[0]["messages"][0]["content"]
        .as_str()
        .expect("系统提示");
    assert!(system.contains("<fusion>"), "{system}");
    assert!(
        tool_names(lead_reqs[0]).contains(&"sidekick"),
        "lead 工具表必须含 sidekick"
    );
    // 阻塞 handoff 的报告走工具结果直达；后续请求不得再出现完成通知。
    for body in &lead_reqs[1..] {
        assert!(
            !body.to_string().contains("Subagent \"Sidekick\""),
            "完成通知不得重复投递：{body}"
        );
    }

    let sidekick_reqs: Vec<&Value> = bodies
        .iter()
        .filter(|body| body["model"] == "gpt-cheap")
        .collect();
    assert_eq!(sidekick_reqs.len(), 2, "两次 handoff 各一轮 sidekick 请求");
    let sidekick_tools = tool_names(sidekick_reqs[0]);
    for denied in ["ask", "sidekick", "spawn_agent", "stop_agent", "wait_agent"] {
        assert!(
            !sidekick_tools.contains(&denied),
            "sidekick 不得拿到 {denied}：{sidekick_tools:?}"
        );
    }
    for kept in ["yield", "execute", "bash", "edit"] {
        assert!(
            sidekick_tools.contains(&kept),
            "sidekick 必须保留 {kept}：{sidekick_tools:?}"
        );
    }
    let sidekick_system = sidekick_reqs[0]["messages"][0]["content"]
        .as_str()
        .expect("sidekick 系统提示");
    assert!(sidekick_system.contains("<subagent>"), "{sidekick_system}");
    assert!(
        sidekick_system.contains("Preset: sidekick"),
        "{sidekick_system}"
    );
    assert!(
        !sidekick_system.contains("<fusion>"),
        "sidekick 不得拿到 lead 契约：{sidekick_system}"
    );
    let woken = sidekick_reqs[1].to_string();
    assert!(woken.contains("HANDOFF_ONE"), "上下文必须跨 handoff 延续");
    assert!(woken.contains("HANDOFF_TWO"), "唤醒消息必须进入上下文");

    let loaded = harness.load(&lead_id).await;
    let handoff_results: Vec<Value> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .filter_map(|result| serde_json::from_str::<Value>(&result.content).ok())
        .filter(|value| value["delivery"].is_string())
        .collect();
    assert_eq!(handoff_results.len(), 2, "两次 handoff 的结果");
    assert_eq!(handoff_results[0]["delivery"], "spawned");
    assert_eq!(handoff_results[0]["status"], "completed");
    assert_eq!(handoff_results[0]["report"], "SIDEKICK_REPORT");
    assert_eq!(handoff_results[1]["delivery"], "woken");
    assert_eq!(handoff_results[1]["report"], "SIDEKICK_REPORT_2");
    assert_eq!(
        handoff_results[0]["agent_id"], handoff_results[1]["agent_id"],
        "同一边 kick 会话必须复用"
    );
    let sidekick_id = handoff_results[0]["agent_id"].as_str().unwrap();
    let documentation = harness
        .client
        .call::<moc_protocol::ResourceRead>(moc_protocol::ResourceReadParams {
            session_id: sidekick_id.to_owned(),
            uri: "tool://message_agent".into(),
            max_bytes: None,
        })
        .await
        .expect("sidekick 仍可发现消息工具");
    assert!(documentation.content.unwrap().contains("message_agent"));
    assert!(
        harness
            .client
            .call::<moc_protocol::ResourceRead>(moc_protocol::ResourceReadParams {
                session_id: sidekick_id.to_owned(),
                uri: "tool://spawn_agent".into(),
                max_bytes: None,
            })
            .await
            .is_err(),
        "按需读取不能绕过 sidekick 的禁止再委派策略"
    );

    // 普通子代理 inherit 的是 lead 的真实 selector，不是组合写法。
    let spawned = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: lead_id.clone(),
            task: "CHILD_TASK".into(),
            title: None,
            role: "explorer".into(),
        })
        .await
        .expect("spawn 成功");
    assert_eq!(spawned.agent.model, "test/gpt-test:off");
    wait_children(&harness, &lead_id, Some(vec![spawned.agent.id]), Some(10)).await;

    // session/usage 聚合整棵会话树：自身 + sidekick + 普通子代理各占一行。
    let usage = harness
        .client
        .call::<SessionUsage>(SessionUsageParams {
            session_id: lead_id.clone(),
        })
        .await
        .expect("usage 查询成功");
    let find = |source: UsageSource, model: &str| {
        usage
            .models
            .iter()
            .find(|summary| summary.source == source && summary.model == model)
            .unwrap_or_else(|| panic!("缺少 {source:?} 的 {model} 用量行"))
    };
    assert!(find(UsageSource::Session, "test/gpt-test:off").calls >= 4);
    assert_eq!(find(UsageSource::Sidekick, "test/gpt-cheap:off").calls, 2);
    assert!(find(UsageSource::Subagent, "test/gpt-test:off").calls >= 1);
    assert_eq!(usage.models.len(), 3, "{:?}", usage.models);
    let calls: u64 = usage.models.iter().map(|summary| summary.calls).sum();
    let input: u64 = usage
        .models
        .iter()
        .map(|summary| summary.input_tokens)
        .sum();
    assert_eq!(
        (usage.total.calls, usage.total.input_tokens),
        (calls, input),
        "合计应等于各行之和"
    );
    harness.close().await;
}

/// `compact` 在投递前产生压缩 checkpoint，新 handoff 排在它之后；`fresh`
/// 新建会话并成为后续 `keep` 的目标；已完成会话的 `retry` 被拒。
#[tokio::test]
async fn fusion_sidekick_compact_fresh_and_completed_guards() {
    let upstream = MockServer::builder().start().await;
    // 工具结果回来的次序决定下一步：compact → fresh → retry（completed 拒）→ 收尾。
    let phase = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&phase);
    mount_router(&upstream, move |body| {
        // 压缩摘要请求（soft checkpoint）。
        if body
            .to_string()
            .contains("compressing an agent conversation")
        {
            return ResponseTemplate::new(200)
                .set_body_json(support::complete(&support::soft_checkpoint("COMPACTED")));
        }
        if body["model"] == "gpt-cheap" {
            return sidekick_yield_call("SK_REPORT");
        }
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last());
        let Some(last) = last else {
            return text_response("LEAD_DONE");
        };
        if last["role"] != "tool" {
            return ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![("call_h1", "sidekick", json!({"message": "H1"}))]),
                "text/event-stream",
            );
        }
        let next = match counter.fetch_add(1, Ordering::SeqCst) {
            0 => json!({"message": "H2", "mode": "compact"}),
            1 => json!({"message": "H3", "mode": "fresh"}),
            2 => json!({"mode": "retry"}),
            _ => return text_response("LEAD_DONE"),
        };
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![("call_next", "sidekick", next)]),
            "text/event-stream",
        )
    })
    .await;
    let harness = fusion_harness(&upstream).await;
    let lead_id = harness.session(moc_protocol::SessionEngine::Standard).await;
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
    let mut sidekicks: Vec<_> = agents
        .agents
        .iter()
        .filter(|agent| agent.preset.as_deref() == Some("sidekick"))
        .collect();
    sidekicks.sort_by_key(|agent| agent.created_at_ms);
    assert_eq!(sidekicks.len(), 2, "fresh 后应有两个 sidekick 会话");

    // 旧会话：压缩 checkpoint 之后才是 H2 的 handoff 消息；且收不到 H3。
    let user_text = |entry: &moc_core::session::entries::Entry| match entry {
        moc_core::session::entries::Entry::Message { message, .. }
            if message.role == Role::User =>
        {
            Some(message.text())
        }
        _ => None,
    };
    let old_entries = transcript_entries(&harness, &sidekicks[0].id).await;
    let checkpoint_at = old_entries
        .iter()
        .position(|entry| matches!(entry, moc_core::session::entries::Entry::Compaction { .. }))
        .expect("compact 应留下压缩 checkpoint");
    let h2_at = old_entries
        .iter()
        .position(|entry| user_text(entry).is_some_and(|text| text.contains("H2")))
        .expect("H2 handoff 消息存在");
    assert!(
        h2_at > checkpoint_at,
        "handoff 消息必须排在 checkpoint 之后"
    );
    assert!(
        !old_entries
            .iter()
            .filter_map(user_text)
            .any(|text| text.contains("H3")),
        "H3 必须进新会话"
    );

    // 新会话拿到了 H3（fresh spawn 的任务）。
    let new_entries = transcript_entries(&harness, &sidekicks[1].id).await;
    assert!(
        new_entries
            .iter()
            .filter_map(user_text)
            .any(|text| text.contains("H3")),
        "fresh spawn 的 handoff 必须落在新会话"
    );

    // 工具结果链：spawned → compacted → spawned → retry 被拒（completed）。
    let loaded = harness.load(&lead_id).await;
    let texts: Vec<String> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .map(|result| result.content.clone())
        .collect();
    let joined = texts.join("\n");
    assert!(joined.contains("\"compacted\""), "{joined}");
    assert!(
        joined.contains("The sidekick's last handoff completed; send a new message instead."),
        "{joined}"
    );
    harness.close().await;
}

/// sidekick 运行中：compact/fresh/retry 都被拒——这三种模式都要求 sidekick
/// 空闲，只有 keep 能把手头的消息注进运行中的会话。
#[tokio::test]
async fn fusion_sidekick_busy_rejects_compact_fresh_and_retry() {
    let upstream = MockServer::builder().start().await;
    let phases = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&phases);
    mount_router(&upstream, move |body| {
        // sidekick 上游慢：handoff 派出后一直 Running。
        if body["model"] == "gpt-cheap" {
            return delayed(sidekick_yield_call("SK_REPORT"), 1_500);
        }
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last());
        if last.is_some_and(|message| message["role"] == "tool") {
            // keep(block=false) 的结果回来后：连着发三次忙碌态应报错的 mode。
            let next = match counter.fetch_add(1, Ordering::SeqCst) {
                1 => json!({"message": "H2", "mode": "compact"}),
                2 => json!({"message": "H3", "mode": "fresh"}),
                3 => json!({"mode": "retry"}),
                _ => return text_response("LEAD_DONE"),
            };
            return ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![("call", "sidekick", next)]),
                "text/event-stream",
            );
        }
        // 首轮提示：先派出后台运行的 handoff。
        counter.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![(
                "call",
                "sidekick",
                json!({"message": "HANDOFF", "block": false}),
            )]),
            "text/event-stream",
        )
    })
    .await;
    let harness = fusion_harness(&upstream).await;
    let lead_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    assert_eq!(
        harness.prompt(&lead_id, "开工").await.message.text(),
        "LEAD_DONE"
    );

    let loaded = harness.load(&lead_id).await;
    let texts: Vec<String> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .map(|result| result.content.clone())
        .collect();
    let joined = texts.join("\n");
    assert_eq!(
        joined.matches("The sidekick is running").count(),
        3,
        "compact/fresh/retry 在运行中的 sidekick 上都必须报错：{joined}"
    );
    harness.close().await;
}

/// 从文档发现到真实 PTC 派发、交付；延迟调用仍受目标工具插件钩子约束。
#[tokio::test]
async fn deferred_agent_documentation_dispatch_and_hooks() {
    assert!(support::require_moc_bin("按需子代理"));
    let upstream = MockServer::builder().start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    mount_router(&upstream, move |body| {
        if is_subagent_request(body) {
            return yield_call("child_yield", "DEFERRED_AGENT_REPORT");
        }
        let response = match counter.fetch_add(1, Ordering::SeqCst) {
            0 => support::tool_stream(vec![("agent_docs", "read", json!({"path":"tool://agents"}))]),
            1 => support::tool_stream(vec![("agent_program", "execute", json!({
                "description":"Delegate work and collect its report",
                "code": "let denied; try { await tools.spawn_agent({role:'explorer',task:'BLOCKED_TASK'}); } catch(e) { denied=e.code; } const spawned=JSON.parse((await tools.spawn_agent({role:'explorer',task:'ALLOWED_TASK'})).content); const waited=await tools.wait_agent({agent_ids:spawned.agents.map(a=>a.agent_id),timeout_secs:10}); const listed=await tools.list_agents({}); return {denied,waited:waited.structuredContent,listed:listed.content};"
            }))]),
            _ => support::text_stream("委派完成"),
        };
        ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
    }).await;
    let harness = harness(&upstream).await;
    super::super::plugin_flow::install_plugin(
        &harness,
        "deferred-policy",
        r#"
        export default { hooks: {
          beforeToolCall(event) {
            if (event.name === 'spawn_agent' && event.arguments.task === 'BLOCKED_TASK') return {deny:'Blocked by target policy'};
          },
          afterToolCall(event) {
            if (event.name === 'list_agents') return {content:'REDACTED_AGENT_LIST'};
          }
        }};
    "#,
    );
    let parent = harness.session(moc_protocol::SessionEngine::Standard).await;
    harness.prompt(&parent, "委派独立任务并收集报告").await;
    let results = support::tool_result_texts(&harness.load(&parent).await);
    let result = results
        .iter()
        .find(|text| text.contains("REDACTED_AGENT_LIST"))
        .expect("目标工具结果钩子必须作用于嵌套调用");
    assert!(
        result.contains("E_PLUGIN_DENIED"),
        "目标工具前置钩子必须阻止派发：{result}"
    );
    assert!(
        result.contains("DEFERRED_AGENT_REPORT"),
        "子代理报告必须回到父代理：{result}"
    );
    let agents = harness
        .client
        .call::<AgentList>(AgentListParams {
            session_id: parent.clone(),
        })
        .await
        .unwrap();
    assert_eq!(agents.agents.len(), 1, "被钩子拒绝的派发不得创建子代理");
    assert_eq!(agents.agents[0].status, AgentStatus::Completed);
    let requests = upstream.received_requests().await.unwrap();
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(!tool_names(&first).contains(&"spawn_agent"));
    assert!(
        !first["messages"].to_string().contains("<agent-roles>"),
        "初始请求不应包含完整角色表"
    );
    assert!(
        results
            .iter()
            .any(|text| text.contains("inputSchema") && text.contains("spawn_agent")),
        "按需读取才获得参数"
    );
    harness.close().await;
}

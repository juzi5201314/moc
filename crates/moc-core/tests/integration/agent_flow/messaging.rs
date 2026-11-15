use super::*;

#[tokio::test]
async fn message_routes_running_as_injected_and_idle_as_woken() {
    let upstream = MockServer::builder().start().await;
    // 门控掐住子代理首响：投递必然落在运行中窗口，不靠时序运气。
    let gate = support::Gate::new();
    let held = gate.clone();
    mount_router(&upstream, move |body| {
        let text = body.to_string();
        // 历史里会同时带着旧任务与旧注入消息，命中顺序必须先看最新一条。
        if text.contains("WAKE_TEXT") {
            return yield_call("call_yield", "WOKEN_DONE");
        }
        if text.contains("INJECT_TEXT") {
            return yield_call("call_yield", "INJECT_DONE");
        }
        if text.contains("SLOW_TASK") {
            held.wait();
            return yield_call("call_yield", "SLOW_DONE");
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "SLOW_TASK").await;

    let first = message_agent(&harness, &parent_id, &child_id, "INJECT_TEXT")
        .await
        .expect("运行中投递成功");
    assert_eq!(first.receipts.len(), 1);
    assert_eq!(first.receipts[0].outcome, AgentMessageOutcome::Injected);
    assert_eq!(first.receipts[0].agent_id, child_id);
    gate.open();

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(10)).await;
    assert!(waited.settled);
    // 注入发生在首轮内还是收尾阶段取决于调度，两种落点都是合法注入。
    let delivered = waited.agents[0].message.as_ref().expect("首轮结果").text();
    assert!(
        matches!(delivered.as_str(), "SLOW_DONE" | "INJECT_DONE"),
        "首轮结果必须来自子代理自己的交付：{delivered}"
    );

    // 注入的消息必须落成带来源元数据的条目，并在模型视图里带 `<agent-message>`。
    let child = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: child_id.clone(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("child transcript 可加载");
    let injected = child
        .entries
        .iter()
        .find(|entry| entry.agent.is_some())
        .expect("注入消息必须落库");
    assert_eq!(injected.message.text(), "INJECT_TEXT");
    let context = injected.context_message().expect("在上下文内");
    assert!(context.text().contains("<agent-message from="));
    assert!(context.text().contains("INJECT_TEXT"));
    let meta = injected.agent.clone().expect("来源元信息");
    assert_eq!(meta.from_id, parent_id);
    let card = harness.updates.lock().iter().any(|update| match update {
        moc_protocol::methods::SessionUpdate::AgentMessage { meta, body } => {
            meta.from_id == parent_id && body == "INJECT_TEXT"
        }
        _ => false,
    });
    assert!(card, "接收侧必须推送 IRC 卡片更新");

    // 子代理已空闲：再投递必须走唤醒，并产生新回合。
    let second = message_agent(&harness, &parent_id, &child_id, "WAKE_TEXT")
        .await
        .expect("空闲投递成功");
    assert_eq!(second.receipts[0].outcome, AgentMessageOutcome::Woken);
    let woken = wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(10)).await;
    assert!(woken.settled);
    assert_eq!(
        woken.agents[0].message.as_ref().expect("唤醒结果").text(),
        "WOKEN_DONE",
        "唤醒消息即新回合的输入"
    );
    harness.close().await;
}

#[tokio::test]
async fn broadcast_reaches_running_and_skips_idle() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        let text = body.to_string();
        if text.contains("FAST_TASK") {
            return yield_call("call_yield", "FAST_DONE");
        }
        if text.contains("SLOW_A") || text.contains("SLOW_B") {
            // 挂在 bash 长任务上保持运行中；harness.close() 的关停路径回收进程。
            return ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![(
                    "call_hold",
                    "bash",
                    json!({"description":"hold","command":"sleep 300","timeout_secs":3600}),
                )]),
                "text/event-stream",
            );
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let idle = spawn_child(&harness, &parent_id, "FAST_TASK").await;
    let running_a = spawn_child(&harness, &parent_id, "SLOW_A").await;
    let running_b = spawn_child(&harness, &parent_id, "SLOW_B").await;
    let settled = wait_children(&harness, &parent_id, Some(vec![idle.clone()]), Some(10)).await;
    assert_eq!(settled.agents[0].status, AgentStatus::Completed);

    let idle_entries = harness.load(&idle).await.entries.len();
    let result = message_agent(&harness, &parent_id, "all", "BROADCAST_TEXT")
        .await
        .expect("广播成功");
    let mut targets: Vec<&str> = result
        .receipts
        .iter()
        .map(|receipt| receipt.agent_id.as_str())
        .collect();
    targets.sort_unstable();
    let mut expected = vec![running_a.as_str(), running_b.as_str()];
    expected.sort_unstable();
    assert_eq!(targets, expected, "广播只覆盖运行中的直接子代理");
    assert!(
        result
            .receipts
            .iter()
            .all(|receipt| receipt.outcome == AgentMessageOutcome::Injected)
    );
    assert_eq!(
        harness.load(&idle).await.entries.len(),
        idle_entries,
        "空闲子代理不得被广播叫醒"
    );
    harness.close().await;
}

#[tokio::test]
async fn child_message_wakes_parent_wait() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        let text = body.to_string();
        if text.contains("PINGER_TASK") && text.contains("CHILD_PING") {
            // 第二次请求：消息已投递，子代理收尾。
            return yield_call("call_yield", "CHILD_DONE");
        }
        if text.contains("PINGER_TASK") {
            // 消息先于或晚于 wait_agent 到达都合法：先到记为待收，晚到打断等待，
            // 两种路径都以 messages_pending=true 收尾，不需要人为延迟。
            return ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![(
                    "call_ping",
                    "message_agent",
                    json!({"target": "parent", "message": "CHILD_PING"}),
                )]),
                "text/event-stream",
            );
        }
        if text.contains("<agent-message") {
            return text_response("PARENT_SAW_PING");
        }
        if text.contains("PARENT_WAIT") {
            return ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![("call_wait", "wait_agent", json!({}))]),
                "text/event-stream",
            );
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "PINGER_TASK").await;

    let result = harness.prompt(&parent_id, "PARENT_WAIT").await;
    assert_eq!(result.message.text(), "PARENT_SAW_PING");

    let parent = harness.load(&parent_id).await;
    let wait_result = parent
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .map(|result| result.content.clone())
        .find(|content| content.contains("messages_pending"))
        .expect("wait_agent 的结果必须落库");
    assert!(
        wait_result.contains("\"messages_pending\":true"),
        "子代理消息必须打断等待：{wait_result}"
    );
    let ping = parent
        .entries
        .iter()
        .find(|entry| entry.agent.is_some())
        .expect("父会话必须收到带来源的代理消息");
    assert_eq!(ping.message.text(), "CHILD_PING");
    assert_eq!(ping.agent.as_ref().expect("来源元信息").from_id, child_id);

    let progress = harness.updates.lock().iter().any(|update| {
        matches!(
            update,
            moc_protocol::methods::SessionUpdate::ToolProgress { .. }
        )
    });
    assert!(progress, "wait_agent 必须推送进度行");
    harness.close().await;
}

/// 收尾窗口投递：`yield` 响应延迟期间注入的消息必须让子代理再跑一轮作答，
/// 不能只在结束后落盘而无人应答。
#[tokio::test]
async fn injected_message_during_closing_window_is_answered() {
    let upstream = MockServer::builder().start().await;
    // 门控掐住子代理首响：注入必然落在首轮在飞窗口，收尾窗口交付由流程保证。
    let gate = support::Gate::new();
    let held = gate.clone();
    mount_router(&upstream, move |body| {
        if body.to_string().contains("PARENT_LATE") {
            return yield_call("call_yield", "FINAL_REPORT");
        }
        if is_subagent_request(body) {
            held.wait();
            return yield_call("call_yield", "FIRST_REPORT");
        }
        text_response("PARENT_OK")
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "SLOW_TASK").await;

    let receipt = message_agent(&harness, &parent_id, &child_id, "PARENT_LATE")
        .await
        .expect("收尾窗口投递");
    assert_eq!(receipt.receipts[0].outcome, AgentMessageOutcome::Injected);
    gate.open();

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id]), Some(10)).await;
    assert!(waited.settled);

    // 子代理必须再发一次请求把注入消息答掉：消息进入后续请求的上下文。
    let requests = upstream.received_requests().await.expect("上游请求");
    let answered = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .any(|body| is_subagent_request(&body) && body.to_string().contains("PARENT_LATE"));
    assert!(answered, "注入消息必须进入子代理的模型请求");
    harness.close().await;
}

/// sidekick 只能经 `sidekick` 工具驱动：lead 直接拿 `message_agent` 寻址
/// sidekick 子会话必须被拒（绕过它会在旧模型上继续计费）。
#[tokio::test]
async fn message_agent_to_a_sidekick_requires_the_sidekick_tool() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body["model"] == "gpt-cheap" {
            return sidekick_yield_call("SK_REPORT");
        }
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last());
        if last.is_some_and(|message| message["role"] == "tool") {
            return text_response("LEAD_DONE");
        }
        ResponseTemplate::new(200).set_body_raw(
            support::tool_stream(vec![("call_sk", "sidekick", json!({"message": "HANDOFF"}))]),
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
    let sidekick = agents
        .agents
        .iter()
        .find(|agent| agent.preset.as_deref() == Some("sidekick"))
        .expect("sidekick 会话存在");

    // 显式寻址 sidekick 子会话：无论 Fusion 与否都拒绝。
    let error = harness
        .client
        .call::<AgentMessage>(AgentMessageParams {
            session_id: lead_id.clone(),
            target: sidekick.id.clone(),
            message: "wake up".into(),
        })
        .await
        .expect_err("直接寻址 sidekick 必须失败");
    assert!(
        error
            .to_string()
            .contains("Address the sidekick through the sidekick tool"),
        "{error}"
    );
    harness.close().await;
}

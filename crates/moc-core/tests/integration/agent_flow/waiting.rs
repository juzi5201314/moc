use super::*;

/// 完整报告跨越状态预览上限，持久化展示结果不得再次进入父模型上下文。
#[tokio::test]
async fn result_card_persists_full_report_without_context_duplication() {
    let upstream = MockServer::builder().start().await;
    let report = format!("{}\n报告末尾", "完整结果\n".repeat(2_000));
    let response_report = report.clone();
    mount_router(&upstream, move |body| {
        if body.to_string().contains("CARD_TASK") {
            yield_call("call_yield", &response_report)
        } else {
            text_response("PARENT_OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let mut events = harness.events.subscribe();
    let child_id = spawn_child(&harness, &parent_id, "CARD_TASK").await;
    let (entry_id, meta, body) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let moc_protocol::SessionUpdate::AgentResult {
                entry_id,
                meta,
                body,
                ..
            } = events.recv().await.expect("连接仍在")
            {
                break (entry_id, meta, body);
            }
        }
    })
    .await
    .expect("结果应及时送达");
    assert_eq!(meta.agent_id, child_id);
    assert_eq!(meta.status, AgentStatus::Completed);
    assert_eq!(body, report, "展示结果不能使用截断的状态预览");
    let entries = transcript_entries(&harness, &parent_id).await;
    let stored = entries
        .iter()
        .find(|entry| entry.id() == entry_id)
        .expect("结果已落盘");
    let result = stored.transcript_entry().expect("结果转写");
    assert_eq!(result.agent_result.as_ref(), Some(&meta));
    assert_eq!(result.message.text(), report);
    assert!(
        result.context_message().is_none(),
        "展示记录不能重复进入上下文"
    );
    harness.prompt(&parent_id, "检查结果提醒").await;
    let requests = upstream.received_requests().await.expect("上游请求");
    let parent_request: Value =
        serde_json::from_slice(&requests.last().expect("父提示").body).expect("请求 JSON");
    let context = parent_request["messages"].to_string();
    assert!(context.contains("Result preview"), "保留原有模型结果提醒");
    assert!(
        !context.contains("报告末尾"),
        "完整展示报告不能泄漏到有界提醒之外"
    );
    harness.close().await;
}

#[tokio::test]
async fn missing_yield_reminds_three_times_then_falls_back() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        if body.to_string().contains("REFUSES_TASK") {
            // 纯文本收尾：既没有 yield，也不调用任何工具。
            text_response("CHILD_ATTEMPT")
        } else {
            text_response("PARENT_OK")
        }
    })
    .await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;
    let child_id = spawn_child(&harness, &parent_id, "REFUSES_TASK").await;

    let waited = wait_children(&harness, &parent_id, Some(vec![child_id.clone()]), Some(20)).await;
    assert!(waited.settled);
    assert_eq!(waited.agents[0].status, AgentStatus::Completed);
    assert_eq!(
        waited.agents[0].message.as_ref().expect("回退结果").text(),
        "CHILD_ATTEMPT",
        "缺 yield 时回退到最后一条 assistant 正文"
    );

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
    let reminders: Vec<String> = child
        .entries
        .iter()
        .map(|entry| entry.message.text())
        .filter(|text| text.contains("Reminder"))
        .collect();
    assert_eq!(reminders.len(), 3, "提醒必须恰好三次：{reminders:?}");
    assert!(reminders[0].contains("Reminder 1/3"));
    assert!(reminders[2].contains("Reminder 3/3"));
    harness.close().await;
}

#[tokio::test]
async fn wait_returns_on_first_settle_with_snapshot() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |body| {
        let text = body.to_string();
        if text.contains("FAST_TASK") {
            return yield_call("call_yield", "FAST_DONE");
        }
        if text.contains("SLOW_TASK") {
            // 慢子代理挂在 bash 长任务上保持 Running：不靠时序猜测，进程由
            // harness.close() 的关停路径回收。
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
    let fast = spawn_child(&harness, &parent_id, "FAST_TASK").await;
    let slow = spawn_child(&harness, &parent_id, "SLOW_TASK").await;

    let waited = wait_children(
        &harness,
        &parent_id,
        Some(vec![fast.clone(), slow.clone()]),
        Some(10),
    )
    .await;
    assert!(waited.settled, "首个终态即返回");
    assert!(!waited.timed_out);
    assert_eq!(waited.agents.len(), 2);
    assert_eq!(waited.agents[0].id, fast);
    assert_eq!(waited.agents[0].status, AgentStatus::Completed);
    assert_eq!(waited.agents[1].id, slow, "快照保持解析顺序，不因终态重排");
    assert_eq!(
        waited.agents[1].status,
        AgentStatus::Running,
        "未结束的代理只回报运行中"
    );
    assert!(waited.agents[1].message.is_none());
    harness.close().await;
}

#[tokio::test]
async fn wait_without_children_is_rejected() {
    let upstream = MockServer::builder().start().await;
    mount_router(&upstream, |_| text_response("PARENT_OK")).await;
    let harness = harness(&upstream).await;
    let parent_id = harness.session(moc_protocol::SessionEngine::Standard).await;

    let error = harness
        .client
        .call::<AgentWait>(AgentWaitParams {
            session_id: parent_id,
            agent_ids: None,
            timeout_secs: Some(1),
        })
        .await
        .expect_err("没有子代理时等待必须明确失败");
    assert!(
        error.to_string().contains("INVALID_PARAMS"),
        "错误必须指明参数问题：{error}"
    );
    harness.close().await;
}

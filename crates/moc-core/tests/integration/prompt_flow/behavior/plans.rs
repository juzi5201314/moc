use super::*;

#[tokio::test]
async fn external_workspace_changes_do_not_interrupt_or_enter_prompt_context() {
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for (plan_active, mode) in [(false, None), (true, None), (false, Some(PromptMode::Ask))] {
        let upstream = MockServer::builder().start().await;
        let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
        let cwd = h.root.path().join("work");
        std::fs::write(cwd.join("target.txt"), "analysis-target").unwrap();
        let external = cwd.join("unrelated-external-change.txt");
        let changed_path = external.clone();
        let counter = AtomicUsize::new(0);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_request: &wiremock::Request| {
                let index = counter.fetch_add(1, Ordering::SeqCst);
                // 在请求已发出、工具尚未执行时模拟其他会话修改，无需依赖时序等待。
                std::fs::write(&changed_path, format!("external revision {index}")).unwrap();
                let response = if index.is_multiple_of(2) {
                    support::tool_stream(vec![("inspect", "read", json!({"path": "target.txt"}))])
                } else {
                    support::text_stream("分析完成")
                };
                ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
            })
            .mount(&upstream)
            .await;
        let session = if plan_active {
            h.session_plan(SessionEngine::Standard).await
        } else {
            h.session(SessionEngine::Standard).await
        };
        for turn in 0..2 {
            // 同时覆盖进入计划模式后与两次提示之间的外部修改。
            std::fs::write(&external, format!("between prompts {turn}")).unwrap();
            let result = h
                .client
                .call::<SessionPrompt>(PromptParams {
                    session_id: session.clone(),
                    input: vec![InputPart::Text {
                        text: "分析 target.txt".into(),
                    }],
                    mode,
                    agent_message: None,
                    goal_round: None,
                    max_turns: None,
                })
                .await
                .expect("外部修改不应中断提示");
            assert_eq!(result.message.text(), "分析完成");
            assert_eq!(result.finish_reason, moc_llm::FinishReason::Stop);
        }
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4);
        for (index, request) in requests.iter().enumerate() {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let messages = body["messages"].to_string();
            assert!(
                !messages.contains("unrelated-external-change.txt"),
                "其他会话的文件变化不应进入上下文：{messages}"
            );
            if !index.is_multiple_of(2) {
                assert!(messages.contains("analysis-target"), "读取结果必须送达模型");
            }
        }
        assert_eq!(
            std::fs::read_to_string(external).unwrap(),
            "external revision 3"
        );
        let loaded = h.load(&session).await;
        assert_eq!(loaded.session.engine, SessionEngine::Standard);
        assert_eq!(loaded.session.plan_active, plan_active);
        assert!(
            !h.updates
                .lock()
                .iter()
                .any(|update| matches!(update, SessionUpdate::EngineChanged { .. })),
            "提示级模式不得改变会话引擎"
        );
        h.core.shutdown().await;
    }
}

/// 审阅批注 → 修订 → 新版提交：旧版被取代、批注保留、diff 指向上一版。
#[tokio::test]
async fn plan_revision_supersedes_previous_version_and_emits_diff() {
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let first_plan = "# 计划\n\n1. 先看代码\n2. 再做修改\n";
    let second_plan = "# 计划\n\n1. 先看 InspectorPanel\n2. 再做修改\n";
    let responses = [
        support::tool_stream(vec![
            (
                "save_one",
                "plan",
                json!({"title": "改版方案", "markdown": first_plan}),
            ),
            ("exit_one", "exit_plan", json!({})),
        ]),
        support::tool_stream(vec![
            (
                "save_two",
                "plan",
                json!({"title": "改版方案", "markdown": second_plan}),
            ),
            ("exit_two", "exit_plan", json!({})),
        ]),
    ];
    let response_index = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&response_index);
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let index = counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_raw(
                responses[index.min(responses.len() - 1)].clone(),
                "text/event-stream",
            )
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let session = h.session_plan(SessionEngine::Standard).await;
    h.prompt(&session, "先出方案").await;

    let first = h.load(&session).await;
    let v1 = first
        .plans
        .iter()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .expect("首版提交")
        .clone();
    assert_eq!(v1.version, 1);
    assert_eq!(v1.previous_plan_id, None);
    assert!(v1.annotations.is_empty());

    let annotations = vec![
        PlanAnnotation {
            id: "a1".into(),
            start_line: 3,
            end_line: 3,
            quote: "先看代码".into(),
            body: "太笼统\n\n```suggestion\n1. 先看 crates/moc-web/web/src/components/panels/inspector-panel.tsx\n```"
                .into(),
        },
        PlanAnnotation {
            id: "a2".into(),
            start_line: 4,
            end_line: 4,
            quote: String::new(),
            body: "这一步保留".into(),
        },
    ];
    let revised = h
        .client
        .call::<SessionPlanRevise>(SessionPlanReviseParams {
            session_id: session.clone(),
            plan_id: v1.plan_id.clone(),
            annotations: annotations.clone(),
            note: Some("整体保持简洁".into()),
        })
        .await
        .expect("提交批注");
    assert!(revised.plan_active);
    assert!(
        revised.prompt.contains("### 1. Lines 3-3"),
        "{}",
        revised.prompt
    );
    assert!(
        revised.prompt.contains("> 1. 先看代码"),
        "原文引用取自已保存的计划：{}",
        revised.prompt
    );
    assert!(
        revised.prompt.contains("```suggestion"),
        "{}",
        revised.prompt
    );
    assert!(revised.prompt.contains("### Overall"), "{}", revised.prompt);
    assert!(
        revised.prompt.contains("整体保持简洁"),
        "{}",
        revised.prompt
    );

    let pending = h.load(&session).await;
    let pending_v1 = pending
        .plans
        .iter()
        .find(|plan| plan.plan_id == v1.plan_id)
        .expect("旧版仍在");
    assert_eq!(pending_v1.status, PlanStatus::Revised);
    assert_eq!(pending_v1.annotations.len(), 2);
    assert_eq!(pending_v1.annotations[0].body, annotations[0].body);

    h.prompt(&session, &revised.prompt).await;

    let after = h.load(&session).await;
    let old = after
        .plans
        .iter()
        .find(|plan| plan.plan_id == v1.plan_id)
        .expect("旧版保留");
    assert_eq!(old.status, PlanStatus::Superseded);
    assert_eq!(old.annotations.len(), 2, "旧版批注必须保留");
    let v2 = after
        .plans
        .iter()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .expect("新版提交")
        .clone();
    assert_eq!(v2.version, 2);
    assert_eq!(v2.previous_plan_id.as_deref(), Some(v1.plan_id.as_str()));
    assert_eq!(v2.markdown, second_plan);

    let diff = h
        .client
        .call::<SessionPlanDiff>(SessionPlanDiffParams {
            session_id: session.clone(),
            plan_id: v2.plan_id.clone(),
        })
        .await
        .expect("取版本 diff");
    assert_eq!(diff.previous_plan_id.as_deref(), Some(v1.plan_id.as_str()));
    assert_eq!(diff.diff.files.len(), 1);
    let hunk_lines: Vec<(DiffLineKind, String)> = diff.diff.files[0]
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter().map(|line| (line.kind, line.text.clone())))
        .collect();
    assert!(
        hunk_lines
            .iter()
            .any(|(kind, text)| *kind == DiffLineKind::Delete && text == "1. 先看代码")
    );
    assert!(
        hunk_lines
            .iter()
            .any(|(kind, text)| *kind == DiffLineKind::Insert && text == "1. 先看 InspectorPanel")
    );

    let first_diff = h
        .client
        .call::<SessionPlanDiff>(SessionPlanDiffParams {
            session_id: session.clone(),
            plan_id: v1.plan_id.clone(),
        })
        .await
        .expect("首版没有上一版");
    assert_eq!(first_diff.previous_plan_id, None);
    assert!(first_diff.diff.files.is_empty());

    // 模型确实看到了批注正文、建议改法与整体意见。
    let requests = upstream.received_requests().await.expect("捕获请求");
    let second_body = requests
        .last()
        .map(|request| String::from_utf8_lossy(&request.body).to_string())
        .expect("第二次请求");
    assert!(second_body.contains("suggestion"), "{second_body}");
    assert!(second_body.contains("整体保持简洁"), "{second_body}");
    assert!(second_body.contains("太笼统"), "{second_body}");

    // 状态迁移对前端是显式的，不靠前端本地推断。
    assert!(h.updates.lock().iter().any(|update| matches!(
        update,
        SessionUpdate::PlanUpdated { plan }
            if plan.plan_id == v1.plan_id && plan.status == PlanStatus::Superseded
    )));
    assert!(h.updates.lock().iter().any(|update| matches!(
        update,
        SessionUpdate::PlanSubmitted { plan }
            if plan.plan_id == v2.plan_id && plan.version == 2
    )));
    h.close().await;
}

/// `plan(edits)` 三态：草稿原地修订、已评审版本播种新草稿、审阅中拒绝。
#[tokio::test]
async fn plan_edits_patch_draft_seed_reviewed_and_reject_submitted() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::{Value, json};

    /// 指定工具调用的结果文本；锚点和 URI 只从工具结果里取（系统提示也含 local://）。
    fn result_text(body: &Value, call_id: &str) -> String {
        body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|message| message["tool_call_id"].as_str() == Some(call_id))
            .map(|message| match &message["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            })
            .unwrap_or_default()
    }
    /// 修订提示是最后一条 user 消息，里面带底版的 `local://` 路径。
    fn last_user_text(body: &Value) -> String {
        body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|message| message["role"].as_str() == Some("user"))
            .filter_map(|message| message["content"].as_str())
            .next_back()
            .unwrap_or_default()
            .to_owned()
    }
    fn plan_uri(text: &str) -> String {
        let start = text.find("local://").expect("消息里应有计划 URI");
        let tail = &text[start..];
        let end = tail.find(".md").map(|at| at + 3).unwrap_or(tail.len());
        tail[..end].to_owned()
    }
    /// `read` 输出里取目标行的行号（`N│line` 中 `│` 前的数字）。
    fn line_for(text: &str, needle: &str) -> usize {
        text.lines()
            .filter_map(|line| line.split_once('│'))
            .find(|(_, content)| *content == needle)
            .and_then(|(number, _)| number.parse().ok())
            .unwrap_or_else(|| panic!("行 {needle} 应有行号：{text}"))
    }
    /// `read` 输出首行 `[path#TAG]` 里的 tag。
    fn tag_in(text: &str) -> String {
        let header = text.lines().next().expect("read 输出应有头部");
        let start = header.rfind('#').expect("头部应带 tag") + 1;
        let end = header.rfind(']').expect("头部应以 ] 结尾");
        header[start..end].to_owned()
    }

    let markdown_a = "# 方案\n1. 第一步\n2. 第二步\n3. 第三步\n";
    let markdown_b = "# 方案\n1. 第一步\n2. 第二步\n3. 第三步\n4. 第四步\n";
    let upstream = MockServer::builder().start().await;
    let index = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&index);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let at = counter.fetch_add(1, Ordering::SeqCst);
            let response = match at {
                0 => support::tool_stream(vec![(
                    "save_a",
                    "plan",
                    json!({"title": "实施", "markdown": markdown_a}),
                )]),
                1 => {
                    support::tool_stream(vec![("save_b", "plan", json!({"markdown": markdown_b}))])
                }
                2 => {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    support::tool_stream(vec![(
                        "read_draft",
                        "read",
                        json!({"path": plan_uri(&result_text(&body, "save_b"))}),
                    )])
                }
                3 => {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    let read_output = result_text(&body, "read_draft");
                    let tag = tag_in(&read_output);
                    let after = line_for(&read_output, "2. 第二步");
                    support::tool_stream(vec![
                        (
                            "patch_draft",
                            "plan",
                            json!({"tag": tag, "edits": [{"op": "insert", "after": after, "text": "2a. 新增的第二步"}]}),
                        ),
                        ("exit_draft", "exit_plan", json!({})),
                    ])
                }
                // 审阅中尝试 edits：应在 tag 校验之前就被拒绝。
                4 => support::tool_stream(vec![(
                    "blocked",
                    "plan",
                    json!({"tag": "0000", "edits": [{"op": "replace", "from": 1, "to": 1, "text": "篡改"}]}),
                )]),
                5 => text_stream("等审阅结果"),
                // 批注返回后：读冻结底版 → edits 播种新草稿 → 提交。
                6 => {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    support::tool_stream(vec![(
                        "read_v1",
                        "read",
                        json!({"path": plan_uri(&last_user_text(&body))}),
                    )])
                }
                7 => {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    let read_output = result_text(&body, "read_v1");
                    let tag = tag_in(&read_output);
                    let from = line_for(&read_output, "3. 第三步");
                    let to = line_for(&read_output, "4. 第四步");
                    support::tool_stream(vec![
                        (
                            "seed_draft",
                            "plan",
                            json!({"tag": tag, "edits": [{"op": "replace", "from": from, "to": to, "text": "3. 细化后的第三步\n4. 第四步"}]}),
                        ),
                        ("exit_seed", "exit_plan", json!({})),
                    ])
                }
                _ => text_stream("修订完成"),
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let session = h.session_plan(SessionEngine::Standard).await;
    let sessions = h.root.path().join("sessions");
    let plans_dir = |loaded: &SessionLoadResult| {
        moc_core::session::store::SessionStore::new(&sessions)
            .paths_for(
                std::path::Path::new(&loaded.session.cwd),
                &session,
                loaded.session.created_at_ms,
            )
            .plans()
    };

    // 第一次提示：markdown 两次保存复用同一 plan_id，edits 原地修订后提交。
    h.prompt(&session, "起草计划").await;
    let loaded = h.load(&session).await;
    let v1 = loaded
        .plans
        .iter()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .expect("提交的 v1")
        .clone();
    assert_eq!(v1.version, 1);
    let expected_v1 = "# 方案\n1. 第一步\n2. 第二步\n2a. 新增的第二步\n3. 第三步\n4. 第四步\n";
    assert_eq!(v1.markdown, expected_v1);
    let v1_file = plans_dir(&loaded).join(format!("{}.md", v1.plan_id));
    assert_eq!(
        std::fs::read_to_string(&v1_file).expect("v1 计划文件"),
        expected_v1
    );

    // 审阅中调用 edits 被拒绝，冻结版不受影响。
    h.prompt(&session, "趁审阅改计划").await;
    let blocked = h
        .load(&session)
        .await
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|result| result.tool_call_id == "blocked")
        .expect("拒绝的工具结果")
        .clone();
    assert!(blocked.is_error);
    assert!(
        blocked.content.contains("under review"),
        "{}",
        blocked.content
    );
    assert_eq!(
        std::fs::read_to_string(&v1_file).expect("v1 计划文件"),
        expected_v1
    );

    // 用户批注后：edits 命中已评审版本，播种出新草稿再提交为 v2。
    let revised = h
        .client
        .call::<SessionPlanRevise>(SessionPlanReviseParams {
            session_id: session.clone(),
            plan_id: v1.plan_id.clone(),
            annotations: vec![PlanAnnotation {
                id: "a1".into(),
                start_line: 5,
                end_line: 5,
                quote: "3. 第三步".into(),
                body: "第三步再细化".into(),
            }],
            note: None,
        })
        .await
        .expect("提交批注");
    h.prompt(&session, &revised.prompt).await;

    let after = h.load(&session).await;
    let old = after
        .plans
        .iter()
        .find(|plan| plan.plan_id == v1.plan_id)
        .expect("v1 保留");
    assert_eq!(old.status, PlanStatus::Superseded);
    // 被播种的底版是冻结 artifact：文件内容与批注都不能被修订过程改写。
    assert_eq!(
        std::fs::read_to_string(&v1_file).expect("v1 计划文件"),
        expected_v1
    );
    assert_eq!(old.annotations.len(), 1);

    let v2 = after
        .plans
        .iter()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .expect("提交的 v2")
        .clone();
    assert_ne!(v2.plan_id, v1.plan_id);
    assert_eq!(v2.version, 2);
    assert_eq!(v2.previous_plan_id.as_deref(), Some(v1.plan_id.as_str()));
    let expected_v2 =
        "# 方案\n1. 第一步\n2. 第二步\n2a. 新增的第二步\n3. 细化后的第三步\n4. 第四步\n";
    assert_eq!(v2.markdown, expected_v2);
    assert_eq!(v2.title, v1.title);
    assert_eq!(
        std::fs::read_to_string(plans_dir(&after).join(format!("{}.md", v2.plan_id)))
            .expect("v2 计划文件"),
        expected_v2
    );

    // 没有遗留的 Draft：新草稿已被 exit_plan 提走，底版不是 Draft。
    assert!(
        after
            .plans
            .iter()
            .all(|plan| plan.status != PlanStatus::Draft)
    );
    h.close().await;
}

/// 条目只有变化时才注入：进出 plan 模式只改写模式条目，引擎与沙盒条目不重发。
#[tokio::test]
async fn changing_the_mode_injects_only_the_mode_entry() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("ok"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "第一问").await;
    h.client
        .call::<SessionPlanMode>(SessionPlanModeParams {
            session_id: id.clone(),
            active: true,
        })
        .await
        .expect("进入 plan 叠加态");
    h.prompt(&id, "第二问").await;
    h.client
        .call::<SessionPlanMode>(SessionPlanModeParams {
            session_id: id.clone(),
            active: false,
        })
        .await
        .expect("退出 plan 叠加态");
    h.prompt(&id, "第三问").await;

    let requests = upstream.received_requests().await.expect("请求记录");
    assert_eq!(requests.len(), 3);
    let body: serde_json::Value =
        serde_json::from_slice(&requests[1].body).expect("第二个请求 JSON");
    let reminders: Vec<String> = body["messages"]
        .as_array()
        .expect("消息数组")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .filter(|text| text.starts_with("<system-reminder>"))
        .map(str::to_owned)
        .collect();
    assert_eq!(reminders.len(), 2, "两次提示各留下一条提醒：{reminders:?}");

    let loaded = h.load(&id).await;
    let stamps: Vec<_> = loaded
        .entries
        .iter()
        .filter_map(|entry| entry.reminder.as_ref())
        .collect();
    assert_eq!(stamps.len(), 3);
    let first_keys: Vec<&str> = stamps[0]
        .items
        .iter()
        .map(|item| item.key.as_str())
        .collect();
    assert!(
        first_keys.contains(&"sandbox")
            && first_keys.contains(&"local-time")
            && first_keys.contains(&"engine"),
        "首次是全量注入，含引擎提醒：{first_keys:?}"
    );
    let entry_keys = |index: usize| -> Vec<&str> {
        stamps[index]
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect()
    };
    assert_eq!(
        entry_keys(1),
        vec!["mode"],
        "只有模式变了，引擎、沙盒与时间不该重发"
    );
    assert_eq!(entry_keys(2), vec!["mode"], "退出 plan 同样只更新模式条目");
    let third_body: serde_json::Value =
        serde_json::from_slice(&requests[2].body).expect("第三个请求 JSON");
    let visible: Vec<&str> = third_body["messages"]
        .as_array()
        .expect("消息数组")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    let last_reminder = visible
        .iter()
        .rev()
        .find(|text| text.starts_with("<system-reminder>"))
        .expect("第三个请求里要能看到最新提醒");
    assert!(
        last_reminder.contains("execution mode no longer applies"),
        "退出 plan 后必须更正旧的只读模式提醒：{last_reminder}"
    );
    h.close().await;
}

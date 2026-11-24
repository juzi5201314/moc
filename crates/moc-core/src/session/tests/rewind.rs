use super::*;

#[tokio::test]
async fn plugin_state_survives_in_place_rewind_and_reset() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let session = create_plugin_state_session(&table).await;
    table
        .plugin_state_set(&session.id, "demo", "count", serde_json::json!(1))
        .await
        .expect("保存原会话状态");
    let message = table
        .append_message(&session.id, Message::user_text("回退目标"), None, None)
        .await
        .expect("追加用户消息");
    let rewound = table
        .rewind(&session.id, &TranscriptEntryId::new(message.id()))
        .await
        .expect("原地回退");
    assert_eq!(rewound.session.id, session.id, "回退不产生新会话");
    assert_eq!(
        table
            .plugin_state_get(&session.id, "demo", "count")
            .await
            .expect("状态随会话保留"),
        Some(serde_json::json!(1))
    );
    table
        .append(&session.id, EntryPayload::ResetBoundary)
        .await
        .expect("清空会话上下文");
    assert_eq!(
        table
            .plugin_state_get(&session.id, "demo", "count")
            .await
            .expect("ResetBoundary 不动插件状态"),
        Some(serde_json::json!(1))
    );
    let record = table.get(&session.id).await.expect("会话记录");
    let path = record.paths.plugin_state("demo");
    table.delete(&session.id).await.expect("删除会话");
    assert!(!path.exists(), "删除会话会清理插件状态");
}

#[tokio::test]
async fn rewind_truncates_user_message_and_keeps_tool_history_complete() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let info = table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            SessionOrigin::root(),
            false,
        )
        .await
        .expect("创建会话");
    let first_user = table
        .append_message(&info.id, Message::user_text("第一条"), None, None)
        .await
        .expect("追加用户消息");
    let assistant = table
        .append_message(
            &info.id,
            Message {
                role: Role::Assistant,
                content: moc_llm::message_content![ContentPart::ToolCall(ToolCall::new(
                    "call_1", "read", "{}"
                ))],
            },
            None,
            None,
        )
        .await
        .expect("追加工具调用");
    table
        .append_message(
            &info.id,
            Message::tool_result("call_1", "结果", false),
            Some(ToolExecution {
                tool_call_id: "call_1".into(),
                status: ToolStatus::Completed,
                duration_ms: Some(1),
                diff: None,
            }),
            None,
        )
        .await
        .expect("追加工具结果");
    let second_user = table
        .append_message(&info.id, Message::user_text("第二条"), None, None)
        .await
        .expect("追加第二条用户消息");
    let second_user_id = TranscriptEntryId::new(second_user.id());
    let assistant_id = TranscriptEntryId::new(assistant.id());

    let rewound = table
        .rewind(&info.id, &second_user_id)
        .await
        .expect("回退到用户消息");
    assert_eq!(rewound.entries.len(), 3);
    assert_eq!(
        rewound.entries.last().expect("工具结果").message.role,
        Role::Tool
    );
    // 原地回退不产生新会话：id、header、出身全部保持原样。
    assert_eq!(rewound.session.id, info.id);
    assert!(rewound.session.parent_session_id.is_none());
    assert!(rewound.session.forked_from.is_none());
    // 截断已落盘：换一张表从磁盘重读，尾部确实不在了。
    let reloaded = SessionTable::new(directory.path().join("sessions"), true)
        .load(&info.id, None)
        .await
        .expect("从磁盘重新读取截断后的会话");
    assert_eq!(reloaded.entries.len(), 3);
    let original = table.load(&info.id, None).await.expect("读取会话");
    assert_eq!(original.entries.len(), 3);

    let rewound_again = table
        .rewind(&info.id, &assistant_id)
        .await
        .expect("回退到助手工具调用");
    assert_eq!(rewound_again.entries.len(), 3);
    assert_eq!(rewound_again.entries[0].id, first_user.id());
}

/// 重新作答前置：含调用的 assistant 消息保留，ask 的结果与之后全部截掉。
#[tokio::test]
async fn prepare_reask_keeps_call_drops_result_and_tail() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let info = table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            SessionOrigin::root(),
            false,
        )
        .await
        .expect("创建会话");
    table
        .append_message(&info.id, Message::user_text("提问"), None, None)
        .await
        .expect("用户消息");
    // 先一个已收场的普通调用：它的结果在 ask 之前，截断后保留。
    table
        .append_message(
            &info.id,
            Message {
                role: Role::Assistant,
                content: moc_llm::message_content![ContentPart::ToolCall(ToolCall::new(
                    "call_read",
                    "read",
                    "{}",
                ))],
            },
            None,
            None,
        )
        .await
        .expect("read 调用");
    table
        .append_message(
            &info.id,
            Message::tool_result("call_read", "读完了", false),
            None,
            None,
        )
        .await
        .expect("read 结果");
    // 含 ask 调用的 assistant 消息；同消息中 ask 之后还有一个兄弟调用，
    // 它的结果随尾部被砍，回到「未回答」状态交给 pending 修补。
    table
        .append_message(
            &info.id,
            Message {
                role: Role::Assistant,
                content: moc_llm::message_content![
                    ContentPart::ToolCall(ToolCall::new(
                        "call_ask",
                        "ask",
                        r#"{"questions":[{"id":"q","question":"选哪个？","options":[{"label":"A"},{"label":"B"}]}]}"#,
                    )),
                    ContentPart::ToolCall(ToolCall::new("call_after", "edit", "{}")),
                ],
            },
            None,
            None,
        )
        .await
        .expect("ask 调用");
    for (id, content) in [("call_ask", "A"), ("call_after", "写完了")] {
        table
            .append_message(
                &info.id,
                Message::tool_result(id, content, false),
                None,
                None,
            )
            .await
            .expect("工具结果");
    }
    table
        .append_message(
            &info.id,
            Message::assistant_text("按你的选择做完了"),
            None,
            None,
        )
        .await
        .expect("后续回复");

    let call = table
        .prepare_reask(&info.id, "call_ask")
        .await
        .expect("截回未回答状态");
    assert_eq!(call.id, "call_ask");
    assert_eq!(call.name, "ask");
    // 保留到 ask 结果之前：user、read 调用+结果、含 ask 的消息，共 4 条。
    let loaded = table.load(&info.id, None).await.expect("重读会话");
    assert_eq!(loaded.entries.len(), 4);
    let last = loaded.entries.last().expect("末尾消息");
    assert_eq!(last.message.role, Role::Assistant);
    assert!(last.message.tool_calls().any(|call| call.id == "call_ask"));
    // 截断已落盘：换一张表重读，ask 之后的尾部确实不在。
    let reloaded = SessionTable::new(directory.path().join("sessions"), true)
        .load(&info.id, None)
        .await
        .expect("从磁盘重读");
    assert_eq!(reloaded.entries.len(), 4);
    // 新答案落库后尾部是 Tool 消息，retry 可以放行（turn_completed 为假）。
    table
        .append_message(
            &info.id,
            Message::tool_result("call_ask", "B", false),
            None,
            None,
        )
        .await
        .expect("新答案落库");
    let entries = table
        .get(&info.id)
        .await
        .unwrap()
        .state
        .lock()
        .await
        .log_entries();
    assert!(!crate::service::prompt::turn_completed(&entries));
}

/// 重新作答只接受已收场的 ask 调用：非 ask、没有结果、不存在的调用都拒绝。
#[tokio::test]
async fn prepare_reask_rejects_non_ask_unsettled_and_unknown_calls() {
    moc_i18n::set_locale("zh");
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let info = table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            SessionOrigin::root(),
            false,
        )
        .await
        .expect("创建会话");
    table
        .append_message(
            &info.id,
            Message {
                role: Role::Assistant,
                content: moc_llm::message_content![
                    ContentPart::ToolCall(ToolCall::new("call_read", "read", "{}")),
                    ContentPart::ToolCall(ToolCall::new("call_ask", "ask", "{}")),
                ],
            },
            None,
            None,
        )
        .await
        .expect("调用消息");
    table
        .append_message(
            &info.id,
            Message::tool_result("call_read", "读完了", false),
            None,
            None,
        )
        .await
        .expect("read 结果");

    let error = table
        .prepare_reask(&info.id, "call_read")
        .await
        .expect_err("非 ask 调用必须拒绝");
    assert!(error.to_string().contains("ask"), "{error}");
    let error = table
        .prepare_reask(&info.id, "call_ask")
        .await
        .expect_err("还没结果的提问必须拒绝");
    assert!(error.to_string().contains("还没有结果"), "{error}");
    let error = table
        .prepare_reask(&info.id, "call_missing")
        .await
        .expect_err("不存在的调用必须拒绝");
    assert!(error.to_string().contains("不存在"), "{error}");
}

#[tokio::test]
async fn redo_reattaches_rewound_tail_until_new_history_lands() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let info = table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            SessionOrigin::root(),
            false,
        )
        .await
        .expect("创建会话");
    let u1 = table
        .append_message(&info.id, Message::user_text("一"), None, None)
        .await
        .expect("一");
    table
        .append_message(&info.id, Message::assistant_text("答一"), None, None)
        .await
        .expect("答一");
    let u2 = table
        .append_message(&info.id, Message::user_text("二"), None, None)
        .await
        .expect("二");
    let a2 = table
        .append_message(&info.id, Message::assistant_text("答二"), None, None)
        .await
        .expect("答二");
    let u3 = table
        .append_message(&info.id, Message::user_text("三"), None, None)
        .await
        .expect("三");
    let a3 = table
        .append_message(&info.id, Message::assistant_text("答三"), None, None)
        .await
        .expect("答三");

    // 回退到 u2：尾部 [u2, a2, u3, a3] 压栈。
    table
        .rewind(&info.id, &TranscriptEntryId::new(u2.id()))
        .await
        .expect("回退到 u2");
    // 分次前滚：先到 a2，剩余 [u3, a3] 留在栈上并可被分页读取。
    let partial = table
        .redo(&info.id, &TranscriptEntryId::new(a2.id()))
        .await
        .expect("前滚到 a2");
    assert_eq!(partial.session.id, info.id);
    assert_eq!(partial.entries.len(), 4);
    assert_eq!(partial.entries[0].id, u1.id());
    let page = table
        .load_page(&info.id, None, 32, None)
        .await
        .expect("分页");
    assert_eq!(page.redo_entries.len(), 2);
    assert!(page.redo_tip_entry_id.is_none(), "旧请求不增加结果字段");
    let mut redo_page = moc_protocol::methods::session::RedoPageParams::default();
    let metadata = table
        .load_pages(&info.id, None, 1, Some(1024), Some(&redo_page), None, false)
        .await
        .unwrap();
    assert!(metadata.redo_entries.is_empty());
    assert_eq!(metadata.redo_tip_entry_id.as_deref(), Some(a3.id()));
    redo_page.max_entries = 1;
    let latest = table
        .load_pages(&info.id, None, 1, Some(1024), Some(&redo_page), None, false)
        .await
        .unwrap();
    assert_eq!(latest.redo_entries[0].id, a3.id());
    assert!(latest.redo_has_more);
    redo_page.before_entry_id = Some(a3.id().to_owned());
    let older = table
        .load_pages(&info.id, None, 1, Some(1024), Some(&redo_page), None, false)
        .await
        .unwrap();
    assert_eq!(older.redo_entries[0].id, u3.id());
    assert!(!older.redo_has_more);
    assert_eq!(older.redo_entries[0].message, page.redo_entries[0].message);
    // 剩余尾部整体接回，条目保持原 id 与顺序。
    let full = table
        .redo(&info.id, &TranscriptEntryId::new(a3.id()))
        .await
        .expect("前滚到 a3");
    assert_eq!(full.entries.len(), 6);
    assert_eq!(full.entries[4].id, u3.id());
    assert_eq!(full.entries[5].id, a3.id());
    assert!(
        table
            .load_page(&info.id, None, 32, None)
            .await
            .expect("分页")
            .redo_entries
            .is_empty()
    );
    // 接回的内容已落盘：换一张表重读仍在。
    let reloaded = SessionTable::new(directory.path().join("sessions"), true)
        .load(&info.id, None)
        .await
        .expect("磁盘重读");
    assert_eq!(reloaded.entries.len(), 6);
    // 栈空后再前滚报错。
    assert!(
        table
            .redo(&info.id, &TranscriptEntryId::new(a3.id()))
            .await
            .is_err(),
        "栈空后前滚必须报错"
    );
    // 再次回退后写入新历史：废弃时间线断链，前滚被拒。
    table
        .rewind(&info.id, &TranscriptEntryId::new(u3.id()))
        .await
        .expect("再回退");
    table
        .append_message(&info.id, Message::user_text("新提问"), None, None)
        .await
        .expect("新历史");
    assert!(
        table
            .redo(&info.id, &TranscriptEntryId::new(u3.id()))
            .await
            .is_err(),
        "新历史落地后前滚必须报错"
    );
}

#[tokio::test]
async fn redo_stack_survives_nested_rewinds() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let info = table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            SessionOrigin::root(),
            false,
        )
        .await
        .expect("创建会话");
    let u1 = table
        .append_message(&info.id, Message::user_text("一"), None, None)
        .await
        .expect("一");
    let a1 = table
        .append_message(&info.id, Message::assistant_text("答一"), None, None)
        .await
        .expect("答一");
    let u2 = table
        .append_message(&info.id, Message::user_text("二"), None, None)
        .await
        .expect("二");
    let a2 = table
        .append_message(&info.id, Message::assistant_text("答二"), None, None)
        .await
        .expect("答二");

    // 连续两次回退压两层：[[u2,a2], [u1,a1]]。
    table
        .rewind(&info.id, &TranscriptEntryId::new(u2.id()))
        .await
        .expect("回退到 u2");
    table
        .rewind(&info.id, &TranscriptEntryId::new(u1.id()))
        .await
        .expect("回退到 u1");
    let page = table
        .load_page(&info.id, None, 32, None)
        .await
        .expect("分页");
    assert_eq!(page.redo_entries.len(), 2, "只有栈顶一层可前滚：{page:?}");
    // 先接回 [u1,a1]：栈顶换成 [u2,a2]，它仍接在新 tip 上。
    let mid = table
        .redo(&info.id, &TranscriptEntryId::new(a1.id()))
        .await
        .expect("前滚一层");
    assert_eq!(mid.entries.len(), 2);
    let full = table
        .redo(&info.id, &TranscriptEntryId::new(a2.id()))
        .await
        .expect("再前滚一层");
    assert_eq!(full.entries.len(), 4);
    assert_eq!(full.entries[0].id, u1.id());
    assert_eq!(full.entries[3].id, a2.id());
}

use super::*;

#[tokio::test]
async fn before_hook_can_deny_and_rewrite_arguments() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "guard",
        r#"
        export default {
          hooks: {
            beforeToolCall(event) {
              if (event.name === "bash") return { deny: "no shell" };
              return { arguments: { ...event.arguments, extra: 1 } };
            }
          }
        };
        "#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    assert_eq!(
        host.before_tool_call("bash", &serde_json::json!({}), &context())
            .await,
        BeforeToolCall::Deny("plugin `guard` denied this call: no shell".into())
    );
    assert_eq!(
        host.before_tool_call("read", &serde_json::json!({"path":"a"}), &context())
            .await,
        BeforeToolCall::Allow(serde_json::json!({"path":"a","extra":1}))
    );
}

#[tokio::test]
async fn prompt_contributions_are_capped() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "talky",
        r#"export default { prompt: () => "x".repeat(100) };"#,
    );
    let mut settings = settings();
    settings.max_prompt_chars = 10;
    let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
    assert_eq!(host.prompt_text(&context()).await, "x".repeat(10));
}

/// 多个插件的 `messageReceived` 按加载顺序串联，后一个看到前一个的改写。
#[tokio::test]
async fn message_received_chains_rewrites_in_load_order() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "first",
        r#"export default { hooks: { messageReceived(event) { return { text: event.text + "A" }; } } };"#,
    );
    install(
        temp.path(),
        "second",
        r#"export default { hooks: { messageReceived(event) { return { text: event.text + "B" }; } } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let rewrite = host
        .message_received(&serde_json::json!({"text": "x"}), &context())
        .await;
    assert_eq!(
        rewrite,
        MessageRewrite {
            text: Some("xAB".into())
        }
    );
}

/// `sessionStart` 可以注入初始消息；非法条目被跳过而不是停用插件。
#[tokio::test]
async fn session_start_collects_injected_messages() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "boot",
        r#"export default { hooks: { sessionStart() {
            return { messages: [
              { role: "system", text: "仓库约定" },
              { text: "第二条" },
              { role: "tool", text: "非法角色" },
              { role: "user", text: 42 }
            ] };
        } } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let injected = host
        .session_start(&serde_json::json!({"atMs": 1}), &context())
        .await;
    assert_eq!(
        injected,
        vec![
            InjectedMessage {
                role: "system".into(),
                text: "仓库约定".into()
            },
            InjectedMessage {
                role: "user".into(),
                text: "第二条".into()
            },
        ]
    );
    assert!(host.info()[0].error.is_none());
}

/// 权限代答只接受本次提供的一次性选项；会话级授权必须由人选择。
#[tokio::test]
async fn permission_hook_can_answer_once_but_not_grant_a_session() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "policy",
        r#"export default { hooks: {
            permissionRequested(event) {
              return { option: event.tool === "write" ? "allow_session" : "allow_once" };
            }
        } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let options = || {
        serde_json::json!([
            {"id": "allow_once", "label": "允许一次", "kind": "allow_once"},
            {"id": "allow_session", "label": "本会话允许", "kind": "allow_session"},
            {"id": "reject_once", "label": "拒绝", "kind": "reject_once"}
        ])
    };
    assert_eq!(
        host.permission_requested(
            &serde_json::json!({"tool": "read", "options": options()}),
            &context()
        )
        .await,
        Some("allow_once".into())
    );
    assert_eq!(
        host.permission_requested(
            &serde_json::json!({"tool": "write", "options": options()}),
            &context()
        )
        .await,
        None,
        "会话级授权不能被插件代答"
    );
}

/// `agentStarted` 可以改写子代理任务；空串被忽略。
#[tokio::test]
async fn agent_started_can_rewrite_the_task() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "tasker",
        r#"export default { hooks: {
            agentStarted(event) { return { task: event.task + "（附仓库约定）" }; }
        } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let task = host
        .agent_started(&serde_json::json!({"task": "调查"}), &context())
        .await;
    assert_eq!(task.as_deref(), Some("调查（附仓库约定）"));
}

/// `afterToolCall` 的改写会被串联；非字符串返回值忽略且不停用插件。
#[tokio::test]
async fn after_tool_call_chains_rewrites_and_ignores_bad_types() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "bracket",
        r#"export default { hooks: {
            afterToolCall(event) {
              return { content: "[" + event.content + "]", structuredContent: { seen: true } };
            }
        } };"#,
    );
    install(
        temp.path(),
        "upper",
        r#"export default { hooks: {
            afterToolCall(event) { return { content: event.content.toUpperCase(), structuredContent: null }; }
        } };"#,
    );
    install(
        temp.path(),
        "z-bad",
        r#"export default { hooks: {
            afterToolCall() { return { content: 42 }; }
        } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let result = ToolResult {
        content: "raw".into(),
        structured: Some(serde_json::json!({"raw": true})),
        is_error: false,
    };
    let rewrite = host
        .after_tool_call("read", &serde_json::json!({}), &result, &context())
        .await;
    assert_eq!(rewrite.content.as_deref(), Some("[RAW]"));
    assert_eq!(rewrite.structured, Some(Value::Null));
    // 最后一个插件返回非法类型，只告警，不影响已完成的改写，也不停用它。
    assert!(
        host.info().iter().all(|info| info.error.is_none()),
        "非法返回值不应停用插件：{:?}",
        host.info()
            .iter()
            .map(|info| (&info.name, &info.error))
            .collect::<Vec<_>>()
    );
}

/// `throw {code, message}`：对象里约定的 code 要生效，消息不能退化成 JSON 文字。
#[tokio::test]
async fn thrown_object_keeps_its_code_and_message() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "object",
        r#"export default { tools: { boom: { description: "抛错", parameters: { type: "object" }, run: () => { throw { code: "E_PLUGIN_DENIED", message: "no" }; } } } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let error = host
        .call_tool(
            "boom",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect_err("必须失败");
    assert_eq!(error.code, "E_PLUGIN_DENIED");
    assert_eq!(error.message, "no");
}

/// 钩子抛异常：本次调用 fail-open 放行，同时插件被停用并通知。
#[tokio::test]
async fn hook_failure_fail_opens_but_disables_and_notifies() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "guard",
        r#"export default { hooks: { beforeToolCall() { throw new Error("hook boom"); } } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let verdict = host
        .before_tool_call("read", &serde_json::json!({}), &context())
        .await;
    assert_eq!(verdict, BeforeToolCall::Allow(serde_json::json!({})));
    assert!(host.info()[0].error.is_some(), "钩子失败后插件停用");
    let notices = host.drain_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].code, "E_PLUGIN");
}

#[tokio::test]
async fn rejected_promise_reports_its_message() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "boom",
        r#"export default { tools: { boom: { description: "抛错", parameters: { type: "object" }, async run() { throw new Error("boom"); } } } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let error = host
        .call_tool(
            "boom",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect_err("必须失败");
    assert!(error.message.contains("boom"), "{error}");
}

/// `agents.registerPreset` 顶层调用按声明收集；allow/deny 互斥，坏声明
/// 以插件错误呈现而非静默跳过。
#[tokio::test]
async fn agent_preset_declarations_collect_top_level_registrations() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "presets",
        r#"
        import { agents } from 'moc';
        agents.registerPreset({
            name: 'scout',
            description: '只读调查',
            systemPrompt: '只读侦查，返回 path:line 证据。',
            tools: { deny: ['edit', 'write', 'spawn_agent'] },
        });
        agents.registerPreset({
            name: 'narrow',
            description: '限定工具的调查角色',
            systemPrompt: '窄面预设。',
            tools: { allow: ['read', 'grep'] },
        });
        agents.registerPreset({ name: 'plain', description: '用户配置的通用角色', systemPrompt: '无工具限制。' });
        export default {};
        "#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    assert_eq!(host.info()[0].error, None);
    let presets = host.agent_preset_declarations();
    assert_eq!(presets.len(), 3);
    assert_eq!(
        presets["scout"].deny,
        Some(vec!["edit".into(), "write".into(), "spawn_agent".into()])
    );
    assert_eq!(
        presets["narrow"].allow,
        Some(vec!["read".into(), "grep".into()])
    );
    assert!(presets["plain"].allow.is_none() && presets["plain"].deny.is_none());
}

#[tokio::test]
async fn agent_preset_declaration_rejects_allow_and_deny_together() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "bad",
        r#"
        import { agents } from 'moc';
        agents.registerPreset({
            name: 'both',
            description: '互斥配置校验',
            systemPrompt: 'x',
            tools: { allow: ['read'], deny: ['write'] },
        });
        export default {};
        "#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let info = host.info();
    let error = info[0].error.as_ref().expect("互斥声明必须报错");
    assert!(
        error.contains("allow") && error.contains("deny"),
        "错误必须点名互斥字段：{error}"
    );
    assert!(host.agent_preset_declarations().is_empty());
}

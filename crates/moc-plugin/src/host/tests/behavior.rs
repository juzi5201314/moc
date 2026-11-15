use super::*;

#[tokio::test]
async fn invalid_json_cannot_be_silently_coerced_or_caught_to_keep_plugin_alive() {
    for value in [
        "NaN",
        "Infinity",
        "undefined",
        "() => 1",
        "[,,]",
        "{x:undefined}",
        "new Date()",
        "({get x(){ return 1; }})",
        "(() => { const x = {}; x.x=x; return x; })()",
    ] {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "bad",
            &format!(
                "import {{session}} from 'moc'; export default {{commands:{{bad:{{description:'bad',run(){{try {{session.state.x={value};}} catch {{}} return 'caught';}}}}}}}};"
            ),
        );
        let host = PluginHost::start_with_persistence(
            temp.path(),
            temp.path(),
            &settings(),
            &[],
            Arc::new(MemoryPersistence::default()),
        )
        .await;
        let error = host
            .run_command("bad", "", &context())
            .await
            .expect_err(value);
        assert_eq!(error.code, "E_PLUGIN_VALUE", "{value}: {error}");
        assert!(host.info()[0].error.is_some(), "{value}");
    }
}

/// 只有声明过的插件会收到事件；未声明的钩子不会触发任何 JS 调用。
#[tokio::test]
async fn lifecycle_events_only_reach_plugins_that_declare_them() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "watcher",
        r#"
        let starts = 0;
        export default {
          hooks: { sessionStart(event) { starts += event.atMs > 0 ? 1 : 0; } },
          commands: { starts: { description: "读取计数", run: () => starts } }
        };
        "#,
    );
    install(temp.path(), "silent", r#"export default {};"#);
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    assert!(host.has_hook(crate::descriptor::HookKind::SessionStart));
    assert!(!host.has_hook(crate::descriptor::HookKind::AfterPrompt));
    host.emit(
        crate::descriptor::HookKind::SessionStart,
        serde_json::json!({"atMs": 1}),
        &context(),
    )
    .await;
    // 未声明的观察型钩子不会调用插件，计数保持 1。
    host.emit(
        crate::descriptor::HookKind::AfterPrompt,
        serde_json::json!({"atMs": 2}),
        &context(),
    )
    .await;
    assert_eq!(
        host.run_command("starts", "", &context()).await.unwrap(),
        "1"
    );
}

#[tokio::test]
async fn broken_plugin_is_listed_with_its_error() {
    let temp = tempfile::tempdir().unwrap();
    install(temp.path(), "broken", "export default { nope: 1 };");
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let info = host.info();
    assert_eq!(info.len(), 1);
    assert!(
        info[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("E_PLUGIN_DESCRIPTOR")
    );
    assert!(host.tools().is_empty());
    // 加载失败也必须通知：否则用户只有手动 /plugins 才发现插件没起来。
    let notices = host.drain_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].plugin, "broken");
    assert_eq!(notices[0].code, "E_PLUGIN_DESCRIPTOR");
}

/// 插件自己抛异常按故障处理：本进程内停用，且只通知一次。
#[tokio::test]
async fn thrown_error_disables_the_plugin_and_notifies_once() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "alpha",
        r#"export default { tools: {
            boom: { description: "抛错", parameters: { type: "object" }, run: () => { throw new Error("bad input"); } },
            fine: { description: "正常", parameters: { type: "object" }, run: () => "ok" }
        } };"#,
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
    assert_eq!(error.code, "E_PLUGIN");
    assert_eq!(error.message, "bad input");
    // 同插件的其它工具也停用：再调用明确说「已禁用」，而不是复读原错误。
    let again = host
        .call_tool(
            "fine",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect_err("已禁用");
    assert_eq!(again.code, "E_PLUGIN_DISABLED");
    assert!(again.message.contains("bad input"), "{}", again.message);
    let notices = host.drain_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].code, "E_PLUGIN");
    assert!(
        notices[0].message.starts_with("bad input\n    at run ("),
        "{}",
        notices[0].message
    );
}

/// `throw "文本"` 的消息不带 JSON 引号。
#[tokio::test]
async fn thrown_string_is_plain_text() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "text",
        r#"export default { tools: { boom: { description: "抛错", parameters: { type: "object" }, run: () => { throw "plain text"; } } } };"#,
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
    assert_eq!(error.message, "plain text");
}

/// 调用栈只进通知与详情（给人排查看），不进模型可见的单行消息。
#[tokio::test]
async fn stack_detail_reaches_the_notice_only() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "stack",
        r#"export default { tools: { boom: { description: "抛错", parameters: { type: "object" }, run: () => { const inner = () => { throw new Error("boom"); }; return inner(); } } } };"#,
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
    assert_eq!(error.message, "boom");
    let detail = error.detail.expect("异常要带栈");
    assert!(!detail.is_empty());
    let notices = host.drain_notices();
    assert!(
        notices[0].message.lines().count() > 1,
        "通知里要能看到栈：{}",
        notices[0].message
    );
}

/// 异步回调必须等到落定再取值，不能把 Promise 序列化成 `{}`。
#[tokio::test]
async fn async_callbacks_are_awaited() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "demo",
        r#"
        export default {
          async activate() { await Promise.resolve(); },
          tools: { demo: { description: "异步工具", parameters: { type: "object" }, async run() { return "tool"; } } },
          commands: { demo: { description: "异步命令", async run() { return "command"; } } },
          hooks: { async beforeToolCall() { return { deny: "blocked" }; } },
          async prompt() { return "prompt text"; }
        };
        "#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    assert_eq!(host.info()[0].error, None);
    assert_eq!(
        host.call_tool(
            "demo",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge)
        )
        .await
        .unwrap()
        .content,
        "tool"
    );
    assert_eq!(
        host.run_command("demo", "", &context()).await.unwrap(),
        "command"
    );
    assert_eq!(
        host.before_tool_call("write", &serde_json::json!({}), &context())
            .await,
        BeforeToolCall::Deny("plugin `demo` denied this call: blocked".into())
    );
    assert_eq!(host.prompt_text(&context()).await, "prompt text");
}

#[tokio::test]
async fn pending_promise_without_timers_fails_fast() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "hang",
        r#"export default { tools: { hang: { description: "永不落定", parameters: { type: "object" }, async run() { await new Promise(() => {}); } } } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let started = std::time::Instant::now();
    let error = host
        .call_tool(
            "hang",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect_err("必须报错");
    assert_eq!(error.code, "E_PLUGIN_STATE");
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "不应等满 call_timeout"
    );
}

/// 宿主工具再慢也不能吃掉调用预算：到点就返回，且不因此禁用插件。
#[tokio::test]
async fn slow_host_bridge_respects_the_call_budget() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "reader",
        r#"import { tools } from "moc"; export default { tools: { read_through: { description: "读文件", parameters: { type: "object" }, run: () => tools.call("read", { path: "x" }) } } };"#,
    );
    let mut settings = settings();
    settings.call_timeout = Duration::from_millis(20);
    let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
    let started = std::time::Instant::now();
    let error = host
        .call_tool(
            "read_through",
            serde_json::json!({"path": "x"}),
            &context(),
            Arc::new(SlowBridge(Duration::from_secs(3600))),
        )
        .await
        .expect_err("必须超时");
    assert_eq!(error.code, "E_PLUGIN_TIMEOUT");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "桥调用不能拖住超时"
    );
    // 等宿主的这条路径不算插件跑飞：它仍然可用。
    assert_eq!(host.info()[0].error, None);
}

#[test]
fn fingerprint_tracks_directory_changes() {
    let temp = tempfile::tempdir().unwrap();
    install(temp.path(), "demo", "export default {};");
    let settings = settings();
    let first = fingerprint(temp.path(), temp.path(), &settings);
    install(temp.path(), "other", "export default {};");
    let second = fingerprint(temp.path(), temp.path(), &settings);
    assert_ne!(first, second, "新增插件目录必须改变指纹");
    std::fs::write(
        temp.path().join(".moc/plugins/demo/index.js"),
        "export default { prompt: () => 1 };",
    )
    .unwrap();
    let third = fingerprint(temp.path(), temp.path(), &settings);
    assert_ne!(second, third, "模块改动必须改变指纹");

    // 多文件插件：子目录里的模块也算运行时内容。
    let lib = temp.path().join(".moc/plugins/demo/lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("util.js"), "export const n = 1;").unwrap();
    let fourth = fingerprint(temp.path(), temp.path(), &settings);
    assert_ne!(third, fourth, "新增子目录模块必须改变指纹");
}

/// 宿主记下的 stamp 必须与外部算的指纹同源，否则 core 永远判不出目录变过。
#[tokio::test]
async fn host_stamp_matches_the_external_fingerprint() {
    let temp = tempfile::tempdir().unwrap();
    install(temp.path(), "demo", "export default {};");
    let settings = settings();
    let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
    assert_eq!(
        host.stamp(),
        fingerprint(temp.path(), temp.path(), &settings),
        "加载时的 stamp 必须等于同一份目录的指纹"
    );
    std::fs::write(
        temp.path().join(".moc/plugins/demo/index.js"),
        "export default { prompt: () => 'changed' };",
    )
    .unwrap();
    assert_ne!(
        host.stamp(),
        fingerprint(temp.path(), temp.path(), &settings),
        "文件改动后指纹必须变化"
    );
}

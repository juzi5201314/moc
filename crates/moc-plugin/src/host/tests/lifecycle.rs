use super::*;

#[tokio::test]
async fn loads_a_project_plugin_and_exposes_every_surface() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "demo",
        r#"
        export default {
          tools: { hello: { description: "打招呼", parameters: { type: "object" } , run: () => "hi" } },
          commands: { demo: { description: "示例命令", usage: "[x]", run: args => `arg=${args}` } },
          hooks: { beforeToolCall: () => null },
          prompt: () => "demo prompt"
        };
        "#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    assert_eq!(host.info().len(), 1);
    assert_eq!(host.info()[0].error, None);
    assert_eq!(host.tools().len(), 1);
    assert_eq!(host.commands()[0].1.name, "demo");

    let result = host
        .call_tool(
            "hello",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .unwrap();
    assert_eq!(result.content, "hi");
    assert_eq!(
        host.run_command("demo", "ignored", &context())
            .await
            .unwrap(),
        "arg=ignored"
    );
    assert_eq!(host.prompt_text(&context()).await, "demo prompt");
}

#[tokio::test]
async fn timeout_poisons_the_plugin_for_the_process() {
    rust_i18n::set_locale("zh");
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "spin",
        r#"export default { tools: { spin: { description: "死循环", parameters: { type: "object" }, run: () => { while (true) {} } } } };"#,
    );
    // 另一个插件必须不受影响：超时只禁用出问题的那个。
    install(
        temp.path(),
        "steady",
        r#"export default { tools: { steady: { description: "正常工具", parameters: { type: "object" }, run: () => "ok" } } };"#,
    );
    let mut settings = settings();
    settings.call_timeout = Duration::from_millis(200);
    let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
    let error = host
        .call_tool(
            "spin",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect_err("必须超时");
    assert_eq!(error.code, "E_PLUGIN_TIMEOUT");
    // 之后立即失败，不再等一轮超时；且明确说明插件已停用、首次失败是什么。
    let again = host
        .call_tool(
            "spin",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect_err("已禁用");
    assert_eq!(again.code, "E_PLUGIN_DISABLED");
    assert!(again.message.contains("首次失败"), "{}", again.message);
    let notices = host.drain_notices();
    assert_eq!(notices.len(), 1, "禁用只通知一次：{notices:?}");
    assert_eq!(notices[0].plugin, "spin");
    assert_eq!(notices[0].code, "E_PLUGIN_TIMEOUT");
    assert!(host.drain_notices().is_empty(), "通知取走后不再重复");
    let info = host.info();
    let spin = info
        .iter()
        .find(|plugin| plugin.name == "spin")
        .expect("有 spin");
    assert!(spin.error.is_some());
    let steady = info
        .iter()
        .find(|plugin| plugin.name == "steady")
        .expect("有 steady");
    assert_eq!(steady.error, None);
    let result = host
        .call_tool(
            "steady",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect("另一个插件仍可用");
    assert_eq!(result.content, "ok");
}

#[tokio::test]
async fn cancelling_a_call_aborts_its_host_task() {
    struct PendingBridge {
        started: tokio::sync::Notify,
        stopped: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl HostBridge for PendingBridge {
        fn call_tool<'a>(
            &'a self,
            _name: String,
            _args: Value,
        ) -> Pin<Box<dyn Future<Output = Result<ToolResult>> + Send + 'a>> {
            Box::pin(async move {
                // sender 的释放是任务已被回收的信号，不靠 sleep 猜测执行进度。
                let _stopped = self.stopped.lock().take().unwrap();
                self.started.notify_one();
                std::future::pending().await
            })
        }
    }

    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "reader",
        r#"import { tools } from "moc"; export default { tools: {
          read_through: { description: "Read through the host", parameters: { type: "object" }, parallel: true,
            run() { return tools.call("read", {}); }
          }
        } };"#,
    );
    let mut settings = settings();
    settings.call_timeout = Duration::from_secs(30);
    let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
    let (stopped, released) = tokio::sync::oneshot::channel();
    let bridge = Arc::new(PendingBridge {
        started: tokio::sync::Notify::new(),
        stopped: Mutex::new(Some(stopped)),
    });
    let context = context();
    let mut invocation = Box::pin(host.call_tool(
        "read_through",
        serde_json::json!({}),
        &context,
        bridge.clone(),
    ));
    tokio::select! {
        result = &mut invocation => panic!("宿主回调应保持挂起：{result:?}"),
        _ = bridge.started.notified() => {},
        _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("宿主回调应启动"),
    }
    drop(invocation);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), released)
            .await
            .expect("取消必须回收宿主任务")
            .is_err()
    );
}

#[tokio::test]
async fn cancelled_or_timed_out_persistence_finishes_writing() {
    struct PausedPersistence {
        dir: std::path::PathBuf,
        started: tokio::sync::Notify,
        resume: tokio::sync::Notify,
        finished: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl PersistenceBridge for PausedPersistence {
        fn request<'a>(
            &'a self,
            plugin: &'a str,
            _path: &'a Path,
            _context: Option<&'a CallContext>,
            op: PersistenceOp,
        ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
            Box::pin(async move {
                match op {
                    PersistenceOp::ConfigSnapshot => Ok(serde_json::json!({})),
                    PersistenceOp::StateSet { key, value } => {
                        let finished = self.finished.lock().take().unwrap();
                        let temp = self.dir.join(format!(".{plugin}.tmp-test"));
                        std::fs::write(&temp, serde_json::json!({key: value}).to_string()).unwrap();
                        // 固定停在临时文件落盘后、rename 前，取消与超时都不依赖调度竞速。
                        self.started.notify_one();
                        self.resume.notified().await;
                        std::fs::rename(temp, self.dir.join(format!("{plugin}.json"))).unwrap();
                        let _ = finished.send(());
                        Ok(Value::Null)
                    }
                    _ => panic!("未预期的持久化操作：{op:?}"),
                }
            })
        }
    }

    for cancel in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "writer",
            r#"export default { commands: { save: { description: "Save state",
              run(_args, ctx) { ctx.state.count = 1; }
            } } };"#,
        );
        let (finished, completed) = tokio::sync::oneshot::channel();
        let bridge = Arc::new(PausedPersistence {
            dir: temp.path().to_owned(),
            started: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
            finished: Mutex::new(Some(finished)),
        });
        let mut settings = settings();
        settings.call_timeout = if cancel {
            Duration::from_secs(30)
        } else {
            Duration::from_millis(200)
        };
        let host = PluginHost::start_with_persistence(
            temp.path(),
            temp.path(),
            &settings,
            &[],
            bridge.clone(),
        )
        .await;
        let context = context();
        let mut invocation = Box::pin(host.run_command("save", "", &context));
        tokio::select! {
            result = &mut invocation => panic!("持久化操作应保持挂起：{result:?}"),
            _ = bridge.started.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("持久化操作应启动"),
        }
        assert!(temp.path().join(".writer.tmp-test").exists());
        if cancel {
            drop(invocation);
        } else {
            assert_eq!(invocation.await.unwrap_err().code, "E_PLUGIN_TIMEOUT");
        }
        bridge.resume.notify_one();
        tokio::time::timeout(Duration::from_secs(2), completed)
            .await
            .expect("持久化操作应完成")
            .expect("持久化任务不应被中止");
        assert!(!temp.path().join(".writer.tmp-test").exists());
        let saved: Value =
            serde_json::from_slice(&std::fs::read(temp.path().join("writer.json")).unwrap())
                .unwrap();
        assert_eq!(saved, serde_json::json!({"count": 1}));
    }
}

/// 仓库里的示例插件必须始终可加载：它是文档承诺的最小可运行形态。
#[tokio::test]
async fn example_plugin_loads_and_answers() {
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins");
    if !examples.is_dir() {
        return;
    }
    let mut settings = settings();
    settings.discovery.user = false;
    settings.discovery.project = false;
    settings.discovery.dirs = vec![examples];
    let host = PluginHost::start(
        tempfile::tempdir().unwrap().path(),
        tempfile::tempdir().unwrap().path(),
        &settings,
        &[],
    )
    .await;
    let info = host.info();
    assert_eq!(info.len(), 1, "示例插件应被加载");
    assert_eq!(info[0].error, None, "{:?}", info[0].error);
    assert_eq!(info[0].name, "hello");
    let result = host
        .call_tool(
            "hello",
            serde_json::json!({"name": "moc"}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect("示例工具可调用");
    assert!(
        result.content.starts_with("你好，moc！"),
        "{}",
        result.content
    );
    let text = host
        .run_command("hello", "soeur", &context())
        .await
        .unwrap();
    assert!(text.contains("hello soeur"), "{text}");
    assert!(host.prompt_text(&context()).await.contains("hello"));
}

#[tokio::test]
async fn disabled_plugins_are_not_loaded() {
    let temp = tempfile::tempdir().unwrap();
    install(temp.path(), "hidden", "export default {};");
    let mut settings = settings();
    settings.disable = vec!["hidden".into()];
    let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
    assert!(host.is_empty());
}

/// 模块求值与 `activate` 都必须受 `load_timeout` 约束：一次死循环不能永久卡住提示。
#[tokio::test]
async fn load_timeout_stops_a_spinning_plugin() {
    for body in [
        "while (true) {} export default {};",
        "export default { activate() { while (true) {} } };",
    ] {
        let temp = tempfile::tempdir().unwrap();
        install(temp.path(), "spin", body);
        let mut settings = settings();
        settings.load_timeout = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{body}: 加载必须被 load_timeout 打断"
        );
        let info = host.info();
        assert_eq!(info.len(), 1);
        let error = info[0].error.as_deref().unwrap_or_default();
        assert!(error.contains("E_PLUGIN_TIMEOUT"), "{body} -> {error}");
        assert!(host.tools().is_empty());
    }
}

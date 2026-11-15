use super::*;
use crate::ModeMask;
use serde_json::json;

fn limits(call: Duration) -> Limits {
    Limits {
        memory_limit_bytes: 32 * 1024 * 1024,
        call_timeout: call,
        load_timeout: Duration::from_secs(2),
    }
}

fn write_plugin(dir: &Path, main: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        format!(r#"{{"name":"demo","main":"{main}"}}"#),
    )
    .unwrap();
    std::fs::write(dir.join(main), body).unwrap();
}

async fn load(dir: &Path, call: Duration) -> (Worker, Descriptor) {
    let (worker, ready) = spawn(
        "demo".into(),
        dir.to_path_buf(),
        dir.join("index.js"),
        limits(call),
    );
    let descriptor = ready.await.expect("插件线程存活").expect("插件加载成功");
    (worker, descriptor)
}

async fn call(
    worker: &Worker,
    op: CallOp,
    payload: Value,
    session: &str,
    tools_allowed: bool,
    timeout: Duration,
) -> Result<Value> {
    let (reply, ready) = tokio::sync::oneshot::channel();
    worker.call(CallRequest {
        op,
        payload,
        context: session.to_owned(),
        tools_allowed,
        deadline: Instant::now() + timeout,
        host_tx: None,
        reply,
    })?;
    ready.await.expect("调用方存活")
}

const SESSION: &str = r#"{"sessionId":"s1","cwd":"/tmp","mode":"standard","permission":"full","model":"m","isSubagent":false}"#;

#[tokio::test]
async fn evaluates_tools_commands() {
    let temp = tempfile::tempdir().unwrap();
    write_plugin(
        temp.path(),
        "index.js",
        r#"
        import { session } from "moc";
        export default {
          tools: {
            greet: {
              description: "打招呼",
              parameters: { type: "object", properties: { name: { type: "string" } } },
              run(input, ctx) { return `hi ${input.name} from ${ctx.model}`; }
            }
          },
          commands: { echo: { description: "回显", run(args) { return `echo:${args}`; } } },
          prompt(ctx) { return `plugin prompt for ${ctx.mode}`; }
        };
        "#,
    );
    let (worker, descriptor) = load(temp.path(), Duration::from_secs(2)).await;
    assert_eq!(descriptor.tools.len(), 1);
    assert_eq!(descriptor.tools[0].modes, ModeMask::ALL);
    assert_eq!(descriptor.commands[0].name, "echo");
    assert!(descriptor.has_prompt);

    let value = call(
        &worker,
        CallOp::Tool,
        json!({"name":"greet","args":{"name":"moc"}}),
        SESSION,
        true,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(value, json!("hi moc from m"));

    let value = call(
        &worker,
        CallOp::Command,
        json!({"name":"echo","args":"abc"}),
        SESSION,
        false,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(value, json!("echo:abc"));

    let value = call(
        &worker,
        CallOp::Prompt,
        Value::Null,
        SESSION,
        false,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(value, json!("plugin prompt for standard"));
}

#[tokio::test]
async fn tool_modes_filter_the_declared_surface() {
    let temp = tempfile::tempdir().unwrap();
    write_plugin(
        temp.path(),
        "index.js",
        r#"
        export default {
          tools: {
            plan_only: {
              description: "只在计划模式可见",
              parameters: { type: "object" },
              modes: ["plan"],
              run() { return "ok"; }
            },
            everywhere: {
              description: "默认全开",
              parameters: { type: "object" },
              run() { return "ok"; }
            }
          }
        };
        "#,
    );
    let (_worker, descriptor) = load(temp.path(), Duration::from_secs(2)).await;
    assert_eq!(descriptor.tool("plan_only").unwrap().modes, ModeMask::PLAN);
    assert_eq!(descriptor.tool("everywhere").unwrap().modes, ModeMask::ALL);
}

#[tokio::test]
async fn rejects_unknown_tool_fields() {
    let temp = tempfile::tempdir().unwrap();
    for (index, field) in ["mode", "unexpected"].into_iter().enumerate() {
        let dir = temp.path().join(format!("case{index}"));
        write_plugin(
            &dir,
            "index.js",
            &format!(
                "export default {{ tools: {{ x: {{ description: \"x\", parameters: {{ type: \"object\" }}, {field}: [\"default\"], run: () => \"ok\" }} }} }};"
            ),
        );
        let (_worker, ready) = spawn(
            "demo".into(),
            dir.clone(),
            dir.join("index.js"),
            limits(Duration::from_secs(1)),
        );
        let error = ready
            .await
            .expect("插件线程存活")
            .expect_err("未知字段应加载失败");
        assert_eq!(error.code, "E_PLUGIN_DESCRIPTOR");
        assert!(error.message.contains(field), "{error}");
    }
}

#[tokio::test]
async fn rejects_invalid_tool_modes() {
    let temp = tempfile::tempdir().unwrap();
    for (index, modes) in [
        r#"["ptc"]"#,
        r#"[]"#,
        r#""plan""#,
        "null",
        r#"["plan", 1]"#,
        r#"{ toJSON: () => ["plan"] }"#,
    ]
    .into_iter()
    .enumerate()
    {
        let dir = temp.path().join(format!("case{index}"));
        write_plugin(
            &dir,
            "index.js",
            &format!(
                "export default {{ tools: {{ x: {{ description: \"x\", parameters: {{ type: \"object\" }}, modes: {modes}, run: () => \"ok\" }} }} }};"
            ),
        );
        let (_worker, ready) = spawn(
            "demo".into(),
            dir.clone(),
            dir.join("index.js"),
            limits(Duration::from_secs(1)),
        );
        let error = ready
            .await
            .expect("插件线程存活")
            .expect_err("非法 modes 应加载失败");
        assert_eq!(error.code, "E_PLUGIN_DESCRIPTOR", "modes={modes}");
    }
}

#[tokio::test]
async fn hooks_receive_events_and_can_return_rewrites() {
    let temp = tempfile::tempdir().unwrap();
    write_plugin(
        temp.path(),
        "index.js",
        r#"
        export default {
          hooks: {
            beforeToolCall(event) {
              if (event.name === "blocked") return { deny: "不允许调用" };
              return { arguments: { ...event.arguments, injected: true } };
            }
          }
        };
        "#,
    );
    let (worker, descriptor) = load(temp.path(), Duration::from_secs(2)).await;
    assert!(descriptor.has_hook(HookKind::BeforeToolCall));
    let value = call(
        &worker,
        CallOp::Hook(HookKind::BeforeToolCall),
        json!({"name":"read","arguments":{"path":"a"}}),
        SESSION,
        false,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(value["arguments"]["injected"], json!(true));
    let value = call(
        &worker,
        CallOp::Hook(HookKind::BeforeToolCall),
        json!({"name":"blocked","arguments":{}}),
        SESSION,
        false,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(value["deny"], json!("不允许调用"));
}

#[tokio::test]
async fn tool_calls_require_the_host_flag_and_an_open_bridge() {
    let temp = tempfile::tempdir().unwrap();
    write_plugin(
        temp.path(),
        "index.js",
        r#"
        import { tools } from "moc";
        export default {
          tools: {
            nested: {
              description: "调用宿主工具",
              parameters: { type: "object" },
              run() { return tools.call("read", { path: "x" }); }
            }
          }
        };
        "#,
    );
    let (worker, _) = load(temp.path(), Duration::from_secs(2)).await;
    let error = call(
        &worker,
        CallOp::Tool,
        json!({"name":"nested","args":{}}),
        SESSION,
        false,
        Duration::from_secs(2),
    )
    .await
    .expect_err("未授权调用");
    assert_eq!(error.code, "E_PLUGIN_TOOLS_UNAVAILABLE");
}

#[tokio::test]
async fn interrupting_stops_a_runaway_loop() {
    let temp = tempfile::tempdir().unwrap();
    write_plugin(
        temp.path(),
        "index.js",
        r#"
        export default {
          tools: { spin: { description: "死循环", parameters: { type: "object" }, run() { while (true) {} } } }
        };
        "#,
    );
    let (worker, _) = load(temp.path(), Duration::from_secs(2)).await;
    let (reply, ready) = tokio::sync::oneshot::channel();
    worker
        .call(CallRequest {
            op: CallOp::Tool,
            payload: json!({"name":"spin","args":{}}),
            context: SESSION.into(),
            tools_allowed: false,
            deadline: Instant::now() + Duration::from_millis(200),
            host_tx: None,
            reply,
        })
        .unwrap();
    // 模拟宿主超时：置中断标志。flag 先于死循环的任何一次 handler 轮询
    // 生效，不需要给调用留启动窗口。
    worker.interrupt();
    let error = tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .expect("中断后应返回")
        .expect("调用方存活")
        .expect_err("死循环必须失败");
    assert_eq!(error.code, "E_PLUGIN_INTERRUPTED", "{error}");
}

#[tokio::test]
async fn rejects_bad_descriptors_and_escaped_imports() {
    let temp = tempfile::tempdir().unwrap();
    let cases = [
        ("export default 1;", "E_PLUGIN_DESCRIPTOR"),
        (
            r#"export default { tools: { bad: { description: "x", parameters: { type: "object" }, run() {} } }, typo: 1 };"#,
            "E_PLUGIN_DESCRIPTOR",
        ),
        (
            r#"import "../outside.js"; export default {};"#,
            "E_PLUGIN_IMPORT",
        ),
        (
            r#"export default { tools: "nope" };"#,
            "E_PLUGIN_DESCRIPTOR",
        ),
        (
            r#"export default { tools: { x: { description: "x", parameters: { type: "object" }, run() {} } }, hooks: { nope() {} } };"#,
            "E_PLUGIN_DESCRIPTOR",
        ),
    ];
    for (index, (body, code)) in cases.iter().enumerate() {
        let dir = temp.path().join(format!("case{index}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        std::fs::write(dir.join("index.js"), body).unwrap();
        let (_worker, ready) = spawn(
            "demo".into(),
            dir.clone(),
            dir.join("index.js"),
            limits(Duration::from_secs(1)),
        );
        let error = ready.await.expect("线程存活").expect_err("必须失败");
        assert!(error.to_string().contains(code), "{body} -> {error}");
    }
}

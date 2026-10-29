//! 插件系统的端到端验收：工具、钩子拒绝与参数改写、提示词注入、`plugin/list`。

mod support;

use moc_protocol::methods::{
    ConfigAction, ConfigManage, ConfigReply, ConfigScope, PluginInfo, PluginList, PluginListParams,
    PluginReload, PluginReloadParams, PluginRunCommand, PluginRunCommandParams, SessionEngine,
    SessionLoad, SessionLoadParams, SessionUpdate,
};
use serde_json::{Value, json};
use support::{Harness, text_stream, tool_result_texts, tool_stream};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 装一个插件到工作区的项目级目录。`Harness` 的 cwd 固定是 `<root>/work`。
fn install_plugin(harness: &Harness, name: &str, body: &str) {
    let dir = harness.root.path().join("work/.moc/plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        format!(r#"{{"name":"{name}","description":"测试插件"}}"#),
    )
    .unwrap();
    std::fs::write(dir.join("index.js"), body).unwrap();
}

async fn plugin_list(harness: &Harness, session: &str) -> Vec<PluginInfo> {
    harness
        .client
        .call::<PluginList>(PluginListParams {
            session_id: session.into(),
        })
        .await
        .unwrap()
        .plugins
}

async fn load(harness: &Harness, session: &str) -> Vec<String> {
    let loaded = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.into(),
            max_entries: None,
            read_only: false,
        })
        .await
        .unwrap();
    tool_result_texts(&loaded)
}

/// 插件工具进模型工具表、可真正执行，提示词贡献进 `<plugins>` 片段。
#[tokio::test]
async fn plugin_tool_runs_and_its_prompt_reaches_the_model() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let has_result = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if has_result {
                ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_raw(
                    tool_stream(vec![("call_1", "plugin_echo", json!({"text": "hi"}))]),
                    "text/event-stream",
                )
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    install_plugin(
        &harness,
        "demo",
        r#"
        export default {
          tools: {
            plugin_echo: {
              description: "回显文本",
              parameters: { type: "object", properties: { text: { type: "string" } } },
              run(input) { return `plugin:${input.text}`; }
            }
          },
          commands: { demo: { description: "示例命令", run: args => `cmd:${args}` } },
          prompt: () => "PLUGIN_PROMPT_TEXT"
        };
        "#,
    );
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "试试插件").await;

    let results = load(&harness, &session).await;
    assert!(
        results.iter().any(|text| text.contains("plugin:hi")),
        "插件工具应执行成功：{results:?}"
    );

    // 模型看到的是原生工具，且插件提示词进了系统提示。
    let requests = upstream.received_requests().await.expect("请求记录");
    let first: Value = serde_json::from_slice(&requests[0].body).expect("请求体是 JSON");
    assert!(
        first["tools"]
            .as_array()
            .expect("请求带工具")
            .iter()
            .any(|tool| tool["function"]["name"] == "plugin_echo"),
        "插件工具必须出现在模型工具表里"
    );
    assert!(
        first["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("PLUGIN_PROMPT_TEXT"),
        "插件提示词必须进系统提示"
    );

    let list = harness
        .client
        .call::<PluginList>(PluginListParams {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    let demo = list
        .plugins
        .iter()
        .find(|plugin| plugin.name == "demo")
        .expect("插件已加载");
    assert_eq!(demo.error, None);
    assert_eq!(demo.tools[0].name, "plugin_echo");
    assert_eq!(demo.tools[0].modes.join(","), "default,ask,plan");
    assert_eq!(demo.commands[0].name, "demo");

    harness.close().await;
}

/// 插件工具声明的 `modes` 约束执行许可但不影响声明面：工具表随引擎恒定，
/// plan 模式下 `default_only` 仍被声明、调用时被 E_MODE 拒绝。
#[tokio::test]
async fn plugin_tool_modes_filter_the_model_tool_table_and_calls() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let has_result = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if has_result {
                ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_raw(
                    tool_stream(vec![
                        ("hidden", "default_only", json!({})),
                        ("visible", "everywhere", json!({})),
                    ]),
                    "text/event-stream",
                )
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    install_plugin(
        &harness,
        "demo",
        r#"
        export default {
          tools: {
            default_only: {
              description: "只在默认模式可见",
              parameters: { type: "object" },
              modes: ["default"],
              run() { return "HIDDEN_PLUGIN_EXECUTED"; }
            },
            everywhere: {
              description: "默认三种模式都可见",
              parameters: { type: "object" },
              run() { return "VISIBLE_PLUGIN_EXECUTED"; }
            }
          }
        };
        "#,
    );
    let session = harness.session_plan(SessionEngine::Standard).await;
    harness.prompt(&session, "先给计划").await;

    let requests = upstream.received_requests().await.expect("请求记录");
    let first: Value = serde_json::from_slice(&requests[0].body).expect("请求体是 JSON");
    let names: Vec<&str> = first["tools"]
        .as_array()
        .expect("请求带工具")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert!(
        names.contains(&"default_only"),
        "模式不裁剪工具表，default_only 仍在声明面：{names:?}"
    );
    assert!(
        names.contains(&"everywhere"),
        "默认全开的插件工具应可见：{names:?}"
    );
    let results = load(&harness, &session).await;
    assert!(
        results.iter().any(|text| text.contains("E_MODE")),
        "{results:?}"
    );
    assert!(
        !results
            .iter()
            .any(|text| text.contains("HIDDEN_PLUGIN_EXECUTED")),
        "隐藏工具不得执行：{results:?}"
    );
    assert!(
        results
            .iter()
            .any(|text| text.contains("VISIBLE_PLUGIN_EXECUTED")),
        "可见工具应执行：{results:?}"
    );
    let plugins = plugin_list(&harness, &session).await;
    let default_only = plugins[0]
        .tools
        .iter()
        .find(|tool| tool.name == "default_only")
        .unwrap();
    assert_eq!(default_only.modes, ["default"]);

    harness.close().await;
}

/// `beforeToolCall` 拒绝后工具的副作用不得发生。
#[tokio::test]
async fn before_hook_denial_stops_the_call() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let done = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if done {
                ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_raw(
                    tool_stream(vec![(
                        "call_1",
                        "write",
                        json!({"path": "denied.txt", "content": "不应该写"}),
                    )]),
                    "text/event-stream",
                )
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    install_plugin(
        &harness,
        "guard",
        r#"
        export default {
          hooks: {
            beforeToolCall(event) {
              if (event.name === "write") return { deny: "本仓库禁止写文件" };
              return undefined;
            }
          }
        };
        "#,
    );
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "写个文件").await;

    let results = load(&harness, &session).await;
    assert!(
        results
            .iter()
            .any(|text| text.contains("E_PLUGIN_DENIED") && text.contains("本仓库禁止写文件")),
        "拒绝原因必须回到模型：{results:?}"
    );
    assert!(
        !harness.root.path().join("work/denied.txt").exists(),
        "被拒绝的写入不得落盘"
    );

    harness.close().await;
}

/// 参数改写发生在授权与工具执行之前。
#[tokio::test]
async fn before_hook_rewrites_arguments_before_execution() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let done = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if done {
                ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_raw(
                    tool_stream(vec![("call_1", "read", json!({"path": "original.txt"}))]),
                    "text/event-stream",
                )
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    std::fs::write(harness.root.path().join("work/original.txt"), "原始内容").unwrap();
    std::fs::write(harness.root.path().join("work/alt.txt"), "改写内容").unwrap();
    install_plugin(
        &harness,
        "rewrite",
        r#"
        export default {
          hooks: {
            beforeToolCall(event) {
              if (event.name !== "read") return undefined;
              return { arguments: { ...event.arguments, path: "alt.txt" } };
            }
          }
        };
        "#,
    );
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "读文件").await;

    let results = load(&harness, &session).await;
    assert!(
        results.iter().any(|text| text.contains("改写内容")),
        "工具应读到改写后的路径：{results:?}"
    );
    assert!(
        !results.iter().any(|text| text.contains("原始内容")),
        "原参数不应被使用：{results:?}"
    );

    harness.close().await;
}

/// 插件目录内容变化要在下一次 `plugin/list` 生效，不能一直用首次加载的宿主。
#[tokio::test]
async fn plugin_list_reloads_after_files_change() {
    let harness = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install_plugin(
        &harness,
        "demo",
        r#"export default { tools: { alpha: { description: "第一个", parameters: { type: "object" }, run: () => "a" } } };"#,
    );
    let session = harness.session(SessionEngine::Standard).await;
    let plugins = plugin_list(&harness, &session).await;
    assert_eq!(plugins[0].tools[0].name, "alpha");

    install_plugin(
        &harness,
        "demo",
        r#"export default { tools: { beta: { description: "改过的工具描述", parameters: { type: "object" }, run: () => "b" } } };"#,
    );
    let plugins = plugin_list(&harness, &session).await;
    assert_eq!(
        plugins[0].tools[0].name, "beta",
        "模块改动后必须重建宿主：{plugins:?}"
    );

    harness.close().await;
}

/// 保存配置要丢弃插件宿主：`disable` 改不到磁盘，指纹察觉不到。
#[tokio::test]
async fn config_save_disables_plugins() {
    let harness = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install_plugin(&harness, "demo", "export default { prompt: () => 'x' };");
    let session = harness.session(SessionEngine::Standard).await;
    assert_eq!(plugin_list(&harness, &session).await.len(), 1);

    let cwd = harness
        .root
        .path()
        .join("work")
        .to_string_lossy()
        .into_owned();
    let reply = harness
        .client
        .call::<ConfigManage>(ConfigAction::Read {
            scope: ConfigScope::Global,
            cwd: cwd.clone(),
        })
        .await
        .unwrap();
    let ConfigReply::Settings(document) = reply else {
        panic!("设置文档");
    };
    let mut edited = document.document.clone();
    edited["plugins"] = json!({"disable": ["demo"]});
    harness
        .client
        .call::<ConfigManage>(ConfigAction::Save {
            scope: ConfigScope::Global,
            cwd,
            document: edited,
            revision: document.revision,
        })
        .await
        .unwrap();

    assert!(
        plugin_list(&harness, &session).await.is_empty(),
        "保存配置后必须按新配置重建宿主"
    );

    harness.close().await;
}

/// 取前端看到的插件失败通知。
fn plugin_notices(harness: &Harness) -> Vec<(String, String, String)> {
    harness
        .updates
        .lock()
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::PluginNotice {
                plugin,
                code,
                message,
            } => Some((plugin.clone(), code.clone(), message.clone())),
            _ => None,
        })
        .collect()
}

/// 工具抛业务异常按插件故障处理：同插件的下一次调用得到 `E_PLUGIN_DISABLED`，不复读原错误。
#[tokio::test]
async fn thrown_tool_error_disables_the_plugin_for_later_calls() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let done = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if done {
                ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_raw(
                    tool_stream(vec![
                        ("call_boom", "plugin_boom", json!({})),
                        ("call_fine", "plugin_fine", json!({})),
                    ]),
                    "text/event-stream",
                )
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    install_plugin(
        &harness,
        "demo",
        r#"
        export default {
          tools: {
            plugin_boom: {
              description: "抛异常",
              parameters: { type: "object" },
              run() { throw new Error("输入有问题"); }
            },
            plugin_fine: {
              description: "同插件的另一个工具",
              parameters: { type: "object" },
              run() { return "fine"; }
            }
          }
        };
        "#,
    );
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "先失败再继续").await;

    let results = load(&harness, &session).await;
    assert!(
        results
            .iter()
            .any(|text| text.contains("[E_PLUGIN]") && text.contains("输入有问题")),
        "首次调用应报告业务异常：{results:?}"
    );
    assert!(
        results
            .iter()
            .any(|text| text.contains("E_PLUGIN_DISABLED")),
        "同插件的下一次调用必须得到 E_PLUGIN_DISABLED：{results:?}"
    );

    let plugins = plugin_list(&harness, &session).await;
    let demo = plugins
        .iter()
        .find(|plugin| plugin.name == "demo")
        .expect("插件仍应出现在清单里");
    assert!(
        demo.error
            .as_deref()
            .is_some_and(|error| error.contains("E_PLUGIN")),
        "停用原因应写进清单：{demo:?}"
    );
    assert!(demo.tools.is_empty(), "停用后不再暴露工具：{demo:?}");

    harness.close().await;
}

/// `plugin/reload` 丢弃宿主缓存重建：停用的插件恢复，清单里的 error 与注册项随之变化。
#[tokio::test]
async fn plugin_reload_recovers_a_disabled_plugin() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    // 用会话级持久化 state 制造「只失败一次」：重载后 JS 重新求值，但 state 不丢。
    install_plugin(
        &harness,
        "demo",
        r#"
        import { session } from "moc";
        export default {
          commands: {
            once: {
              description: "第一次失败，之后成功",
              run() {
                const state = session.state;
                if (!state.failed) {
                  state.failed = true;
                  throw new Error("第一次失败");
                }
                return "插件恢复了";
              }
            }
          }
        };
        "#,
    );
    let session = harness.session(SessionEngine::Standard).await;
    let command = |name: &str| PluginRunCommandParams {
        session_id: session.clone(),
        name: name.into(),
        args: String::new(),
    };
    let error = harness
        .client
        .call::<PluginRunCommand>(command("once"))
        .await
        .expect_err("命令应失败");
    let message = match error {
        moc_protocol::RpcCallError::Rpc(error) => error.message,
        other => panic!("预期服务端错误：{other:?}"),
    };
    assert!(
        message.contains("E_PLUGIN") && message.contains("第一次失败"),
        "{message}"
    );

    let plugins = plugin_list(&harness, &session).await;
    assert_eq!(plugins.len(), 1);
    assert!(
        plugins[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("E_PLUGIN")),
        "失败的插件在清单里带 error：{plugins:?}"
    );
    assert!(
        plugins[0].commands.is_empty(),
        "停用后不再列出命令：{plugins:?}"
    );

    let reloaded = harness
        .client
        .call::<PluginReload>(PluginReloadParams {
            session_id: session.clone(),
        })
        .await
        .unwrap()
        .plugins;
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].error, None, "重载后错误应清空：{reloaded:?}");
    assert_eq!(reloaded[0].commands[0].name, "once");

    let text = harness
        .client
        .call::<PluginRunCommand>(command("once"))
        .await
        .expect("重载后命令应恢复")
        .text;
    assert_eq!(text, "插件恢复了");

    // 命令失败时已经先把通知送给前端，不必等下一轮提示。
    let notices = plugin_notices(&harness);
    assert_eq!(notices.len(), 1, "失败应只通知一次：{notices:?}");
    assert_eq!(notices[0].0, "demo");
    assert_eq!(notices[0].1, "E_PLUGIN");
    assert!(notices[0].2.contains("第一次失败"), "{notices:?}");

    // 重载撤掉排队中的「已停用」提醒：修好之后再发提示，模型不该被告知插件还坏着。
    harness.prompt(&session, "你好").await;
    let requests = upstream.received_requests().await.expect("请求记录");
    let stale = requests.iter().any(|request| {
        let body: Value = serde_json::from_slice(&request.body).expect("请求体是 JSON");
        body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("插件 `demo`"))
            })
        })
    });
    assert!(!stale, "重载后不得再把已恢复的插件报给模型");

    harness.close().await;
}

/// 批次里刚挂掉的插件立刻通知：前端收到 PluginNotice，下一轮请求带上模型可见提醒。
#[tokio::test]
async fn plugin_failure_reaches_frontend_and_next_model_turn() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let done = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if done {
                ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_raw(
                    tool_stream(vec![("call_boom", "plugin_boom", json!({}))]),
                    "text/event-stream",
                )
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    install_plugin(
        &harness,
        "demo",
        r#"
        export default {
          tools: {
            plugin_boom: {
              description: "抛异常",
              parameters: { type: "object" },
              run() { throw new Error("插件炸了"); }
            }
          }
        };
        "#,
    );
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "触发失败").await;

    let notices = plugin_notices(&harness);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(
        (notices[0].0.as_str(), notices[0].1.as_str()),
        ("demo", "E_PLUGIN")
    );
    assert!(notices[0].2.contains("插件炸了"), "{notices:?}");
    assert!(
        notices[0].2.contains("at ") && notices[0].2.lines().count() > 1,
        "前端通知里要能看到 JS 栈：{notices:?}"
    );

    // 工具批次结束后立刻抽干通知：紧接着的模型请求就能看到插件提醒。
    let requests = upstream.received_requests().await.expect("请求记录");
    let reminder = requests.iter().skip(1).any(|request| {
        let body: Value = serde_json::from_slice(&request.body).expect("请求体是 JSON");
        body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"].as_str().is_some_and(|content| {
                    content.contains("插件 `demo`：插件炸了")
                        && content.contains("plugin/reload")
                        && content.contains("已停用")
                })
            })
        })
    });
    assert!(reminder, "下一轮请求必须带上插件失败提醒");

    harness.close().await;
}

/// 提示开始时也要抽干积压通知：加载失败的插件在第一轮就进模型提醒。
#[tokio::test]
async fn load_failure_notice_reaches_the_model_on_prompt_start() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    install_plugin(&harness, "broken", "export default { nope: 1 };");
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "你好").await;

    let notices = plugin_notices(&harness);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].0, "broken");
    assert_eq!(notices[0].1, "E_PLUGIN_DESCRIPTOR");

    let requests = upstream.received_requests().await.expect("请求记录");
    let first: Value = serde_json::from_slice(&requests[0].body).expect("请求体是 JSON");
    let reminded = first["messages"].as_array().is_some_and(|messages| {
        messages.iter().any(|message| {
            message["content"].as_str().is_some_and(|content| {
                content.contains("插件 `broken`")
                    && content.contains("plugin/reload")
                    && content.contains("已停用")
            })
        })
    });
    assert!(reminded, "加载失败的通知必须进第一轮系统提醒");

    harness.close().await;
}

/// `messageReceived` 在落库前改写正文：transcript 与模型请求都只看到改写后的文本。
#[tokio::test]
async fn message_received_rewrites_user_input_before_persisting() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    install_plugin(
        &harness,
        "rewrite",
        r#"
        export default {
          hooks: {
            messageReceived(event) {
              if (event.source !== "user") return undefined;
              return { text: event.text.replace("原始", "改写") };
            }
          }
        };
        "#,
    );
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "原始指令").await;

    let loaded = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.clone(),
            max_entries: None,
            read_only: false,
        })
        .await
        .unwrap();
    let users = loaded
        .entries
        .iter()
        .filter(|entry| entry.message.role == moc_llm::Role::User)
        .map(|entry| entry.message.text())
        .collect::<Vec<_>>();
    assert!(
        users.iter().any(|text| text.contains("改写指令")),
        "改写后的正文必须落库：{users:?}"
    );
    assert!(
        !users.iter().any(|text| text.contains("原始指令")),
        "原文不得进入 transcript：{users:?}"
    );

    harness.close().await;
}

/// `sessionStart` 注入的消息在会话创建后立即落库，并随 transcript 一起返回。
#[tokio::test]
async fn session_start_injects_messages_into_the_transcript() {
    let harness = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install_plugin(
        &harness,
        "bootstrap",
        r#"
        export default {
          hooks: {
            sessionStart() {
              return {
                messages: [
                  { role: "system", text: "会话初始化规则" },
                  { role: "user", text: "先读 AGENTS.md" }
                ]
              };
            }
          }
        };
        "#,
    );
    let session = harness.session(SessionEngine::Standard).await;

    let loaded = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session,
            max_entries: None,
            read_only: false,
        })
        .await
        .unwrap();
    let texts = loaded
        .entries
        .iter()
        .map(|entry| entry.message.text())
        .collect::<Vec<_>>();
    assert!(
        texts.iter().any(|text| text.contains("会话初始化规则")),
        "系统消息必须落库：{texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("先读 AGENTS.md")),
        "用户消息必须落库：{texts:?}"
    );

    harness.close().await;
}

/// `afterToolCall` 的改写发生在落库与回灌模型之前，transcript 与工具结果一致。
#[tokio::test]
async fn after_tool_call_rewrites_the_tool_result() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let done = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if done {
                ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_raw(
                    tool_stream(vec![("call_1", "read", json!({"path": "secret.txt"}))]),
                    "text/event-stream",
                )
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    std::fs::write(harness.root.path().join("work/secret.txt"), "机密内容").unwrap();
    install_plugin(
        &harness,
        "redact",
        r#"
        export default {
          hooks: {
            afterToolCall(event) {
              if (event.name !== "read") return undefined;
              return { content: "[已脱敏]" };
            }
          }
        };
        "#,
    );
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "读文件").await;

    let results = load(&harness, &session).await;
    assert!(
        results.iter().any(|text| text.contains("[已脱敏]")),
        "工具结果必须被改写：{results:?}"
    );
    assert!(
        !results.iter().any(|text| text.contains("机密内容")),
        "原文不得进入 transcript：{results:?}"
    );

    harness.close().await;
}

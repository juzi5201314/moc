//! 插件系统的端到端验收：工具、钩子拒绝与参数改写、提示词注入、`plugin/list`。

mod support;

use moc_protocol::methods::{
    PluginList, PluginListParams, SessionLoad, SessionLoadParams, SessionMode,
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
    let session = harness.session(SessionMode::Normal).await;
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
    assert_eq!(demo.tools[0].tier, "read");
    assert_eq!(demo.commands[0].name, "demo");

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
    let session = harness.session(SessionMode::Normal).await;
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
    let session = harness.session(SessionMode::Normal).await;
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

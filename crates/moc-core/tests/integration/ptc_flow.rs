//! PTC 入口的真实 worker 进程与提示闭环验收。

use crate::support;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crate::support::Harness;
use serde_json::json;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn ptc_execute_executes_worker_in_normal_mode() {
    if !support::require_moc_bin("PTC flow") {
        return;
    }
    let upstream = MockServer::builder().start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &Request| {
            let response = match counter.fetch_add(1, Ordering::SeqCst) {
                0 => support::tool_stream(vec![(
                    "call_run", "execute",
                    json!({"description":"Sum numbers in the worker","code":"return {sum: [1,2,3].reduce((a,b)=>a+b,0)};"}),
                )]),
                2 => support::tool_stream(vec![(
                    "nested_run", "execute",
                    json!({"description":"Write two files","code":"await tools.write({path:'first.txt', content:'已提交'}); await tools.write({path:'second.txt', content:'同样提交'});"}),
                )]),
                _ => support::text_stream("guest sum=6"),
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;

    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[tools.ptc]\ndefault_timeout_secs=30\nmax_timeout_secs=30\n",
    )
    .await;
    let session = harness
        .session(moc_protocol::methods::SessionEngine::Standard)
        .await;
    let result = harness.prompt(&session, "计算 1+2+3").await;

    assert_eq!(result.message.text(), "guest sum=6");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let loaded = harness.load(&session).await;
    assert!(loaded.entries.iter().any(|entry| {
        matches!(entry, moc_protocol::methods::TranscriptEntry { message, .. } if message.text().contains("sum"))
    }));
    // 嵌套调用不受提示级预算约束：同一脚本里的两次写入都执行并保留。
    let nested = harness.prompt(&session, "执行嵌套工具场景").await;
    assert_eq!(nested.finish_reason, moc_llm::FinishReason::Stop);
    assert_eq!(
        std::fs::read_to_string(harness.root.path().join("work/first.txt")).unwrap(),
        "已提交"
    );
    assert_eq!(
        std::fs::read_to_string(harness.root.path().join("work/second.txt")).unwrap(),
        "同样提交"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    // 沙盒里的嵌套调用与模型直发的调用同形：参数与父调用都在 `ToolStarted` /
    // `ToolFinished` 里，前端因此能像普通调用一样渲染摘要。
    let nested = harness
        .updates
        .lock()
        .iter()
        .filter_map(|update| match update {
            moc_protocol::methods::SessionUpdate::ToolStarted {
                tool_call_id,
                name,
                parent_tool_call_id: Some(parent),
                arguments,
                ..
            }
            | moc_protocol::methods::SessionUpdate::ToolFinished {
                tool_call_id,
                name,
                parent_tool_call_id: Some(parent),
                arguments,
                ..
            } => Some((
                tool_call_id.clone(),
                name.clone(),
                parent.clone(),
                arguments.clone(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(nested.len(), 4, "两次嵌套写入各有起止两条更新：{nested:?}");
    for (id, name, parent, arguments) in &nested {
        assert_eq!(name, "write");
        assert_eq!(parent, "nested_run", "父调用 ID 必须是发起它的 execute");
        assert!(
            id.starts_with("nested_run:"),
            "嵌套调用 ID 挂在父调用下：{id}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(arguments).expect("嵌套调用的参数是完整 JSON");
        assert!(
            parsed["path"]
                .as_str()
                .is_some_and(|path| path.ends_with(".txt")),
            "参数必须带上路径：{arguments}"
        );
    }
    harness.close().await;
}

/// 模型经 execute 调用内置与插件工具时，同样应用钩子；插件内部回调不重入。
#[tokio::test]
async fn execute_preserves_plugin_hooks_and_callback_boundaries() {
    assert!(support::require_moc_bin("execute 插件与钩子"));
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let done = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool");
            let response = if done {
                support::text_stream("验证完成")
            } else {
                support::tool_stream(vec![(
                    "execute_checks",
                    "execute",
                    json!({
                        "description": "核对插件调用与钩子边界",
                        "code": r#"
                            const result = {};
                            try { await tools.read({path:'blocked.txt'}); }
                            catch (error) { result.denied = error.code; }
                            result.read = (await tools.read({path:'original.txt'})).content;
                            result.plugin = JSON.parse((await tools.plugin_probe({path:'original.txt'})).content);
                            result.definition = (await catalog.describe('plugin_probe')).name;
                            result.documentation = (await tools.read({path:'tool://plugin_probe'})).content;
                            try { await tools.execute({description:'递归调用',code:'return 0;'}); }
                            catch (error) { result.recursion = error.code; }
                            return result;
                        "#
                    }),
                )])
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    for (path, text) in [
        ("original.txt", "原始文件"),
        ("alt.txt", "改写后的文件"),
        ("bridge.txt", "插件内部读取"),
        ("blocked.txt", "禁止读取"),
    ] {
        std::fs::write(harness.root.path().join("work").join(path), text).unwrap();
    }
    crate::plugin_flow::install_plugin(
        &harness,
        "parity",
        r#"
        import { tools } from 'moc';
        export default {
          tools: {
            plugin_probe: {
              description: '检查插件内部的只读工具边界',
              parameters: {type:'object',properties:{path:{type:'string'}},required:['path']},
              run(input, ctx) {
                const result = {mode:ctx.mode, read:tools.call('read', {path:input.path}).content};
                for (const name of ['write', 'execute', 'plugin_probe']) {
                  try { tools.call(name, {}); }
                  catch (error) { result[name] = error.code; }
                }
                return JSON.stringify(result);
              }
            }
          },
          hooks: {
            beforeToolCall(event) {
              if (event.name === 'plugin_probe') return {arguments:{path:'bridge.txt'}};
              if (event.name !== 'read') return;
              if (['blocked.txt','bridge.txt'].includes(event.arguments.path)) return {deny:'禁止模型读取'};
              if (event.arguments.path === 'original.txt') return {arguments:{path:'alt.txt'}};
            },
            afterToolCall(event) {
              if (event.name === 'read') return {content:'读取钩子:' + event.content};
              if (event.name === 'plugin_probe') return {content:JSON.stringify({...JSON.parse(event.content),after:true})};
            }
          }
        };
        "#,
    );
    for (engine, mode) in [
        (moc_protocol::SessionEngine::Standard, "standard"),
        (moc_protocol::SessionEngine::Ptc, "ptc"),
    ] {
        let session = harness.session(engine).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            harness.prompt(&session, "验证调用语义"),
        )
        .await
        .expect("插件内部读取不能因钩子重入而死锁");
        let loaded = harness.load(&session).await;
        let result = loaded
            .entries
            .iter()
            .flat_map(|entry| entry.message.tool_results())
            .find(|result| result.tool_call_id == "execute_checks")
            .expect("execute 结果");
        assert!(!result.is_error, "{engine:?}: {}", result.content);
        let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(value["denied"], "E_PLUGIN_DENIED");
        let read = value["read"].as_str().unwrap();
        assert!(read.starts_with("读取钩子:"), "{read}");
        assert!(read.contains("改写后的文件"), "{read}");
        assert!(!read.contains("原始文件"), "{read}");
        let plugin = &value["plugin"];
        assert_eq!(plugin["mode"], mode, "保留真实会话引擎");
        assert_eq!(plugin["after"], true);
        let read = plugin["read"].as_str().unwrap();
        assert!(read.contains("插件内部读取"), "{read}");
        assert!(!read.contains("读取钩子:"), "插件内部不能重入钩子：{read}");
        for name in ["write", "execute", "plugin_probe"] {
            assert_eq!(plugin[name], "E_PLUGIN_TOOLS_UNAVAILABLE", "{name}");
        }
        assert_eq!(value["definition"], "plugin_probe");
        assert!(
            value["documentation"]
                .as_str()
                .unwrap()
                .contains("plugin_probe")
        );
        assert_eq!(value["recursion"], "E_PTC_RECURSION");
    }
    harness.close().await;
}

/// 直接执行模型收到的签名，覆盖连字符、数字开头与 then 保留属性。
#[tokio::test]
async fn plugin_tool_signatures_are_callable_in_execute() {
    assert!(support::require_moc_bin("插件工具名兼容性"));
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let messages = body["messages"].as_array().unwrap();
            let response = if messages.iter().any(|message| message["role"] == "tool") {
                support::text_stream("验证完成")
            } else {
                let system = messages
                    .iter()
                    .find(|message| message["role"] == "system")
                    .unwrap()["content"]
                    .as_str()
                    .unwrap();
                let calls = ["Read-Tool2", "1probe", "then"]
                    .map(|name| {
                        let expression = system
                            .lines()
                            .filter_map(|line| line.split_once("(args:"))
                            .map(|(expression, _)| expression)
                            .find(|expression| expression.contains(name))
                            .expect("模型能发现插件工具签名");
                        format!("(await {expression}({{n:7}})).content")
                    })
                    .join(",");
                support::tool_stream(vec![(
                    "plugin_names",
                    "execute",
                    json!({"description":"执行插件签名", "code":format!("return [{calls}];")}),
                )])
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    crate::plugin_flow::install_plugin(
        &harness,
        "names",
        r#"
        const tool = factor => ({
          description: '计算整数倍数',
          parameters: {type:'object', properties:{n:{type:'integer'}}, required:['n']},
          run({n}) { return String(n * factor); }
        });
        export default {tools:{'Read-Tool2':tool(2), '1probe':tool(3), then:tool(4)}};
        "#,
    );
    let session = harness.session(moc_protocol::SessionEngine::Ptc).await;
    harness.prompt(&session, "执行插件工具").await;
    let loaded = harness.load(&session).await;
    let result = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|result| result.tool_call_id == "plugin_names")
        .expect("execute 结果");
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result.content).unwrap(),
        json!(["14", "21", "28"])
    );
    harness.close().await;
}

/// 真实 worker 串起网页快照、重复查询与 read；验证模型接收的文本而非 UI 副本。
#[tokio::test]
async fn fetch_documents_are_reused_and_readable_through_execute() {
    assert!(support::require_moc_bin("网页快照与 PTC"));
    let upstream = MockServer::builder().start().await;
    let document = format!(
        "# API\n\n## Request\n\n- `temperature: number`\n\n{}\n## Last section\nFINAL-FIELD\n",
        "body line\n".repeat(60_000)
    );
    let last_line = document
        .lines()
        .position(|line| line == "## Last section")
        .unwrap()
        + 1;
    Mock::given(method("GET"))
        .and(path("/document"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(document, "text/markdown"))
        .expect(1)
        .mount(&upstream)
        .await;
    let url = format!("{}/document", upstream.uri());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(move |_request: &Request| {
            let response = match counter.fetch_add(1, Ordering::SeqCst) {
                0 => support::tool_stream(vec![("fetch_docs", "execute", json!({
                    "description": "Fetch a document and reuse its query",
                    "code": format!("const first = await tools.web_fetch({{urls:[{url:?}],query:'temperature'}}); await state.store('document',first.artifact); const repeated = await tools.web_fetch({{urls:[{url:?}],query:'temperature'}}); return {{first,repeated}};")
                }))]),
                1 => support::tool_stream(vec![("read_doc", "execute", json!({
                    "description": "Read the final document section",
                    "code": format!("const path = await state.load('document'); return (await tools.read({{path,offset:{last_line},limit:2}})).content;")
                }))]),
                _ => support::text_stream("完成"),
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        }).mount(&upstream).await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(moc_protocol::SessionEngine::Standard).await;
    harness.prompt(&session, "保存网页，然后读取末尾字段").await;
    let loaded = harness.load(&session).await;
    let results: Vec<_> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .collect();
    let fetch = results
        .iter()
        .find(|result| result.tool_call_id == "fetch_docs")
        .unwrap();
    assert!(!fetch.is_error, "{}", fetch.content);
    let value: serde_json::Value = serde_json::from_str(&fetch.content).unwrap();
    assert!(
        value["first"]["artifact"]
            .as_str()
            .unwrap()
            .starts_with("artifact://artifacts/web-")
    );
    assert_eq!(value["first"]["artifact"], value["repeated"]["artifact"]);
    assert!(
        !value["repeated"]["content"]
            .as_str()
            .unwrap()
            .contains("- `temperature: number`")
    );
    assert_eq!(
        value["first"]["structuredContent"]["pages"][0]["source_truncated"],
        false
    );
    let read = results
        .iter()
        .find(|result| result.tool_call_id == "read_doc")
        .unwrap();
    assert!(!read.is_error, "{}", read.content);
    assert!(
        read.content.starts_with("[artifact://"),
        "字符串不能再包成 JSON 字面量"
    );
    assert!(read.content.contains(&format!(
        "{last_line}│## Last section\n{}│FINAL-FIELD",
        last_line + 1
    )));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    harness.close().await;
}

#[tokio::test]
async fn execute_user_wait_outlives_deadline_and_keeps_answer() {
    use moc_protocol::{ClientBuilder, methods::*, transport};
    assert!(support::require_moc_bin("PTC 用户等待"));
    let upstream = MockServer::builder().start().await;
    let calls = AtomicUsize::new(0);
    Mock::given(method("POST")).respond_with(move |_: &Request| {
        let response = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            support::tool_stream(vec![("wait_answer", "execute", json!({
                "description":"等待用户决定后返回回答",
                "timeout_secs":1,
                "code":"const answer = await tools.ask({questions:[{id:'choice',question:'Choose',options:[{label:'Option A'},{label:'Option B'}]}]}); console.log('Answer received'); return answer.content;"
            }))])
        } else { support::text_stream("完成") };
        ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
    }).mount(&upstream).await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let (server_side, client_side) = transport::duplex(65536);
    let connection = harness.server.spawn(server_side);
    let (client, _) = ClientBuilder::new(ClientInfo {
        name: "等待验收".into(),
        version: "1".into(),
        frontend: "test".into(),
    })
    .capabilities(ClientCapabilities {
        permission_requests: false,
        ask_requests: true,
    })
    .on_request::<SessionRequestAsk, _, _>(|params, _| async move {
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        Ok(AskResult {
            answers: vec![AskAnswer {
                id: params.questions[0].id.clone(),
                selected: vec!["Option A".into()],
                custom: None,
            }],
        })
    })
    .connect(client_side)
    .await
    .unwrap();
    let session = client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Ptc,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "等待用户回答".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
        })
        .await
        .unwrap();
    let loaded = harness.load(&session.id).await;
    let result = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|r| r.tool_call_id == "wait_answer")
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("Answer received\n") && result.content.contains("Option A"));
    client.close().await;
    connection.close();
    connection.wait().await;
    harness.close().await;
}

#[tokio::test]
async fn failed_execute_preserves_completed_reads_in_readable_artifact() {
    assert!(support::require_moc_bin("PTC 失败恢复"));
    let upstream = MockServer::builder().start().await;
    let calls = AtomicUsize::new(0);
    Mock::given(method("POST")).respond_with(move |request: &Request| {
        let response = match calls.fetch_add(1, Ordering::SeqCst) {
            0 => support::tool_stream(vec![("failed_batch", "execute", json!({
                "description":"读取后触发参数错误",
                "code":"await tools.read({path:'kept.txt'}); return await tools.grep({pattern:'hit',limit:80});"
            }))]),
            1 => {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let result = body["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap();
                let uri = result.split_whitespace().find(|word| word.starts_with("artifact://")).expect("失败结果必须能回读").trim_end_matches('.');
                support::tool_stream(vec![("recover_batch", "execute", json!({
                    "description":"读取已完成子调用结果",
                    "code":format!("return (await tools.read({{path:{uri:?}}})).content;")
                }))])
            }
            _ => support::text_stream("完成"),
        };
        ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
    }).mount(&upstream).await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    std::fs::write(
        harness.root.path().join("work/kept.txt"),
        "unique_read_evidence\n",
    )
    .unwrap();
    let session = harness.session(moc_protocol::SessionEngine::Ptc).await;
    harness.prompt(&session, "保留已完成结果").await;
    let loaded = harness.load(&session).await;
    let results: Vec<_> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .collect();
    let failed = results
        .iter()
        .find(|r| r.tool_call_id == "failed_batch")
        .unwrap();
    assert!(failed.is_error);
    assert_eq!(failed.content.matches("[E_BAD_PAYLOAD]").count(), 1);
    let recovered = results
        .iter()
        .find(|r| r.tool_call_id == "recover_batch")
        .unwrap();
    assert!(!recovered.is_error, "{}", recovered.content);
    assert!(recovered.content.contains("unique_read_evidence"));
    assert!(recovered.content.contains("unknown field `limit`"));
    harness.close().await;
}

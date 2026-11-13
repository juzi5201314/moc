use super::*;

/// Anthropic 协议下的缓存断点落位：静态前缀末块与系统边界各一个断点，尾部断点落在
/// 最后一条会进入历史的用户消息上，而本轮才出现的提醒块不得落位（它永远不命中）。
#[tokio::test]
async fn anthropic_requests_put_cache_breakpoints_on_static_prefix_and_stable_tail() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_raw(
                support::anthropic_text_stream("缓存冒烟完成"),
                "text/event-stream",
            )
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "anthropic_messages", "").await;
    // 计划模式：本轮带模式提醒（尾部条目），断点必须避开它。
    let id = h.session_plan(SessionEngine::Standard).await;
    h.prompt(&id, "缓存冒烟").await;

    let requests = upstream.received_requests().await.expect("请求记录");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("请求 JSON");

    let system = body["system"].as_array().expect("系统提示为内容块数组");
    let cached: Vec<usize> = system
        .iter()
        .enumerate()
        .filter(|(_, block)| block.get("cache_control").is_some())
        .map(|(index, _)| index)
        .collect();
    let ptc = system
        .iter()
        .position(|block| {
            block["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("execute evaluates"))
        })
        .expect("PTC 静态片段");
    assert_eq!(
        cached,
        vec![ptc, system.len() - 1],
        "断点应落在静态前缀末块与系统边界块：{system:?}"
    );
    assert!(
        system[0]["text"]
            .as_str()
            .expect("首块文本")
            .starts_with("You are a precise software-engineering agent"),
        "{system:?}"
    );
    assert!(
        system
            .last()
            .and_then(|block| block["text"].as_str())
            .expect("系统末块")
            .starts_with("<env>"),
        "{system:?}"
    );

    // 用户消息与运行时提醒在 Anthropic 上合并为同一条线上消息。提醒已落库成为历史的一部分，
    // 所以尾断点应该落在它上面：下个请求的前缀包含它，能直接命中。
    let messages = body["messages"].as_array().expect("线上消息");
    assert_eq!(messages.len(), 1, "{messages:?}");
    let content = messages[0]["content"].as_array().expect("消息内容块");
    assert_eq!(content.len(), 2, "{content:?}");
    assert!(
        content[0]["text"]
            .as_str()
            .expect("用户文本")
            .contains("缓存冒烟")
    );
    assert!(
        content[0].get("cache_control").is_none(),
        "尾断点应落在最后一条会持久化的消息上"
    );
    assert!(
        content[1]["text"]
            .as_str()
            .expect("提醒文本")
            .contains("<system-reminder>")
    );
    assert!(
        content[1].get("cache_control").is_some(),
        "提醒进入历史后前缀稳定，断点必须落在它上面"
    );
    h.close().await;
}

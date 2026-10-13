mod support;

use moc_protocol::methods::{
    AgentList, AgentListParams, AgentSpawn, AgentSpawnParams, AgentStatus, AgentWait,
    AgentWaitParams, SessionLoad, SessionLoadParams,
};
use serde_json::Value;
use support::Harness;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn spawn_wait_list_persists_parent_and_child_output() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求是 JSON");
            let text = if body.to_string().contains("CHILD_TASK") {
                "CHILD_OK"
            } else {
                "PARENT_OK"
            };
            ResponseTemplate::new(200).set_body_raw(support::text_stream(text), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "test/gpt-test:off",
        "",
    )
    .await;
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
    let spawned = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            parent_session_id: parent_id.clone(),
            task: "CHILD_TASK".into(),
            title: Some("child".into()),
            model: Some("inherit".into()),
        })
        .await
        .expect("spawn 成功");
    assert_eq!(spawned.agent.parent_id, parent_id);
    let waited = harness
        .client
        .call::<AgentWait>(AgentWaitParams {
            parent_session_id: spawned.agent.parent_id.clone(),
            agent_id: spawned.agent.id.clone(),
            timeout_ms: Some(5_000),
        })
        .await
        .expect("wait 成功");
    assert!(waited.completed);
    assert_eq!(waited.message.expect("子代理有输出").text(), "CHILD_OK");
    assert_eq!(waited.agent.status, AgentStatus::Completed);
    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            parent_session_id: parent_id,
        })
        .await
        .expect("list 成功");
    assert_eq!(listed.agents.len(), 1);
    assert_eq!(listed.agents[0].id, spawned.agent.id);
    assert_eq!(listed.agents[0].status, AgentStatus::Completed);
    let child = harness
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: spawned.agent.id,
            max_entries: None,
        })
        .await
        .expect("child transcript 可加载");
    assert_eq!(child.entries[0].message.text(), "CHILD_TASK");
    harness.close().await;
}

#[tokio::test]
async fn failed_child_agent_reports_its_error_to_the_parent() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求是 JSON");
            if body.to_string().contains("CHILD_TASK") {
                return ResponseTemplate::new(500).set_body_json(serde_json::json!({
                    "error": {"message": "上游拒绝子代理", "code": "boom"}
                }));
            }
            ResponseTemplate::new(200)
                .set_body_raw(support::text_stream("PARENT_OK"), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "test/gpt-test:off",
        "",
    )
    .await;
    let parent_id = harness.session(moc_protocol::SessionMode::Normal).await;
    let spawned = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            parent_session_id: parent_id.clone(),
            task: "CHILD_TASK".into(),
            title: Some("child".into()),
            model: Some("inherit".into()),
        })
        .await
        .expect("spawn 成功");
    let waited = harness
        .client
        .call::<AgentWait>(AgentWaitParams {
            parent_session_id: parent_id.clone(),
            agent_id: spawned.agent.id.clone(),
            timeout_ms: Some(10_000),
        })
        .await
        .expect("wait 成功");
    assert!(waited.completed);
    assert_eq!(waited.agent.status, AgentStatus::Failed);
    let message = waited.message.expect("失败消息必须对父会话可见");
    assert!(
        message.text().contains("Subagent prompt failed"),
        "失败消息应说明原因：{}",
        message.text()
    );
    let listed = harness
        .client
        .call::<AgentList>(AgentListParams {
            parent_session_id: parent_id,
        })
        .await
        .expect("list 成功");
    assert!(
        listed.agents[0]
            .last_message
            .as_deref()
            .is_some_and(|text| text.contains("Subagent prompt failed")),
        "列表必须带出失败原因：{:?}",
        listed.agents[0].last_message
    );
    harness.close().await;
}

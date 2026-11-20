use super::support;
use moc_protocol::methods::{
    AgentMessage, AgentMessageOutcome, AgentMessageParams, AgentSpawn, AgentSpawnParams,
    AgentStatus, AgentWait, AgentWaitParams, SessionEngine, SessionUpdate,
};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::timeout;
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::method};

struct OpenGatesOnDrop([support::Gate; 2]);

impl Drop for OpenGatesOnDrop {
    fn drop(&mut self) {
        for gate in &self.0 {
            gate.open();
        }
    }
}

#[tokio::test]
async fn message_wakeup_waits_for_old_finish_before_replacing_agent() {
    let upstream = MockServer::builder().start().await;
    let first_gate = support::Gate::new();
    let second_gate = support::Gate::new();
    // 断言失败也放行 wiremock 工作线程，避免失败用例卡到门控兜底超时。
    let _open_gates = OpenGatesOnDrop([first_gate.clone(), second_gate.clone()]);
    let first_response = first_gate.clone();
    let second_response = second_gate.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let report = if String::from_utf8_lossy(&request.body).contains("SECOND_TASK") {
                second_response.wait();
                "SECOND_REPORT"
            } else {
                first_response.wait();
                "FIRST_REPORT"
            };
            ResponseTemplate::new(200).set_body_raw(
                support::tool_stream(vec![("call_yield", "yield", json!({"report": report}))]),
                "text/event-stream",
            )
        })
        .mount(&upstream)
        .await;
    let harness = support::Harness::new(&upstream.uri(), "openai_chat", "").await;
    let mut events = harness.events.subscribe();
    let parent_id = harness.session(SessionEngine::Standard).await;
    let child = harness
        .client
        .call::<AgentSpawn>(AgentSpawnParams {
            session_id: parent_id.clone(),
            task: "FIRST_TASK".into(),
            title: Some("child".into()),
            role: "explorer".into(),
        })
        .await
        .unwrap()
        .agent;
    let inner = &harness.core.inner;
    let runtime = inner.runtime(&child.id).await;
    let record = inner.sessions.get(&child.id).await.unwrap();
    let finish_gate = Arc::new(tokio::sync::Barrier::new(2));
    *runtime.agent_finish_gate.lock().await = Some(Arc::clone(&finish_gate));
    first_gate.open();
    timeout(Duration::from_secs(10), finish_gate.wait())
        .await
        .expect("首轮必须进入 finish 暂停点");
    assert!(runtime.agent_finishing.load(Ordering::Acquire));
    drop(record.try_prompt_lock().expect("finish 前提示锁已释放"));
    let first_notify = Arc::clone(&inner.agents.lock().await.items[&child.id].notify);

    // 旧 finish 暂停时真正投递 RPC；等唤醒拿到提示锁，确认它停在收尾屏障上。
    let client = harness.client.clone();
    let params = AgentMessageParams {
        session_id: parent_id.clone(),
        target: child.id.clone(),
        message: "SECOND_TASK".into(),
    };
    let wakeup = tokio::spawn(async move { client.call::<AgentMessage>(params).await });
    timeout(Duration::from_secs(10), async {
        while record.try_prompt_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("消息唤醒必须取得提示锁");
    assert!(!wakeup.is_finished(), "旧 finish 未结束，唤醒不能完成");
    assert!(
        Arc::ptr_eq(
            &first_notify,
            &inner.agents.lock().await.items[&child.id].notify
        ),
        "旧 finish 未结束，prepare 不得替换注册表条目"
    );

    finish_gate.wait().await;
    let receipt = timeout(Duration::from_secs(10), wakeup)
        .await
        .expect("旧 finish 返回后必须放行唤醒")
        .unwrap()
        .unwrap();
    assert_eq!(receipt.receipts[0].outcome, AgentMessageOutcome::Woken);
    {
        let registry = inner.agents.lock().await;
        let agent = &registry.items[&child.id];
        assert_eq!(agent.info.status, AgentStatus::Running);
        assert!(agent.info.last_message.is_none());
        assert!(agent.message.is_none());
        assert!(agent.finish_reason.is_none());
        assert!(!Arc::ptr_eq(&first_notify, &agent.notify));
    }

    // 第二轮上游仍被门控，零等待窗口只能返回 Running，不能误用首轮结果。
    let wait_params = AgentWaitParams {
        session_id: parent_id.clone(),
        agent_ids: Some(vec![child.id.clone()]),
        timeout_secs: Some(0),
    };
    let pending = harness
        .client
        .call::<AgentWait>(wait_params.clone())
        .await
        .unwrap();
    assert!(!pending.settled);
    assert!(pending.timed_out);
    assert_eq!(pending.agents[0].status, AgentStatus::Running);
    assert!(pending.agents[0].message.is_none());
    assert!(pending.agents[0].finish_reason.is_none());

    second_gate.open();
    let completed = harness
        .client
        .call::<AgentWait>(AgentWaitParams {
            timeout_secs: Some(10),
            ..wait_params
        })
        .await
        .unwrap();
    assert!(completed.settled);
    assert_eq!(completed.agents[0].status, AgentStatus::Completed);
    assert_eq!(
        completed.agents[0].message.as_ref().unwrap().text(),
        "SECOND_REPORT"
    );

    // 父会话收到首轮结果与终态后才能看到新轮 Running，过期通知不得跨过新轮。
    let (statuses, reports) = timeout(Duration::from_secs(10), async {
        let mut statuses = Vec::new();
        let mut reports = Vec::new();
        loop {
            match events.recv().await.unwrap() {
                SessionUpdate::AgentResult { meta, body, .. } if meta.agent_id == child.id => {
                    reports.push(body);
                }
                SessionUpdate::AgentStatusChanged {
                    agent_id, status, ..
                } if agent_id == child.id => {
                    statuses.push(status);
                    if reports.len() == 2 && status == AgentStatus::Completed {
                        break (statuses, reports);
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .expect("两轮结果与终态通知必须到达");
    assert_eq!(
        statuses,
        [
            AgentStatus::Running,
            AgentStatus::Completed,
            AgentStatus::Running,
            AgentStatus::Completed
        ]
    );
    assert_eq!(reports, ["FIRST_REPORT", "SECOND_REPORT"]);
    let parent = harness.load(&parent_id).await;
    let persisted_reports: Vec<_> = parent
        .entries
        .iter()
        .filter(|entry| {
            entry
                .agent_result
                .as_ref()
                .is_some_and(|meta| meta.agent_id == child.id)
        })
        .map(|entry| entry.message.text())
        .collect();
    assert_eq!(persisted_reports, ["FIRST_REPORT", "SECOND_REPORT"]);
    harness.close().await;
}

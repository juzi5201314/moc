//! `/goal`：持久目标的生命周期、armed 语义与自动续跑链。
//!
//! 续跑轮在 core 内部 spawn_detached，不等调用方——断言一律走
//! `wait_update` 观测 `goal_round_message` / `goal_changed`，不 sleep 猜时序。

use std::time::Duration;

use moc_protocol::methods::*;
use moc_protocol::{FinishReason, RpcCallError};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use crate::support::{Harness, text_stream};

/// `session/goal` 薄封装；`Err` 原样返回供断言错误码。
async fn call(
    h: &Harness,
    session_id: &str,
    action: GoalAction,
) -> Result<SessionGoalResult, RpcCallError> {
    h.client
        .call::<SessionGoal>(SessionGoalParams {
            session_id: session_id.into(),
            action,
        })
        .await
}

/// 等到第一条满足条件的 update；10s 超时视为失败。
async fn wait_update(h: &Harness, pred: impl Fn(&SessionUpdate) -> bool) -> SessionUpdate {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(update) = h.updates.lock().iter().find(|u| pred(u)).cloned() {
                return update;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("等到了目标 update")
}

/// 等 `round` 号及以上的 GoalRoundMessage。
async fn wait_round(h: &Harness, round: u32) -> GoalRoundMeta {
    let update = wait_update(
        h,
        |u| matches!(u, SessionUpdate::GoalRoundMessage { meta, .. } if meta.round >= round),
    )
    .await;
    match update {
        SessionUpdate::GoalRoundMessage { meta, .. } => meta,
        _ => unreachable!(),
    }
}

fn goal_view(result: &SessionGoalResult) -> &GoalView {
    result.goal.as_ref().expect("返回当前目标视图")
}

/// create → 自动续跑（GoalRoundMessage 带 `<goal_round>` 与 objective）→
/// pause 停链 → resume 复跑 → 换目标重建 → clear 清除。
#[tokio::test]
async fn goal_create_rounds_pause_resume_and_clear() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(text_stream("done")))
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = h.session(SessionEngine::Standard).await;

    // 建目标：armed，立即开始自动续跑。
    let created = call(
        &h,
        &id,
        GoalAction::Create {
            objective: "把 readme 写好".into(),
        },
    )
    .await
    .expect("创建目标");
    let view = goal_view(&created);
    assert_eq!(view.phase, GoalPhase::Active);
    assert!(view.armed);
    assert_eq!(view.revision, 1);
    let goal_id = view.id.clone();

    // 第一轮续跑：元数据指向本目标，消息体是 <goal_round> 指令。
    let round = wait_round(&h, 1).await;
    assert_eq!(round.goal_id, goal_id);
    let body = h
        .updates
        .lock()
        .iter()
        .find_map(|u| match u {
            SessionUpdate::GoalRoundMessage { body, .. } => Some(body.clone()),
            _ => None,
        })
        .expect("轮次消息正文");
    assert!(body.contains("<goal_round>"));
    assert!(body.contains("把 readme 写好"));

    // 暂停：相位落库为 paused、armed 摘掉，正在跑的轮被中止。
    let paused = call(&h, &id, GoalAction::Pause).await.expect("暂停");
    let view = goal_view(&paused);
    assert_eq!(view.phase, GoalPhase::Paused);
    assert!(!view.armed);

    // 恢复：重新 armed，链继续 → 轮号 > 已跑轮数。
    let started = view.rounds_started;
    let resumed = call(&h, &id, GoalAction::Resume).await.expect("恢复");
    let view = goal_view(&resumed);
    assert_eq!(view.phase, GoalPhase::Active);
    assert!(view.armed);
    wait_round(&h, started + 1).await;

    // 再建一个目标被拒：先清再建。
    let dup = call(
        &h,
        &id,
        GoalAction::Create {
            objective: "新目标".into(),
        },
    )
    .await
    .expect_err("已有目标");
    assert!(dup.to_string().contains("GOAL_ALREADY_EXISTS"));

    // 暂停后编辑：原文与 revision 更新，相位保留（不悄悄复活续跑）。
    call(&h, &id, GoalAction::Pause).await.expect("再暂停");
    let edited = call(
        &h,
        &id,
        GoalAction::Edit {
            objective: "改成写 API 文档".into(),
        },
    )
    .await
    .expect("编辑");
    let view = goal_view(&edited);
    assert_eq!(view.objective, "改成写 API 文档");
    assert_eq!(view.revision, 2);
    assert_eq!(view.phase, GoalPhase::Paused);

    // 清除：目标消失，armed 为 false；clear 幂等。
    let cleared = call(&h, &id, GoalAction::Unset).await.expect("清除");
    assert!(cleared.goal.is_none());
    let cleared = call(&h, &id, GoalAction::Unset)
        .await
        .expect("二次清除幂等");
    assert!(cleared.goal.is_none());
    let miss = call(&h, &id, GoalAction::Resume)
        .await
        .expect_err("无目标不可恢复");
    assert!(miss.to_string().contains("GOAL_NOT_FOUND"));

    // 转写里能看到 goal_round 条目与 GoalChange 持久化。
    let loaded = h.load(&id).await;
    assert!(
        loaded
            .entries
            .iter()
            .any(|entry| entry.goal_round.as_ref().is_some_and(|m| m.round >= 1))
    );

    h.close().await;
}

/// 无目标与非法参数的错误面；「链停」由 pause 用例覆盖，
/// `blocked`/`complete` 的 CAS 路径由 `goal.rs` 单测与工具用例覆盖。
#[tokio::test]
async fn goal_rejects_empty_and_missing() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(text_stream("done")))
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = h.session(SessionEngine::Standard).await;

    let shown = call(&h, &id, GoalAction::Show).await.expect("查询");
    assert!(shown.goal.is_none());
    let invalid = call(
        &h,
        &id,
        GoalAction::Create {
            objective: "   ".into(),
        },
    )
    .await
    .expect_err("空目标拒绝");
    assert!(invalid.to_string().contains("GOAL_INVALID_OBJECTIVE"));
    let edit = call(
        &h,
        &id,
        GoalAction::Edit {
            objective: "x".into(),
        },
    )
    .await
    .expect_err("无目标不可编辑");
    assert!(edit.to_string().contains("GOAL_NOT_FOUND"));

    h.close().await;
}

/// `GoalChanged` 随相位迁移推送；load 结果里带目标视图（armed 补丁）。
#[tokio::test]
async fn goal_changed_updates_and_load_projection() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(text_stream("done")))
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = h.session(SessionEngine::Standard).await;

    call(
        &h,
        &id,
        GoalAction::Create {
            objective: "跑三轮".into(),
        },
    )
    .await
    .expect("创建");
    wait_update(&h, |u| {
        matches!(u, SessionUpdate::GoalChanged { goal: Some(view) } if view.phase == GoalPhase::Active)
    })
    .await;
    wait_round(&h, 1).await;
    call(&h, &id, GoalAction::Pause).await.expect("暂停");

    // load 投影：phase/armed 与 live 一致，rounds 来自落库条目。
    let loaded = h.load(&id).await;
    let view = loaded.goal.expect("load 带目标视图");
    assert_eq!(view.phase, GoalPhase::Paused);
    assert!(!view.armed);
    assert!(view.rounds_started >= 1);
    assert_eq!(view.objective, "跑三轮");

    h.close().await;
}

/// goal 的模型工具面：create_goal 在人类轮建目标；get_goal 读回同一视图。
#[tokio::test]
async fn goal_tools_in_user_turn() {
    let upstream = MockServer::builder().start().await;
    // 首个模型应答带工具调用；后续（工具结果后的继续 + goal 续跑轮）一律文本收尾。
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(crate::support::tool_stream(vec![
                (
                    "call_1",
                    "create_goal",
                    json!({ "objective": "工具面建目标" }),
                ),
                ("call_2", "get_goal", json!({})),
            ])),
        )
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(text_stream("done")))
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = h.session(SessionEngine::Standard).await;

    let result = h.prompt(&id, "给这个会话立个目标").await;
    assert_eq!(result.finish_reason, FinishReason::Stop);
    // 工具落地后目标存在且处于 active；随后会自然续跑，这里只断言建立本身。
    let shown = call(&h, &id, GoalAction::Show).await.expect("读回");
    let view = goal_view(&shown);
    assert_eq!(view.objective, "工具面建目标");
    call(&h, &id, GoalAction::Pause).await.expect("收尾暂停");

    h.close().await;
}

use super::*;

/// 前台 bash 也登记为 job：其开始/结束更新不得挂「后台」徽标或推「后台任务结束」
/// 便签——结果已随工具卡片落地。转后台后 origin 改写，徽标挂上、结束才提示。
#[test]
fn foreground_job_updates_do_not_masquerade_as_background() {
    use crate::transcript::Block;
    use moc_protocol::methods::{JobInfo, JobLifecycle, JobOrigin, JobStatus};

    let job = |origin: JobOrigin, status: JobStatus, exit_code: Option<i32>| JobInfo {
        job_id: "job_1".into(),
        command: "cargo build".into(),
        description: None,
        tool_call_id: "call_1".into(),
        status,
        exit_code,
        lifecycle: JobLifecycle::Monitored,
        origin,
        stalled: false,
        output_elided: false,
        started_at_ms: 0,
        duration_ms: Some(1_600),
        log_uri: "artifact://jobs/job_1.log".into(),
    };
    let tool_job_id = |app: &App| match &app.blocks[0].block {
        Block::Tool(tool) => tool.job_id.clone(),
        other => panic!("首个块应为工具块：{other:?}"),
    };

    let mut app = app();
    apply_update(
        &mut app,
        SessionUpdate::ToolStarted {
            prompt_id: "prompt_1".into(),
            tool_call_id: "call_1".into(),
            name: "bash".into(),
            parent_tool_call_id: None,
            arguments: "{}".into(),
        },
    );

    // 前台来源：不挂徽标，结束也不推便签。
    apply_update(
        &mut app,
        SessionUpdate::JobStarted {
            job: job(JobOrigin::Foreground, JobStatus::Running, None),
        },
    );
    assert_eq!(tool_job_id(&app), None);
    apply_update(
        &mut app,
        SessionUpdate::JobFinished {
            job: job(JobOrigin::Foreground, JobStatus::Exited, Some(0)),
        },
    );
    assert_eq!(app.blocks.len(), 1, "前台任务结束不得追加便签");

    // 自动转后台：徽标挂上；结束推便签并摘除徽标。
    apply_update(
        &mut app,
        SessionUpdate::JobUpdated {
            job: job(JobOrigin::AutoBackground, JobStatus::Running, None),
        },
    );
    assert_eq!(tool_job_id(&app), Some("job_1".into()));
    apply_update(
        &mut app,
        SessionUpdate::JobFinished {
            job: job(JobOrigin::AutoBackground, JobStatus::Exited, Some(0)),
        },
    );
    assert_eq!(tool_job_id(&app), None, "任务结束后摘除徽标");
    let Some(Block::Note { text, error }) = app.blocks.last().map(|entry| &entry.block) else {
        panic!("后台任务结束应追加便签");
    };
    assert!(!*error);
    assert_eq!(text, "后台任务结束：cargo build（退出码 0）");
}

/// 上游改写折叠成告警文案：模型与推理强度各成一段、` ｜ ` 连接；再次不一致只替换。
#[test]
fn upstream_mismatch_folds_into_warning_text() {
    let mut app = app();
    apply_update(
        &mut app,
        SessionUpdate::UpstreamMismatch {
            prompt_id: "prompt_1".into(),
            fields: vec![
                UpstreamFieldMismatch {
                    field: UpstreamField::Model,
                    requested: "glm-5.3".into(),
                    reported: "glm-5.3-air".into(),
                },
                UpstreamFieldMismatch {
                    field: UpstreamField::Effort,
                    requested: "max".into(),
                    reported: "high".into(),
                },
            ],
        },
    );
    assert_eq!(
        app.warning.as_deref(),
        Some("model glm-5.3 → glm-5.3-air ｜ effort max → high")
    );
    assert!(app.warning_visible());

    // 同一次提示的下一次上游请求再次不一致：覆盖而非堆叠。
    apply_update(
        &mut app,
        SessionUpdate::UpstreamMismatch {
            prompt_id: "prompt_1".into(),
            fields: vec![UpstreamFieldMismatch {
                field: UpstreamField::Effort,
                requested: "high".into(),
                reported: "medium".into(),
            }],
        },
    );
    assert_eq!(app.warning.as_deref(), Some("effort high → medium"));
}

/// 压缩提示从 0s 开始计时，完成后原位替换，不追加第二条。
#[test]
fn compaction_note_ticks_then_replaces_in_place() {
    use crate::transcript::Block;
    use moc_protocol::{SessionEngine, SessionInfo};

    let mut app = app();
    app.session = Some(SessionInfo {
        id: "ses_compact".into(),
        title: "压缩".into(),
        model: "test/model".into(),
        permission: moc_protocol::SessionPermission::Full,
        engine: SessionEngine::Standard,
        autonomy: moc_protocol::SessionAutonomy::Default,
        plan_active: false,
        fast_mode: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: "/tmp".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        lock: None,
    });
    let note = |app: &App| match app.blocks.last().map(|entry| &entry.block) {
        Some(Block::Note { text, error }) => (text.clone(), *error),
        other => panic!("压缩提示应为 Note：{other:?}"),
    };

    apply_update(&mut app, SessionUpdate::ContextCompactionStarted);
    assert_eq!(note(&app), ("上下文压缩中…(0s)".into(), false));

    // 3 秒后同一条提示只改秒数。
    app.compaction_progress
        .as_mut()
        .expect("压缩在飞")
        .started_at -= std::time::Duration::from_secs(3);
    assert!(app.tick_compaction());
    assert_eq!(note(&app), ("上下文压缩中…(3s)".into(), false));
    assert_eq!(app.blocks.len(), 1, "计时不应追加新块");

    apply_update(
        &mut app,
        SessionUpdate::ContextCompacted {
            method: "soft".into(),
            tokens_before: 426_070,
            tokens_after: 247_960,
            full: true,
        },
    );
    assert_eq!(
        note(&app),
        ("上下文已压缩：426.07k → 247.96k".into(), false)
    );
    assert_eq!(app.blocks.len(), 1, "完成时应原位替换");
    assert!(app.compaction_progress.is_none());
}

/// 仅裁剪历史时不能说成「已压缩」：措辞与全量摘要区分开。
#[test]
fn pruning_compaction_is_labelled_as_trim() {
    use crate::transcript::Block;
    use moc_protocol::{SessionEngine, SessionInfo};

    let mut app = app();
    app.session = Some(SessionInfo {
        id: "ses_prune".into(),
        title: "裁剪".into(),
        model: "test/model".into(),
        permission: moc_protocol::SessionPermission::Full,
        engine: SessionEngine::Standard,
        autonomy: moc_protocol::SessionAutonomy::Default,
        plan_active: false,
        fast_mode: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: "/tmp".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        lock: None,
    });

    apply_update(&mut app, SessionUpdate::ContextCompactionStarted);
    apply_update(
        &mut app,
        SessionUpdate::ContextCompacted {
            method: "soft".into(),
            tokens_before: 426_070,
            tokens_after: 247_960,
            full: false,
        },
    );
    let Some(Block::Note { text, error }) = app.blocks.last().map(|entry| &entry.block) else {
        panic!("裁剪提示应为 Note");
    };
    assert_eq!(text, "上下文已裁剪：426.07k → 247.96k（未生成摘要）");
    assert!(!error);
    assert_eq!(app.blocks.len(), 1, "裁剪只留一条终态提示");
    assert!(app.compaction_progress.is_none());

    // 本地裁剪没有进行中提示：完成事件直接落一条终态，不留下“压缩中”。
    app.blocks.clear();
    apply_update(
        &mut app,
        SessionUpdate::ContextCompacted {
            method: "shake".into(),
            tokens_before: 426_070,
            tokens_after: 247_960,
            full: false,
        },
    );
    let Some(Block::Note { text, .. }) = app.blocks.last().map(|entry| &entry.block) else {
        panic!("裁剪完成应为 Note");
    };
    assert_eq!(text, "上下文已裁剪：426.07k → 247.96k（未生成摘要）");
    assert_eq!(app.blocks.len(), 1);
}

/// 压缩失败同样收掉进行中提示，不留一条永远计时的挂起块。
#[test]
fn failed_compaction_ends_the_running_note() {
    use crate::transcript::Block;
    use moc_protocol::{SessionEngine, SessionInfo};

    let mut app = app();
    app.session = Some(SessionInfo {
        id: "ses_failed".into(),
        title: "失败".into(),
        model: "test/model".into(),
        permission: moc_protocol::SessionPermission::Full,
        engine: SessionEngine::Standard,
        autonomy: moc_protocol::SessionAutonomy::Default,
        plan_active: false,
        fast_mode: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: "/tmp".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        lock: None,
    });

    apply_update(&mut app, SessionUpdate::ContextCompactionStarted);
    apply_update(
        &mut app,
        SessionUpdate::ContextCompactionFailed {
            message: "上游不可用".into(),
        },
    );
    let Some(Block::Note { text, error }) = app.blocks.last().map(|entry| &entry.block) else {
        panic!("失败提示应为 Note");
    };
    assert_eq!(text, "上下文压缩失败：上游不可用");
    assert!(error);
    assert!(app.compaction_progress.is_none());
}

#[tokio::test]
async fn skills_compaction_updates_inline_without_modal_or_dismissal() {
    use crate::transcript::Block;
    let (core, connection, mut app) = setup_test_app().await;
    app.start_skills_compaction(moc_protocol::SkillsCompactAction::Missing);
    app.update_skills_compaction(&moc_protocol::SkillsProgressParams {
        phase: moc_protocol::SkillsProgressPhase::Generating,
        completed: 7,
        total: 39,
        skipped: 2,
    });
    assert!(app.modal.is_none());
    assert_eq!(app.blocks.len(), 1);
    assert!(matches!(&app.blocks[0].block, Block::Note { text, .. } if text.contains("7 / 39")));
    app.finish_skills_compaction(Ok(moc_protocol::SkillsCompactResult {
        compressed: 39,
        skipped: 2,
        removed: 0,
        tokens_before: 3920,
        tokens_after: 900,
        message: "技能摘要：压缩 39 个；目录预估 3920 → 900 tokens".into(),
        warnings: Vec::new(),
    }));
    app.notice = None;
    assert!(!app.skills_compacting && !app.prompt_running);
    assert_eq!(app.blocks.len(), 1);
    assert!(app.modal.is_none());
    let terminal = draw_frame(&mut app, 100, 24);
    let screen = terminal.backend().buffer();
    let visible = (0..24)
        .map(|y| {
            (0..100)
                .map(|x| screen[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(visible.contains("3920 → 900"));
    // 后续内容在同一滚动流追加，无需关闭状态块。
    app.push_note("next".into(), false);
    assert_eq!(app.blocks.len(), 2);
    assert!(
        matches!(&app.blocks[0].block, Block::Note { text, .. } if text.contains("3920 → 900"))
    );
    connection.shutdown().await;
    core.shutdown().await;
}

/// M17+M18：运行中切会话后，旧会话的更新不得写进新会话，收尾仍回收在飞记账。
///
/// 不按会话/代理路由时，旧会话的 delta 直接落进新会话转写、PromptStarted 复位
/// 新会话状态、工具块与耗时回填全都串台。
#[tokio::test]
async fn foreign_session_updates_are_dropped_and_recycled() {
    use moc_protocol::methods::{AssistantDelta, SessionUpdate};
    use moc_protocol::{ContentPart, FinishReason, Message, Role, Usage};

    let (core, connection, mut app) = setup_test_app().await;
    let old_session = app.session.as_ref().expect("测试会话").id.clone();
    // 旧会话的提示仍在飞，前端运行中切到另一个会话。
    app.inflight_prompts.insert(old_session.clone());
    app.interrupt_pending.insert(old_session.clone());
    let mut other = app.session.clone().expect("测试会话");
    other.id = "ses_other".into();
    app.switch_session(other, Vec::new());
    assert!(!app.prompt_running);

    // 旧会话开始的推送：不得把新会话置为运行中
    apply_update_and_refresh(
        &mut app,
        &connection,
        old_session.clone(),
        SessionUpdate::PromptStarted {
            prompt_id: "prompt_old".into(),
            model: "test/model".into(),
            at_ms: 0,
        },
    );
    assert!(!app.prompt_running, "旧会话的 PromptStarted 不得复位新会话");

    // 旧会话的正文增量：不得进入当前转写
    apply_update_and_refresh(
        &mut app,
        &connection,
        old_session.clone(),
        SessionUpdate::AssistantDelta {
            prompt_id: "prompt_old".into(),
            delta: AssistantDelta::Text {
                delta: "旧会话内容".into(),
            },
        },
    );
    assert!(app.blocks.is_empty(), "旧会话增量不得写入当前转写");

    // 旧会话收尾：只回收本前端的在飞记账，仍不改当前会话状态
    apply_update_and_refresh(
        &mut app,
        &connection,
        old_session.clone(),
        SessionUpdate::PromptFinished {
            prompt_id: "prompt_old".into(),
            message: Message {
                role: Role::Assistant,
                content: moc_protocol::message_content![ContentPart::Text("旧回复".into())],
            },
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    );
    assert!(
        !app.inflight_prompts.contains(&old_session),
        "旧会话收尾应回收在飞记账"
    );
    assert!(!app.interrupt_pending.contains(&old_session));
    assert!(app.blocks.is_empty(), "旧会话内容不得落到新会话转写");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 思考耗时按「一段思考」定稿：工具调用与下一轮生成都必须先收掉上一块，
/// 否则工具循环里除最后一段之外全是没测到的 0。
#[test]
fn thinking_duration_is_stamped_per_response() {
    use crate::transcript::Block;
    use moc_protocol::methods::{AssistantDelta, SessionUpdate};
    use std::time::{Duration, Instant};

    let mut app = app();
    let duration_of = |app: &App, index: usize| match &app.blocks[index].block {
        Block::Thinking { duration_ms, .. } => *duration_ms,
        _ => panic!("下标 {index} 不是思考块"),
    };

    // 第一响应：思考后直接调工具。
    app.start_thinking();
    app.thinking_started_at = Some(Instant::now() - Duration::from_millis(1_200));
    apply_update(
        &mut app,
        SessionUpdate::ToolStarted {
            prompt_id: "prompt_1".into(),
            tool_call_id: "call_1".into(),
            name: "read".into(),
            parent_tool_call_id: None,
            arguments: "{}".into(),
        },
    );
    assert!(
        duration_of(&app, 0).is_some_and(|ms| ms >= 1_000),
        "工具开始即定稿第一段思考"
    );

    // 第二响应：生成开始后另起一块，正文到达时定稿。
    apply_update(
        &mut app,
        SessionUpdate::GenerationStarted {
            prompt_id: "prompt_1".into(),
        },
    );
    apply_update(
        &mut app,
        SessionUpdate::AssistantDelta {
            prompt_id: "prompt_1".into(),
            delta: AssistantDelta::Reasoning {
                delta: "再想".into(),
            },
        },
    );
    assert_eq!(app.blocks.len(), 3, "新响应另起思考块");
    app.thinking_started_at = Some(Instant::now() - Duration::from_millis(900));
    apply_update(
        &mut app,
        SessionUpdate::AssistantDelta {
            prompt_id: "prompt_1".into(),
            delta: AssistantDelta::Text {
                delta: "答案".into(),
            },
        },
    );
    assert!(
        duration_of(&app, 2).is_some_and(|ms| ms >= 800),
        "正文开始即定稿第二段思考"
    );
}

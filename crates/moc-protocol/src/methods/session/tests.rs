use super::*;

#[test]
fn session_new_accepts_partial_params() {
    let parsed: SessionNewParams =
        serde_json::from_value(serde_json::json!({})).expect("可反序列化");
    assert_eq!(parsed, SessionNewParams::default());
    let parsed: SessionNewParams =
        serde_json::from_value(serde_json::json!({ "cwd": "/tmp/ws" })).expect("可反序列化");
    assert_eq!(parsed.cwd.as_deref(), Some("/tmp/ws"));
    assert!(parsed.model.is_none());
    // 未知字段必须暴露
    assert!(serde_json::from_value::<SessionNewParams>(serde_json::json!({ "cwdly": 1 })).is_err());
    // 空结果序列化为 {}
    assert_eq!(
        serde_json::to_value(crate::methods::Empty {}).expect("可序列化"),
        serde_json::json!({})
    );
}

#[test]
fn load_and_lock_shapes() {
    let params = SessionLoadParams {
        session_id: "ses_1".into(),
        max_entries: None,
        omit_auxiliary: false,
        read_only: false,
    };
    assert_eq!(
        serde_json::to_value(&params).expect("可序列化"),
        serde_json::json!({ "session_id": "ses_1" })
    );
    assert_eq!(
        serde_json::from_value::<SessionLoadParams>(serde_json::json!({ "session_id": "ses_1" }))
            .unwrap(),
        params,
    );
    let light = SessionLoadParams {
        omit_auxiliary: true,
        max_entries: Some(0),
        ..params.clone()
    };
    assert_eq!(
        serde_json::to_value(light).unwrap(),
        serde_json::json!({
            "session_id": "ses_1", "max_entries": 0, "omit_auxiliary": true
        })
    );
    let old_page: SessionLoadPageParams = serde_json::from_value(serde_json::json!({
        "session_id": "ses_1", "max_entries": 64
    }))
    .unwrap();
    assert!(old_page.plan_page.is_none());
    assert!(!old_page.full_messages);
    assert_eq!(
        serde_json::to_value(old_page).unwrap(),
        serde_json::json!({
            "session_id": "ses_1", "max_entries": 64
        })
    );
    let readonly = SessionLoadParams {
        session_id: "ses_1".into(),
        max_entries: None,
        omit_auxiliary: false,
        read_only: true,
    };
    assert_eq!(
        serde_json::to_value(&readonly).expect("可序列化"),
        serde_json::json!({ "session_id": "ses_1", "read_only": true })
    );
    let lock = SessionLockInfo {
        pid: 4242,
        frontend: "tui".into(),
        process_name: Some("moc-tui".into()),
        acquired_at_ms: 1_724_000_000_000,
    };
    assert_eq!(
        serde_json::to_value(&lock).expect("可序列化"),
        serde_json::json!({
            "pid": 4242,
            "frontend": "tui",
            "process_name": "moc-tui",
            "acquired_at_ms": 1_724_000_000_000u64
        })
    );
    assert_eq!(
        serde_json::to_value(ToolStatus::Denied).expect("可序列化"),
        serde_json::json!("denied")
    );
    let info = SessionInfo {
        id: "ses_1".into(),
        title: "t".into(),
        model: "m".into(),
        permission: SessionPermission::Full,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: "/tmp".into(),
        created_at_ms: 1,
        engine: SessionEngine::Ptc,
        autonomy: SessionAutonomy::Unattended,
        plan_active: true,
        fast_mode: true,
        engine_locked: true,
        updated_at_ms: 2,
        lock: None,
    };
    assert_eq!(
        serde_json::to_value(&info).expect("可序列化"),
        serde_json::json!({
            "id": "ses_1", "title": "t", "model": "m", "permission": "full",
            "engine": "ptc", "autonomy": "unattended",
            "plan_active": true, "fast_mode": true, "engine_locked": true,
            "delegation_depth": 0,
            "cwd": "/tmp", "created_at_ms": 1, "updated_at_ms": 2
        })
    );
    // 旧报文的 `mode: "normal"` 读作 `engine: "standard"`；`autonomy` 缺省为 `default`。
    let old: SessionInfo = serde_json::from_value(serde_json::json!({
        "id": "ses_1", "title": "t", "model": "m", "engine": "normal", "cwd": "/tmp",
        "created_at_ms": 1, "updated_at_ms": 2
    }))
    .expect("旧取值可回读");
    assert_eq!(old.engine, SessionEngine::Standard);
    assert_eq!(old.autonomy, SessionAutonomy::Default);
    assert!(!old.fast_mode);
    assert_eq!(old.permission, SessionPermission::Full);
    assert_eq!(
        serde_json::to_value(SessionPermission::Workspace).unwrap(),
        "workspace"
    );
    assert_eq!(
        serde_json::to_value(SessionPermission::WorkspaceOnly).unwrap(),
        "workspace_only"
    );
    assert_eq!(SessionPermission::WorkspaceOnly.as_str(), "workspace-only");
    assert!(SessionPermission::Workspace.can_widen_to(SessionPermission::Full));
    assert!(!SessionPermission::Full.can_widen_to(SessionPermission::Workspace));
    assert!(SessionPermission::Workspace.can_widen_to(SessionPermission::Workspace));
    assert!(
        !serde_json::from_value::<SessionSetPermissionParams>(serde_json::json!({
            "session_id": "ses_1", "permission": "workspace_only"
        }))
        .expect("clear_grants 可缺省")
        .clear_grants
    );
}

/// `session/goal` 的线协议形状：action 按 tag 平铺、相位小写蛇形、
/// `clear` 子命令映射到 `Unset` 变体（Rust 侧避 `Clear` 关键字用途）。
#[test]
fn goal_params_and_view_shapes() {
    use serde_json::json;

    for (json, expected) in [
        (json!({ "action": "show" }), GoalAction::Show),
        (
            json!({ "action": "create", "objective": "做到 x" }),
            GoalAction::Create {
                objective: "做到 x".into(),
            },
        ),
        (
            json!({ "action": "edit", "objective": "改成 y" }),
            GoalAction::Edit {
                objective: "改成 y".into(),
            },
        ),
        (json!({ "action": "pause" }), GoalAction::Pause),
        (json!({ "action": "resume" }), GoalAction::Resume),
        (json!({ "action": "clear" }), GoalAction::Unset),
    ] {
        let parsed: GoalAction = serde_json::from_value(json.clone()).expect("动作可反序列化");
        assert_eq!(parsed, expected, "{json}");
        assert_eq!(serde_json::to_value(&parsed).expect("可序列化"), json);
    }
    assert!(serde_json::from_value::<GoalAction>(json!({ "action": "nope" })).is_err());

    let params = SessionGoalParams {
        session_id: "ses_1".into(),
        action: GoalAction::Pause,
    };
    // `action` 字段内嵌 tagged 动作对象：`{action:{action:"pause"}}`。
    assert_eq!(
        serde_json::to_value(&params).expect("可序列化"),
        json!({ "session_id": "ses_1", "action": { "action": "pause" } })
    );
    assert!(
        serde_json::from_value::<SessionGoalParams>(json!({
            "session_id": "ses_1",
            "action": "show",
            "extra": 1
        }))
        .is_err()
    );

    for (phase, name) in [
        (GoalPhase::Active, "active"),
        (GoalPhase::Paused, "paused"),
        (GoalPhase::Blocked, "blocked"),
        (GoalPhase::Complete, "complete"),
    ] {
        assert_eq!(serde_json::to_value(phase).unwrap(), json!(name));
    }

    let view = GoalView {
        id: "goal_1".into(),
        revision: 2,
        objective: "目标".into(),
        phase: GoalPhase::Blocked,
        blocked_reason: Some(GoalBlockReason {
            code: "model-reported".into(),
            message: "做不到".into(),
        }),
        rounds_started: 4,
        created_at_ms: 1,
        updated_at_ms: 2,
        armed: false,
    };
    assert_eq!(
        serde_json::to_value(&view).expect("可序列化"),
        json!({
            "id": "goal_1", "revision": 2, "objective": "目标",
            "phase": "blocked",
            "blocked_reason": { "code": "model-reported", "message": "做不到" },
            "rounds_started": 4, "created_at_ms": 1, "updated_at_ms": 2,
            "armed": false
        })
    );
    let meta = GoalRoundMeta {
        goal_id: "goal_1".into(),
        revision: 2,
        round: 4,
    };
    assert_eq!(
        serde_json::to_value(&meta).expect("可序列化"),
        json!({ "goal_id": "goal_1", "revision": 2, "round": 4 })
    );
}

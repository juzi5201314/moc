use super::*;
use serde_json::json;

fn plan() -> Value {
    json!({
        "phases": [
            {"id":"phase#1","name":"plan","kind":"phase","sites":["phase#1","actor#1","ask#1"]},
            {"id":"phase#2","name":"build","kind":"phase","sites":["phase#2","actor#2","ask#2","world#1"]},
            {"id":"phase#3","name":"ship","kind":"phase","sites":["phase#3","world#2"]}
        ],
        "edges": [
            {"from":"ask#1","to":"ask#2","kind":"data"},
            {"from":"phase#1","to":"phase#2","kind":"order"},
            {"from":"world#1","to":"world#2","kind":"data"}
        ]
    })
}

#[test]
fn layers_follow_longest_path() {
    let layers = phase_layers(&plan());
    assert_eq!(
        layers,
        vec![
            vec!["phase#1".to_string()],
            vec!["phase#2".to_string()],
            vec!["phase#3".to_string()]
        ]
    );
}

#[test]
fn sibling_phases_share_a_layer() {
    let plan = json!({
        "phases": [
            {"id":"p1","name":"a","kind":"phase","sites":["s1"]},
            {"id":"p2","name":"b","kind":"phase","sites":["s2"]},
            {"id":"p3","name":"c","kind":"phase","sites":["s3"]}
        ],
        "edges": [
            {"from":"s1","to":"s3","kind":"data"},
            {"from":"s2","to":"s3","kind":"data"}
        ]
    });
    let layers = phase_layers(&plan);
    assert!(layers[0].contains(&"p1".to_string()) && layers[0].contains(&"p2".to_string()));
    assert_eq!(layers[1], vec!["p3".to_string()]);
}

#[test]
fn narrow_fallback_lists_phases_vertically() {
    // 无边：全部在第 0 层，渲染层负责窄宽度退化展示。
    let layers = phase_layers(&json!({
        "phases": [
            {"id":"p1","name":"a","kind":"phase","sites":["s1"]},
            {"id":"p2","name":"b","kind":"phase","sites":["s2"]}
        ],
        "edges": []
    }));
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].len(), 2);
}

fn approval(
    kind: moc_protocol::methods::WorkflowApprovalKind,
) -> Box<moc_protocol::methods::WorkflowApproval> {
    Box::new(moc_protocol::methods::WorkflowApproval {
        request_id: "req-1".into(),
        run_id: "run-1".into(),
        script: "phase(\"plan\"); const a = agent(\"p\"); return 1;".into(),
        plan: json!({
            "phases": [
                {"id":"phase#1","name":"plan","kind":"phase","sites":["phase#1","actor#1","ask#1"]},
                {"id":"phase#2","name":"build","kind":"phase","sites":["phase#2","actor#2","ask#2","world#1"]}
            ],
            "edges": [{"from":"ask#1","to":"ask#2","kind":"data"}],
            "roles": [{"role_id":"actor#1","name_pattern":"planner","fan_out":false},{"role_id":"actor#2","name_pattern":"worker","fan_out":true}],
            "model_refs": [{"ref_id":"model#1"}],
            "commands": [{"command":"cargo test"}]
        }),
        defaults: moc_protocol::methods::WorkflowApprovalDefaults {
            role_models: [("actor#1".to_string(), "gpt-x".to_string())]
                .into_iter()
                .collect(),
            model_ref_map: [("model#1".to_string(), "gpt-y".to_string())]
                .into_iter()
                .collect(),
        },
        args: json!(null),
        max_concurrency: 4,
        kind,
        new_commands: None,
    })
}

#[test]
fn approval_lines_have_role_rows_and_hits() {
    let state = WorkflowApprovalState::new(approval(
        moc_protocol::methods::WorkflowApprovalKind::Launch,
    ));
    let (lines, hits) = approval_lines(&state, 60, crate::theme::Theme::global());
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    assert!(text.iter().any(|l| l.contains("planner")));
    assert!(text.iter().any(|l| l.contains("×N")), "fan-out 标记");
    assert!(text.iter().any(|l| l.contains("model(model#1)")));
    assert!(text.iter().any(|l| l.contains("cargo test")));
    assert!(
        text.iter().any(|l| l.contains("phase(\"plan\")")),
        "脚本全文"
    );
    // 2 roles + 1 model_ref = 3 个命中行，行号指向实际行。
    assert_eq!(hits.len(), 3);
    for (row, line_idx) in &hits {
        assert!(*line_idx < lines.len());
        assert!(*row <= 2);
    }
    // hole_fill：只列新增命令、不渲脚本。
    let mut hole = WorkflowApprovalState::new(approval(
        moc_protocol::methods::WorkflowApprovalKind::HoleFill,
    ));
    hole.request.new_commands = Some(vec!["ls".into()]);
    let (lines, _) = approval_lines(&hole, 60, crate::theme::Theme::global());
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    assert!(text.iter().any(|l| l.contains("新增命令")));
    assert!(text.iter().any(|l| l.contains("ls")));
    assert!(
        !text.iter().any(|l| l.contains("phase(\"plan\")")),
        "hole_fill 不渲脚本"
    );
}

fn run_state() -> WorkflowRunState {
    WorkflowRunState {
        run_id: "run-1".into(),
        title: "run".into(),
        status: WorkflowRunStatus::Running,
        plan: plan(),
        phases: vec![],
        roles: vec![WorkflowRoleState {
            role_id: "actor#1".into(),
            name: Some("planner".into()),
            fan_out: false,
            status: "running".into(),
            model: Some("gpt-x".into()),
            asks_in_flight: 1,
            asks_done: 2,
            spent_tokens: 42,
        }],
        commands: vec![],
        reports: vec![
            json!({"item":{"id":"a","v":1.0,"status":"done"},"artifact_id":"t1"}),
            json!({"item":{"id":"a","v":2.0,"status":"wip"},"artifact_id":"t1"}),
            json!({"item":{"id":"b","v":3.0,"status":"done"},"artifact_id":"t1"}),
        ],
        artifacts: vec![json!({
            "member": "table",
            "artifact_id": "t1",
            "spec": {"title":"rows","key":"id","columns":[{"field":"id"},{"field":"v"},{"field":"status"}]}
        })],
        holes: vec![],
        usage: json!({"spent_tokens": 42}),
        log: vec!["line".into()],
        artifact_versions: BTreeMap::new(),
        artifact_view: None,
        focus: 0,
        body_scroll: 0,
        list_scroll: 0,
        scroll: crate::modal::ModelScroll::default(),
        pointer: None,
    }
}

#[test]
fn run_lines_render_table_with_key_dedup() {
    let (lines, hits) = run_lines(&run_state(), 60, crate::theme::Theme::global());
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    // table：key=id 去重——a 只一行且取最后值 v=2.0/status=wip；b 一行。
    let rows: Vec<&String> = text
        .iter()
        .filter(|l| l.trim_start().starts_with("a  ") || l.trim_start().starts_with("b  "))
        .collect();
    assert_eq!(rows.len(), 2, "key 去重后两行");
    assert!(text.iter().any(|l| l.contains("2.0") && l.contains("wip")));
    assert!(
        text.iter()
            .any(|l| l.trim_start().starts_with("b  ") && l.contains("3.0")),
        "b 行存在"
    );
    // role 行命中 + 用量行。
    assert_eq!(hits.len(), 1);
    assert!(
        text.iter()
            .any(|l| l.contains("[2/3 asks") && l.contains("42 tok"))
    );
    assert!(text.iter().any(|l| l.contains("42 tokens")));
}

#[test]
fn run_lines_metrics_board_chart() {
    let mut state = run_state();
    state.artifacts = vec![
        json!({"member":"metrics","artifact_id":"m1","spec":{"metrics":[{"field":"v","label":"speed"}]}}),
        json!({"member":"board","artifact_id":"b1","spec":{"key":"id","status":"status","columns":["done","wip"]}}),
        json!({"member":"chart","artifact_id":"c1","spec":{"x":{"field":"x"},"y":{"field":"v"},"type":"line"}}),
    ];
    state.reports = vec![
        json!({"item":{"id":"a","v":1.0,"status":"done"},"artifact_id":"m1"}),
        json!({"item":{"id":"a","v":2.0,"status":"wip"},"artifact_id":"m1"}),
        json!({"item":{"id":"a","status":"done"},"artifact_id":"b1"}),
        json!({"item":{"id":"b","status":"wip"},"artifact_id":"b1"}),
        json!({"item":{"id":"a","status":"done"},"artifact_id":"b1"}),
        json!({"item":{"x":0,"v":1.0},"artifact_id":"c1"}),
        json!({"item":{"x":1,"v":4.0},"artifact_id":"c1"}),
        json!({"item":{"x":2,"v":2.0},"artifact_id":"c1"}),
    ];
    let (lines, _) = run_lines(&state, 60, crate::theme::Theme::global());
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    // metrics：最新值 v=2.0。
    assert!(
        text.iter()
            .any(|l| l.contains("speed") && l.contains("2.0"))
    );
    // board：a 最后 status=done → done 列；b 在 wip。
    assert!(text.iter().any(|l| l.contains("done ×1")));
    assert!(text.iter().any(|l| l.contains("wip ×1")));
    // chart：有 sparkline 与范围标注。
    assert!(text.iter().any(|l| l.contains("[1.00..4.00]")));
}

#[test]
fn run_lines_version_switch_and_view() {
    let mut state = run_state();
    state.artifacts = vec![
        json!({"member":"markdown","artifact_id":"doc","spec":{"content":"# v1"}}),
        json!({"member":"markdown","artifact_id":"doc","spec":{"content":"# v2"}}),
    ];
    let (lines, _) = run_lines(&state, 60, crate::theme::Theme::global());
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    assert!(text.iter().any(|l| l.contains("v2/2")), "默认最新版");
    state.artifact_versions.insert("doc".into(), 1);
    let (lines, _) = run_lines(&state, 60, crate::theme::Theme::global());
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    assert!(text.iter().any(|l| l.contains("v1/2")));
    // 查看态整幅替换。
    state.artifact_view = Some(("doc".into(), vec![Line::raw("hello")]));
    let (lines, _) = run_lines(&state, 60, crate::theme::Theme::global());
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    assert!(text.iter().any(|l| l.contains("查看：doc")));
    assert!(text.iter().any(|l| l.contains("hello")));
    assert!(!text.iter().any(|l| l.contains("状态：")));
}

#[test]
fn pick_writes_override_back_to_approval() {
    let back = Box::new(WorkflowRoleBack::Approval(Box::new(
        WorkflowApprovalState::new(approval(
            moc_protocol::methods::WorkflowApprovalKind::Launch,
        )),
    )));
    let (back, action) = apply_workflow_pick(
        back,
        &FocusTarget::Role("actor#2".into()),
        "test/gpt-alt".into(),
    );
    assert!(action.is_none());
    let WorkflowRoleBack::Approval(state) = back else {
        panic!("应回审批窗");
    };
    assert_eq!(
        state.role_models.get("actor#2").map(String::as_str),
        Some("test/gpt-alt"),
        "覆盖写回批准决策用的 role_models"
    );
}

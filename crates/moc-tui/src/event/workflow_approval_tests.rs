use super::*;
use crate::event::workflow::apply_workflow_update;
use crate::event::workflow::take_next_approval;
use moc_protocol::methods::{WorkflowApproval, WorkflowApprovalKind, WorkflowEvent};

fn approval(id: &str) -> Box<WorkflowApproval> {
    Box::new(WorkflowApproval {
        request_id: id.into(),
        run_id: "r1".into(),
        script: String::new(),
        plan: serde_json::json!({}),
        defaults: moc_protocol::methods::WorkflowApprovalDefaults {
            role_models: Default::default(),
            model_ref_map: Default::default(),
        },
        args: serde_json::Value::Null,
        max_concurrency: 0,
        kind: WorkflowApprovalKind::Launch,
        new_commands: None,
    })
}

#[test]
fn approval_queues_while_modal_busy() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), tx);
    // modal 占用 → 完整载荷进队列。
    app.modal = Some(Modal::Help { scroll: 0 });
    apply_workflow_update(
        &mut app,
        "r1".into(),
        WorkflowEvent::ApprovalRequested {
            request: approval("a1"),
        },
    );
    assert_eq!(app.workflow.queued_approvals.len(), 1);
    assert_eq!(app.workflow.queued_approvals[0].request_id, "a1");
    // ApprovalResolved 按 request_id 移除。
    apply_workflow_update(
        &mut app,
        "r1".into(),
        WorkflowEvent::ApprovalResolved {
            request_id: "a1".into(),
            approved: true,
            feedback: None,
        },
    );
    assert!(app.workflow.queued_approvals.is_empty());
}

#[test]
fn approval_opens_modal_and_take_next_pops_queue() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), tx);
    // 空档 → 直接弹窗。
    apply_workflow_update(
        &mut app,
        "r1".into(),
        WorkflowEvent::ApprovalRequested {
            request: approval("a1"),
        },
    );
    assert!(matches!(app.modal, Some(Modal::WorkflowApproval { .. })));
    // 打开中的审批被服务端 resolve → modal 关闭。
    apply_workflow_update(
        &mut app,
        "r1".into(),
        WorkflowEvent::ApprovalResolved {
            request_id: "a1".into(),
            approved: true,
            feedback: None,
        },
    );
    assert!(app.modal.is_none());
    // 排队请求可被胶囊点击打开。
    app.modal = Some(Modal::Help { scroll: 0 });
    apply_workflow_update(
        &mut app,
        "r1".into(),
        WorkflowEvent::ApprovalRequested {
            request: approval("a2"),
        },
    );
    app.modal = None;
    let state = take_next_approval(&mut app).expect("队首应可弹出");
    assert_eq!(state.request.request_id, "a2");
    assert!(app.workflow.queued_approvals.is_empty());
}

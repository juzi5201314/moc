use super::WorkflowManager;
use crate::error::CoreError;
use crate::workflow::compile::CompiledWorkflow;
use crate::workflow::compile::Diagnostic;
use crate::workflow::compile::compile;
use moc_protocol::UpdateSink;
use moc_protocol::WorkflowEvent;
use tokio::sync::oneshot;

impl WorkflowManager {
    /// 待决审批：注册并发出 `approval_requested` 事件。
    pub fn request_approval(
        &self,
        approval: moc_protocol::WorkflowApproval,
        sink: &UpdateSink,
    ) -> oneshot::Receiver<moc_protocol::WorkflowDecision> {
        let (tx, rx) = oneshot::channel();
        let request_id = approval.request_id.clone();
        let run_id = approval.run_id.clone();
        self.approvals.lock().insert(request_id, tx);
        let session_id = self.session_id.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            let _ = sink
                .emit(
                    session_id,
                    moc_protocol::SessionUpdate::Workflow {
                        run_id,
                        event: WorkflowEvent::ApprovalRequested {
                            request: Box::new(approval),
                        },
                    },
                )
                .await;
        });
        rx
    }

    /// `session/workflow_approve` 决策落地。
    pub fn resolve_approval(
        &self,
        request_id: &str,
        decision: moc_protocol::WorkflowDecision,
        sink: &UpdateSink,
    ) -> Result<String, CoreError> {
        let Some(tx) = self.approvals.lock().remove(request_id) else {
            return Err(CoreError::Session(
                moc_i18n::t!("core.err.workflow_approval_missing", id = request_id).into_owned(),
            ));
        };
        let approved = matches!(decision, moc_protocol::WorkflowDecision::Approve { .. });
        let feedback = match &decision {
            moc_protocol::WorkflowDecision::Reject { feedback } => feedback.clone(),
            _ => None,
        };
        let _ = tx.send(decision);
        let session_id = self.session_id.clone();
        let sink = sink.clone();
        let request_id = request_id.to_string();
        tokio::spawn(async move {
            let _ = sink
                .emit(
                    session_id,
                    moc_protocol::SessionUpdate::Workflow {
                        run_id: String::new(),
                        event: WorkflowEvent::ApprovalResolved {
                            request_id,
                            approved,
                            feedback,
                        },
                    },
                )
                .await;
        });
        Ok(String::new())
    }

    /// 编译 + 落草稿；诊断非空时返回它们。
    pub fn compile_script(
        &self,
        run_id: &str,
        source: &str,
    ) -> Result<CompiledWorkflow, Vec<Diagnostic>> {
        let compiled = compile(source, &crate::workflow::compile::CompileOptions::default())?;
        let dir = self.run_dir(run_id).map_err(|e| {
            vec![Diagnostic {
                code: "E_IO",
                message: e.to_string(),
                line: 0,
                col: 0,
            }]
        })?;
        std::fs::create_dir_all(&dir).map_err(|e| {
            vec![Diagnostic {
                code: "E_IO",
                message: format!("failed to create run directory: {e}"),
                line: 0,
                col: 0,
            }]
        })?;
        std::fs::write(dir.join("script.ts"), source).map_err(|e| {
            vec![Diagnostic {
                code: "E_IO",
                message: format!("failed to write script draft: {e}"),
                line: 0,
                col: 0,
            }]
        })?;
        Ok(compiled)
    }
}

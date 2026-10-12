//! 计划批准方法与计划 DTO。

use serde::{Deserialize, Serialize};

use super::session::SessionMode;
use crate::method::{ClientRequest, Method};

/// `session/plan_approve`：批准或拒绝已提交计划。
///
/// 批准后原地执行退出计划模式并使用指定执行模型；新会话执行也使用该模型。
/// execution_model 缺省时使用审批发生时的当前会话模型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionPlanApprove;

impl Method for SessionPlanApprove {
    const NAME: &'static str = "session/plan_approve";
    type Params = SessionPlanApproveParams;
    type Result = SessionPlanApproveResult;
}

impl ClientRequest for SessionPlanApprove {}

/// 计划批准参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPlanApproveParams {
    pub session_id: String,
    pub plan_id: String,
    pub action: PlanAction,
    /// 缺省时使用审批发生时的会话模型；完整 selector 可保留推理强度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_model: Option<String>,
}

/// 用户对计划的处理动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    Execute,
    ExecuteCompacted,
    ExecuteNewSession,
    Reject,
}

/// 计划批准后的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPlanApproveResult {
    pub session_id: String,
    pub mode: SessionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_markdown: Option<String>,
}

/// 会话当前已知的计划。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanInfo {
    pub plan_id: String,
    pub title: String,
    pub path: String,
    pub markdown: String,
    pub status: PlanStatus,
}

/// 计划生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Submitted,
    Approved,
    Rejected,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_action_and_result_shapes() {
        assert_eq!(
            serde_json::to_value(PlanAction::ExecuteNewSession).expect("动作可序列化"),
            serde_json::json!("execute_new_session")
        );
        assert_eq!(
            serde_json::to_value(PlanStatus::Submitted).expect("状态可序列化"),
            serde_json::json!("submitted")
        );
        let result = SessionPlanApproveResult {
            session_id: "ses_2".into(),
            mode: SessionMode::Normal,
            auto_prompt: Some("开始执行计划。".into()),
            plan_markdown: Some("# 计划".into()),
        };
        let params = SessionPlanApproveParams {
            session_id: "ses_1".into(),
            plan_id: "plan_1".into(),
            action: PlanAction::Execute,
            execution_model: None,
        };
        assert_eq!(
            serde_json::to_value(params).expect("缺省执行模型可序列化"),
            serde_json::json!({"session_id": "ses_1", "plan_id": "plan_1", "action": "execute"})
        );
        assert_eq!(
            serde_json::from_value::<SessionPlanApproveParams>(serde_json::json!({
                "session_id": "ses_1", "plan_id": "plan_1", "action": "execute"
            }))
            .expect("缺省执行模型可反序列化")
            .execution_model,
            None
        );
        assert_eq!(
            serde_json::from_value::<SessionPlanApproveParams>(serde_json::json!({
                "session_id": "ses_1", "plan_id": "plan_1", "action": "execute", "execution_model": "vendor/model:high"
            }))
            .expect("指定执行模型可反序列化")
            .execution_model
            .as_deref(),
            Some("vendor/model:high")
        );
        assert_eq!(
            serde_json::to_value(SessionPlanApproveParams {
                session_id: "ses_1".into(),
                plan_id: "plan_1".into(),
                action: PlanAction::Execute,
                execution_model: Some("vendor/model:high".into()),
            })
            .expect("执行模型可序列化"),
            serde_json::json!({"session_id": "ses_1", "plan_id": "plan_1", "action": "execute", "execution_model": "vendor/model:high"})
        );
        assert_eq!(
            serde_json::to_value(result).expect("结果可序列化"),
            serde_json::json!({
                "session_id": "ses_2", "mode": "normal",
                "auto_prompt": "开始执行计划。", "plan_markdown": "# 计划"
            })
        );
    }
}

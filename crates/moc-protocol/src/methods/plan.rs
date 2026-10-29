//! 计划方法（批准 / 批注修订 / 版本 diff）与计划 DTO。

use serde::{Deserialize, Serialize};

use super::diff::ToolDiff;
use super::session::SessionEngine;
use crate::method::{ClientRequest, Method};

/// `session/plan_mode`：进入或退出 plan 模式。
///
/// plan 是叠加在会话引擎上的持久模式：进入时会话模型切到
/// `models.roles.plan` 指定的模型（该角色为 `none` 时沿用当前模型），退出时
/// 恢复进入前的模型；模型切换经 [`super::update::SessionUpdate::ModelChanged`]
/// 推送，plan 状态本身经 [`super::update::SessionUpdate::PlanModeChanged`] 推送。
/// 可在任意引擎、会话任意阶段进出，不影响原生工具表。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionPlanMode;

impl Method for SessionPlanMode {
    const NAME: &'static str = "session/plan_mode";
    type Params = SessionPlanModeParams;
    type Result = super::session::SessionInfo;
}

impl ClientRequest for SessionPlanMode {}

/// plan 模式切换参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPlanModeParams {
    /// 目标会话。
    pub session_id: String,
    /// `true` 进入 plan，`false` 退出。
    pub active: bool,
}

/// `session/plan_approve`：批准或拒绝已提交计划。
///
/// 批准后原地执行退出 plan 模式并使用指定执行模型；新会话执行也使用该模型。
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
    /// `execute_new_session` 的目标会话引擎；缺省沿用源会话引擎。
    /// 其他动作下忽略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_engine: Option<SessionEngine>,
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
    /// 批准后生效的会话引擎：`execute_new_session` 是新会话引擎，其余是源会话引擎。
    pub engine: SessionEngine,
    /// 批准后 `session_id` 所指会话的 plan 状态。
    pub plan_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_markdown: Option<String>,
}

/// `session/plan_revise`：提交审阅批注，请求模型按批注修订计划。
///
/// 本方法只负责落库与生成修订提示：它把批注写进会话、把计划置为「已收到修改
/// 意见」，并返回一条 core 生成的用户消息；前端随后按普通提示
/// （`session/prompt`）发送这条消息，修订轮因此与其他轮共用同一套流式、取消与
/// 压缩语义。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionPlanRevise;

impl Method for SessionPlanRevise {
    const NAME: &'static str = "session/plan_revise";
    type Params = SessionPlanReviseParams;
    type Result = SessionPlanReviseResult;
}

impl ClientRequest for SessionPlanRevise {}

/// 计划修订请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPlanReviseParams {
    pub session_id: String,
    pub plan_id: String,
    /// 逐条批注；行号锚定在该计划版本的 markdown 上。
    pub annotations: Vec<PlanAnnotation>,
    /// 整体意见；与逐条批注一起发给模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 计划修订请求的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPlanReviseResult {
    pub session_id: String,
    /// 会话引擎；修订在 plan 模式内进行，引擎本身不变。
    pub engine: SessionEngine,
    /// 会话 plan 状态；修订要求它保持 `true`。
    pub plan_active: bool,
    /// 由 core 生成的修订提示；前端按普通用户消息发送。
    pub prompt: String,
}

/// `session/plan_diff`：某个计划版本与它上一版的差异。
///
/// 没有上一版时返回空的 `diff` 与 `previous_plan_id: None`，不视为错误。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionPlanDiff;

impl Method for SessionPlanDiff {
    const NAME: &'static str = "session/plan_diff";
    type Params = SessionPlanDiffParams;
    type Result = SessionPlanDiffResult;
}

impl ClientRequest for SessionPlanDiff {}

/// 计划版本 diff 参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPlanDiffParams {
    pub session_id: String,
    pub plan_id: String,
}

/// 计划版本 diff 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPlanDiffResult {
    pub plan_id: String,
    /// 作为对比基准的上一版；`None` 表示这是首版。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_plan_id: Option<String>,
    /// 结构化 diff；`files` 为空表示没有改动或没有上一版。
    pub diff: ToolDiff,
}

/// 会话当前已知的计划版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanInfo {
    pub plan_id: String,
    pub title: String,
    pub path: String,
    pub markdown: String,
    pub status: PlanStatus,
    /// 审阅版本号：从 1 开始，起草中的计划为 0。
    #[serde(default)]
    pub version: u32,
    /// 本次修订基于的上一版；首次提交为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_plan_id: Option<String>,
    /// 用户针对这一版提交过的批注；未提交过为空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<PlanAnnotation>,
}

/// 计划生命周期状态。
///
/// 一次计划审阅的典型轨迹：`write_plan` 反复起草（`draft`）→ `exit_plan`
/// 提交（`submitted`）→ 用户提交批注（`revised`）→ 模型写出新版后旧版
/// （`superseded`）。`approved` / `rejected` 是终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// `write_plan` 已保存、还没 `exit_plan`；不进入审阅列表。
    Draft,
    /// 已提交，等待用户审阅。
    Submitted,
    /// 用户已提交批注，等待模型修订。
    Revised,
    /// 已有更新版本；保留用于回看与 diff。
    Superseded,
    Approved,
    Rejected,
}

/// 一条计划批注。
///
/// 批注锚定在某个计划版本的行区间上：`body` 是用户写的整段正文，允许在其中
/// 使用 ```` ```suggestion ```` 围栏直接给出「把这几行替换成……」，语义与
/// GitHub PR suggestion 一致；`quote` 只是用户实际选中的文本，供界面回显。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAnnotation {
    /// 前端生成的稳定 id。
    pub id: String,
    /// 锚定区间起始行（1-based，含）。
    pub start_line: u32,
    /// 锚定区间结束行（1-based，含）。
    pub end_line: u32,
    /// 用户选中的文本；按块级锚定时为空。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub quote: String,
    /// 批注正文。
    pub body: String,
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
        assert_eq!(
            serde_json::to_value(PlanStatus::Draft).expect("起草态可序列化"),
            serde_json::json!("draft")
        );
        assert_eq!(
            serde_json::to_value(PlanStatus::Revised).expect("修订态可序列化"),
            serde_json::json!("revised")
        );
        assert_eq!(
            serde_json::to_value(PlanStatus::Superseded).expect("被取代态可序列化"),
            serde_json::json!("superseded")
        );
        let result = SessionPlanApproveResult {
            session_id: "ses_2".into(),
            engine: SessionEngine::Standard,
            plan_active: false,
            auto_prompt: Some("开始执行计划。".into()),
            plan_markdown: Some("# 计划".into()),
        };
        let params = SessionPlanApproveParams {
            session_id: "ses_1".into(),
            plan_id: "plan_1".into(),
            action: PlanAction::Execute,
            execution_model: None,
            execution_engine: None,
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
                action: PlanAction::ExecuteNewSession,
                execution_model: Some("vendor/model:high".into()),
                execution_engine: Some(SessionEngine::Ptc),
            })
            .expect("执行模型与引擎可序列化"),
            serde_json::json!({"session_id": "ses_1", "plan_id": "plan_1", "action": "execute_new_session", "execution_model": "vendor/model:high", "execution_engine": "ptc"})
        );
        assert_eq!(
            serde_json::to_value(result).expect("结果可序列化"),
            serde_json::json!({
                "session_id": "ses_2", "engine": "standard", "plan_active": false,
                "auto_prompt": "开始执行计划。", "plan_markdown": "# 计划"
            })
        );
    }

    /// 批注与版本信息按「缺省即省略」上线，旧计划数据不带这些键也能解析。
    #[test]
    fn plan_projection_and_annotations_round_trip() {
        let info = PlanInfo {
            plan_id: "plan_1".into(),
            title: "计划".into(),
            path: "local://plan_1.md".into(),
            markdown: "# 计划\n\n1. 做点什么\n".into(),
            status: PlanStatus::Revised,
            version: 2,
            previous_plan_id: Some("plan_0".into()),
            annotations: vec![PlanAnnotation {
                id: "a1".into(),
                start_line: 3,
                end_line: 3,
                quote: "做点什么".into(),
                body: "太笼统\n\n```suggestion\n改 App.tsx\n```".into(),
            }],
        };
        let value = serde_json::to_value(&info).expect("计划可序列化");
        assert_eq!(value["status"], serde_json::json!("revised"));
        assert_eq!(value["version"], serde_json::json!(2));
        assert_eq!(value["previous_plan_id"], serde_json::json!("plan_0"));
        assert_eq!(value["annotations"][0]["start_line"], serde_json::json!(3));
        assert_eq!(
            value["annotations"][0]["body"],
            serde_json::json!("太笼统\n\n```suggestion\n改 App.tsx\n```")
        );

        let legacy = serde_json::json!({
            "plan_id": "plan_1", "title": "计划", "path": "local://plan_1.md",
            "markdown": "# 计划", "status": "submitted"
        });
        let parsed: PlanInfo = serde_json::from_value(legacy).expect("旧形状可反序列化");
        assert_eq!(parsed.version, 0);
        assert_eq!(parsed.previous_plan_id, None);
        assert!(parsed.annotations.is_empty());
    }

    #[test]
    fn revise_and_diff_shapes() {
        let params = SessionPlanReviseParams {
            session_id: "ses_1".into(),
            plan_id: "plan_1".into(),
            annotations: Vec::new(),
            note: None,
        };
        assert_eq!(
            serde_json::to_value(params).expect("修订参数可序列化"),
            serde_json::json!({"session_id": "ses_1", "plan_id": "plan_1", "annotations": []})
        );
        let result = SessionPlanReviseResult {
            session_id: "ses_1".into(),
            engine: SessionEngine::Standard,
            plan_active: true,
            prompt: "请按批注修订".into(),
        };
        assert_eq!(
            serde_json::to_value(result).expect("修订结果可序列化"),
            serde_json::json!({"session_id": "ses_1", "engine": "standard", "plan_active": true, "prompt": "请按批注修订"})
        );
        let diff = SessionPlanDiffResult {
            plan_id: "plan_2".into(),
            previous_plan_id: None,
            diff: ToolDiff {
                files: Vec::new(),
                truncated: false,
            },
        };
        assert_eq!(
            serde_json::to_value(diff).expect("首版 diff 可序列化"),
            serde_json::json!({"plan_id": "plan_2", "diff": {"files": [], "truncated": false}})
        );
    }
}

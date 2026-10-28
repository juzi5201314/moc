//! `session/update` 通知：core 单向推送提示过程。
//!
//! 通知**不透传** `moc_llm::StreamEvent`：那是供应商细节（内容块序号、签名、
//! 上游响应 ID），前端既无法渲染也不需要。core 负责把它投影成 UI 友好的
//! [`AssistantDelta`]，只保留前端能直接渲染的文本类内容。
//!
//! # `StreamEvent` → `AssistantDelta` 投影表
//!
//! | `moc_llm::StreamEvent` | 动作 |
//! |---|---|
//! | `Start { id, model, effort }` | 丢弃（`model` 已由 `prompt_started` 给出，上游 `id` 前端不需要）。`model` / `effort` 被累加进完整响应，core 在响应结束后与请求值比对，不一致时另发 `upstream_mismatch` |
//! | `TextDelta { index, text }` | `Text { delta: text }` |
//! | `ReasoningDelta { index, text }` | `Reasoning { delta: text }` |
//! | `ReasoningSignature { index, signature }` | 丢弃（不透明，前端无法渲染；完整内容随 `prompt_finished.message` 给出） |
//! | `ReasoningItem { index, item }` | 丢弃（同上） |
//! | `ToolCallStart { index, id, name }` | 丢弃（调用块由 [`SessionUpdate::ToolStarted`] 建立，名字随它给出） |
//! | `ToolCallArgumentsDelta { index, json }` | 丢弃（参数随 [`SessionUpdate::ToolStarted`] 的 `arguments` 一次性给出） |
//! | `Usage(usage)` | `Usage { usage }` |
//!
//! `index` 是同一响应内内容块的位置，只在一次流内有效。
//!
//! # 工具调用参数
//!
//! 参数**只**经 [`SessionUpdate::ToolStarted`] / [`SessionUpdate::ToolFinished`] 的
//! `arguments` 下发，内容是 core 实际执行的那一份；前端不必自己拼接增量，也就不存在
//! 「增量与执行分叉」的形态。模型直发的调用与 `run_code` 沙盒里的嵌套调用共用这两个
//! 更新：嵌套调用的 `parent_tool_call_id` 指向发起它的 `run_code`，其余字段含义相同。
//!
//! # 兼容规则
//!
//! [`SessionUpdate`] 与 [`AssistantDelta`] 是会增长的枚举：core 可以新增变体
//! （不升 major），前端遇到不认识的 `type` 必须**整条丢弃**而不是报错或断开连接。

use serde::{Deserialize, Serialize};

use super::diff::ToolDiff;
use super::session::{SessionLockInfo, ToolStatus};
use super::{session::SessionMode, todo::TodoPhase};
use crate::method::{Method, ServerNotification};
use crate::{FinishReason, Message, ToolResult, Usage};

/// `session/update` 的方法标记。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUpdateMethod;

impl Method for SessionUpdateMethod {
    const NAME: &'static str = "session/update";
    type Params = SessionUpdateParams;
    type Result = crate::methods::Empty;
}

// 通知不返回结果；`Result` 只是 `Method` 的形状要求。
impl ServerNotification for SessionUpdateMethod {}

/// `session/update` 的参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdateParams {
    /// 会话 ID。
    pub session_id: String,
    /// 本次更新。
    pub update: SessionUpdate,
}
/// 同一模型响应返回的一批工具调用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolBatchCall {
    /// 调用 ID。
    pub tool_call_id: String,
    /// 工具名。
    pub name: String,
}

/// 一条会话更新。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// 父会话中的子代理卡片已创建。
    AgentStarted { agent: super::agent::AgentInfo },
    /// 子代理运行状态或最新输出变化。
    AgentStatusChanged {
        agent_id: String,
        status: super::agent::AgentStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_message: Option<String>,
    },
    /// 其它代理向本会话投递了一条消息（已写入 transcript）。
    AgentMessage {
        /// 来源元信息。
        meta: super::agent::AgentMessageMeta,
        /// 消息正文。
        body: String,
    },
    /// 运行中工具的进度快照（目前仅 `wait_agent` 推送）。
    ToolProgress {
        /// 提示 ID。
        prompt_id: String,
        /// 调用 ID，与 [`SessionUpdate::ToolFinished`] 配对。
        tool_call_id: String,
        /// 进度行。
        rows: Vec<super::agent::ToolProgressRow>,
    },
    /// 一批工具调用开始执行；前端可按工具名聚合展示。
    ToolBatchStarted {
        /// 提示 ID。
        prompt_id: String,
        /// 批次 ID。
        batch_id: String,
        /// 批次内的调用，保持模型返回顺序。
        calls: Vec<ToolBatchCall>,
    },
    /// 提示开始。
    PromptStarted {
        /// prompt ID。
        prompt_id: String,
        /// 实际使用的模型 ID。
        model: String,
        /// 开始时间的 UNIX 毫秒时间戳。
        at_ms: u64,
    },
    /// assistant 增量。
    AssistantDelta {
        /// prompt ID。
        prompt_id: String,
        /// 增量内容。
        delta: AssistantDelta,
    },
    /// 新一轮模型生成开始；前端据此限定失败预览的清除边界。
    GenerationStarted { prompt_id: String },
    /// 用户手动执行的命令开始；后续仍以 `ToolFinished` 回填结果。
    BashStarted {
        tool_call_id: String,
        command: super::bash::BashCommand,
    },
    /// 一次工具调用开始执行（模型直发与 `run_code` 沙盒内的嵌套调用共用）。
    ToolStarted {
        /// prompt ID。
        prompt_id: String,
        /// 调用 ID，与后续 [`SessionUpdate::ToolFinished`] 配对。
        tool_call_id: String,
        /// 工具名。
        name: String,
        /// 父调用 ID；`Some` 表示这是 `run_code` 沙盒里发起、以该调用为父的嵌套调用。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
        /// 参数 JSON 原文：core 实际执行的那一份，前端直接展示。
        arguments: String,
    },
    /// 一次工具调用结束。
    ToolFinished {
        /// prompt ID。
        prompt_id: String,
        /// 调用 ID。
        tool_call_id: String,
        /// 工具名；与 [`SessionUpdate::ToolStarted`] 同源，前端补建缺席的调用块时据此命名。
        name: String,
        /// 父调用 ID；与 [`SessionUpdate::ToolStarted`] 同源。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
        /// 参数 JSON 原文；与 [`SessionUpdate::ToolStarted`] 同源，供前端补建缺席的调用块。
        arguments: String,
        /// 终态。
        status: ToolStatus,
        /// 工具回灌给模型的结果原文。
        result: ToolResult,
        /// 执行耗时毫秒；`None` 表示不计时。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// 文件变更的结构化 diff；仅 `edit` / `write` 成功时由 core 给出，
        /// 授权弹窗与 transcript 的 diff 渲染都以此为准。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<ToolDiff>,
    },
    /// core 正在等前端授权（与 [`SessionUpdate::PermissionResolved`] 成对）。
    ///
    /// 这两条镜像更新由 **core** 负责发：`ask_permission` 只发反向请求，
    /// 不隐式发通知，好让 transcript 里能内联渲染「等待授权」状态。
    PermissionRequested {
        /// prompt ID。
        prompt_id: String,
        /// 授权请求 ID。
        request_id: String,
        /// 相关调用 ID。
        tool_call_id: String,
    },
    /// 授权询问结束。
    PermissionResolved {
        /// prompt ID。
        prompt_id: String,
        /// 授权请求 ID。
        request_id: String,
        /// 前端选中的选项；`None` 表示前端没给选择（未注册处理器、选项非法、连接中断），core 按拒绝处理。
        option_id: Option<String>,
    },
    /// 提示结束；`message` 与 `session/prompt` 响应的 [`super::prompt::PromptResult::message`] 相同。
    PromptFinished {
        /// prompt ID。
        prompt_id: String,
        /// 权威 assistant 消息。
        message: Message,
        /// 结束原因。
        finish_reason: FinishReason,
        /// token 用量。
        usage: Usage,
    },
    /// 本地循环纠偏、慢请求警告或循环终止；终止时丢弃尚未提交的生成预览。
    GuardNotice {
        prompt_id: String,
        message: String,
        terminal: bool,
    },
    /// 插件失败：加载或调用出错，且插件因此在本进程内停用。
    ///
    /// 修复插件后需要 `plugin/reload` 或重启 moc 才能恢复；`message` 面向人，
    /// 有 JS 调用栈时是多行文本。
    PluginNotice {
        /// 插件名。
        plugin: String,
        /// 稳定错误码（`E_PLUGIN_*`）。
        code: String,
        /// 失败原因。
        message: String,
    },
    /// 标题变化（core 自动命名或 `session/rename` 之后）。
    TitleChanged {
        /// 新标题。
        title: String,
    },
    /// 锁投影变化。
    LockChanged {
        /// 新的锁投影；`None` 表示锁已释放。
        lock: Option<SessionLockInfo>,
    },
    /// 会话命令执行权限变化。
    PermissionChanged {
        permission: super::session::SessionPermission,
    },
    /// 会话工作模式变化。
    ModeChanged { mode: SessionMode },
    /// 会话模型变化（`session/set_model`，或计划模式切换引起的模型覆盖与恢复）。
    ModelChanged { model: String },
    /// Todo 清单变化。
    TodoChanged { phases: Vec<TodoPhase> },
    /// 隐式注入了一次 todo 提醒；前端按工具样式展示占位行，提醒本身无参数与结果。
    TodoReminder {
        /// prompt ID。
        prompt_id: String,
        /// 仍未完成的待办数。
        open_items: usize,
    },
    /// 提交新计划；`plan` 是该版本的完整投影。
    PlanSubmitted { plan: super::plan::PlanInfo },
    /// 计划投影变化（收到批注、被新版取代、批准或拒绝）。
    ///
    /// 前端按 `plan.plan_id` 覆盖本地投影，不要自己推断状态迁移。
    PlanUpdated { plan: super::plan::PlanInfo },
    /// 上下文压缩开始：前端应开始计时，并在完成或失败时原位替换这条提示。
    ///
    /// 只有需要等待上游模型的压缩（`remote` / `soft`）才发这条；纯本地裁剪
    /// （`shake`）瞬时可成，不单独占用一条进行中提示。
    ContextCompactionStarted,
    /// 上下文压缩完成。
    ContextCompacted {
        method: String,
        tokens_before: u64,
        tokens_after: u64,
        /// 本次是否由模型重新生成了摘要；`false` 表示只裁剪了历史（`shake`）。
        full: bool,
    },
    /// 上下文压缩失败，用来终止一次 [`SessionUpdate::ContextCompactionStarted`]。
    ContextCompactionFailed {
        /// 失败原因。
        message: String,
    },
    /// 后台任务结束。
    JobFinished {
        job_id: String,
        command: String,
        exit_code: Option<i32>,
        log_path: String,
    },
    /// 工作区发生变化。
    WorkspaceChanged {
        violation: bool,
        changes: Vec<PathChange>,
    },
    /// 上游实际使用的模型或推理强度与本地请求不一致。
    ///
    /// 网关可能静默改写模型（降级 / 映射）或降低推理强度；每次上游请求完成后由
    /// core 比对一次，只在不一致时推送。前端应把它渲染成醒目的告警，并在重复
    /// 出现时替换而非堆叠。
    UpstreamMismatch {
        /// prompt ID。
        prompt_id: String,
        /// 每一处不一致；至少一项（空列表没有任何意义，core 不会发送）。
        fields: Vec<UpstreamFieldMismatch>,
    },
    /// 一条补充消息已经由 core 写入当前会话。
    ///
    /// 前端在收到它之前只能把消息放在待发送区；收到后应把对应项移入转写。
    /// `id` 是 `session/steer` 返回的 core ID，`text` 仅用于事件先于 RPC
    /// 响应到达时兜底展示。
    SteeringApplied {
        /// core 分配的 steering ID。
        id: u64,
        /// 已写入会话的正文字本；图片消息可能为空。
        text: String,
    },
    /// 提示内不可恢复错误。
    Error {
        prompt_id: String,
        message: String,
        retryable: bool,
    },
    /// `session/retry` 已经剪掉上一轮没跑完的尾巴：前端应把那条提示留下的块撤掉。
    ///
    /// 只会紧跟在进行中的重试之前到达，被剪掉的内容已从会话文件里删除，重新加载后
    /// 同样看不到。`replayed_tools` 表示那批工具调用被留下了、正在原样重放——此时
    /// assistant 消息仍在会话里，前端不能把它的正文当成失败内容撤掉。
    PromptRewound { replayed_tools: bool },
}

/// 工作区路径变化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathChange {
    pub path: String,
    pub kind: String,
}

/// 一处「本地请求值 ≠ 上游回显值」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamFieldMismatch {
    /// 被改写的字段。
    pub field: UpstreamField,
    /// 本地写入请求的值。
    pub requested: String,
    /// 上游回显的值。
    pub reported: String,
}

/// 可能被上游静默改写的请求字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamField {
    /// 模型 id（上游 `model` 字段）。
    Model,
    /// 推理强度（线上原文，未经 moc-llm 侧的名称映射还原）。
    Effort,
}

/// assistant 增量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantDelta {
    /// 正文增量。
    Text {
        /// 增量文本。
        delta: String,
    },
    /// 思维链增量。
    Reasoning {
        /// 增量文本。
        delta: String,
    },
    /// token 用量刷新。
    Usage {
        /// 用量。
        usage: Usage,
    },
}

/// `session/prompt` 的提示结束通知与响应共用的构造。
impl SessionUpdate {
    /// 提示开始。
    pub fn prompt_started(
        prompt_id: impl Into<String>,
        model: impl Into<String>,
        at_ms: u64,
    ) -> Self {
        Self::PromptStarted {
            prompt_id: prompt_id.into(),
            model: model.into(),
            at_ms,
        }
    }

    /// assistant 增量。
    pub fn delta(prompt_id: impl Into<String>, delta: AssistantDelta) -> Self {
        Self::AssistantDelta {
            prompt_id: prompt_id.into(),
            delta,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentPart, Role};
    use serde_json::json;

    fn usage() -> Usage {
        Usage {
            input_tokens: 12,
            output_tokens: 3,
            ..Usage::default()
        }
    }

    #[test]
    fn notification_shape() {
        assert_eq!(<SessionUpdateMethod as Method>::NAME, "session/update");
        let params = SessionUpdateParams {
            session_id: "ses_7f3a".into(),
            update: SessionUpdate::delta(
                "prompt_1",
                AssistantDelta::Text {
                    delta: "好的".into(),
                },
            ),
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            json!({
                "session_id": "ses_7f3a",
                "update": {
                    "type": "assistant_delta",
                    "prompt_id": "prompt_1",
                    "delta": { "type": "text", "delta": "好的" }
                }
            })
        );
    }

    #[test]
    fn every_variant_shape() {
        let agent_info = crate::methods::AgentInfo {
            id: "ses_child".into(),
            parent_id: "ses_parent".into(),
            title: "child".into(),
            model: "p/model".into(),
            cwd: "/tmp".into(),
            status: crate::methods::AgentStatus::Queued,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_message: None,
        };
        let cases = [
            (
                SessionUpdate::prompt_started("prompt_1", "deepseek-flash", 1_724_000_000_000),
                json!({ "type": "prompt_started", "prompt_id": "prompt_1", "model": "deepseek-flash", "at_ms": 1_724_000_000_000u64 }),
            ),
            (
                SessionUpdate::delta(
                    "prompt_1",
                    AssistantDelta::Reasoning {
                        delta: "想".into()
                    },
                ),
                json!({ "type": "assistant_delta", "prompt_id": "prompt_1", "delta": { "type": "reasoning", "delta": "想" } }),
            ),
            (
                SessionUpdate::delta("prompt_1", AssistantDelta::Usage { usage: usage() }),
                json!({ "type": "assistant_delta", "prompt_id": "prompt_1", "delta": { "type": "usage", "usage": { "input_tokens": 12, "output_tokens": 3, "total_tokens": null, "cached_input_tokens": null, "cache_creation_input_tokens": null, "reasoning_tokens": null } } }),
            ),
            (
                SessionUpdate::ToolStarted {
                    prompt_id: "prompt_1".into(),
                    tool_call_id: "call_9".into(),
                    name: "bash".into(),
                    parent_tool_call_id: None,
                    arguments: "{\"command\":\"ls\"}".into(),
                },
                json!({ "type": "tool_started", "prompt_id": "prompt_1", "tool_call_id": "call_9", "name": "bash", "arguments": "{\"command\":\"ls\"}" }),
            ),
            (
                SessionUpdate::ToolStarted {
                    prompt_id: "prompt_1".into(),
                    tool_call_id: "call_0:1".into(),
                    name: "read".into(),
                    parent_tool_call_id: Some("call_0".into()),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                },
                json!({ "type": "tool_started", "prompt_id": "prompt_1", "tool_call_id": "call_0:1", "name": "read", "parent_tool_call_id": "call_0", "arguments": "{\"path\":\"a.rs\"}" }),
            ),
            (
                SessionUpdate::ToolFinished {
                    prompt_id: "prompt_1".into(),
                    tool_call_id: "call_9".into(),
                    name: "bash".into(),
                    parent_tool_call_id: None,
                    arguments: "{\"command\":\"cargo test\"}".into(),
                    status: ToolStatus::Denied,
                    result: ToolResult {
                        tool_call_id: "call_9".into(),
                        content: "用户拒绝".into(),
                        is_error: true,

                        images: Vec::new(),
                    },
                    duration_ms: None,
                    diff: None,
                },
                json!({ "type": "tool_finished", "prompt_id": "prompt_1", "tool_call_id": "call_9", "name": "bash", "arguments": "{\"command\":\"cargo test\"}", "status": "denied", "result": { "tool_call_id": "call_9", "content": "用户拒绝", "is_error": true } }),
            ),
            (
                SessionUpdate::ToolFinished {
                    prompt_id: "prompt_1".into(),
                    tool_call_id: "call_9".into(),
                    name: "edit".into(),
                    parent_tool_call_id: Some("call_8".into()),
                    arguments: "{\"path\":\"/tmp/a.rs\"}".into(),
                    status: ToolStatus::Completed,
                    result: ToolResult {
                        tool_call_id: "call_9".into(),
                        content: "已编辑".into(),
                        is_error: false,
                        images: Vec::new(),
                    },
                    duration_ms: Some(12),
                    diff: Some(ToolDiff {
                        files: vec![super::super::diff::ToolFileDiff {
                            path: "/tmp/a.rs".into(),
                            hunks: vec![super::super::diff::DiffHunk {
                                old_start: 1,
                                new_start: 1,
                                lines: vec![super::super::diff::DiffLine {
                                    kind: super::super::diff::DiffLineKind::Delete,
                                    old_no: Some(1),
                                    new_no: None,
                                    text: "旧行".into(),
                                }],
                            }],
                        }],
                        truncated: false,
                    }),
                },
                json!({ "type": "tool_finished", "prompt_id": "prompt_1", "tool_call_id": "call_9", "name": "edit", "parent_tool_call_id": "call_8", "arguments": "{\"path\":\"/tmp/a.rs\"}", "status": "completed", "result": { "tool_call_id": "call_9", "content": "已编辑", "is_error": false }, "duration_ms": 12, "diff": { "files": [{ "path": "/tmp/a.rs", "hunks": [{ "old_start": 1, "new_start": 1, "lines": [{ "kind": "delete", "old_no": 1, "text": "旧行" }] }] }], "truncated": false } }),
            ),
            (
                SessionUpdate::PermissionRequested {
                    prompt_id: "prompt_1".into(),
                    request_id: "perm_1".into(),
                    tool_call_id: "call_9".into(),
                },
                json!({ "type": "permission_requested", "prompt_id": "prompt_1", "request_id": "perm_1", "tool_call_id": "call_9" }),
            ),
            (
                SessionUpdate::PermissionResolved {
                    prompt_id: "prompt_1".into(),
                    request_id: "perm_1".into(),
                    option_id: None,
                },
                json!({ "type": "permission_resolved", "prompt_id": "prompt_1", "request_id": "perm_1", "option_id": null }),
            ),
            (
                SessionUpdate::PromptFinished {
                    prompt_id: "prompt_1".into(),
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentPart::Text("好的".into())],
                    },
                    finish_reason: FinishReason::Stop,
                    usage: usage(),
                },
                json!({ "type": "prompt_finished", "prompt_id": "prompt_1", "message": { "role": "Assistant", "content": [{ "Text": "好的" }] }, "finish_reason": "Stop", "usage": { "input_tokens": 12, "output_tokens": 3, "total_tokens": null, "cached_input_tokens": null, "cache_creation_input_tokens": null, "reasoning_tokens": null } }),
            ),
            (
                SessionUpdate::TitleChanged {
                    title: "新标题".into(),
                },
                json!({ "type": "title_changed", "title": "新标题" }),
            ),
            (
                SessionUpdate::LockChanged {
                    lock: Some(SessionLockInfo {
                        pid: 7,
                        frontend: "tui".into(),
                        process_name: None,
                        acquired_at_ms: 5,
                    }),
                },
                json!({ "type": "lock_changed", "lock": { "pid": 7, "frontend": "tui", "acquired_at_ms": 5 } }),
            ),
            (
                SessionUpdate::LockChanged { lock: None },
                json!({ "type": "lock_changed", "lock": null }),
            ),
            (
                SessionUpdate::ModeChanged {
                    mode: SessionMode::Ask,
                },
                json!({ "type": "mode_changed", "mode": "ask" }),
            ),
            (
                SessionUpdate::ModelChanged {
                    model: "deepseek-v4".into(),
                },
                json!({ "type": "model_changed", "model": "deepseek-v4" }),
            ),
            (
                SessionUpdate::TodoChanged { phases: Vec::new() },
                json!({ "type": "todo_changed", "phases": [] }),
            ),
            (
                SessionUpdate::TodoReminder {
                    prompt_id: "p1".into(),
                    open_items: 3,
                },
                json!({ "type": "todo_reminder", "prompt_id": "p1", "open_items": 3 }),
            ),
            (
                SessionUpdate::PlanSubmitted {
                    plan: crate::methods::plan::PlanInfo {
                        plan_id: "plan_1".into(),
                        title: "标题".into(),
                        path: "/tmp/plan.md".into(),
                        markdown: "# 计划".into(),
                        status: crate::methods::plan::PlanStatus::Submitted,
                        version: 1,
                        previous_plan_id: None,
                        annotations: Vec::new(),
                    },
                },
                json!({
                    "type": "plan_submitted",
                    "plan": {
                        "plan_id": "plan_1", "title": "标题", "path": "/tmp/plan.md",
                        "markdown": "# 计划", "status": "submitted", "version": 1
                    }
                }),
            ),
            (
                SessionUpdate::ContextCompactionStarted,
                json!({ "type": "context_compaction_started" }),
            ),
            (
                SessionUpdate::ContextCompacted {
                    method: "soft".into(),
                    tokens_before: 10,
                    tokens_after: 4,
                    full: true,
                },
                json!({ "type": "context_compacted", "method": "soft", "tokens_before": 10, "tokens_after": 4, "full": true }),
            ),
            (
                SessionUpdate::ContextCompacted {
                    method: "shake".into(),
                    tokens_before: 10,
                    tokens_after: 7,
                    full: false,
                },
                json!({ "type": "context_compacted", "method": "shake", "tokens_before": 10, "tokens_after": 7, "full": false }),
            ),
            (
                SessionUpdate::ContextCompactionFailed {
                    message: "上游不可用".into(),
                },
                json!({ "type": "context_compaction_failed", "message": "上游不可用" }),
            ),
            (
                SessionUpdate::JobFinished {
                    job_id: "job_1".into(),
                    command: "cargo test".into(),
                    exit_code: Some(0),
                    log_path: "/tmp/job.log".into(),
                },
                json!({ "type": "job_finished", "job_id": "job_1", "command": "cargo test", "exit_code": 0, "log_path": "/tmp/job.log" }),
            ),
            (
                SessionUpdate::WorkspaceChanged {
                    violation: true,
                    changes: vec![PathChange {
                        path: "src/lib.rs".into(),
                        kind: "modified".into(),
                    }],
                },
                json!({ "type": "workspace_changed", "violation": true, "changes": [{ "path": "src/lib.rs", "kind": "modified" }] }),
            ),
            (
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
                json!({ "type": "upstream_mismatch", "prompt_id": "prompt_1", "fields": [
                    { "field": "model", "requested": "glm-5.3", "reported": "glm-5.3-air" },
                    { "field": "effort", "requested": "max", "reported": "high" },
                ] }),
            ),
            (
                SessionUpdate::AgentStarted {
                    agent: agent_info.clone(),
                },
                json!({"type":"agent_started","agent":serde_json::to_value(&agent_info).unwrap()}),
            ),
            (
                SessionUpdate::AgentStatusChanged {
                    agent_id: "ses_child".into(),
                    status: crate::methods::AgentStatus::Running,
                    last_message: None,
                },
                json!({"type":"agent_status_changed","agent_id":"ses_child","status":"running"}),
            ),
            (
                SessionUpdate::AgentMessage {
                    meta: crate::methods::AgentMessageMeta {
                        from_id: "ses_parent".into(),
                        from_title: "主会话".into(),
                        delivery: crate::methods::AgentMessageOutcome::Injected,
                    },
                    body: "继续处理".into(),
                },
                json!({
                    "type": "agent_message",
                    "meta": {
                        "from_id": "ses_parent",
                        "from_title": "主会话",
                        "delivery": "injected"
                    },
                    "body": "继续处理"
                }),
            ),
            (
                SessionUpdate::ToolProgress {
                    prompt_id: "prompt_1".into(),
                    tool_call_id: "call_9".into(),
                    rows: vec![crate::methods::ToolProgressRow {
                        agent_id: "ses_child".into(),
                        title: "子代理 A".into(),
                        status: crate::methods::AgentStatus::Running,
                        elapsed_ms: 1_200,
                    }],
                },
                json!({
                    "type": "tool_progress",
                    "prompt_id": "prompt_1",
                    "tool_call_id": "call_9",
                    "rows": [{
                        "agent_id": "ses_child",
                        "title": "子代理 A",
                        "status": "running",
                        "elapsed_ms": 1200
                    }]
                }),
            ),
            (
                SessionUpdate::SteeringApplied {
                    id: 7,
                    text: "先保留原文件".into(),
                },
                json!({ "type": "steering_applied", "id": 7, "text": "先保留原文件" }),
            ),
            (
                SessionUpdate::Error {
                    prompt_id: "prompt_1".into(),
                    message: "失败".into(),
                    retryable: false,
                },
                json!({ "type": "error", "prompt_id": "prompt_1", "message": "失败", "retryable": false }),
            ),
            (
                SessionUpdate::PromptRewound {
                    replayed_tools: false,
                },
                json!({ "type": "prompt_rewound", "replayed_tools": false }),
            ),
        ];
        for (update, expected) in cases {
            let actual = serde_json::to_value(&update).expect("可序列化");
            assert_eq!(actual, expected);
            let round_trip: SessionUpdate = serde_json::from_value(actual).expect("可反序列化");
            assert_eq!(round_trip, update);
        }
    }

    #[test]
    fn unknown_type_is_rejected() {
        // 前端「忽略未知更新」的落地方式：解析失败即整条丢弃，不让连接受影响
        assert!(
            serde_json::from_value::<SessionUpdate>(json!({ "type": "future_thing", "x": 1 }))
                .is_err()
        );
        assert!(
            serde_json::from_value::<AssistantDelta>(json!({ "type": "future_delta" })).is_err()
        );
    }
}

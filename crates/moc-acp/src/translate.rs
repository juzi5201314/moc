//! moc 协议 DTO 与 ACP schema 之间的纯转换：不发起任何调用，只负责形状映射。

use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    self as acp, ConfigOptionUpdate, Content, ContentBlock, ContentChunk, Cost, CurrentModeUpdate,
    Diff, EmbeddedResourceResource, ImageContent, Notice, NoticeSeverity, Plan, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionConfigSelectOptions, SessionInfoUpdate, SessionMode,
    SessionModeState, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use moc_protocol::methods::{
    AssistantDelta, ConfigGetResult, DiffLineKind, InputPart, ModelInfo, PlanInfo, PlanStatus,
    SessionAutonomy, SessionInfo, SessionUpdate as MocUpdate, TodoPhase, TodoStatus, ToolDiff,
    ToolStatus, TranscriptEntry, UpstreamFieldMismatch,
};
use moc_protocol::{
    ContentPart, FinishReason, ImageSource, ReasoningBlock, Role, ToolResult, Usage,
};

/// ACP 会话模式 ID：直接复用 moc `SessionAutonomy` 的线上名，外加 `plan`。
pub(crate) const MODE_DEFAULT: &str = "default";
pub(crate) const MODE_DONT_ASK: &str = "dont_ask";
pub(crate) const MODE_UNATTENDED: &str = "unattended";
pub(crate) const MODE_PLAN: &str = "plan";

/// 模型配置项的 config_id；`session/set_config_option` 只认这一个。
pub(crate) const CONFIG_MODEL: &str = "model";

/// 会话当前状态的投影：由 `SessionInfo` 初始化、由会话内更新事件增量维护，
/// 供 `current_mode`、usage 上下文与 config_option 当前值使用。
#[derive(Debug, Default, Clone)]
pub(crate) struct SessionProjection {
    pub model: String,
    pub autonomy: SessionAutonomy,
    pub plan_active: bool,
}

impl SessionProjection {
    pub fn from_info(info: &SessionInfo) -> Self {
        Self {
            model: info.model.clone(),
            autonomy: info.autonomy,
            plan_active: info.plan_active,
        }
    }

    /// 当前模式 ID：plan 激活时优先于自主度档位。
    pub fn mode_id(&self) -> &'static str {
        if self.plan_active {
            MODE_PLAN
        } else {
            match self.autonomy {
                SessionAutonomy::Default => MODE_DEFAULT,
                SessionAutonomy::DontAsk => MODE_DONT_ASK,
                SessionAutonomy::Unattended => MODE_UNATTENDED,
            }
        }
    }
}

/// `session/new` 等响应里的 modes 块：固定四种，current 由投影决定。
pub(crate) fn mode_state(proj: &SessionProjection) -> SessionModeState {
    SessionModeState::new(
        proj.mode_id(),
        vec![
            SessionMode::new(MODE_DEFAULT, "默认").description("所有确认都等用户本人回答"),
            SessionMode::new(MODE_DONT_ASK, "免询问")
                .description("问题由代理代答：有推荐项选推荐项"),
            SessionMode::new(MODE_UNATTENDED, "无人值守")
                .description("在免询问之上，计划提交自动批准「压缩并执行」"),
            SessionMode::new(MODE_PLAN, "计划")
                .description("先产出实施计划，批准后才允许写入与执行"),
        ],
    )
}

/// 会话 config_options 里的模型下拉框；模型表为空（config/get 失败）时返回 `None`。
pub(crate) fn model_option(models: &[ModelInfo], current: &str) -> Option<SessionConfigOption> {
    if models.is_empty() {
        return None;
    }
    let options = models
        .iter()
        .map(|m| {
            let option = SessionConfigSelectOption::new(m.id.clone(), m.display_name.clone());
            match m.context_window {
                Some(window) => option.description(format!("{} tokens 上下文", window)),
                None => option,
            }
        })
        .collect();
    Some(
        SessionConfigOption::select(
            CONFIG_MODEL,
            "模型",
            current.to_owned(),
            SessionConfigSelectOptions::Ungrouped(options),
        )
        .category(SessionConfigOptionCategory::Model)
        .description("本次会话使用的模型"),
    )
}

/// 客户端支持 config_options 时的当前模型配置项更新。
fn config_options_update(
    proj: &SessionProjection,
    caps: &acp::ClientCapabilities,
    config: Option<&ConfigGetResult>,
) -> Option<SessionUpdate> {
    caps.session
        .as_ref()
        .and_then(|session| session.config_options.as_ref())?;
    config
        .and_then(|c| model_option(&c.models, &proj.model))
        .map(|option| SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![option])))
}

// ---------- 消息块 ----------

fn chunk(variant: fn(ContentChunk) -> SessionUpdate, text: impl Into<String>) -> SessionUpdate {
    variant(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text.into(),
    ))))
}

fn agent_chunk(text: impl Into<String>) -> SessionUpdate {
    chunk(SessionUpdate::AgentMessageChunk, text)
}

fn user_chunk(text: impl Into<String>) -> SessionUpdate {
    chunk(SessionUpdate::UserMessageChunk, text)
}

fn thought_chunk(text: impl Into<String>) -> SessionUpdate {
    chunk(SessionUpdate::AgentThoughtChunk, text)
}

/// 客户端声明 `session.notices` 时发 `notice`；否则降级为一条加粗标题的 agent 消息。
fn notice(
    caps: &acp::ClientCapabilities,
    severity: NoticeSeverity,
    title: impl Into<String>,
    description: impl Into<String>,
) -> SessionUpdate {
    let title = title.into();
    let description = description.into();
    if caps
        .session
        .as_ref()
        .is_some_and(|session| session.notices.is_some())
    {
        SessionUpdate::Notice(Notice::new(severity, title).description(description))
    } else {
        agent_chunk(format!("**{title}**\n\n{description}"))
    }
}

// ---------- 工具调用 ----------

/// 工具参数里按优先级提取的「一句话摘要」键。
const TITLE_ARG_KEYS: &[&str] = &[
    "command",
    "path",
    "file_path",
    "task",
    "objective",
    "query",
    "pattern",
    "prompt",
    "url",
];
/// 映射成 ACP locations 的参数键。
const LOCATION_ARG_KEYS: &[&str] = &[
    "path",
    "file_path",
    "file",
    "old_path",
    "new_path",
    "src",
    "dst",
];

fn args_json(arguments: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(arguments).ok()
}

/// 工具卡片标题：`bash: cargo test`；没有可摘要参数时退化为工具名。
fn tool_title(name: &str, arguments: &str) -> String {
    let Some(args) = args_json(arguments) else {
        return name.to_owned();
    };
    let summary = TITLE_ARG_KEYS
        .iter()
        .find_map(|key| args.get(*key).and_then(|v| v.as_str()))
        .map(|v| {
            let mut v = v.trim().replace('\n', "⏎");
            if v.chars().count() > 80 {
                v = v.chars().take(80).collect::<String>() + "…";
            }
            v
        })
        .filter(|v| !v.is_empty());
    match summary {
        Some(summary) => format!("{name}: {summary}"),
        None => name.to_owned(),
    }
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" | "read_file" => ToolKind::Read,
        "edit" | "write" | "write_file" | "multi_edit" | "patch" => ToolKind::Edit,
        "delete" | "remove" | "delete_file" => ToolKind::Delete,
        "move" | "rename" | "move_file" => ToolKind::Move,
        "grep" | "glob" | "find" | "search" | "search_code" | "list" => ToolKind::Search,
        "bash" | "shell" | "ptc" | "execute" => ToolKind::Execute,
        "web_fetch" | "fetch" | "web_search" => ToolKind::Fetch,
        "think" | "reason" => ToolKind::Think,
        "plan" | "exit_plan" | "update_plan" => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

fn tool_locations(arguments: &str) -> Vec<ToolCallLocation> {
    args_json(arguments)
        .map(|args| {
            LOCATION_ARG_KEYS
                .iter()
                .filter_map(|key| args.get(*key).and_then(|v| v.as_str()))
                .map(|path| ToolCallLocation::new(PathBuf::from(path)))
                .collect()
        })
        .unwrap_or_default()
}

/// 一次工具调用的 ACP 起始卡片。
pub(crate) fn tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall::new(id.to_owned(), tool_title(name, arguments))
        .kind(tool_kind(name))
        .status(ToolCallStatus::InProgress)
        .locations(tool_locations(arguments))
        .raw_input(args_json(arguments))
}

/// 用户手动 `!command` 的卡片：moc 以 `BashStarted` + `ToolFinished` 成对推送。
fn bash_tool_call(id: &str, command: &moc_protocol::methods::BashCommand) -> ToolCall {
    ToolCall::new(id.to_owned(), format!("! {}", command.command))
        .kind(ToolKind::Execute)
        .status(ToolCallStatus::InProgress)
}

fn tool_status(status: ToolStatus) -> ToolCallStatus {
    match status {
        ToolStatus::Completed => ToolCallStatus::Completed,
        // 拒绝与取消对编辑器而言都是「这次调用没跑成」。
        ToolStatus::Error | ToolStatus::Denied | ToolStatus::Cancelled => ToolCallStatus::Failed,
    }
}

/// `ToolResult` + 结构化 diff → `tool_call.content`。
fn result_content(result: &ToolResult, diff: Option<&ToolDiff>) -> Vec<ToolCallContent> {
    let mut content = Vec::new();
    if !result.content.is_empty() {
        content.push(ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(result.content.clone()),
        ))));
    }
    content.extend(result.images.iter().map(|image| {
        ToolCallContent::Content(Content::new(match image {
            ImageSource::Base64 { media_type, data } => {
                ContentBlock::Image(ImageContent::new(data.clone(), media_type.clone()))
            }
            ImageSource::Url(url) => {
                ContentBlock::ResourceLink(acp::ResourceLink::new(url.clone(), url.clone()))
            }
        }))
    }));
    if let Some(diff) = diff {
        content.extend(diff_content(diff));
    }
    content
}

/// `ToolDiff`（hunk 行序列）→ 每文件一条 ACP `diff` 内容（整文件新旧文本重建）。
pub(crate) fn diff_content(diff: &ToolDiff) -> Vec<ToolCallContent> {
    diff.files
        .iter()
        .map(|file| {
            let (mut old, mut new) = (String::new(), String::new());
            for line in file.hunks.iter().flat_map(|h| h.lines.iter()) {
                match line.kind {
                    DiffLineKind::Equal => {
                        old.push_str(&line.text);
                        old.push('\n');
                        new.push_str(&line.text);
                        new.push('\n');
                    }
                    DiffLineKind::Delete => {
                        old.push_str(&line.text);
                        old.push('\n');
                    }
                    DiffLineKind::Insert => {
                        new.push_str(&line.text);
                        new.push('\n');
                    }
                }
            }
            let diff = Diff::new(PathBuf::from(&file.path), new);
            ToolCallContent::Diff(if old.is_empty() {
                diff
            } else {
                diff.old_text(old)
            })
        })
        .collect()
}

fn raw_output(result: &ToolResult) -> serde_json::Value {
    serde_json::json!({
        "content": result.content,
        "is_error": result.is_error,
    })
}

// ---------- 计划 / 待办 ----------

pub(crate) fn todo_plan(phases: &[TodoPhase]) -> Plan {
    let multi_phase = phases.len() > 1;
    Plan::new(
        phases
            .iter()
            .flat_map(|phase| {
                phase.items.iter().map(move |item| {
                    let mut content = if multi_phase {
                        format!("{}：{}", phase.phase, item.task)
                    } else {
                        item.task.clone()
                    };
                    let status = match item.status {
                        TodoStatus::Pending | TodoStatus::Blocked => PlanEntryStatus::Pending,
                        TodoStatus::InProgress => PlanEntryStatus::InProgress,
                        TodoStatus::Completed | TodoStatus::Abandoned => PlanEntryStatus::Completed,
                    };
                    if item.status == TodoStatus::Blocked
                        && let Some(reason) = &item.reason
                    {
                        content = format!("{content}（阻塞：{reason}）");
                    }
                    PlanEntry::new(content, PlanEntryPriority::Medium, status)
                })
            })
            .collect(),
    )
}

/// `PlanUpdated` 的终端状态提示；批注/修订中间态不回显（噪音）。
fn plan_status_update(plan: &PlanInfo) -> Option<SessionUpdate> {
    let text = match plan.status {
        PlanStatus::Approved => format!("计划「{}」已批准", plan.title),
        PlanStatus::Rejected => format!("计划「{}」已拒绝", plan.title),
        PlanStatus::Superseded => format!("计划「{}」已被新版本取代", plan.title),
        _ => return None,
    };
    Some(agent_chunk(text))
}

/// `PlanSubmitted`：整份计划正文以 agent 消息回显。
pub(crate) fn plan_submitted_update(plan: &PlanInfo) -> SessionUpdate {
    agent_chunk(if plan.title.is_empty() {
        plan.markdown.clone()
    } else {
        format!("## {}\n\n{}", plan.title, plan.markdown)
    })
}

// ---------- usage ----------

fn usage_update(
    usage: &Usage,
    proj: &SessionProjection,
    config: Option<&ConfigGetResult>,
) -> Option<SessionUpdate> {
    // 模型按精确 ID 匹配；`session.model` 总是规范 ID，selector 在 set_model 时已被 core 解析。
    let model = config?.models.iter().find(|m| m.id == proj.model)?;
    let size = model.context_window?;
    let used = usage
        .total_tokens
        .unwrap_or(usage.input_tokens + usage.output_tokens);
    let mut update = UsageUpdate::new(used, size);
    if let Some(pricing) = &model.pricing {
        // 与 moc 的计费口径一致：`input - cached` 按 input 单价、`cached` 按 cache_read、
        // cache_write 不单独计价（已含在 input 内）。
        let cached = usage.cached_input_tokens.unwrap_or(0) as f64;
        let uncached = usage
            .input_tokens
            .saturating_sub(usage.cached_input_tokens.unwrap_or(0)) as f64;
        let amount = uncached * pricing.input / 1e6
            + cached * pricing.cache_read.unwrap_or(pricing.input) / 1e6
            + usage.output_tokens as f64 * pricing.output / 1e6;
        if amount > 0.0 {
            update = update.cost(Cost::new(amount, "USD"));
        }
    }
    Some(SessionUpdate::UsageUpdate(update))
}

fn mismatch_text(fields: &[UpstreamFieldMismatch]) -> String {
    fields
        .iter()
        .map(|f| {
            format!(
                "- {:?}：请求 {}，上游实际 {}",
                f.field, f.requested, f.reported
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------- `session/update` → ACP ----------

/// 把一条 moc 会话更新翻译成 0..n 条 ACP 会话更新；顺手维护会话投影。
///
/// 返回顺序即发送顺序。不产生 ACP 对应物的事件（锁、工作区、workflow、任务等）
/// 静默丢弃——ACP 客户端没有这些概念。
pub(crate) fn session_update(
    update: &MocUpdate,
    proj: &mut SessionProjection,
    caps: &acp::ClientCapabilities,
    config: Option<&ConfigGetResult>,
) -> Vec<SessionUpdate> {
    match update {
        MocUpdate::AssistantDelta { delta, .. } => match delta {
            AssistantDelta::Text { delta } => vec![agent_chunk(delta.clone())],
            AssistantDelta::Reasoning { delta } => vec![thought_chunk(delta.clone())],
            AssistantDelta::Usage { usage } => {
                usage_update(usage, proj, config).into_iter().collect()
            }
        },
        MocUpdate::ToolStarted {
            tool_call_id,
            name,
            arguments,
            ..
        } => vec![SessionUpdate::ToolCall(tool_call(
            tool_call_id,
            name,
            arguments,
        ))],
        MocUpdate::BashStarted {
            tool_call_id,
            command,
        } => vec![SessionUpdate::ToolCall(bash_tool_call(
            tool_call_id,
            command,
        ))],
        MocUpdate::ToolFinished {
            tool_call_id,
            status,
            result,
            diff,
            ..
        } => vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            tool_call_id.clone(),
            ToolCallUpdateFields::new()
                .status(tool_status(*status))
                .content(result_content(result, diff.as_ref()))
                .raw_output(raw_output(result)),
        ))],
        MocUpdate::TodoChanged { phases } => vec![SessionUpdate::Plan(todo_plan(phases))],
        MocUpdate::TitleChanged { title } => vec![SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title(title.clone()),
        )],
        MocUpdate::AutonomyChanged { autonomy } => {
            proj.autonomy = *autonomy;
            vec![SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                proj.mode_id(),
            ))]
        }
        MocUpdate::PlanModeChanged { active } => {
            proj.plan_active = *active;
            vec![SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                proj.mode_id(),
            ))]
        }
        MocUpdate::ModelChanged { model } => {
            proj.model = model.clone();
            config_options_update(proj, caps, config)
                .into_iter()
                .collect()
        }
        MocUpdate::PromptStarted { model, .. } => {
            proj.model = model.clone();
            vec![]
        }
        MocUpdate::PlanSubmitted { plan } => vec![plan_submitted_update(plan)],
        MocUpdate::PlanUpdated { plan } => plan_status_update(plan).into_iter().collect(),
        MocUpdate::GuardNotice { message, .. } => {
            vec![notice(
                caps,
                NoticeSeverity::Warning,
                "循环守卫",
                message.clone(),
            )]
        }
        MocUpdate::PluginNotice {
            plugin,
            code,
            message,
        } => vec![notice(
            caps,
            NoticeSeverity::Error,
            format!("插件 {plugin} 已停用"),
            format!("{code}：{message}"),
        )],
        MocUpdate::UpstreamMismatch { fields, .. } => vec![notice(
            caps,
            NoticeSeverity::Warning,
            "上游与请求不一致",
            mismatch_text(fields),
        )],
        MocUpdate::ContextCompactionStarted => {
            vec![thought_chunk("正在压缩上下文…")]
        }
        MocUpdate::ContextCompacted {
            tokens_before,
            tokens_after,
            ..
        } => vec![thought_chunk(format!(
            "上下文压缩完成：{tokens_before} → {tokens_after} tokens"
        ))],
        MocUpdate::ContextCompactionFailed { message } => vec![notice(
            caps,
            NoticeSeverity::Warning,
            "上下文压缩失败",
            message.clone(),
        )],
        MocUpdate::AgentMessage { meta, body } => {
            vec![user_chunk(format!("[{}] {}", meta.from_title, body))]
        }
        MocUpdate::GoalRoundMessage { meta, body } => {
            vec![user_chunk(format!("[goal round {}] {}", meta.round, body))]
        }
        MocUpdate::SteeringApplied { text, .. } if !text.is_empty() => {
            vec![user_chunk(text.clone())]
        }
        MocUpdate::Error { message, .. } => {
            vec![notice(
                caps,
                NoticeSeverity::Error,
                "提示错误",
                message.clone(),
            )]
        }
        // 其余事件（权限镜像、批、任务、工作区、workflow、锁等）无 ACP 对应物。
        _ => vec![],
    }
}

// ---------- `session/prompt` 输入 ----------

/// ACP content block → moc `InputPart`。
///
/// `ResourceLink` / 嵌入文本资源摊平成文本——编辑器用它贴文件引用，moc 侧没有
/// 对应的一等资源概念。blob 资源与音频报 invalid_params（moc 只收文本与图片）。
pub(crate) fn input_part(block: &ContentBlock) -> Result<InputPart, acp::Error> {
    match block {
        ContentBlock::Text(text) => Ok(InputPart::Text {
            text: text.text.clone(),
        }),
        ContentBlock::Image(image) => Ok(InputPart::Image {
            source: ImageSource::Base64 {
                media_type: image.mime_type.clone(),
                data: image.data.clone(),
            },
            detail: None,
        }),
        ContentBlock::ResourceLink(link) => Ok(InputPart::Text {
            text: match &link.description {
                Some(description) => {
                    format!("资源：{}（{}）\n{}", link.name, link.uri, description)
                }
                None => format!("资源：{}（{}）", link.name, link.uri),
            },
        }),
        ContentBlock::Resource(resource) => match &resource.resource {
            EmbeddedResourceResource::TextResourceContents(text) => Ok(InputPart::Text {
                text: format!(
                    "资源：{}（{}）\n\n{}",
                    text.uri,
                    text.mime_type.as_deref().unwrap_or("text"),
                    text.text
                ),
            }),
            EmbeddedResourceResource::BlobResourceContents(_) => {
                Err(acp::Error::invalid_params().data("不支持二进制嵌入资源；请以文本或图片发送"))
            }
            _ => Err(acp::Error::invalid_params().data("不支持该类型的嵌入资源")),
        },
        _ => Err(acp::Error::invalid_params().data("不支持该类型的内容块")),
    }
}

// ---------- `session/prompt` 结果 ----------

pub(crate) fn stop_reason(reason: &FinishReason) -> StopReason {
    match reason {
        FinishReason::Stop
        | FinishReason::StopSequence
        | FinishReason::ToolCalls
        | FinishReason::PauseTurn
        | FinishReason::LoopDetected => StopReason::EndTurn,
        FinishReason::Length => StopReason::MaxTokens,
        FinishReason::ContentFilter | FinishReason::Refusal => StopReason::Refusal,
        FinishReason::Cancelled | FinishReason::Interrupted => StopReason::Cancelled,
        FinishReason::Other(_) => StopReason::EndTurn,
    }
}

// ---------- transcript 重放（session/load） ----------

/// 一条 transcript 条目 → 0..n 条 ACP 更新。
///
/// 与实时路径共用同一套工具卡片构造函数；工具条目是「ToolCall 部分 + 后续
/// ToolResult 条目」配对，所以这里先按 content 发 ToolCall、再按 `entry.tool`
/// 或 ToolResult 部分发收尾更新。
pub(crate) fn replay_entry(entry: &TranscriptEntry) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    let message = &entry.message;
    // 代理间消息 / goal 续跑：前缀来源标记，与实时推送的展示保持一致。
    let prefix = if let Some(meta) = &entry.agent {
        Some(format!("[{}] ", meta.from_title))
    } else {
        entry
            .goal_round
            .as_ref()
            .map(|meta| format!("[goal round {}] ", meta.round))
    };
    for part in &message.content {
        match part {
            ContentPart::ToolCall(call) => {
                updates.push(SessionUpdate::ToolCall(tool_call(
                    &call.id,
                    &call.name,
                    &call.arguments,
                )));
            }
            ContentPart::ToolResult(result) => {
                updates.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    result.tool_call_id.clone(),
                    ToolCallUpdateFields::new()
                        .status(if result.is_error {
                            ToolCallStatus::Failed
                        } else {
                            ToolCallStatus::Completed
                        })
                        .content(result_content(result, None))
                        .raw_output(raw_output(result)),
                )));
            }
            ContentPart::Text(text) => match message.role {
                Role::User => updates.push(user_chunk(format!(
                    "{}{}",
                    prefix.as_deref().unwrap_or(""),
                    text
                ))),
                Role::Assistant => updates.push(agent_chunk(text.clone())),
                _ => {}
            },
            ContentPart::Reasoning(ReasoningBlock::Text { text, .. })
                if message.role == Role::Assistant =>
            {
                updates.push(thought_chunk(text.clone()));
            }
            _ => {}
        }
    }
    if let Some(exec) = &entry.tool {
        updates.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            exec.tool_call_id.clone(),
            ToolCallUpdateFields::new()
                .status(tool_status(exec.status))
                .content(exec.diff.as_ref().map(diff_content).unwrap_or_default()),
        )));
    }
    updates
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::methods::{AskOption, AskQuestion, PermissionOption};
    use serde_json::json;

    fn projection() -> SessionProjection {
        SessionProjection {
            model: "gpt-5".into(),
            autonomy: SessionAutonomy::Default,
            plan_active: false,
        }
    }

    #[test]
    fn mode_from_projection() {
        let mut proj = projection();
        assert_eq!(proj.mode_id(), MODE_DEFAULT);
        proj.autonomy = SessionAutonomy::Unattended;
        assert_eq!(proj.mode_id(), MODE_UNATTENDED);
        proj.plan_active = true;
        assert_eq!(proj.mode_id(), MODE_PLAN);
        let state = mode_state(&proj);
        assert_eq!(state.current_mode_id.0.as_ref(), MODE_PLAN);
        assert_eq!(state.available_modes.len(), 4);
    }

    #[test]
    fn tool_title_summarizes_args() {
        assert_eq!(
            tool_title("bash", r#"{"command":"cargo test -p moc"}"#),
            "bash: cargo test -p moc"
        );
        assert_eq!(
            tool_title("read", r#"{"path":"src/lib.rs"}"#),
            "read: src/lib.rs"
        );
        assert_eq!(tool_title("todo", r#"{"items":[]}"#), "todo");
        assert_eq!(tool_title("bash", "not json"), "bash");
    }

    #[test]
    fn tool_kind_maps_names() {
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("edit"), ToolKind::Edit);
        assert_eq!(tool_kind("grep"), ToolKind::Search);
        assert_eq!(tool_kind("spawn_agent"), ToolKind::Other);
    }

    #[test]
    fn diff_becomes_old_new_text() {
        let diff: ToolDiff = serde_json::from_value(json!({
            "files": [{
                "path": "src/a.rs",
                "hunks": [{
                    "old_start": 1, "new_start": 1,
                    "lines": [
                        {"kind": "equal", "old_no": 1, "new_no": 1, "text": "a"},
                        {"kind": "delete", "old_no": 2, "new_no": null, "text": "b"},
                        {"kind": "insert", "old_no": null, "new_no": 2, "text": "c"},
                    ]
                }]
            }],
            "truncated": false
        }))
        .unwrap();
        let result = ToolResult {
            tool_call_id: "t1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        };
        let content = result_content(&result, Some(&diff));
        assert_eq!(content.len(), 2);
        match &content[1] {
            ToolCallContent::Diff(d) => {
                assert_eq!(d.path, PathBuf::from("src/a.rs"));
                assert_eq!(d.old_text.as_deref(), Some("a\nb\n"));
                assert_eq!(d.new_text, "a\nc\n");
            }
            other => panic!("应是 diff 内容：{other:?}"),
        }
    }

    #[test]
    fn finish_reason_maps_to_stop_reason() {
        assert_eq!(stop_reason(&FinishReason::Stop), StopReason::EndTurn);
        assert_eq!(stop_reason(&FinishReason::Length), StopReason::MaxTokens);
        assert_eq!(stop_reason(&FinishReason::Cancelled), StopReason::Cancelled);
        assert_eq!(stop_reason(&FinishReason::Refusal), StopReason::Refusal);
    }

    #[test]
    fn content_block_to_input_part() {
        let text = ContentBlock::Text(TextContent::new("你好"));
        match input_part(&text).unwrap() {
            InputPart::Text { text } => assert_eq!(text, "你好"),
            other => panic!("应为文本输入：{other:?}"),
        }
        let image = ContentBlock::Image(ImageContent::new("AAAA", "image/png"));
        match input_part(&image).unwrap() {
            InputPart::Image { source, .. } => assert!(matches!(
                source,
                ImageSource::Base64 { ref media_type, .. } if media_type == "image/png"
            )),
            other => panic!("应为图片输入：{other:?}"),
        }
    }

    #[test]
    fn autonomy_update_emits_mode_update() {
        let mut proj = projection();
        let caps = acp::ClientCapabilities::default();
        let updates = session_update(
            &MocUpdate::AutonomyChanged {
                autonomy: SessionAutonomy::Unattended,
            },
            &mut proj,
            &caps,
            None,
        );
        assert_eq!(proj.mode_id(), MODE_UNATTENDED);
        match &updates[0] {
            SessionUpdate::CurrentModeUpdate(u) => {
                assert_eq!(u.current_mode_id.0.as_ref(), MODE_UNATTENDED)
            }
            other => panic!("应为 CurrentModeUpdate：{other:?}"),
        }
    }

    #[test]
    fn todos_become_plan() {
        let phases: Vec<TodoPhase> = serde_json::from_value(json!([{
            "phase": "实现", "items": [
                {"status": "completed", "task": "建骨架"},
                {"status": "in_progress", "task": "写适配器"},
                {"status": "blocked", "task": "等评审", "reason": "缺确认"}
            ]
        }]))
        .unwrap();
        let plan = todo_plan(&phases);
        assert_eq!(plan.entries.len(), 3);
        assert_eq!(plan.entries[1].status, PlanEntryStatus::InProgress);
        assert!(plan.entries[2].content.contains("阻塞"));
    }

    // `AskQuestion`/`PermissionOption` 在 reverse.rs 中翻译；这里只为保持 import 有效性。
    #[allow(dead_code)]
    fn touch(_: Option<AskQuestion>, _: Option<AskOption>, _: Option<PermissionOption>) {}
}

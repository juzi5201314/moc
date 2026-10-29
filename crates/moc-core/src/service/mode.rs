//! 会话引擎与执行模式的判定、plan 模式切换、计划批准与只读工作区约束。

use moc_protocol::methods::{
    PlanAction, PlanAnnotation, PlanInfo, PlanStatus, PromptMode, SessionAutonomy, SessionEngine,
    SessionPlanApproveParams, SessionPlanApproveResult, SessionPlanDiffParams,
    SessionPlanDiffResult, SessionPlanModeParams, SessionPlanReviseParams, SessionPlanReviseResult,
    SessionUpdate, ToolDiff, ToolFileDiff,
};
use moc_protocol::{RequestContext, RpcError, RpcErrorCode, build_hunks};

use super::{CoreInner, protocol_to_rpc, to_rpc};
use crate::compact;
use crate::error::CoreError;
use crate::session::entries::{EntryPayload, SessionOrigin};

/// 执行模式：与引擎正交，只收紧工具许可。
///
/// 模式不改变原生工具表——表只随引擎与配置变化（保障请求前缀缓存稳定）；模式
/// 作用在 [`mode_allows`] 的执行许可上：被拒的工具调用以 `E_MODE` 结果回给模型，
/// 配合模式提醒引导它改用只读手段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 默认：不收紧，许可 = 引擎面。
    Default,
    /// `/ask` 声明的单次提示只读模式，提示结束即失效。
    Ask,
    /// plan 模式：目标收敛到产出计划，写工具被拒，`write_plan`/`exit_plan` 放行。
    Plan,
}

impl Mode {
    /// 插件上下文 `session.mode` 与模式提醒使用的稳定名字，与插件 `modes` 声明同词表。
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Ask => "ask",
            Self::Plan => "plan",
        }
    }

    /// 只读模式：工作区改动一律按违规上报。
    pub fn read_only(self) -> bool {
        !matches!(self, Self::Default)
    }

    /// 本次提示的有效模式：plan 模式持久生效，优先于提示级 ask——ask 不能削弱
    /// 会话级的计划目标。
    pub fn effective(plan_active: bool, prompt_mode: Option<PromptMode>) -> Self {
        if plan_active {
            Self::Plan
        } else {
            prompt_mode.map(Self::from).unwrap_or(Self::Default)
        }
    }
}

impl From<PromptMode> for Mode {
    fn from(mode: PromptMode) -> Self {
        match mode {
            PromptMode::Ask => Self::Ask,
        }
    }
}

/// `session/plan_mode`：进入或退出 plan 模式。
///
/// 进入时按 `models.roles.plan` 覆盖会话模型并建立工作区基线，退出时恢复进入前
/// 的模型；原生工具表不受影响，plan 工具由模式放行。
pub async fn plan_mode(
    inner: &CoreInner,
    ctx: RequestContext,
    params: SessionPlanModeParams,
) -> Result<moc_protocol::methods::SessionInfo, RpcError> {
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(to_rpc)?;
    let (cwd, was_active, model_before) = {
        let state = record.state.lock().await;
        (
            std::path::PathBuf::from(state.session.cwd.clone()),
            state.plan_active,
            state.model.clone(),
        )
    };
    let config = inner.project_config(&cwd).map_err(to_rpc)?;
    // 进入前先解析计划模型，避免解析失败时叠加态已切换而模型没切换。
    let plan_model = if params.active && !was_active {
        config
            .resolve_plan_mode_model()
            .map_err(to_rpc)?
            .map(|model| model.selector)
    } else {
        None
    };
    let mut info = inner
        .sessions
        .set_plan_active(&params.session_id, params.active)
        .await
        .map_err(to_rpc)?;
    let switched = sync_plan_model(inner, &params.session_id, params.active, plan_model)
        .await
        .map_err(to_rpc)?;
    if switched.is_some() {
        info = record.state.lock().await.session_info();
    }
    info.lock = None;
    info.permission = inner.effective_permission(info.permission);
    if params.active && !was_active && config.workspace.watch {
        let snapshot = super::snapshot_workspace(cwd, config.workspace.max_scan_entries)
            .await
            .map_err(to_rpc)?;
        *inner
            .runtime(&params.session_id)
            .await
            .workspace
            .lock()
            .await = Some(snapshot);
    }
    emit_plan_change(&ctx, &params.session_id, params.active, switched.clone()).await?;
    let state = record.state.lock().await.clone();
    super::emit_session_config_changed(
        inner,
        &state,
        "plan_mode",
        if was_active { "on" } else { "off" },
        if params.active { "on" } else { "off" },
    )
    .await;
    if let Some(model) = &switched {
        super::emit_session_config_changed(inner, &state, "model", &model_before, model).await;
    }
    Ok(info)
}

/// plan 模式变化时同步会话模型：进入时切到 `plan` 角色模型，退出时恢复进入前的模型。
///
/// `plan_model` 是进入前解析好的目标模型选择器；`plan = "none"` 表示不切换，此时传
/// `None`。`active` 是切换后的模式。返回切换后的选择器，模型没变时返回 `None`，
/// 调用方据此决定是否推送 [`SessionUpdate::ModelChanged`]。
pub async fn sync_plan_model(
    inner: &CoreInner,
    session_id: &str,
    active: bool,
    plan_model: Option<String>,
) -> Result<Option<String>, CoreError> {
    let record = inner.sessions.get(session_id).await?;
    let target = {
        let state = record.state.lock().await;
        let target = if active {
            plan_model
        } else {
            Some(state.exit_plan_model().to_owned())
        };
        target.filter(|target| *target != state.model)
    };
    let Some(target) = target else {
        return Ok(None);
    };
    inner.sessions.set_model(session_id, target.clone()).await?;
    Ok(Some(target))
}

/// 推送引擎变化；模型随之变化时，先推引擎再推模型。
pub async fn emit_engine_change(
    ctx: &RequestContext,
    session_id: &str,
    engine: SessionEngine,
    model: Option<String>,
) -> Result<(), RpcError> {
    ctx.updates()
        .emit(
            session_id.to_owned(),
            SessionUpdate::EngineChanged { engine },
        )
        .await
        .map_err(protocol_to_rpc)?;
    if let Some(model) = model {
        ctx.updates()
            .emit(session_id.to_owned(), SessionUpdate::ModelChanged { model })
            .await
            .map_err(protocol_to_rpc)?;
    }
    Ok(())
}

/// 推送自主程度变化。
pub async fn emit_autonomy_change(
    ctx: &RequestContext,
    session_id: &str,
    autonomy: SessionAutonomy,
) -> Result<(), RpcError> {
    ctx.updates()
        .emit(
            session_id.to_owned(),
            SessionUpdate::AutonomyChanged { autonomy },
        )
        .await
        .map_err(protocol_to_rpc)
}

/// 推送 plan 模式变化；模型随之变化时，先推状态再推模型。
pub async fn emit_plan_change(
    ctx: &RequestContext,
    session_id: &str,
    active: bool,
    model: Option<String>,
) -> Result<(), RpcError> {
    ctx.updates()
        .emit(
            session_id.to_owned(),
            SessionUpdate::PlanModeChanged { active },
        )
        .await
        .map_err(protocol_to_rpc)?;
    if let Some(model) = model {
        ctx.updates()
            .emit(session_id.to_owned(), SessionUpdate::ModelChanged { model })
            .await
            .map_err(protocol_to_rpc)?;
    }
    Ok(())
}

/// 写入计划状态并推送最新投影；前端按 `plan_id` 覆盖本地视图，不再自己推断迁移。
async fn set_plan_status(
    inner: &CoreInner,
    ctx: &RequestContext,
    session_id: &str,
    plan: &PlanInfo,
    status: PlanStatus,
) -> Result<PlanInfo, RpcError> {
    let mut updated = plan.clone();
    updated.status = status;
    inner
        .sessions
        .append(session_id, EntryPayload::plan(&updated, status))
        .await
        .map_err(to_rpc)?;
    ctx.updates()
        .emit(
            session_id.to_owned(),
            SessionUpdate::PlanUpdated {
                plan: updated.clone(),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    Ok(updated)
}

pub async fn approve_plan(
    inner: &CoreInner,
    ctx: RequestContext,
    params: SessionPlanApproveParams,
) -> Result<SessionPlanApproveResult, RpcError> {
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(to_rpc)?;
    let state = record.state.lock().await.clone();
    let plan = state
        .plans
        .iter()
        .find(|plan| plan.plan_id == params.plan_id)
        .cloned()
        .ok_or_else(|| RpcError::of(RpcErrorCode::InvalidParams))?;
    if plan.status != PlanStatus::Submitted {
        return Err(RpcError::new(
            RpcErrorCode::InvalidParams.code(),
            "计划已经处理过",
        ));
    }
    let config = inner
        .project_config(std::path::Path::new(&state.session.cwd))
        .map_err(to_rpc)?;
    let requested_model = params.execution_model.as_deref().unwrap_or(&state.model);
    let execution_model = config.resolve_model(requested_model).map_err(to_rpc)?;

    if params.action == PlanAction::ExecuteCompacted {
        compact::manual(
            inner,
            &ctx,
            &params.session_id,
            Some("Preserve the plan and key decisions"),
            Some(&execution_model.selector),
            Some(plan.markdown.as_str()),
        )
        .await
        .map_err(to_rpc)?;
    }

    if params.action == PlanAction::Reject {
        set_plan_status(inner, &ctx, &params.session_id, &plan, PlanStatus::Rejected).await?;
        let info = record.state.lock().await.session_info();
        return Ok(SessionPlanApproveResult {
            session_id: params.session_id,
            engine: info.engine,
            plan_active: info.plan_active,
            auto_prompt: None,
            plan_markdown: None,
        });
    }

    if params.action == PlanAction::ExecuteNewSession {
        // 新会话引擎由评审界面选择，缺省沿用源会话引擎；新会话不继承 plan 模式。
        let fork_engine = params.execution_engine.unwrap_or(state.engine);
        let fork = inner
            .sessions
            .create(
                &state.session.cwd,
                &execution_model.selector,
                fork_engine,
                state.permission,
                Some(&plan.title),
                SessionOrigin::forked(params.session_id.clone()),
                config.session.compress,
            )
            .await
            .map_err(to_rpc)?;
        let fork_record = inner.sessions.get(&fork.id).await.map_err(to_rpc)?;
        let fork_path = fork_record
            .paths
            .plans()
            .join(format!("{}.md", plan.plan_id));
        tokio::fs::create_dir_all(fork_record.paths.plans())
            .await
            .map_err(CoreError::Io)
            .map_err(to_rpc)?;
        tokio::fs::write(&fork_path, &plan.markdown)
            .await
            .map_err(CoreError::Io)
            .map_err(to_rpc)?;
        inner
            .sessions
            .append(&fork.id, EntryPayload::plan(&plan, PlanStatus::Approved))
            .await
            .map_err(to_rpc)?;
        if let Some(host) =
            crate::service::plugin_events::host_for(inner, std::path::Path::new(&fork.cwd)).await
        {
            let context = crate::service::plugin_events::context_from_info(&fork);
            for message in crate::service::plugin_events::session_start(
                &host,
                &context,
                &fork,
                "user",
                Some(&params.session_id),
            )
            .await
            {
                let role = if message.role == "system" {
                    moc_llm::Role::System
                } else {
                    moc_llm::Role::User
                };
                inner
                    .sessions
                    .append_message(
                        &fork.id,
                        moc_llm::Message {
                            role,
                            content: vec![moc_llm::ContentPart::Text(message.text)],
                        },
                        None,
                        None,
                    )
                    .await
                    .map_err(to_rpc)?;
            }
        }
        inner
            .sessions
            .append_message(
                &fork.id,
                moc_llm::Message::user_text(format!(
                    "<active-plan id=\"{}\">\n{}\n</active-plan>",
                    plan.plan_id, plan.markdown
                )),
                None,
                None,
            )
            .await
            .map_err(to_rpc)?;
        set_plan_status(inner, &ctx, &params.session_id, &plan, PlanStatus::Approved).await?;
        return Ok(SessionPlanApproveResult {
            session_id: fork.id,
            engine: fork_engine,
            plan_active: false,
            auto_prompt: Some("Execute the approved plan.".into()),
            plan_markdown: Some(plan.markdown),
        });
    }

    set_plan_status(inner, &ctx, &params.session_id, &plan, PlanStatus::Approved).await?;
    inner
        .sessions
        .append_message(
            &params.session_id,
            moc_llm::Message::user_text(format!(
                "<active-plan id=\"{}\">\n{}\n</active-plan>",
                plan.plan_id, plan.markdown
            )),
            None,
            None,
        )
        .await
        .map_err(to_rpc)?;
    // 批准 = 退出 plan 模式并在当前引擎上继续执行；引擎本身不变。
    inner
        .sessions
        .set_plan_active(&params.session_id, false)
        .await
        .map_err(to_rpc)?;
    let current_model = record.state.lock().await.model.clone();
    let model =
        (current_model != execution_model.selector).then(|| execution_model.selector.clone());
    if model.is_some() {
        inner
            .sessions
            .set_model(&params.session_id, execution_model.selector.clone())
            .await
            .map_err(to_rpc)?;
    }
    emit_plan_change(&ctx, &params.session_id, false, model).await?;
    Ok(SessionPlanApproveResult {
        session_id: params.session_id,
        engine: state.engine,
        plan_active: false,
        auto_prompt: Some("Execute the approved plan.".into()),
        plan_markdown: None,
    })
}

/// 单条批注正文的字节上限。
const MAX_PLAN_ANNOTATION_BODY_BYTES: usize = 16 * 1024;
/// 选中文本的字节上限（只用于界面回显）。
const MAX_PLAN_ANNOTATION_QUOTE_BYTES: usize = 4 * 1024;
/// 整体意见的字节上限。
const MAX_PLAN_NOTE_BYTES: usize = 8 * 1024;
/// 单次提交的批注条数上限。
const MAX_PLAN_ANNOTATIONS: usize = 64;
/// 单次提交的批注总字节上限。
const MAX_PLAN_FEEDBACK_BYTES: usize = 64 * 1024;
/// 修订提示里引用原文的行数上限。
const MAX_PLAN_EXCERPT_LINES: usize = 40;
/// 修订提示里引用原文的字节上限。
const MAX_PLAN_EXCERPT_BYTES: usize = 4 * 1024;

/// 提交一批批注并生成修订提示。
///
/// 这里只做落库与提示生成，不发起提示：前端拿到 `prompt` 后按普通
/// `session/prompt` 发送，修订轮因此复用同一套流式、取消与压缩语义。
pub async fn revise_plan(
    inner: &CoreInner,
    ctx: RequestContext,
    params: SessionPlanReviseParams,
) -> Result<SessionPlanReviseResult, RpcError> {
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(to_rpc)?;
    let state = record.state.lock().await.clone();
    let plan = state
        .plans
        .iter()
        .find(|plan| plan.plan_id == params.plan_id)
        .cloned()
        .ok_or_else(|| RpcError::of(RpcErrorCode::InvalidParams))?;
    if plan.status != PlanStatus::Submitted {
        return Err(RpcError::new(
            RpcErrorCode::InvalidParams.code(),
            "计划不在待审阅状态",
        ));
    }
    let note = params
        .note
        .map(|note| note.trim().to_owned())
        .filter(|note| !note.is_empty());
    validate_plan_feedback(&plan, &params.annotations, note.as_deref())?;
    let prompt = compose_revision_prompt(&plan, &params.annotations, note.as_deref());
    inner
        .sessions
        .append(
            &params.session_id,
            EntryPayload::PlanFeedback {
                plan_id: plan.plan_id.clone(),
                annotations: params.annotations,
                note,
            },
        )
        .await
        .map_err(to_rpc)?;
    let updated = record
        .state
        .lock()
        .await
        .plans
        .iter()
        .find(|candidate| candidate.plan_id == plan.plan_id)
        .cloned();
    if let Some(updated) = updated {
        ctx.updates()
            .emit(
                params.session_id.clone(),
                SessionUpdate::PlanUpdated { plan: updated },
            )
            .await
            .map_err(protocol_to_rpc)?;
    }
    Ok(SessionPlanReviseResult {
        session_id: params.session_id,
        engine: state.engine,
        plan_active: state.plan_active,
        prompt,
    })
}

/// 计划版本与上一版的差异；没有上一版时返回空 diff。
pub async fn plan_diff(
    inner: &CoreInner,
    params: SessionPlanDiffParams,
) -> Result<SessionPlanDiffResult, RpcError> {
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let state = record.state.lock().await;
    let plan = state
        .plans
        .iter()
        .find(|plan| plan.plan_id == params.plan_id)
        .ok_or_else(|| RpcError::of(RpcErrorCode::InvalidParams))?;
    let previous = plan.previous_plan_id.as_ref().and_then(|previous_id| {
        state
            .plans
            .iter()
            .find(|candidate| candidate.plan_id == *previous_id)
    });
    let Some(previous) = previous else {
        return Ok(SessionPlanDiffResult {
            plan_id: plan.plan_id.clone(),
            previous_plan_id: None,
            diff: ToolDiff {
                files: Vec::new(),
                truncated: false,
            },
        });
    };
    let old_lines: Vec<&str> = previous.markdown.lines().collect();
    let new_lines: Vec<&str> = plan.markdown.lines().collect();
    let (hunks, truncated) = build_hunks(&old_lines, &new_lines);
    Ok(SessionPlanDiffResult {
        plan_id: plan.plan_id.clone(),
        previous_plan_id: plan.previous_plan_id.clone(),
        diff: ToolDiff {
            files: vec![ToolFileDiff {
                path: plan.path.clone(),
                hunks,
            }],
            truncated,
        },
    })
}

/// 校验一批批注；错误一律报 `-32602`，由前端回显并保留草稿。
fn validate_plan_feedback(
    plan: &PlanInfo,
    annotations: &[PlanAnnotation],
    note: Option<&str>,
) -> Result<(), RpcError> {
    let invalid = |message: &str| RpcError::new(RpcErrorCode::InvalidParams.code(), message);
    if annotations.is_empty() && note.is_none() {
        return Err(invalid("批注与整体意见不能同时为空"));
    }
    if annotations.len() > MAX_PLAN_ANNOTATIONS {
        return Err(invalid("批注条数超过上限"));
    }
    if note.is_some_and(|note| note.len() > MAX_PLAN_NOTE_BYTES) {
        return Err(invalid("整体意见超过上限"));
    }
    let line_count = plan.markdown.lines().count();
    let mut total = note.map_or(0, str::len);
    for annotation in annotations {
        if annotation.id.trim().is_empty() || annotation.id.len() > 64 {
            return Err(invalid("批注 id 无效"));
        }
        let start = annotation.start_line as usize;
        let end = annotation.end_line as usize;
        if start == 0 || start > end || end > line_count {
            return Err(invalid("批注行号超出计划范围"));
        }
        if annotation.body.trim().is_empty() {
            return Err(invalid("批注正文不能为空"));
        }
        if annotation.body.len() > MAX_PLAN_ANNOTATION_BODY_BYTES {
            return Err(invalid("批注正文超过上限"));
        }
        if annotation.quote.len() > MAX_PLAN_ANNOTATION_QUOTE_BYTES {
            return Err(invalid("批注引用超过上限"));
        }
        total = total
            .saturating_add(annotation.body.len())
            .saturating_add(annotation.quote.len());
    }
    if total > MAX_PLAN_FEEDBACK_BYTES {
        return Err(invalid("批注总量超过上限"));
    }
    Ok(())
}

/// 生成发给模型的修订提示。
///
/// 原文取自计划 markdown（而不是前端回显的选中文本），这样行区间与内容永远
/// 一致；`body` 原样保留，`suggestion` 围栏由模型按 GitHub 语义理解。
pub fn compose_revision_prompt(
    plan: &PlanInfo,
    annotations: &[PlanAnnotation],
    note: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(&format!(
        "I reviewed plan {} (v{}: {}) and left the notes below. Revise the plan accordingly, change only what the notes require, and keep the rest intact. A fenced suggestion block inside a note is the exact replacement for the quoted lines; keep its content as written. Save the complete revised plan with write_plan, then call exit_plan to submit it for review again. The plan text is also at {}.\n",
        plan.plan_id, plan.version, plan.title, plan.path
    ));
    let lines: Vec<&str> = plan.markdown.lines().collect();
    for (index, annotation) in annotations.iter().enumerate() {
        prompt.push_str(&format!(
            "\n### {}. Lines {}-{}\nOriginal:\n",
            index + 1,
            annotation.start_line,
            annotation.end_line
        ));
        let (excerpt, truncated) =
            excerpt_lines(&lines, annotation.start_line, annotation.end_line);
        for line in excerpt {
            prompt.push_str("> ");
            prompt.push_str(line);
            prompt.push('\n');
        }
        if truncated {
            prompt.push_str("> … (excerpt truncated; read the plan file for the full text)\n");
        }
        prompt.push_str("Note:\n");
        prompt.push_str(annotation.body.trim_end());
        prompt.push('\n');
    }
    if let Some(note) = note {
        prompt.push_str("\n### Overall\n");
        prompt.push_str(note.trim_end());
        prompt.push('\n');
    }
    prompt
}

/// 截取批注锚定的原文行；超过行数或字节上限时截断。
fn excerpt_lines<'a>(lines: &[&'a str], start: u32, end: u32) -> (Vec<&'a str>, bool) {
    let start = start as usize;
    let end = (end as usize).min(lines.len());
    if start == 0 || start > end {
        return (Vec::new(), false);
    }
    let mut excerpt = Vec::new();
    let mut bytes = 0usize;
    for line in &lines[start - 1..end] {
        bytes = bytes.saturating_add(line.len() + 1);
        if excerpt.len() >= MAX_PLAN_EXCERPT_LINES || bytes > MAX_PLAN_EXCERPT_BYTES {
            return (excerpt, true);
        }
        excerpt.push(*line);
    }
    (excerpt, false)
}

/// 只在 CLM 引擎开放的内置工具；白名单里的其余工具在 Standard 和 CLM 下都可用。
const CLM_ONLY_TOOLS: &[&str] = &["clm_context"];

/// ask 模式的只读白名单：引擎工具面内再收紧一层。
///
/// `bash` 保留是因为只读问答也需要跑只读命令（沙盒与授权层拦写操作）；
/// `clm_context` 保留是因为 CLM 会话必须持续维护实时上下文，只读提示也不例外。
const ASK_MODE_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "todo",
    "job",
    "bash",
    "ask",
    "skill",
    "clm_context",
    "run_code",
    "web_search",
    "web_fetch",
];

/// plan 模式的白名单：ask 的全部只读能力外加 `write_plan`/`exit_plan`。
const PLAN_MODE_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "todo",
    "job",
    "bash",
    "ask",
    "skill",
    "clm_context",
    "write_plan",
    "exit_plan",
    "run_code",
    "web_search",
    "web_fetch",
];

/// 模式对应的内置工具白名单；`None` 表示不收紧（许可 = 引擎面）。
fn mode_tools(mode: Mode) -> Option<&'static [&'static str]> {
    match mode {
        Mode::Default => None,
        Mode::Ask => Some(ASK_MODE_TOOLS),
        Mode::Plan => Some(PLAN_MODE_TOOLS),
    }
}

/// 引擎的原生工具表：只随引擎变化，模式切换不重写表（前缀缓存因此稳定）。
///
/// `write_plan`/`exit_plan` 常驻引擎面（dsh 式）：进出 plan 不动 schema，工具自己
/// 在非 plan 模式下拒绝执行；`clm_context` 是 CLM 专属工具，其它引擎下由
/// [`mode_allows`] 拒绝。
pub fn allowed_tools(engine: SessionEngine) -> &'static [&'static str] {
    match engine {
        SessionEngine::Standard | SessionEngine::ClmExperimental => &[
            "read",
            "grep",
            "find",
            "todo",
            "job",
            "bash",
            "edit",
            "write",
            "ask",
            "spawn_agent",
            "list_agents",
            "message_agent",
            "stop_agent",
            "wait_agent",
            "yield",
            "skill",
            "clm_context",
            "write_plan",
            "exit_plan",
            "run_code",
            "web_search",
            "web_fetch",
        ],
        SessionEngine::Ptc => &["run_code"],
    }
}

/// 当前引擎 + 模式是否允许执行该工具。
///
/// 两层判定：可达面归引擎（内置工具必须在引擎白名单里；MCP/插件等非内置工具在
/// Standard/CLM 引擎下可达，PTC 下不可达），许可再按模式收紧（只读模式里内置工具
/// 须在模式白名单内，非内置工具按 `Tier::Read` 放行）。
///
/// `clm_context` 只在 CLM 引擎可用：如果 Standard 下也能调用，一次工具调用就能伪造
/// 实时上下文视图。
///
/// 这是 [`crate::tools::Tool::visible_in`] 的默认实现：内置工具与 MCP 工具走这里
/// （MCP 一律按 `Tier::Exec` 处理，只读模式不放行）；插件工具用自己声明的 `modes`
/// 覆盖，不经过风险等级推断。
pub fn mode_allows(
    engine: SessionEngine,
    mode: Mode,
    name: &str,
    tier: crate::tools::Tier,
    origin: crate::tools::ToolOrigin,
) -> bool {
    if CLM_ONLY_TOOLS.contains(&name) && engine != SessionEngine::ClmExperimental {
        return false;
    }
    let reachable = allowed_tools(engine).contains(&name)
        || (!matches!(origin, crate::tools::ToolOrigin::Builtin)
            && matches!(
                engine,
                SessionEngine::Standard | SessionEngine::ClmExperimental
            ));
    if !reachable {
        return false;
    }
    let Some(whitelist) = mode_tools(mode) else {
        return true;
    };
    whitelist.contains(&name)
        || (!matches!(origin, crate::tools::ToolOrigin::Builtin)
            && tier == crate::tools::Tier::Read)
}

pub fn tool_enabled(
    name: &str,
    origin: crate::tools::ToolOrigin,
    ask_supported: bool,
    config: &crate::config::ResolvedConfig,
) -> bool {
    if !matches!(origin, crate::tools::ToolOrigin::Builtin) {
        return true;
    }
    match name {
        "ask" => ask_supported,
        "todo" => config.tools.todo.enabled,
        "job" => config.tools.job.enabled,
        "skill" => config.skills.enabled,
        "web_search" => config.web_search.available(),
        "web_fetch" => config.web_fetch.enabled,
        _ => true,
    }
}

#[cfg(test)]
mod agent_mode_tests {
    use super::{Mode, allowed_tools, mode_allows};
    use crate::tools::{Tier, ToolOrigin};
    use moc_protocol::SessionEngine;

    #[test]
    fn clm_context_is_limited_to_clm_engine() {
        assert!(mode_allows(
            SessionEngine::ClmExperimental,
            Mode::Default,
            "clm_context",
            Tier::Write,
            ToolOrigin::Builtin,
        ));
        for engine in [SessionEngine::Standard, SessionEngine::Ptc] {
            assert!(
                !mode_allows(
                    engine,
                    Mode::Default,
                    "clm_context",
                    Tier::Write,
                    ToolOrigin::Builtin
                ),
                "{engine:?} 不能开放实时上下文写入"
            );
        }
    }

    #[test]
    fn child_agent_controls_only_exist_in_full_engines() {
        let agents = [
            "spawn_agent",
            "list_agents",
            "message_agent",
            "stop_agent",
            "wait_agent",
            "yield",
        ];
        for engine in [SessionEngine::Standard, SessionEngine::ClmExperimental] {
            let allowed = allowed_tools(engine);
            assert!(agents.iter().all(|tool| allowed.contains(tool)));
        }
        assert!(
            agents
                .iter()
                .all(|tool| !allowed_tools(SessionEngine::Ptc).contains(tool))
        );
        // 只读模式在引擎面之上收紧：spawn/stop 等控制工具被剔除。
        for mode in [Mode::Ask, Mode::Plan] {
            assert!(
                agents.iter().all(|tool| !mode_allows(
                    SessionEngine::Standard,
                    mode,
                    tool,
                    Tier::Exec,
                    ToolOrigin::Builtin
                )),
                "{mode:?} 模式不能放行代理控制"
            );
        }
    }

    #[test]
    fn mode_tightens_permission_not_engine_table() {
        // 引擎表常驻写工具与 plan 工具：进出模式不重写表。
        for engine in [SessionEngine::Standard, SessionEngine::ClmExperimental] {
            let allowed = allowed_tools(engine);
            for tool in ["edit", "write", "write_plan", "exit_plan"] {
                assert!(allowed.contains(&tool), "{engine:?} 引擎面缺 {tool}");
            }
        }
        // 模式只拦执行：内置写工具被拒，plan 工具按模式放行。
        assert!(!mode_allows(
            SessionEngine::Standard,
            Mode::Plan,
            "edit",
            Tier::Write,
            ToolOrigin::Builtin,
        ));
        assert!(mode_allows(
            SessionEngine::Standard,
            Mode::Plan,
            "write_plan",
            Tier::Write,
            ToolOrigin::Builtin,
        ));
        assert!(!mode_allows(
            SessionEngine::Standard,
            Mode::Ask,
            "write_plan",
            Tier::Write,
            ToolOrigin::Builtin,
        ));
        // 只读模式里非内置工具按 Tier::Read 放行，MCP（Exec）一律被拒。
        assert!(!mode_allows(
            SessionEngine::Standard,
            Mode::Ask,
            "mcp__s__t",
            Tier::Exec,
            ToolOrigin::Mcp,
        ));
        assert!(mode_allows(
            SessionEngine::Standard,
            Mode::Ask,
            "mcp__s__t",
            Tier::Read,
            ToolOrigin::Mcp,
        ));
        // PTC 引擎下模式不放宽可达面：MCP 依旧只能走 run_code。
        assert!(!mode_allows(
            SessionEngine::Ptc,
            Mode::Default,
            "mcp__s__t",
            Tier::Exec,
            ToolOrigin::Mcp,
        ));
    }
}

#[cfg(test)]
mod plan_feedback_tests {
    use super::*;

    fn plan() -> PlanInfo {
        PlanInfo {
            plan_id: "plan_1".into(),
            title: "方案".into(),
            path: "local://plan_1.md".into(),
            markdown: "# 方案\n\n1. 先看代码\n2. 再做修改\n".into(),
            status: PlanStatus::Submitted,
            version: 2,
            previous_plan_id: Some("plan_0".into()),
            annotations: Vec::new(),
        }
    }

    fn annotation(start: u32, end: u32, body: &str) -> PlanAnnotation {
        PlanAnnotation {
            id: format!("a{start}"),
            start_line: start,
            end_line: end,
            quote: "先看代码".into(),
            body: body.into(),
        }
    }

    #[test]
    fn prompt_quotes_saved_lines_and_keeps_suggestion_fence() {
        let annotations = vec![
            annotation(3, 3, "太笼统\n\n```suggestion\n1. 先看 InspectorPanel\n```"),
            annotation(4, 4, "保留"),
        ];
        let prompt = compose_revision_prompt(&plan(), &annotations, Some("保持简洁"));
        assert!(prompt.contains("(v2: 方案)"), "{prompt}");
        assert!(prompt.contains("### 1. Lines 3-3"), "{prompt}");
        assert!(prompt.contains("> 1. 先看代码"), "{prompt}");
        assert!(prompt.contains("```suggestion"), "{prompt}");
        assert!(prompt.contains("### 2. Lines 4-4"), "{prompt}");
        assert!(prompt.contains("### Overall"), "{prompt}");
        assert!(prompt.contains("保持简洁"), "{prompt}");
    }

    #[test]
    fn long_excerpts_are_truncated() {
        let mut markdown = String::new();
        for index in 0..80 {
            markdown.push_str(&format!("line {index}\n"));
        }
        let mut info = plan();
        info.markdown = markdown;
        let prompt = compose_revision_prompt(&info, &[annotation(1, 80, "整段改")], None);
        assert!(prompt.contains("excerpt truncated"), "{prompt}");
        assert_eq!(
            prompt.matches("> line ").count(),
            MAX_PLAN_EXCERPT_LINES,
            "引用行数必须被截断"
        );
    }

    #[test]
    fn validation_rejects_out_of_range_and_empty_feedback() {
        let info = plan();
        assert!(
            validate_plan_feedback(&info, &[], None).is_err(),
            "批注与整体意见不能同时为空"
        );
        assert!(
            validate_plan_feedback(&info, &[annotation(3, 99, "越界")], None).is_err(),
            "行号不能超出计划"
        );
        assert!(
            validate_plan_feedback(&info, &[annotation(3, 3, "   ")], None).is_err(),
            "正文不能全空白"
        );
        assert!(
            validate_plan_feedback(&info, &[], Some("整体说两句")).is_ok(),
            "只给整体意见也合法"
        );
        assert!(
            validate_plan_feedback(&info, &[annotation(3, 4, "合并两行")], None).is_ok(),
            "跨行批注合法"
        );
    }
}

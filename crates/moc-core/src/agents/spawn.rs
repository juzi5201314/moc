use super::AgentRuntime;
use super::SpawnProfile;
use super::SpawnSpec;
use super::Wakeup;
use super::info_from_session;
use super::invalid;
use super::lifecycle::activate;
use super::parent_snapshot;
use super::preset;
use super::roles;
use super::roster::current_info;
use super::validate_text;
use super::workflow_scope;
use crate::agents::ToolPolicy;
use crate::agents::child_cancel;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::entries::SessionOrigin;
use moc_llm::Message;
use moc_llm::Role;
use moc_protocol::RequestContext;
use moc_protocol::methods::AgentInfo;
use moc_protocol::methods::AgentSpawnResult;
use moc_protocol::methods::AgentStatus;
use moc_protocol::methods::SessionUpdate;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub(super) fn resolve_model(
    config: &crate::ResolvedConfig,
    parent_model: &str,
    selector: &str,
) -> Result<String, CoreError> {
    let selector = if selector == "inherit" {
        parent_model
    } else {
        selector
    };
    Ok(config.resolve_model(selector)?.actual_selector().to_owned())
}

/// 登记即将启动的子代理，并先打开收件箱。
///
/// 顺序不能反：`accepting = true` 必须在注册表条目出现之前生效。投递消息时先看收件箱
/// 再看状态，否则在「已登记、提示还没拿到锁」的窗口里，投递会走唤醒路径，和即将启动的
/// 提示抢同一个会话。
pub(super) async fn prepare_agent(inner: &CoreInner, info: &AgentInfo, cancel: CancellationToken) {
    inner
        .runtime(&info.id)
        .await
        .agent_inbox
        .lock()
        .await
        .accepting = true;
    inner.agents.lock().await.items.insert(
        info.id.clone(),
        AgentRuntime {
            info: info.clone(),
            cancel,
            notify: Arc::new(Notify::new()),
            message: None,
            finish_reason: None,
            yielded: None,
            structured: None,
            awaited: false,
            activity: None,
        },
    );
}

pub(super) async fn resolve_profile(
    inner: &CoreInner,
    parent: &crate::session::state::SessionState,
    config: &crate::ResolvedConfig,
    profile: SpawnProfile,
) -> Result<(String, crate::session::entries::AgentSessionMeta), CoreError> {
    use crate::session::entries::AgentSessionMeta;
    let (selector, meta) = match profile {
        SpawnProfile::FixedRole(name) => {
            if parent
                .agent
                .as_ref()
                .and_then(|meta| meta.tool_policy.as_ref())
                .is_some_and(|policy| !policy.permits("spawn_agent"))
            {
                return Err(invalid("当前角色不允许派发子代理"));
            }
            let plugins = inner
                .project_plugins(Path::new(&parent.session.cwd))
                .await?;
            let mut roles = roles::fixed_roles(inner, config, &plugins);
            let role = roles.remove(&name).ok_or_else(|| {
                invalid(format!(
                    "未知或已禁用的 role `{name}`；可用 roles：{}",
                    roles
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            (
                role.model,
                AgentSessionMeta {
                    preset: Some(role.name),
                    prompt: Some(role.system_prompt),
                    tool_policy: Some(role.tools),
                    ..Default::default()
                },
            )
        }
        SpawnProfile::Sidekick { model } => {
            if parent.session.is_subagent()
                || !crate::config::model::is_fusion_selector(&parent.model)
            {
                return Err(invalid("sidekick 只能由 Fusion lead 创建"));
            }
            let role = inner
                .agent_presets
                .get(preset::SIDEKICK_PRESET)
                .expect("Fusion 执行侧已注册");
            (
                model,
                AgentSessionMeta {
                    preset: Some(role.name.clone()),
                    prompt: Some(role.system_prompt.clone()),
                    tool_policy: Some(role.tools.clone()),
                    output_schema: Some(preset::sidekick_report_schema()),
                    ..Default::default()
                },
            )
        }
        SpawnProfile::Workflow {
            run_id,
            model,
            system,
            tools,
            output_schema,
        } => {
            if !crate::workflow::workflows_active(parent, &config.workflow) {
                return Err(invalid("动态角色只能由已激活的 workflow 创建"));
            }
            workflow_scope::active_run(inner, &parent.session.id, &run_id).await?;
            // actor 只能执行脚本分配的 ask；新增 actor 与生命周期编排归 driver 所有。
            let mut tools = tools.unwrap_or_else(|| ToolPolicy::Deny(Default::default()));
            let control = ["spawn_agent", "sidekick", "stop_agent", "wait_agent"];
            match &mut tools {
                ToolPolicy::Allow(names) => names.retain(|name| !control.contains(&name.as_str())),
                ToolPolicy::Deny(names) => names.extend(control.into_iter().map(str::to_owned)),
            }
            tools.ensure_deliverable();
            if let Some(schema) = &output_schema {
                validate_output_schema(schema)?;
            }
            (
                model,
                AgentSessionMeta {
                    prompt: system,
                    tool_policy: Some(tools),
                    output_schema,
                    workflow_run: Some(run_id),
                    ..Default::default()
                },
            )
        }
    };
    Ok((resolve_model(config, &parent.model, &selector)?, meta))
}

/// spawn 参数里的 `output_schema` 必须是合法的 JSON Schema 对象。
pub(super) fn validate_output_schema(schema: &serde_json::Value) -> Result<(), CoreError> {
    jsonschema::validator_for(schema)
        .map_err(|error| invalid(format!("output_schema 不是合法的 JSON Schema：{error}")))?;
    Ok(())
}

pub(crate) async fn spawn(
    inner: Arc<CoreInner>,
    request: RequestContext,
    session_id: String,
    spec: SpawnSpec,
) -> Result<AgentSpawnResult, CoreError> {
    let task = validate_text(&spec.task, "task")?;
    let parent = parent_snapshot(&inner, &session_id).await?;
    let config = inner.project_config(Path::new(&parent.session.cwd))?;
    let depth = parent.session.delegation_depth + 1;
    if depth > config.agents.max_depth {
        return Err(invalid(format!(
            "已达到最大委托深度 {}：该会话不能再 spawn 子代理",
            config.agents.max_depth
        )));
    }
    let (model, agent_meta) = resolve_profile(&inner, &parent, &config, spec.profile).await?;
    let session = inner
        .sessions
        .create(
            &parent.session.cwd,
            &model,
            parent.engine,
            parent.permission,
            spec.title.as_deref(),
            SessionOrigin::spawned(session_id.clone(), depth, Some(agent_meta)),
            config.session.compress,
        )
        .await?;
    let record = inner.sessions.get(&session.id).await?;
    inner
        .sessions
        .ensure_write_lease(&session.id, &record, &request.client().frontend)
        .await?;
    // SeedFrom：导入的 transcript 前缀写在 header 之后、插件消息与 task 之前，
    // 让新子会话的前缀历史与前驱 actor 会话逐字一致（真 fork，不写前驱文件）。
    if !spec.seed_entries.is_empty() {
        let mut combined = record.state.lock().await.log_entries();
        combined.extend(spec.seed_entries.iter().cloned());
        inner.sessions.rewrite(&session.id, combined).await?;
    }
    let info = info_from_session(&session, AgentStatus::Queued);
    let cancel = match workflow_scope::actor_run(&inner, &session).await? {
        Some(run) => run.cancel.child_token(),
        None => child_cancel(&inner, &session_id).await,
    };
    prepare_agent(&inner, &info, cancel.clone()).await;
    let plugin_host =
        crate::service::plugin_events::host_for(&inner, Path::new(&parent.session.cwd)).await;
    if let Some(host) = &plugin_host {
        // 子代理会话同样是「会话」：`sessionStart` 注入的初始消息进入它的 transcript。
        let child_context = crate::service::plugin_events::context_from_info(&session);
        for message in crate::service::plugin_events::session_start(
            host,
            &child_context,
            &session,
            "subagent",
            Some(&session_id),
        )
        .await
        {
            let role = if message.role == "system" {
                Role::System
            } else {
                Role::User
            };
            inner
                .sessions
                .append_message(
                    &session.id,
                    Message {
                        role,
                        content: vec![moc_llm::ContentPart::Text(message.text)].into(),
                    },
                    None,
                    None,
                )
                .await?;
        }
    }
    let mut task = task;
    if let Some(host) = &plugin_host {
        let parent_context = crate::service::plugin_events::context(
            &session_id,
            &parent.session.cwd,
            parent.engine,
            crate::service::mode::Mode::effective(parent.plan_active, None),
            parent.permission,
            &parent.model,
            parent.session.is_subagent(),
        );
        if let Some(rewritten) =
            crate::service::plugin_events::agent_started(host, &parent_context, &info, &task).await
        {
            task = rewritten;
        }
    }
    let _ = request
        .updates()
        .emit(
            session_id.clone(),
            SessionUpdate::AgentStarted {
                agent: info.clone(),
            },
        )
        .await;
    activate(
        Arc::clone(&inner),
        request,
        session_id,
        info.id.clone(),
        task,
        cancel,
        Wakeup::default(),
    )
    .await?;
    let info = current_info(&inner, &info.id).await?;
    Ok(AgentSpawnResult { agent: info })
}

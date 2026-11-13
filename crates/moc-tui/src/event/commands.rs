//! 命令执行与会话设置。

use super::agents::open_subagents;
use super::context::open_context;
use super::context::open_tools;
use super::plan::confirm_plan;
use super::plugins::refresh_plugins;
use super::usage::refresh_usage;
use super::workflow::open_workflow_runs;
use crate::app::App;
use crate::commands::CommandId;
use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use moc_protocol::PluginRunCommand;
use moc_protocol::PluginRunCommandParams;
use moc_protocol::SessionCompact;
use moc_protocol::SessionCompactParams;
use moc_protocol::SessionEngine;
use moc_protocol::SessionList;
use moc_protocol::SessionListParams;
use moc_protocol::SessionNew;
use moc_protocol::SessionNewParams;
use moc_protocol::SessionPermission;
use moc_protocol::SessionPlanMode;
use moc_protocol::SessionPlanModeParams;
use moc_protocol::SessionSetEngine;
use moc_protocol::SessionSetEngineParams;
use moc_protocol::SessionSetPermission;
use moc_protocol::SessionSetPermissionParams;
use moc_protocol::commands::CommandRef;
use moc_protocol::commands::CommandSpec;

/// `execute_command` 的解析结果：先把借用 `app.commands` 的部分取出成自有值，
/// 分派阶段才能对 `app` 做可变操作。
pub(super) enum ResolvedCommand {
    Builtin(&'static CommandSpec<CommandId>),
    Plugin(moc_protocol::commands::PluginCommand),
    Unknown,
}

pub(super) fn execute_command(app: &mut App, connection: &EmbeddedConnection, line: String) {
    if app.skills_compacting && !matches!(line.trim(), "/quit" | "/exit") {
        app.set_notice("正在维护技能摘要；请等待完成或按 Esc 取消");
        return;
    }
    let (name, rest) = line
        .split_once(char::is_whitespace)
        .unwrap_or((line.as_str(), ""));
    let resolved = match app.commands.resolve(name) {
        Some(CommandRef::Builtin(spec)) => ResolvedCommand::Builtin(spec),
        Some(CommandRef::Plugin(command)) => ResolvedCommand::Plugin(command.clone()),
        None => ResolvedCommand::Unknown,
    };
    match resolved {
        ResolvedCommand::Builtin(spec) => {
            // 注册表静态门控：flag、会话、旁支聚焦、运行中。
            if let Some(flag) = spec.flag
                && !app.features.enabled(flag)
            {
                app.set_notice(format!(
                    "/{} 依赖的特性 {} 已禁用（见 /features）",
                    spec.name,
                    flag.spec().key
                ));
                return;
            }
            if spec.needs_session && app.session.is_none() {
                app.set_notice("当前没有活动会话");
                return;
            }
            if !spec.available_in_side && app.focused.is_some() {
                app.set_notice(format!("聚焦子代理时不能执行 /{}", spec.name));
                return;
            }
            if !spec.available_during_task && app.busy() {
                app.set_notice(format!("正在运行中，暂时不能执行 /{}", spec.name));
                return;
            }
            dispatch_command(app, connection, spec.id, rest);
        }
        ResolvedCommand::Plugin(command) => {
            // 插件命令兜底：名字由 `plugin/list` 提供，执行结果写进转写。
            let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
                app.set_notice("当前没有活动会话");
                return;
            };
            let client = connection.client.clone();
            let events = app.events.clone();
            let name = command.name;
            let args = rest.trim().to_owned();
            tokio::spawn(async move {
                match client
                    .call::<PluginRunCommand>(PluginRunCommandParams {
                        session_id,
                        name: name.clone(),
                        args,
                    })
                    .await
                {
                    Ok(result) => {
                        let _ = events.send(AppEvent::PluginOutput {
                            name,
                            text: result.text,
                            error: false,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(AppEvent::PluginOutput {
                            name,
                            text: format!("插件命令执行失败：{error}"),
                            error: true,
                        });
                    }
                }
            });
        }
        ResolvedCommand::Unknown => {
            app.set_notice(format!("未知命令：{name}"));
        }
    }
}

/// 内置命令分派：注册表门控过了以后按 `CommandId` 落到具体处理器。
pub(super) fn dispatch_command(
    app: &mut App,
    connection: &EmbeddedConnection,
    id: CommandId,
    rest: &str,
) {
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    match id {
        CommandId::Btw => crate::btw::reopen(app),
        CommandId::ConfirmPlan => confirm_plan(app, connection),
        CommandId::Close => {
            let Some(session_id) = session_id else {
                app.set_notice("当前没有活动会话");
                return;
            };
            let client = connection.client.clone();
            let events = app.events.clone();
            tokio::spawn(async move {
                match client
                    .call::<moc_protocol::SessionUnload>(moc_protocol::SessionUnloadParams {
                        session_id: session_id.clone(),
                    })
                    .await
                {
                    Ok(_) => {
                        let _ = events.send(AppEvent::SessionClosed { session_id });
                    }
                    Err(error) => {
                        let _ = events.send(AppEvent::Notice(format!("卸载会话失败：{error}")));
                    }
                }
            });
        }
        CommandId::Agents => open_subagents(app, connection),
        CommandId::Config => crate::settings::open(app, connection),
        CommandId::New => {
            let client = connection.client.clone();
            let events = app.events.clone();
            let (cwd, engine, fast_mode) = app
                .session
                .as_ref()
                .map(|session| (Some(session.cwd.clone()), session.engine, session.fast_mode))
                .unwrap_or((None, SessionEngine::Standard, false));
            tokio::spawn(async move {
                match client
                    .call::<SessionNew>(SessionNewParams {
                        cwd,
                        model: None,
                        title: None,
                        engine,
                        plan_active: false,
                        fast_mode,
                    })
                    .await
                {
                    Ok(info) => {
                        let _ = events.send(AppEvent::Loaded(
                            info,
                            Vec::new(),
                            false,
                            None,
                            Vec::new(),
                            false,
                            None,
                        ));
                    }
                    Err(error) => {
                        let _ = events.send(AppEvent::Notice(format!("新建会话失败：{error}")));
                    }
                }
            });
        }
        CommandId::Plugins => match rest.trim() {
            "" => refresh_plugins(app, connection, true, false),
            "reload" => refresh_plugins(app, connection, true, true),
            _ => app.set_notice("用法：/plugins [reload]"),
        },
        CommandId::Usage => refresh_usage(app, connection, true),
        CommandId::Monitor => app.modal = Some(Modal::Monitor),
        CommandId::Doctor => app.push_note(
            crate::doctor::report(app),
            crate::transcript::NoteTone::Info,
        ),
        CommandId::Help => app.modal = Some(Modal::Help { scroll: 0 }),
        CommandId::Context => open_context(app, connection),
        CommandId::Tools => open_tools(app, connection),
        CommandId::Quit => app.should_quit = true,
        CommandId::CompactSkills => {
            let action = match moc_protocol::SkillsCompactAction::parse_args(rest) {
                Ok(action) => action,
                Err(error) => {
                    app.set_error_notice(error);
                    return;
                }
            };
            let params = moc_protocol::SkillsCompactParams {
                session_id,
                cwd: Some(app.startup_cwd.clone()),
                model: app.session.as_ref().map(|session| session.model.clone()),
                action,
            };
            app.start_skills_compaction(action);
            let client = connection.client.clone();
            let events = app.events.clone();
            tokio::spawn(async move {
                let result = client
                    .call::<moc_protocol::SkillsCompact>(params)
                    .await
                    .map_err(Into::into);
                let _ = events.send(AppEvent::SkillsCompacted(result));
            });
        }
        CommandId::Compact => {
            // needs_session 已在分派前检查。
            let session_id = session_id.expect("needs_session 已校验");
            let client = connection.client.clone();
            let events = app.events.clone();
            let instructions = (!rest.trim().is_empty()).then(|| rest.trim().to_owned());
            app.inflight_session_ops.insert(session_id.clone());
            tokio::spawn(async move {
                let outcome = client
                    .call::<SessionCompact>(SessionCompactParams {
                        session_id: session_id.clone(),
                        instructions,
                    })
                    .await;
                let _ = events.send(AppEvent::SessionControlSettled {
                    session_id,
                    notice: Some("上下文压缩已完成".into()),
                    result: outcome
                        .map(|_| None)
                        .map_err(|error| format!("上下文压缩失败：{error}")),
                });
            });
        }
        CommandId::Engine => cycle_engine(app, connection),
        CommandId::ClmExperimental => set_engine(app, connection, SessionEngine::ClmExperimental),
        CommandId::Autonomy => set_autonomy(app, connection, rest),
        CommandId::Permission => set_permission(app, connection, rest),
        CommandId::Plan => toggle_plan(app, connection),
        CommandId::Goal => goal_command(app, connection, rest),
        CommandId::Ask => ask_once(app, connection, rest),
        CommandId::Retry => retry(app, connection),
        CommandId::Model => app.open_model_picker(),
        CommandId::Workflow => {
            let session_id = session_id.expect("needs_session 已校验");
            let client = connection.client.clone();
            let events = app.events.clone();
            let prompt_text = rest.to_string();
            tokio::spawn(async move {
                match client
                    .call::<moc_protocol::methods::SessionWorkflowActivate>(
                        moc_protocol::methods::SessionWorkflowActivateParams {
                            session_id: session_id.clone(),
                        },
                    )
                    .await
                {
                    Ok(_) => {
                        let _ = events.send(AppEvent::Notice("workflow 已激活".into()));
                        // 附带的指令作为下一条提示发送，让模型直接开写。
                        if !prompt_text.trim().is_empty() {
                            crate::queue::send_prompt(
                                &client,
                                session_id,
                                vec![moc_protocol::InputPart::Text { text: prompt_text }],
                                None,
                                events.clone(),
                            );
                        }
                    }
                    Err(error) => {
                        let _ =
                            events.send(AppEvent::Notice(format!("workflow 激活失败：{error}")));
                    }
                }
            });
        }
        CommandId::Workflows => open_workflow_runs(app, connection),
        CommandId::Jobs => {
            let session_id = session_id.expect("needs_session 已校验");
            let client = connection.client.clone();
            let events = app.events.clone();
            tokio::spawn(async move {
                match client
                    .call::<moc_protocol::methods::JobList>(moc_protocol::methods::JobListParams {
                        session_id,
                        tail_lines: None,
                    })
                    .await
                {
                    Ok(result) => {
                        let _ = events.send(AppEvent::OpenJobsModal(result.jobs));
                    }
                    Err(error) => {
                        let _ = events.send(AppEvent::Notice(format!("获取任务列表失败：{error}")));
                    }
                }
            });
        }
        CommandId::Resume => open_session_picker(app, connection),
        CommandId::Features => show_features(app),
    }
}

/// `/features`：特性注册表一览——阶段、有效状态与上下文成本。
pub(super) fn show_features(app: &mut App) {
    app.push_note(
        moc_protocol::features::format_features(&app.features),
        crate::transcript::NoteTone::Info,
    );
}

/// 循环切换引擎：按特性 flag 过滤后的清单循环。会话跑过第一条提示后引擎锁定。
pub(super) fn cycle_engine(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    if session.engine_locked {
        app.set_notice("会话引擎已锁定；换引擎请新建会话");
        return;
    }
    let engines: Vec<SessionEngine> = app.engines().collect();
    let index = engines
        .iter()
        .position(|engine| *engine == session.engine)
        .unwrap_or(0);
    set_engine(app, connection, engines[(index + 1) % engines.len()]);
}

/// `/plan`：切换 plan 模式；引擎不受影响，可在任意引擎上叠加。
pub(super) fn toggle_plan(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let active = !session.plan_active;
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<SessionPlanMode>(SessionPlanModeParams { session_id, active })
            .await
        {
            Ok(_) => {}
            Err(error) => {
                let _ = events.send(AppEvent::Notice(format!("切换计划状态失败：{error}")));
            }
        }
    });
}

/// `/goal […]` 的参数 → `session/goal` 动作：空为查看，`edit <objective>` 改原文，
/// 其余整段当新目标建。
pub(super) fn parse_goal_action(rest: &str) -> Result<moc_protocol::methods::GoalAction, String> {
    use moc_protocol::methods::GoalAction;
    let rest = rest.trim();
    match rest {
        "" => Ok(GoalAction::Show),
        "clear" => Ok(GoalAction::Unset),
        "pause" => Ok(GoalAction::Pause),
        "resume" => Ok(GoalAction::Resume),
        "edit" => Err("用法：/goal edit <objective>".to_owned()),
        _ => match rest.strip_prefix("edit") {
            Some(after) if after.starts_with(char::is_whitespace) => Ok(GoalAction::Edit {
                objective: after.trim().to_owned(),
            }),
            _ => Ok(GoalAction::Create {
                objective: rest.to_owned(),
            }),
        },
    }
}

/// `/goal` 回执写进转写的渲染文本。
pub(super) fn render_goal_note(goal: Option<&moc_protocol::methods::GoalView>) -> String {
    let Some(goal) = goal else {
        return "当前没有目标；/goal <objective> 创建".to_owned();
    };
    let phase = match goal.phase {
        moc_protocol::methods::GoalPhase::Active => crate::render::labels::status::ACTIVE,
        moc_protocol::methods::GoalPhase::Paused => crate::render::labels::status::PAUSED,
        moc_protocol::methods::GoalPhase::Blocked => crate::render::labels::status::BLOCKED,
        moc_protocol::methods::GoalPhase::Complete => crate::render::labels::status::DONE,
    };
    let mut text = format!(
        "目标：{}\n状态：{phase}{}轮次：{}\n续跑：{}",
        goal.objective,
        goal.blocked_reason
            .as_ref()
            .map(|reason| format!("\n阻塞：{}: {}\n", reason.code, reason.message))
            .unwrap_or_default(),
        goal.rounds_started,
        if goal.armed {
            "自动"
        } else {
            "已停（/goal resume 恢复）"
        }
    );
    text.push_str("\n命令：/goal edit <objective>｜/goal pause｜/goal resume｜/goal clear");
    text
}

/// `/goal`：查看或变更会话目标；`GoalChanged` 更新随后会把 `app.goal` 同步到权威值。
pub(super) fn goal_command(app: &mut App, connection: &EmbeddedConnection, rest: &str) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    let action = match parse_goal_action(rest) {
        Ok(action) => action,
        Err(usage) => {
            app.set_notice(usage);
            return;
        }
    };
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<moc_protocol::methods::SessionGoal>(moc_protocol::methods::SessionGoalParams {
                session_id,
                action,
            })
            .await
        {
            Ok(result) => {
                let _ = events.send(AppEvent::GoalResult {
                    text: render_goal_note(result.goal.as_ref()),
                    goal: result.goal,
                });
            }
            Err(error) => {
                let _ = events.send(AppEvent::Notice(format!("目标操作失败：{error}")));
            }
        }
    });
}

/// `/ask <问题>`：仅本条消息进入问答模式（只读），不改变会话引擎与 plan 状态。
///
/// 该次提示无论当前是否空闲都排进队列：模式覆盖是提示级属性，无法注入正在飞行的提示。
pub(super) fn ask_once(app: &mut App, connection: &EmbeddedConnection, rest: &str) {
    let text = rest.trim();
    if text.is_empty() {
        app.set_notice("用法：/ask <问题>");
        return;
    }
    if app.session.is_none() {
        app.set_notice("当前没有活动会话");
        return;
    }
    crate::queue::submit_with_mode(
        app,
        connection,
        crate::widgets::prompt::PromptState::from_text(text.to_owned()),
        moc_protocol::methods::PromptMode::Ask,
    );
}

/// `/retry`：不追加任何用户消息，按现有上下文重新请求一次；只有空闲时可用。
pub(super) fn retry(app: &mut App, connection: &EmbeddedConnection) {
    if app.read_only {
        app.set_notice("会话只读：另一个 moc 进程正在写入，关闭它或退出后才能操作");
        return;
    }
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.set_notice("当前没有活动会话");
        return;
    };
    // 重试复用同一条提示通道：正在生成或有压缩等会话操作时互斥，避免撞成 SessionBusy。
    if app.prompt_running
        || app.inflight_prompts.contains(&session_id)
        || app.inflight_session_ops.contains(&session_id)
    {
        app.set_notice("正在运行中，暂时不能重试");
        return;
    }
    app.inflight_prompts.insert(session_id.clone());
    app.prompt_running = true;
    crate::queue::send_retry(&connection.client, session_id, app.events.clone());
}

/// 请求 core 切换会话引擎（引擎循环与就地菜单共用）。
pub(super) fn set_engine(app: &mut App, connection: &EmbeddedConnection, engine: SessionEngine) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    if session.engine_locked {
        app.set_notice("会话引擎已锁定；换引擎请新建会话");
        return;
    }
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<SessionSetEngine>(SessionSetEngineParams { session_id, engine })
            .await
        {
            Ok(_) => {}
            Err(error) => {
                let _ = events.send(AppEvent::Notice(format!("切换引擎失败：{error}")));
            }
        }
    });
}

/// `/autonomy [default|dont_ask|unattended]`：无参数时循环切换自主程度，带参数直接设置。
pub(super) fn set_autonomy(app: &mut App, connection: &EmbeddedConnection, rest: &str) {
    use moc_protocol::SessionAutonomy;
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    let autonomy = match rest.trim() {
        "" => match session.autonomy {
            SessionAutonomy::Default => SessionAutonomy::DontAsk,
            SessionAutonomy::DontAsk => SessionAutonomy::Unattended,
            SessionAutonomy::Unattended => SessionAutonomy::Default,
        },
        "default" => SessionAutonomy::Default,
        "dont_ask" => SessionAutonomy::DontAsk,
        "unattended" => SessionAutonomy::Unattended,
        _ => {
            app.set_notice("用法：/autonomy [default|dont_ask|unattended]");
            return;
        }
    };
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        if let Err(error) = client
            .call::<moc_protocol::SessionSetAutonomy>(moc_protocol::SessionSetAutonomyParams {
                session_id,
                autonomy,
            })
            .await
        {
            let _ = events.send(AppEvent::Notice(format!("切换自主程度失败：{error}")));
        }
    });
}

pub(super) fn set_permission(app: &mut App, connection: &EmbeddedConnection, rest: &str) {
    let available = app
        .sandbox_modes
        .iter()
        .map(|mode| mode.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let rest = rest.trim();
    let (permission, clear_grants) = match rest {
        "" => {
            let current = app
                .session
                .as_ref()
                .map(|session| session.permission.as_str())
                .unwrap_or("none");
            app.set_notice(format!(
                "当前权限：{current}；可用模式：{available}；/permission clear 清空本会话沙盒授权"
            ));
            return;
        }
        "clear" => {
            let Some(session) = app.session.as_ref() else {
                app.set_notice("当前没有活动会话");
                return;
            };
            (session.permission, true)
        }
        "full" => (SessionPermission::Full, false),
        "workspace" => (SessionPermission::Workspace, false),
        "workspace-only" | "workspace_only" => (SessionPermission::WorkspaceOnly, false),
        _ => {
            app.set_notice(format!(
                "用法：/permission <{}|clear>",
                available.replace(", ", "|")
            ));
            return;
        }
    };
    apply_permission(app, connection, permission, clear_grants);
}

/// 请求 core 切换权限模式；`/permission` 与图例菜单共用。平台不支持或没有会话时只提示。
pub(super) fn apply_permission(
    app: &mut App,
    connection: &EmbeddedConnection,
    permission: SessionPermission,
    clear_grants: bool,
) {
    if !app.sandbox_modes.contains(&permission) {
        let available = app
            .sandbox_modes
            .iter()
            .map(|mode| mode.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        app.set_notice(format!(
            "当前平台不支持 {}；可用模式：{available}",
            permission.as_str()
        ));
        return;
    }
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<SessionSetPermission>(SessionSetPermissionParams {
                session_id,
                permission,
                clear_grants,
            })
            .await
        {
            Ok(_) if clear_grants => {
                let _ = events.send(AppEvent::Notice("已清空本会话的沙盒授权".into()));
            }
            Ok(_) => {}
            Err(error) => {
                let _ = events.send(AppEvent::Notice(format!("切换权限失败：{error}")));
            }
        }
    });
}

/// ctrl+r / `/resume`：拉取会话列表后打开选择器。
pub(super) fn open_session_picker(app: &mut App, connection: &EmbeddedConnection) {
    let client = connection.client.clone();
    let events = app.events.clone();
    app.open_session_picker();
    tokio::spawn(async move {
        match client.call::<SessionList>(SessionListParams {}).await {
            Ok(list) => {
                let _ = events.send(AppEvent::Sessions(list.sessions));
            }
            Err(error) => {
                let _ = events.send(AppEvent::PickerError(format!("加载会话列表失败：{error}")));
            }
        }
    });
}

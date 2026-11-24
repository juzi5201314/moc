use super::CommandResult;
use super::Input;
use super::display::capacity_notice;
use super::display::parse_engine;
use super::display::print_sessions;
use super::display::print_usage;
use super::display::render_goal;
use super::interaction::approve_plan;
use super::interaction::read_line;
use super::interaction::send_prompt;
use super::interaction::send_retry;
use crate::commands::CommandId;
use crate::commands::Registry;
use anyhow::bail;
use moc_protocol::ClientHandle;
use moc_protocol::commands::CommandRef;
use moc_protocol::features::FeatureStates;
use moc_protocol::methods::McpList;
use moc_protocol::methods::McpListParams;
use moc_protocol::methods::McpReconnect;
use moc_protocol::methods::McpReconnectParams;
use moc_protocol::methods::McpServerPhase;
use moc_protocol::methods::McpStatusSnapshot;
use moc_protocol::methods::PluginInfo;
use moc_protocol::methods::PluginList;
use moc_protocol::methods::PluginListParams;
use moc_protocol::methods::PluginReload;
use moc_protocol::methods::PluginReloadParams;
use moc_protocol::methods::PluginRunCommand;
use moc_protocol::methods::PluginRunCommandParams;
use moc_protocol::methods::SessionAutonomy;
use moc_protocol::methods::SessionCancel;
use moc_protocol::methods::SessionCancelParams;
use moc_protocol::methods::SessionCompact;
use moc_protocol::methods::SessionCompactParams;
use moc_protocol::methods::SessionEngine;
use moc_protocol::methods::SessionGoal;
use moc_protocol::methods::SessionGoalParams;
use moc_protocol::methods::SessionInfo;
use moc_protocol::methods::SessionList;
use moc_protocol::methods::SessionListParams;
use moc_protocol::methods::SessionModelOverflowAction;
use moc_protocol::methods::SessionNew;
use moc_protocol::methods::SessionNewParams;
use moc_protocol::methods::SessionPermission;
use moc_protocol::methods::SessionPlanMode;
use moc_protocol::methods::SessionPlanModeParams;
use moc_protocol::methods::SessionSetAutonomy;
use moc_protocol::methods::SessionSetAutonomyParams;
use moc_protocol::methods::SessionSetEngine;
use moc_protocol::methods::SessionSetEngineParams;
use moc_protocol::methods::SessionSetModel;
use moc_protocol::methods::SessionSetModelParams;
use moc_protocol::methods::SessionSetPermission;
use moc_protocol::methods::SessionSetPermissionParams;
use moc_protocol::methods::SessionUsage;
use moc_protocol::methods::SessionUsageParams;
use moc_protocol::methods::prompt::PromptMode;
use moc_i18n::t;
use std::io::Write;

pub(super) async fn run_command(
    client: &ClientHandle,
    registry: &Registry,
    features: &FeatureStates,
    command: &str,
    session: &mut SessionInfo,
    input: &Input,
) -> anyhow::Result<CommandResult> {
    let (name, argument) = command.split_once(' ').unwrap_or((command, ""));
    let argument = argument.trim();
    if name.is_empty() {
        return Ok(CommandResult::Continue);
    }
    // 命令表是唯一来源：解析（含别名）由注册表完成，`/help` 与分派分支共用同一批
    // `CommandSpec`；未命中内置命令时再问 core 这是不是插件注册的命令。
    let Some(CommandRef::Builtin(spec)) = registry.resolve(name) else {
        if run_plugin_command(client, session, name, argument).await? {
            return Ok(CommandResult::Continue);
        }
        print_unknown_command(name);
        return Ok(CommandResult::Continue);
    };
    if let Some(flag) = spec.flag
        && !features.enabled(flag)
    {
        eprintln!(
            "{}",
            t!(
                "cli.cmd.flag_disabled",
                name = spec.name,
                flag = flag.spec().key
            )
        );
        return Ok(CommandResult::Continue);
    }
    match spec.id {
        CommandId::Quit => return Ok(CommandResult::Quit),
        CommandId::Cancel => {
            let result = client
                .call::<SessionCancel>(SessionCancelParams {
                    session_id: session.id.clone(),
                })
                .await?;
            println!(
                "{}",
                if result.cancelled {
                    t!("cli.cmd.cancelled")
                } else {
                    t!("cli.cmd.no_prompt_running")
                }
            );
        }
        CommandId::Help => print!("{}", crate::commands::help_text(features)),
        CommandId::Features => {
            print!("{}", moc_protocol::features::format_features(features))
        }
        CommandId::Mcp => {
            if argument.is_empty() {
                print_mcp(client, &session.id).await?;
            } else if let Some(rest) = argument.strip_prefix("reconnect")
                && (rest.is_empty() || rest.starts_with(char::is_whitespace))
            {
                let target = rest.trim();
                reconnect_mcp(
                    client,
                    &session.id,
                    (!target.is_empty()).then(|| target.to_owned()),
                )
                .await?;
            } else {
                bail!("{}", t!("cli.cmd.usage_mcp"));
            }
        }
        CommandId::Plugins => match argument {
            "" => print_plugins(client, &session.id).await?,
            "reload" => reload_plugins(client, &session.id).await?,
            _ => bail!("{}", t!("cli.cmd.usage_plugins")),
        },
        CommandId::New => {
            *session = client
                .call::<SessionNew>(SessionNewParams {
                    cwd: Some(session.cwd.clone()),
                    model: None,
                    title: None,
                    engine: session.engine,
                    plan_active: false,
                    fast_mode: session.fast_mode,
                })
                .await?;
            println!("{}", t!("cli.cmd.new_session", id = session.id));
        }
        CommandId::Sessions => {
            let result = client.call::<SessionList>(SessionListParams {}).await?;
            print_sessions(&result.sessions);
        }
        CommandId::Engine => {
            let next = parse_engine(argument)?;
            if session.engine_locked {
                bail!("{}", t!("cli.cmd.engine_locked"));
            }
            *session = client
                .call::<SessionSetEngine>(SessionSetEngineParams {
                    session_id: session.id.clone(),
                    engine: next,
                })
                .await?;
            if next == SessionEngine::ClmExperimental {
                println!("{}", t!("cli.cmd.clm_enabled"));
            }
        }
        CommandId::ClmExperimental => {
            if session.engine_locked {
                bail!("{}", t!("cli.cmd.engine_locked"));
            }
            *session = client
                .call::<SessionSetEngine>(SessionSetEngineParams {
                    session_id: session.id.clone(),
                    engine: SessionEngine::ClmExperimental,
                })
                .await?;
            println!("{}", t!("cli.cmd.clm_enabled"));
        }
        CommandId::Autonomy => {
            let autonomy = match argument {
                "" => match session.autonomy {
                    SessionAutonomy::Default => SessionAutonomy::DontAsk,
                    SessionAutonomy::DontAsk => SessionAutonomy::Unattended,
                    SessionAutonomy::Unattended => SessionAutonomy::Default,
                },
                "default" => SessionAutonomy::Default,
                "dont_ask" => SessionAutonomy::DontAsk,
                "unattended" => SessionAutonomy::Unattended,
                _ => bail!("{}", t!("cli.cmd.usage_autonomy")),
            };
            *session = client
                .call::<SessionSetAutonomy>(SessionSetAutonomyParams {
                    session_id: session.id.clone(),
                    autonomy,
                })
                .await?;
        }
        CommandId::Permission => {
            let clear = argument == "clear";
            let permission = match argument {
                "full" => SessionPermission::Full,
                "workspace" => SessionPermission::Workspace,
                "workspace-only" | "workspace_only" => SessionPermission::WorkspaceOnly,
                "clear" => session.permission,
                _ => bail!("{}", t!("cli.cmd.usage_permission")),
            };
            *session = client
                .call::<SessionSetPermission>(SessionSetPermissionParams {
                    session_id: session.id.clone(),
                    permission,
                    clear_grants: clear,
                })
                .await?;
            if clear {
                println!("{}", t!("cli.cmd.grants_cleared"));
            } else {
                println!(
                    "{}",
                    t!("cli.cmd.permission_set", mode = session.permission.as_str())
                );
            }
        }
        CommandId::Plan => {
            let active = if argument.is_empty() {
                !session.plan_active
            } else {
                true
            };
            *session = client
                .call::<SessionPlanMode>(SessionPlanModeParams {
                    session_id: session.id.clone(),
                    active,
                })
                .await?;
            println!(
                "{}",
                t!(
                    "cli.cmd.plan_state",
                    state = if active {
                        t!("common.on")
                    } else {
                        t!("common.off")
                    }
                    .as_ref()
                )
            );
            if !argument.is_empty() {
                send_prompt(client, &session.id, argument.to_owned(), None, None).await?;
            }
        }
        CommandId::Goal => {
            use moc_protocol::methods::GoalAction;
            let action = match argument {
                "" => GoalAction::Show,
                "clear" => GoalAction::Unset,
                "pause" => GoalAction::Pause,
                "resume" => GoalAction::Resume,
                "edit" => bail!("{}", t!("cli.cmd.usage_goal_edit")),
                _ => match argument.strip_prefix("edit") {
                    Some(rest) if rest.starts_with(char::is_whitespace) => GoalAction::Edit {
                        objective: rest.trim().to_owned(),
                    },
                    _ => GoalAction::Create {
                        objective: argument.to_owned(),
                    },
                },
            };
            let result = client
                .call::<SessionGoal>(SessionGoalParams {
                    session_id: session.id.clone(),
                    action,
                })
                .await?;
            print!("{}", render_goal(result.goal.as_ref()));
        }
        CommandId::Ask => {
            if argument.is_empty() {
                bail!("{}", t!("cli.cmd.usage_ask"));
            }
            send_prompt(
                client,
                &session.id,
                argument.to_owned(),
                Some(PromptMode::Ask),
                None,
            )
            .await?;
        }
        CommandId::Approve => approve_plan(client, argument, session).await?,
        CommandId::CompactSkills => {
            let action = moc_protocol::SkillsCompactAction::parse_args(argument)
                .map_err(anyhow::Error::msg)?;
            let pending =
                client.call::<moc_protocol::SkillsCompact>(moc_protocol::SkillsCompactParams {
                    session_id: Some(session.id.clone()),
                    cwd: None,
                    model: None,
                    action,
                });
            tokio::pin!(pending);
            eprintln!("{}", t!("cli.cmd.skills_working"));
            let result = loop {
                tokio::select! {
                    result = &mut pending => break result?,
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        client.call::<moc_protocol::SkillsCancel>(moc_protocol::EmptyParams {}).await?;
                    }
                    line = read_line(input) => {
                        if line?.is_some() { eprintln!("{}", t!("cli.cmd.skills_busy")); }
                        else { client.call::<moc_protocol::SkillsCancel>(moc_protocol::EmptyParams {}).await?; break pending.await?; }
                    }
                }
            };
            println!("{}", result.message);
            for warning in result.warnings {
                eprintln!("{}", t!("cli.cmd.warning", msg = warning));
            }
        }
        CommandId::Compact => {
            let result = client
                .call::<SessionCompact>(SessionCompactParams {
                    session_id: session.id.clone(),
                    instructions: None,
                })
                .await?;
            if result.method == "noop" {
                println!("{}", t!("core.history.nothing_to_compact"));
            } else if result.full {
                println!(
                    "{}",
                    t!(
                        "cli.cmd.compacted",
                        method = result.method,
                        before = result.tokens_before,
                        after = result.tokens_after
                    )
                );
            } else {
                println!(
                    "{}",
                    t!(
                        "cli.cmd.trimmed",
                        before = result.tokens_before,
                        after = result.tokens_after
                    )
                );
            }
        }
        CommandId::Usage => {
            let result = client
                .call::<SessionUsage>(SessionUsageParams {
                    session_id: session.id.clone(),
                })
                .await?;
            print_usage(&result);
        }
        CommandId::Export => {
            let result = client
                .call::<moc_protocol::SessionExport>(moc_protocol::SessionExportParams {
                    session_id: session.id.clone(),
                    path: (!argument.is_empty()).then(|| argument.to_owned()),
                })
                .await?;
            println!("{}", t!("cli.cmd.exported", path = result.path));
        }
        CommandId::DebugInfo => {
            if !argument.is_empty() {
                bail!("{}", t!("cli.cmd.usage_debuginfo"));
            }
            let text = client
                .call::<moc_protocol::SessionDebugInfo>(moc_protocol::SessionDebugInfoParams {
                    session_id: session.id.clone(),
                })
                .await?;
            println!("{text}");
        }
        CommandId::Session => {
            if !argument.is_empty() {
                bail!("{}", t!("cli.cmd.usage_session"));
            }
            let text = client
                .call::<moc_protocol::SessionInspect>(moc_protocol::SessionInspectParams {
                    session_id: session.id.clone(),
                })
                .await?;
            println!("{text}");
        }
        CommandId::Model => {
            if argument.is_empty() {
                bail!("{}", t!("cli.cmd.usage_model"));
            }
            let mut result = client
                .call::<SessionSetModel>(SessionSetModelParams {
                    session_id: session.id.clone(),
                    selector: argument.to_owned(),
                    overflow: SessionModelOverflowAction::Ask,
                })
                .await?;
            if let Some(overflow) = &result.overflow {
                println!(
                    "{}",
                    t!(
                        "cli.cmd.model_compact_notice",
                        notice = capacity_notice(overflow, argument),
                        model = argument,
                        target = overflow.target_tokens
                    )
                );
                print!("{}", t!("cli.cmd.model_confirm", model = argument));
                std::io::stdout().flush()?;
                let confirmed = read_line(input)
                    .await?
                    .is_some_and(|line| matches!(line.trim(), "y" | "Y" | "yes" | "YES"));
                if !confirmed {
                    println!("{}", t!("cli.cmd.model_cancelled", model = session.model));
                    return Ok(CommandResult::Continue);
                }
                result = client
                    .call::<SessionSetModel>(SessionSetModelParams {
                        session_id: session.id.clone(),
                        selector: argument.to_owned(),
                        overflow: SessionModelOverflowAction::Compact,
                    })
                    .await?;
            }
            *session = result.session;
            match result.compaction_error {
                Some(error) => eprintln!(
                    "{}: {error}",
                    t!("cli.cmd.model_compact_failed", model = session.model)
                ),
                None => println!("{}", t!("cli.cmd.model_switched", model = session.model)),
            }
        }
        CommandId::Retry => {
            send_retry(client, &session.id).await?;
        }
        CommandId::Agents => {
            let result = client
                .call::<moc_protocol::methods::AgentList>(moc_protocol::methods::AgentListParams {
                    session_id: session.id.clone(),
                })
                .await?;
            for agent in result.agents {
                println!(
                    "{}\t{:?}\t{}\t{}",
                    agent.id,
                    agent.status,
                    agent.model,
                    agent.last_message.unwrap_or_default()
                );
            }
        }
    }
    Ok(CommandResult::Continue)
}

/// 执行插件注册的斜杠命令；名字不存在时返回 `false`，交给内置命令的未知分支。
pub(super) async fn run_plugin_command(
    client: &ClientHandle,
    session: &SessionInfo,
    name: &str,
    argument: &str,
) -> anyhow::Result<bool> {
    let list = client
        .call::<PluginList>(PluginListParams {
            session_id: session.id.clone(),
        })
        .await?;
    if !list
        .plugins
        .iter()
        .any(|plugin| plugin.commands.iter().any(|command| command.name == name))
    {
        return Ok(false);
    }
    let result = client
        .call::<PluginRunCommand>(PluginRunCommandParams {
            session_id: session.id.clone(),
            name: name.to_owned(),
            args: argument.to_owned(),
        })
        .await?;
    println!("{}", result.text);
    Ok(true)
}

pub(super) async fn print_mcp(client: &ClientHandle, session_id: &str) -> anyhow::Result<()> {
    let snapshot = client
        .call::<McpList>(McpListParams {
            session_id: Some(session_id.to_owned()),
            cwd: None,
        })
        .await?;
    print_mcp_status(&snapshot, false);
    Ok(())
}

pub(super) async fn reconnect_mcp(
    client: &ClientHandle,
    session_id: &str,
    server: Option<String>,
) -> anyhow::Result<()> {
    let snapshot = client
        .call::<McpReconnect>(McpReconnectParams {
            session_id: Some(session_id.to_owned()),
            cwd: None,
            server,
        })
        .await?;
    print_mcp_status(&snapshot, true);
    Ok(())
}

fn print_mcp_status(snapshot: &McpStatusSnapshot, reconnected: bool) {
    if let Some(error) = &snapshot.config_error {
        println!("{}", t!("cli.cmd.mcp_config_error", err = error.as_str()));
    }
    if snapshot.servers.is_empty() {
        if snapshot.config_error.is_none() {
            println!("{}", t!("cli.cmd.no_mcp"));
        }
        return;
    }
    let connected = snapshot
        .servers
        .iter()
        .filter(|server| server.phase == McpServerPhase::Connected)
        .count();
    let connecting = snapshot
        .servers
        .iter()
        .filter(|server| {
            matches!(
                server.phase,
                McpServerPhase::Connecting | McpServerPhase::Reconnecting
            )
        })
        .count();
    let failed = snapshot
        .servers
        .iter()
        .filter(|server| server.phase == McpServerPhase::Failed)
        .count();
    let header_key = if reconnected {
        "cli.cmd.mcp_reconnected"
    } else {
        "cli.cmd.mcp_header"
    };
    println!(
        "{}",
        t!(
            header_key,
            total = snapshot.servers.len(),
            connected = connected,
            connecting = connecting,
            failed = failed
        )
    );
    for server in &snapshot.servers {
        let status = match server.phase {
            McpServerPhase::Connected => {
                t!("cli.cmd.mcp_connected", tools = server.tool_count).into_owned()
            }
            McpServerPhase::Connecting => t!("cli.cmd.mcp_connecting").into_owned(),
            McpServerPhase::Reconnecting => t!(
                "cli.cmd.mcp_reconnecting",
                attempts = server.attempts,
                err = server.error.as_deref().unwrap_or("")
            )
            .into_owned(),
            McpServerPhase::Failed => {
                t!("cli.cmd.mcp_failed", err = server.error.as_deref().unwrap_or("")).into_owned()
            }
            McpServerPhase::Disabled => t!("cli.cmd.mcp_disabled").into_owned(),
        };
        println!(
            "- {} [{}] {} — {}",
            server.name, server.transport, server.target, status
        );
        if server.phase != McpServerPhase::Connected && !server.recent_stderr.is_empty() {
            let start = server.recent_stderr.len().saturating_sub(3);
            for line in &server.recent_stderr[start..] {
                println!("  stderr: {line}");
            }
        }
    }
}

pub(super) async fn print_plugins(client: &ClientHandle, session_id: &str) -> anyhow::Result<()> {
    let list = client
        .call::<PluginList>(PluginListParams {
            session_id: session_id.to_owned(),
        })
        .await?;
    print_plugin_list(&list.plugins);
    Ok(())
}

/// `/plugins reload`：丢弃插件宿主后重新发现并加载，再打印新清单。
pub(super) async fn reload_plugins(client: &ClientHandle, session_id: &str) -> anyhow::Result<()> {
    let result = client
        .call::<PluginReload>(PluginReloadParams {
            session_id: session_id.to_owned(),
        })
        .await?;
    let disabled = result
        .plugins
        .iter()
        .filter(|plugin| plugin.error.is_some())
        .count();
    println!(
        "{}",
        t!(
            "cli.cmd.plugins_reloaded",
            ok = result.plugins.len() - disabled,
            disabled = disabled
        )
    );
    print_plugin_list(&result.plugins);
    Ok(())
}

/// 打印插件清单：一行标题，随后每个插件的来源、目录、工具与命令。
pub(super) fn print_plugin_list(plugins: &[PluginInfo]) {
    if plugins.is_empty() {
        println!("{}", t!("cli.cmd.no_plugins"));
        return;
    }
    println!("{}", t!("cli.cmd.plugins_loaded", n = plugins.len()));
    for plugin in plugins {
        let version = plugin
            .version
            .as_deref()
            .map(|version| format!(" v{version}"))
            .unwrap_or_default();
        println!(
            "{}",
            t!(
                "cli.cmd.plugin_row",
                name = plugin.name,
                version = version,
                source = plugin.source,
                desc = plugin.description.as_deref().unwrap_or_default()
            )
        );
        println!("  {}", plugin.dir);
        if let Some(error) = &plugin.error {
            println!("{}", t!("cli.cmd.plugin_unavailable", error = error));
            continue;
        }
        for tool in &plugin.tools {
            println!(
                "{}",
                t!(
                    "cli.cmd.plugin_tool",
                    name = tool.name,
                    modes = tool.modes.join(",")
                )
            );
        }
        for command in &plugin.commands {
            println!("{}", t!("cli.cmd.plugin_command", name = command.name));
        }
    }
}

pub(super) fn print_unknown_command(name: &str) {
    eprintln!("{}", t!("cli.cmd.unknown", name = name));
}

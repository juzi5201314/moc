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
            "/{} 依赖的特性 {} 已禁用（见 /features）",
            spec.name,
            flag.spec().key
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
                    "提示取消请求已发送"
                } else {
                    "当前没有进行中的提示"
                }
            );
        }
        CommandId::Help => print!("{}", crate::commands::help_text(features)),
        CommandId::Features => {
            print!("{}", moc_protocol::features::format_features(features))
        }
        CommandId::Plugins => match argument {
            "" => print_plugins(client, &session.id).await?,
            "reload" => reload_plugins(client, &session.id).await?,
            _ => bail!("用法：/plugins [reload]"),
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
            println!("新会话：{}", session.id);
        }
        CommandId::Sessions => {
            let result = client.call::<SessionList>(SessionListParams {}).await?;
            print_sessions(&result.sessions);
        }
        CommandId::Engine => {
            let next = parse_engine(argument)?;
            if session.engine_locked {
                bail!("会话引擎已锁定；换引擎请新建会话");
            }
            *session = client
                .call::<SessionSetEngine>(SessionSetEngineParams {
                    session_id: session.id.clone(),
                    engine: next,
                })
                .await?;
            if next == SessionEngine::ClmExperimental {
                println!("CLM 实验引擎已启用；本会话首次请求后引擎锁定");
            }
        }
        CommandId::ClmExperimental => {
            if session.engine_locked {
                bail!("会话引擎已锁定；换引擎请新建会话");
            }
            *session = client
                .call::<SessionSetEngine>(SessionSetEngineParams {
                    session_id: session.id.clone(),
                    engine: SessionEngine::ClmExperimental,
                })
                .await?;
            println!("CLM 实验引擎已启用；本会话首次请求后引擎锁定");
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
                _ => bail!("用法：/autonomy [default|dont_ask|unattended]"),
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
                _ => bail!("用法：/permission <full|workspace|workspace-only|clear>"),
            };
            *session = client
                .call::<SessionSetPermission>(SessionSetPermissionParams {
                    session_id: session.id.clone(),
                    permission,
                    clear_grants: clear,
                })
                .await?;
            if clear {
                println!("已清空本会话的沙盒授权");
            } else {
                println!("权限已切换为 {}", session.permission.as_str());
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
            println!("计划状态：{}", if active { "开启" } else { "关闭" });
            if !argument.is_empty() {
                send_prompt(client, &session.id, argument.to_owned(), None).await?;
            }
        }
        CommandId::Goal => {
            use moc_protocol::methods::GoalAction;
            let action = match argument {
                "" => GoalAction::Show,
                "clear" => GoalAction::Unset,
                "pause" => GoalAction::Pause,
                "resume" => GoalAction::Resume,
                "edit" => bail!("用法：/goal edit <objective>"),
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
                bail!("用法：/ask <问题>");
            }
            send_prompt(
                client,
                &session.id,
                argument.to_owned(),
                Some(PromptMode::Ask),
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
            eprintln!("Working：正在维护技能摘要，Ctrl+C 取消");
            let result = loop {
                tokio::select! {
                    result = &mut pending => break result?,
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        client.call::<moc_protocol::SkillsCancel>(moc_protocol::EmptyParams {}).await?;
                    }
                    line = read_line(input) => {
                        if line?.is_some() { eprintln!("技能摘要操作尚未完成，输入未发送也未排队"); }
                        else { client.call::<moc_protocol::SkillsCancel>(moc_protocol::EmptyParams {}).await?; break pending.await?; }
                    }
                }
            };
            println!("{}", result.message);
            for warning in result.warnings {
                eprintln!("警告：{warning}");
            }
        }
        CommandId::Compact => {
            let result = client
                .call::<SessionCompact>(SessionCompactParams {
                    session_id: session.id.clone(),
                    instructions: None,
                })
                .await?;
            if result.full {
                println!(
                    "上下文压缩：{}，{} → {} token",
                    result.method, result.tokens_before, result.tokens_after
                );
            } else {
                println!(
                    "上下文裁剪：{} → {} token（未生成摘要）",
                    result.tokens_before, result.tokens_after
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
        CommandId::Model => {
            if argument.is_empty() {
                bail!("用法：/model <selector>");
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
                    "{}；切换会先用 {} 压缩上下文（压缩后不超过 {} token）",
                    capacity_notice(overflow, argument),
                    argument,
                    overflow.target_tokens
                );
                print!("是否切换到 {argument} 并压缩？[y/N] ");
                std::io::stdout().flush()?;
                let confirmed = read_line(input)
                    .await?
                    .is_some_and(|line| matches!(line.trim(), "y" | "Y" | "yes" | "YES"));
                if !confirmed {
                    println!("已取消切换，模型仍为 {}", session.model);
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
                Some(error) => eprintln!("已切换到 {}，但上下文压缩失败：{error}", session.model),
                None => println!("模型已切换为 {}", session.model),
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
        "插件已重载：{} 个可用 / {disabled} 个仍不可用",
        result.plugins.len() - disabled
    );
    print_plugin_list(&result.plugins);
    Ok(())
}

/// 打印插件清单：一行标题，随后每个插件的来源、目录、工具与命令。
pub(super) fn print_plugin_list(plugins: &[PluginInfo]) {
    if plugins.is_empty() {
        println!(
            "当前工作区没有插件。把插件放进 .moc/plugins/<name>/（或 $MOC_HOME/plugins）即可自动启用。"
        );
        return;
    }
    println!("已加载 {} 个插件：", plugins.len());
    for plugin in plugins {
        let version = plugin
            .version
            .as_deref()
            .map(|version| format!(" v{version}"))
            .unwrap_or_default();
        println!(
            "- {}{version}（{}）{}",
            plugin.name,
            plugin.source,
            plugin.description.as_deref().unwrap_or_default()
        );
        println!("  {}", plugin.dir);
        if let Some(error) = &plugin.error {
            println!("  不可用：{error}");
            continue;
        }
        for tool in &plugin.tools {
            println!("  工具 {} [{}]", tool.name, tool.modes.join(","));
        }
        for command in &plugin.commands {
            println!("  命令 /{}", command.name);
        }
    }
}

pub(super) fn print_unknown_command(name: &str) {
    eprintln!("未知命令 `/{name}`；输入 /help 查看命令。");
}

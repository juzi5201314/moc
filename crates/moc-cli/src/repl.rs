use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use moc_core::{Core, CoreConfig, PtcWorkerCommand, resolve_models};
use moc_protocol::commands::CommandRef;
use moc_protocol::features::FeatureStates;
use moc_protocol::methods::{
    AskAnswer, AskResult, ClientCapabilities, ClientInfo, ConfigGet, ConfigGetParams, InputPart,
    ModelSwitchOverflow, PermissionRequestParams, PermissionResult, PlanAction, PlanStatus,
    PluginInfo, PluginList, PluginListParams, PluginReload, PluginReloadParams, PluginRunCommand,
    PluginRunCommandParams, SessionAutonomy, SessionCancel, SessionCancelParams, SessionCompact,
    SessionCompactParams, SessionEngine, SessionGoal, SessionGoalParams, SessionInfo, SessionList,
    SessionListParams, SessionLoad, SessionLoadParams, SessionModelOverflowAction, SessionNew,
    SessionNewParams, SessionPermission, SessionPlanApprove, SessionPlanApproveParams,
    SessionPlanMode, SessionPlanModeParams, SessionPrompt, SessionRequestAsk,
    SessionRequestPermission, SessionRetry, SessionRetryParams, SessionSetAutonomy,
    SessionSetAutonomyParams, SessionSetEngine, SessionSetEngineParams, SessionSetModel,
    SessionSetModelParams, SessionSetPermission, SessionSetPermissionParams, SessionUpdate,
    SessionUpdateMethod, SessionUsage, SessionUsageParams, prompt::PromptMode,
};
use moc_protocol::{
    ClientBuilder, ClientHandle, RpcError, RpcErrorCode, Server, ServerOptions, transport,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::args::CliArgs;
use crate::commands::{CommandId, Registry};
use crate::render::Renderer;

pub(crate) type Input = Arc<AsyncMutex<tokio::sync::mpsc::Receiver<std::io::Result<String>>>>;

fn input_reader() -> Input {
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    // 独立线程允许主运行时在 Ctrl+C 后退出，不等待 Tokio 的不可取消 stdin 阻塞任务。
    std::thread::spawn(move || {
        let input = std::io::stdin();
        for line in input.lock().lines() {
            if sender.blocking_send(line).is_err() {
                break;
            }
        }
    });
    Arc::new(AsyncMutex::new(receiver))
}

pub async fn run(args: CliArgs, frontend: crate::Frontend) -> anyhow::Result<()> {
    if frontend == crate::Frontend::Acp {
        validate_acp_args(&args)?;
    }
    let config = make_core_config(&args)?;
    if args.models {
        for model in resolve_models(&config)? {
            println!(
                "{}\t{}\t{} tokens",
                model.selector, model.display_name, model.context_window
            );
        }
        return Ok(());
    }
    let core = Core::new(config).context("初始化 MOC core 失败")?;
    // SIGTERM / SIGHUP 不会走前端的正常退出路径：先把前端 future 收掉（TUI 的
    // TerminalGuard Drop 恢复终端、连接 Drop 触发收尾），再统一执行 core.shutdown()，
    // 保证 raw mode 与 MCP 子进程都不因信号退出而丢失。
    let result = tokio::select! {
        result = run_frontend(Arc::clone(&core), args, frontend) => result,
        signal = termination_signal() => {
            eprintln!("收到 {signal}，正在退出");
            Ok(())
        }
    };
    core.shutdown().await;
    result
}

/// `moc acp` 下没有意义的 CLI 参数：会话与提示都由 ACP 客户端驱动。
fn validate_acp_args(args: &CliArgs) -> anyhow::Result<()> {
    let mut bad = Vec::new();
    if args.prompt.is_some() {
        bad.push("提示词");
    }
    if args.resume.is_some() {
        bad.push("--resume");
    }
    if args.continue_session {
        bad.push("--continue");
    }
    if args.list {
        bad.push("--list");
    }
    if args.models {
        bad.push("--models");
    }
    if args.print {
        bad.push("--print");
    }
    if args.repl {
        bad.push("--repl");
    }
    if args.detail.is_some() {
        bad.push("--detail");
    }
    if args.hide_thinking {
        bad.push("--hide-thinking");
    }
    if args.port.is_some() || args.token.is_some() || args.no_open {
        bad.push("web 选项");
    }
    anyhow::ensure!(bad.is_empty(), "moc acp 不支持这些参数：{}", bad.join("、"));
    Ok(())
}

/// 跑选定的前端：`moc web`、`moc acp`、TUI（默认）或行式 REPL / print。
async fn run_frontend(
    core: Arc<Core>,
    args: CliArgs,
    frontend: crate::Frontend,
) -> anyhow::Result<()> {
    if frontend == crate::Frontend::Acp {
        return moc_acp::serve(
            core,
            moc_acp::AcpOptions {
                model: args.model.clone(),
                engine: args.engine.into(),
                plan: args.plan,
            },
        )
        .await;
    }
    if frontend == crate::Frontend::Web {
        let token = args
            .token
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().simple().to_string());
        let (port, fallback) = match args.port {
            Some(port) => (port, false),
            None => (moc_web::DEFAULT_PORT, true),
        };
        let open_browser = !args.no_open;
        return moc_web::run(
            core,
            moc_web::WebOptions {
                port,
                fallback,
                token,
                open_browser,
            },
        )
        .await;
    }
    if use_tui(&args) {
        moc_tui::run(core, tui_options(&args)).await
    } else {
        run_embedded(core, args).await
    }
}

/// 等 SIGTERM / SIGHUP 到来，返回信号名（清理由调用方负责）。
#[cfg(unix)]
fn termination_signal() -> impl std::future::Future<Output = &'static str> {
    use tokio::signal::unix::{SignalKind, signal};
    let terminate = signal(SignalKind::terminate());
    let hangup = signal(SignalKind::hangup());
    async move {
        match (terminate, hangup) {
            (Ok(mut terminate), Ok(mut hangup)) => tokio::select! {
                _ = terminate.recv() => "SIGTERM",
                _ = hangup.recv() => "SIGHUP",
            },
            // 注册失败（极罕见）：退回永不就绪，不改变正常退出路径
            _ => std::future::pending().await,
        }
    }
}

/// Windows 没有这两个信号：永不就绪，退出仍走交互路径的 Ctrl+C。
#[cfg(not(unix))]
fn termination_signal() -> impl std::future::Future<Output = &'static str> {
    std::future::pending()
}

/// 交互模式默认进 TUI；`--repl` 强制行式，非 TTY（管道 / CI）自动降级。
pub(crate) fn use_tui(args: &CliArgs) -> bool {
    if args.print || args.list || args.repl {
        return false;
    }
    use std::io::IsTerminal as _;
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// 把 CLI 参数翻译成 TUI 启动选项。
fn tui_options(args: &CliArgs) -> moc_tui::TuiOptions {
    let resume = if let Some(id) = &args.resume {
        Some(if id.is_empty() {
            None
        } else {
            Some(id.clone())
        })
    } else if args.continue_session {
        Some(None)
    } else {
        None
    };
    moc_tui::TuiOptions {
        cwd: args
            .cwd
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        model: args.model.clone(),
        engine: args.engine.into(),
        plan: args.plan,
        resume,
        initial_prompt: args.prompt.clone(),
        detail: args.detail.map(Into::into),
    }
}

fn make_core_config(args: &CliArgs) -> anyhow::Result<CoreConfig> {
    let cwd = match &args.cwd {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => std::env::current_dir()?.join(path),
        None => std::env::current_dir()?,
    };
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("无法打开工作目录 {}", cwd.display()))?;
    let worker = std::env::current_exe().context("无法定位 moc 可执行文件")?;
    Ok(CoreConfig {
        cwd: cwd.clone(),
        overlays: args
            .config_files
            .iter()
            .map(|path| absolute_path(path, &cwd))
            .collect(),
        session_dir: args
            .session_dir
            .as_ref()
            .map(|path| absolute_path(path, &cwd)),
        ptc_worker: Some(PtcWorkerCommand {
            program: worker,
            args: vec!["--ptc-worker".into()],
        }),
        ..CoreConfig::default()
    })
}

fn absolute_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

async fn run_embedded(core: Arc<Core>, args: CliArgs) -> anyhow::Result<()> {
    let input = input_reader();
    let mut initial_prompt = args.prompt.clone();
    if args.print && initial_prompt.is_none() {
        let mut prompt = String::new();
        loop {
            tokio::select! {
                signal = tokio::signal::ctrl_c() => { signal?; return Ok(()); }
                line = read_line(&input) => match line? {
                    Some(line) => { prompt.push_str(&line); prompt.push('\n'); }
                    None => break,
                }
            }
        }
        if prompt.is_empty() {
            bail!("--print 未收到提示词或 stdin 输入");
        }
        initial_prompt = Some(prompt);
    }

    let (core_side, frontend_side) = transport::duplex(64 * 1024);
    let server = Server::with_options((*core).clone(), ServerOptions::default());
    let connection = server.spawn(core_side);
    let renderer = Renderer::new(args.print, args.hide_thinking);
    let update_renderer = renderer.clone();
    let permission_input = Arc::clone(&input);
    let ask_input = Arc::clone(&input);
    // 通知回调在 client 建立前注册；槽位在握手完成后填充。
    let notification_client = Arc::new(std::sync::Mutex::new(Option::<ClientHandle>::None));
    let update_client = Arc::clone(&notification_client);
    let builder = ClientBuilder::new(ClientInfo {
        name: "moc-cli".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        frontend: "tui".into(),
    })
    .capabilities(ClientCapabilities {
        permission_requests: !args.print,
        ask_requests: !args.print,
    })
    .on_notification::<SessionUpdateMethod, _, _>(move |params, _ctx| {
        // unattended：计划提交后直接自动批准「压缩并执行」，不等 /approve。
        if let SessionUpdate::PlanSubmitted { plan } = &params.update
            && let Some(client) = update_client.lock().expect("通知 client 槽位").clone()
        {
            let session_id = params.session_id.clone();
            let plan_id = plan.plan_id.clone();
            tokio::spawn(async move {
                let unattended = client
                    .call::<SessionLoad>(SessionLoadParams {
                        session_id: session_id.clone(),
                        max_entries: Some(0),
                        read_only: true,
                    })
                    .await
                    .map(|loaded| loaded.session.autonomy == SessionAutonomy::Unattended)
                    .unwrap_or(false);
                if unattended {
                    let _ = client
                        .call::<SessionPlanApprove>(SessionPlanApproveParams {
                            session_id,
                            plan_id,
                            action: PlanAction::ExecuteCompacted,
                            execution_model: None,
                            execution_engine: None,
                        })
                        .await;
                }
            });
        }
        update_renderer.handle(params);
        std::future::ready(())
    })
    .on_request::<SessionRequestPermission, _, _>(move |params, ctx| {
        let input = Arc::clone(&permission_input);
        async move {
            tokio::select! {
                _ = ctx.cancel.cancelled() => Err(RpcError::of(RpcErrorCode::RequestCancelled)),
                _ = tokio::signal::ctrl_c() => Err(RpcError::of(RpcErrorCode::RequestCancelled)),
                result = permission_prompt(input, params) => result,
            }
        }
    })
    .on_request::<SessionRequestAsk, _, _>(move |params, ctx| {
        let input = Arc::clone(&ask_input);
        async move {
            tokio::select! {
                _ = ctx.cancel.cancelled() => Err(RpcError::of(RpcErrorCode::RequestCancelled)),
                _ = tokio::signal::ctrl_c() => Err(RpcError::of(RpcErrorCode::RequestCancelled)),
                result = ask_prompt(input, params) => result,
            }
        }
    });

    let (client, handshake) = match builder.connect(frontend_side).await {
        Ok(connection) => connection,
        Err(error) => {
            server.shutdown();
            connection.wait().await;
            return Err(error.into());
        }
    };
    if handshake.server.name != "moc-core" {
        bail!("连接到非 moc-core 服务：{}", handshake.server.name);
    }
    *notification_client.lock().expect("通知 client 槽位") = Some(client.clone());
    let result = run_repl(&client, &input, &args, initial_prompt, &renderer).await;
    client.close().await;
    server.shutdown();
    connection.wait().await;
    result
}

async fn run_repl(
    client: &ClientHandle,
    input: &Input,
    args: &CliArgs,
    initial_prompt: Option<String>,
    renderer: &Renderer,
) -> anyhow::Result<()> {
    if args.list {
        let result = client.call::<SessionList>(SessionListParams {}).await?;
        print_sessions(&result.sessions);
        return Ok(());
    }

    let cwd = make_core_config(args)?.cwd;
    let mut session = if let Some(id) = args.resume.as_ref().filter(|id| !id.is_empty()) {
        client
            .call::<SessionLoad>(SessionLoadParams {
                session_id: id.clone(),
                max_entries: None,
                read_only: false,
            })
            .await?
            .session
    } else if args.resume.is_some() || args.continue_session {
        let sessions = client
            .call::<SessionList>(SessionListParams {})
            .await?
            .sessions;
        sessions
            .into_iter()
            .filter(|session| session.cwd == cwd.to_string_lossy())
            .max_by_key(|session| session.updated_at_ms)
            .ok_or_else(|| anyhow::anyhow!("当前目录没有可恢复的会话"))?
    } else {
        client
            .call::<SessionNew>(SessionNewParams {
                cwd: Some(cwd.to_string_lossy().into_owned()),
                model: args.model.clone(),
                title: None,
                engine: args.engine.into(),
                plan_active: args.plan,
                fast_mode: false,
            })
            .await?
    };
    renderer.bind_workflow(client.clone(), session.id.clone(), input.clone());
    if let Some(selector) = &args.model
        && selector != &session.model
    {
        let result = client
            .call::<SessionSetModel>(SessionSetModelParams {
                session_id: session.id.clone(),
                selector: selector.clone(),
                overflow: SessionModelOverflowAction::Switch,
            })
            .await?;
        session = result.session;
        if let Some(overflow) = &result.overflow {
            eprintln!(
                "警告：{}；可用 /compact 立即压缩",
                capacity_notice(overflow, selector)
            );
        }
    }
    if let Some(prompt) = initial_prompt {
        match send_prompt(client, &session.id, prompt, None).await {
            Ok(result) if args.print => {
                println!("{}", result.message.text());
                if result.finish_reason.is_guard_stop() {
                    bail!("提示已由保护机制终止（{}）", result.finish_reason);
                }
                return Ok(());
            }
            Err(error) if args.print => return Err(error),
            Err(error) => eprintln!("请求失败：{error:#}"),
            Ok(_) => {}
        }
    }
    if args.print {
        return Ok(());
    }

    println!(
        "会话 {} · {} · {} · {} · permission={}",
        session.id,
        session.model,
        state_label(&session),
        session.cwd,
        session.permission.as_str()
    );
    // 特性快照与 TUI 同一来源：config/get 返回的 FeatureStates；拉取失败按默认处理。
    let registry = crate::commands::registry();
    let features = client
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .map(|config| config.features)
        .unwrap_or_else(|_| FeatureStates::defaults());
    loop {
        print!("\n> ");
        std::io::stdout().flush()?;
        let line = tokio::select! {
            signal = tokio::signal::ctrl_c() => { signal?; break; }
            line = read_line(input) => line?,
        };
        let Some(line) = line else {
            break;
        };
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            continue;
        }
        if let Some(command) = moc_protocol::BashCommand::parse(line) {
            if let Err(error) = send_bash(client, &session.id, command).await {
                eprintln!("命令失败：{error:#}");
            }
            continue;
        }
        if let Some(command) = line.strip_prefix('/') {
            match run_command(client, &registry, &features, command, &mut session, input).await {
                Ok(CommandResult::Continue) => continue,
                Ok(CommandResult::Quit) => break,
                Err(error) => eprintln!("命令失败：{error:#}"),
            }
        } else {
            if let Err(error) = send_prompt(client, &session.id, line.to_owned(), None).await {
                eprintln!("请求失败：{error:#}");
            }
        }
    }
    Ok(())
}

async fn run_command(
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
                    model: Some(session.model.clone()),
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
async fn run_plugin_command(
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

async fn print_plugins(client: &ClientHandle, session_id: &str) -> anyhow::Result<()> {
    let list = client
        .call::<PluginList>(PluginListParams {
            session_id: session_id.to_owned(),
        })
        .await?;
    print_plugin_list(&list.plugins);
    Ok(())
}

/// `/plugins reload`：丢弃插件宿主后重新发现并加载，再打印新清单。
async fn reload_plugins(client: &ClientHandle, session_id: &str) -> anyhow::Result<()> {
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
fn print_plugin_list(plugins: &[PluginInfo]) {
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

fn print_unknown_command(name: &str) {
    eprintln!("未知命令 `/{name}`；输入 /help 查看命令。");
}

async fn approve_plan(
    client: &ClientHandle,
    command: &str,
    session: &mut SessionInfo,
) -> anyhow::Result<()> {
    let load = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id.clone(),
            max_entries: None,
            read_only: false,
        })
        .await?;
    let plan = load
        .plans
        .iter()
        .rev()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .ok_or_else(|| anyhow::anyhow!("当前会话没有等待批准的计划"))?;
    // `new` 可带目标引擎：`/approve new ptc`；缺省沿用当前会话引擎。
    let (verb, engine_arg) = command
        .split_once(' ')
        .map_or((command, ""), |(v, rest)| (v, rest.trim()));
    let (action, execution_engine) = match verb {
        "execute" if engine_arg.is_empty() => (PlanAction::Execute, None),
        "compact" if engine_arg.is_empty() => (PlanAction::ExecuteCompacted, None),
        "new" => (
            PlanAction::ExecuteNewSession,
            if engine_arg.is_empty() {
                None
            } else {
                Some(parse_engine(engine_arg)?)
            },
        ),
        "reject" if engine_arg.is_empty() => (PlanAction::Reject, None),
        _ => bail!("用法：/approve <execute|compact|new [引擎]|reject>"),
    };
    let approved = client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: session.id.clone(),
            plan_id: plan.plan_id.clone(),
            action,
            execution_model: None,
            execution_engine,
        })
        .await?;
    let next = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: approved.session_id,
            max_entries: None,
            read_only: false,
        })
        .await?;
    *session = next.session;
    if let Some(markdown) = approved.plan_markdown {
        println!("\n{markdown}\n");
    }
    if let Some(prompt) = approved.auto_prompt {
        send_prompt(client, &session.id, prompt, None).await?;
    }
    Ok(())
}

async fn send_prompt(
    client: &ClientHandle,
    session_id: &str,
    prompt: String,
    mode: Option<PromptMode>,
) -> anyhow::Result<moc_protocol::methods::PromptResult> {
    let pending = client.call::<SessionPrompt>(moc_protocol::methods::PromptParams {
        session_id: session_id.to_owned(),
        input: vec![InputPart::Text { text: prompt }],
        mode,
        agent_message: None,
        goal_round: None,
    });
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => result.map_err(Into::into),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            client.call::<SessionCancel>(SessionCancelParams { session_id: session_id.to_owned() }).await?;
            pending.await.map_err(Into::into)
        }
    }
}

/// `/retry`：接着现有上下文继续跑，不追加用户消息。
///
/// 上一轮没跑完（取消、上游中断）时，core 会先把那半截回合从会话里剪掉再继续，
/// 所以这里只管发请求、等它结束。
async fn send_retry(client: &ClientHandle, session_id: &str) -> anyhow::Result<()> {
    let pending = client.call::<SessionRetry>(SessionRetryParams {
        session_id: session_id.to_owned(),
    });
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => result.map(|_| ()).map_err(Into::into),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            client.call::<SessionCancel>(SessionCancelParams { session_id: session_id.to_owned() }).await?;
            pending.await.map(|_| ()).map_err(Into::into)
        }
    }
}

async fn send_bash(
    client: &ClientHandle,
    session_id: &str,
    command: moc_protocol::BashCommand,
) -> anyhow::Result<()> {
    let pending = client.call::<moc_protocol::SessionBash>(moc_protocol::BashParams {
        session_id: session_id.to_owned(),
        command,
    });
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => result.map(|_| ()).map_err(Into::into),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            client.call::<SessionCancel>(SessionCancelParams { session_id: session_id.to_owned() }).await?;
            pending.await.map(|_| ()).map_err(Into::into)
        }
    }
}

async fn permission_prompt(
    input: Input,
    params: PermissionRequestParams,
) -> Result<PermissionResult, RpcError> {
    println!("\n{}", params.title);
    if let Some(body) = params.body {
        println!("{body}");
    }
    for (index, option) in params.options.iter().enumerate() {
        println!("  {}. {}", index + 1, option.label);
    }
    print!("选择 [默认拒绝]：");
    let _ = std::io::stdout().flush();
    let line = read_line(&input)
        .await
        .map_err(rpc_io_error)?
        .unwrap_or_default();
    let option = line
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| params.options.get(index))
        .or_else(|| {
            params
                .options
                .iter()
                .find(|option| option.id == "reject_once")
        })
        .ok_or_else(|| RpcError::of(RpcErrorCode::InvalidParams))?;
    Ok(PermissionResult {
        option_id: option.id.clone(),
    })
}

async fn ask_prompt(
    input: Input,
    params: moc_protocol::methods::AskParams,
) -> Result<AskResult, RpcError> {
    let mut answers = Vec::with_capacity(params.questions.len());
    for question in params.questions {
        println!(
            "\n{}",
            question.header.as_deref().unwrap_or(&question.question)
        );
        if question.header.is_some() {
            println!("{}", question.question);
        }
        for (index, option) in question.options.iter().enumerate() {
            let recommended = question.recommended.contains(&(index as u32));
            println!(
                "  {}. {}{}",
                index + 1,
                option.label,
                if recommended { " (推荐)" } else { "" }
            );
            if let Some(description) = &option.description {
                println!("     {description}");
            }
            if let Some(preview) = &option.preview {
                // CLI 不渲染 markdown：预览按原样缩进打印，保住 ASCII 示意图的对齐。
                for row in preview.lines() {
                    println!("     {row}");
                }
            }
        }
        print!(
            "输入编号{}或自定义内容：",
            if question.multi {
                "（可用逗号多选）"
            } else {
                ""
            }
        );
        let _ = std::io::stdout().flush();
        let line = read_line(&input)
            .await
            .map_err(rpc_io_error)?
            .unwrap_or_default();
        let raw = line.trim();
        let mut selected = Vec::new();
        let mut custom = None;
        if raw.is_empty() {
            for option in question
                .recommended
                .iter()
                .filter_map(|&index| question.options.get(index as usize))
            {
                selected.push(option.label.clone());
            }
        } else {
            let choices: Vec<_> = if question.multi {
                raw.split(',').map(str::trim).collect()
            } else {
                vec![raw]
            };
            for choice in &choices {
                if let Ok(index) = choice.parse::<usize>()
                    && let Some(option) = index
                        .checked_sub(1)
                        .and_then(|index| question.options.get(index))
                {
                    selected.push(option.label.clone());
                } else {
                    custom = Some(raw.to_owned());
                    selected.clear();
                    break;
                }
            }
        }
        answers.push(AskAnswer {
            id: question.id,
            selected,
            custom,
        });
    }
    Ok(AskResult { answers })
}

async fn read_line(input: &Input) -> std::io::Result<Option<String>> {
    input.lock().await.recv().await.transpose()
}

fn rpc_io_error(error: std::io::Error) -> RpcError {
    RpcError::new(RpcErrorCode::InternalError.code(), error.to_string())
}

fn parse_engine(engine: &str) -> anyhow::Result<SessionEngine> {
    match engine {
        "standard" => Ok(SessionEngine::Standard),
        "ptc" => Ok(SessionEngine::Ptc),
        "clm" | "clm-experimental" => Ok(SessionEngine::ClmExperimental),
        _ => bail!("引擎必须是 standard、ptc 或 clm；计划状态用 /plan，单条只读问答用 /ask <问题>"),
    }
}

/// 状态栏标签：引擎 + plan 模式 + 非默认自主程度。
fn state_label(session: &SessionInfo) -> String {
    let engine = match session.engine {
        SessionEngine::Standard => "standard",
        SessionEngine::Ptc => "ptc",
        SessionEngine::ClmExperimental => "(clm)",
    };
    let mut label = engine.to_owned();
    if session.plan_active {
        label.push_str("+plan");
    }
    match session.autonomy {
        SessionAutonomy::Default => {}
        SessionAutonomy::DontAsk => label.push_str("+dont_ask"),
        SessionAutonomy::Unattended => label.push_str("+unattended"),
    }
    label
}

/// 容量缺口的中文描述；token 按原值输出，与 REPL 其它统计一致。
fn capacity_notice(overflow: &ModelSwitchOverflow, selector: &str) -> String {
    if overflow.tokens > overflow.context_window {
        format!(
            "当前上下文 {} token 超过 {} 的窗口 {} token",
            overflow.tokens, selector, overflow.context_window
        )
    } else {
        format!(
            "当前上下文 {} token 超过 {} 的压缩触发线 {} token（窗口 {} token）",
            overflow.tokens, selector, overflow.threshold, overflow.context_window
        )
    }
}

fn print_sessions(sessions: &[SessionInfo]) {
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            session.id,
            state_label(session),
            session.model,
            session.permission.as_str(),
            session.title,
            session.cwd
        );
    }
}

fn print_usage(result: &moc_protocol::SessionUsageResult) {
    for summary in result.models.iter().chain(std::iter::once(&result.total)) {
        let cache = summary.cached_input_tokens.unwrap_or(0);
        let cache_rate = if summary.input_tokens == 0 {
            0.0
        } else {
            cache as f64 * 100.0 / summary.input_tokens as f64
        };
        let reasoning = summary
            .reasoning_tokens
            .map_or_else(|| "未知".to_owned(), |tokens| tokens.to_string());
        let split = if summary.gen_ms > 0 {
            format!(
                "（生成 {}，网络等待 {}）",
                format_duration(summary.gen_ms),
                format_duration(summary.active_ms.saturating_sub(summary.gen_ms))
            )
        } else {
            String::new()
        };
        let ttft = average_ttft(summary).map_or_else(|| "—".to_owned(), format_duration);
        let tps = usage_tps(summary).map_or_else(|| "—".to_owned(), |tps| format!("{tps:.2}"));
        // 后代会话的用量行标出来源：Fusion sidekick 与普通子代理。
        let model_label = match summary.source {
            moc_protocol::methods::UsageSource::Sidekick => {
                format!("{} (sidekick)", summary.model)
            }
            moc_protocol::methods::UsageSource::Subagent => format!("{} (子代理)", summary.model),
            moc_protocol::methods::UsageSource::Session => summary.model.clone(),
        };
        println!(
            "模型 {}：调用 {} 次，输入 {}，输出 {}，缓存命中 {} ({cache_rate:.1}%)，缓存写入 {}，思考 {}，用时 {}{split}，首 token 平均 {ttft}，TPS {tps}",
            model_label,
            summary.calls,
            summary.input_tokens,
            summary.output_tokens,
            cache,
            summary.cache_creation_input_tokens.unwrap_or(0),
            reasoning,
            format_duration(summary.active_ms),
        );
    }
    println!("工具调用用时 {}", format_duration(result.tool_ms));
}

fn average_ttft(summary: &moc_protocol::ModelUsageSummary) -> Option<u64> {
    (summary.ttft_calls > 0).then(|| summary.ttft_ms / summary.ttft_calls)
}

/// 输出速率：优先用生成阶段耗时；没有可拆分数据（旧会话）时退回整段用时。
fn usage_tps(summary: &moc_protocol::ModelUsageSummary) -> Option<f64> {
    if summary.output_tokens == 0 {
        return None;
    }
    let ms = if summary.gen_ms > 0 {
        summary.gen_ms
    } else if summary.ttft_calls == 0 {
        summary.active_ms
    } else {
        return None;
    };
    (ms > 0).then(|| summary.output_tokens as f64 * 1000.0 / ms as f64)
}

fn format_duration(active_ms: u64) -> String {
    if active_ms < 1_000 {
        format!("{active_ms} ms")
    } else {
        format!("{:.2} s", active_ms as f64 / 1000.0)
    }
}

/// `/goal` 回执的渲染文本：目标原文、相位、已跑轮次与续跑状态。
fn render_goal(goal: Option<&moc_protocol::methods::GoalView>) -> String {
    let Some(goal) = goal else {
        return "当前没有目标；/goal <objective> 创建\n".to_owned();
    };
    let phase = match goal.phase {
        moc_protocol::methods::GoalPhase::Active => "进行中",
        moc_protocol::methods::GoalPhase::Paused => "已暂停",
        moc_protocol::methods::GoalPhase::Blocked => "已阻塞",
        moc_protocol::methods::GoalPhase::Complete => "已完成",
    };
    let mut text = format!("目标：{}\n状态：{phase}\n", goal.objective);
    if let Some(reason) = &goal.blocked_reason {
        text.push_str(&format!("阻塞：{}: {}\n", reason.code, reason.message));
    }
    text.push_str(&format!(
        "轮次：{}\n续跑：{}\n命令：/goal edit <objective> | /goal pause | /goal resume | /goal clear\n",
        goal.rounds_started,
        if goal.armed {
            "自动"
        } else {
            "已停（/goal resume 恢复）"
        }
    ));
    text
}

enum CommandResult {
    Continue,
    Quit,
}

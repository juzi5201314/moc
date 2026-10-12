use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use moc_core::{Core, CoreConfig, resolve_models};
use moc_protocol::methods::{
    AskAnswer, AskResult, ClientCapabilities, ClientInfo, InputPart, ModelSwitchOverflow,
    PermissionRequestParams, PermissionResult, PlanAction, PlanStatus, SessionCancel,
    SessionCancelParams, SessionCompact, SessionCompactParams, SessionInfo, SessionList,
    SessionListParams, SessionLoad, SessionLoadParams, SessionMode, SessionModelOverflowAction,
    SessionNew, SessionNewParams, SessionPermission, SessionPlanApprove, SessionPlanApproveParams,
    SessionPrompt, SessionRequestAsk, SessionRequestPermission, SessionSetMode,
    SessionSetModeParams, SessionSetModel, SessionSetModelParams, SessionSetPermission,
    SessionSetPermissionParams, SessionUpdateMethod, SessionUsage, SessionUsageParams,
};
use moc_protocol::{
    ClientBuilder, ClientHandle, RpcError, RpcErrorCode, Server, ServerOptions, transport,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::args::CliArgs;
use crate::render::Renderer;

type Input = Arc<AsyncMutex<tokio::sync::mpsc::Receiver<std::io::Result<String>>>>;

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

pub async fn run(args: CliArgs) -> anyhow::Result<()> {
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
    let result = if use_tui(&args) {
        moc_tui::run(core.clone(), tui_options(&args)).await
    } else {
        run_embedded(core.clone(), args).await
    };
    core.shutdown().await;
    result
}

/// 交互模式默认进 TUI；`--repl` 强制行式，非 TTY（管道 / CI）自动降级。
fn use_tui(args: &CliArgs) -> bool {
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
        mode: args.mode.into(),
        resume,
        initial_prompt: args.prompt.clone(),
        hide_thinking: args.hide_thinking,
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
    let result = run_repl(&client, &input, &args, initial_prompt).await;
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
                mode: args.mode.into(),
            })
            .await?
    };
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
    let mut mode = session.mode;

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
        "会话 {} · {} · {:?} · {} · permission={}",
        session.id,
        session.model,
        mode,
        session.cwd,
        session.permission.as_str()
    );
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
            match run_command(client, command, &mut session, &mut mode, input).await {
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
    command: &str,
    session: &mut SessionInfo,
    mode: &mut SessionMode,
    input: &Input,
) -> anyhow::Result<CommandResult> {
    let (name, argument) = command.split_once(' ').unwrap_or((command, ""));
    let argument = argument.trim();
    match name {
        "quit" | "exit" => return Ok(CommandResult::Quit),
        "cancel" => {
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
        "help" => print_help(),
        "new" => {
            *session = client
                .call::<SessionNew>(SessionNewParams {
                    cwd: Some(session.cwd.clone()),
                    model: Some(session.model.clone()),
                    title: None,
                    mode: *mode,
                })
                .await?;
            println!("新会话：{}", session.id);
        }
        "sessions" => {
            let result = client.call::<SessionList>(SessionListParams {}).await?;
            print_sessions(&result.sessions);
        }
        "mode" => {
            let next = parse_mode(argument)?;
            *session = client
                .call::<SessionSetMode>(SessionSetModeParams {
                    session_id: session.id.clone(),
                    mode: next,
                })
                .await?;
            *mode = next;
            if next == SessionMode::ClmExperimental {
                println!("CLM 实验模式已启用；本会话首次请求后不可退出 CLM");
            }
        }
        "clm-experimental" => {
            *session = client
                .call::<SessionSetMode>(SessionSetModeParams {
                    session_id: session.id.clone(),
                    mode: SessionMode::ClmExperimental,
                })
                .await?;
            *mode = SessionMode::ClmExperimental;
            println!("CLM 实验模式已启用；本会话首次请求后不可退出 CLM");
        }
        "permission" => {
            let permission = match argument {
                "full" => SessionPermission::Full,
                "workspace" => SessionPermission::Workspace,
                _ => bail!("用法：/permission <full|workspace>"),
            };
            *session = client
                .call::<SessionSetPermission>(SessionSetPermissionParams {
                    session_id: session.id.clone(),
                    permission,
                })
                .await?;
            println!("权限已切换为 {}", session.permission.as_str());
        }
        "plan" => {
            if argument.is_empty() {
                bail!("用法：/plan <提示词>");
            }
            *session = client
                .call::<SessionSetMode>(SessionSetModeParams {
                    session_id: session.id.clone(),
                    mode: SessionMode::Plan,
                })
                .await?;
            *mode = SessionMode::Plan;
            send_prompt(client, &session.id, argument.to_owned(), None).await?;
        }
        "ask" => {
            if argument.is_empty() {
                bail!("用法：/ask <问题>");
            }
            send_prompt(
                client,
                &session.id,
                argument.to_owned(),
                Some(SessionMode::Ask),
            )
            .await?;
        }
        "approve" => approve_plan(client, argument, session, mode).await?,
        "compact" => {
            let result = client
                .call::<SessionCompact>(SessionCompactParams {
                    session_id: session.id.clone(),
                    instructions: None,
                })
                .await?;
            println!(
                "上下文压缩：{}，{} → {} token",
                result.method, result.tokens_before, result.tokens_after
            );
        }
        "usage" => {
            let result = client
                .call::<SessionUsage>(SessionUsageParams {
                    session_id: session.id.clone(),
                })
                .await?;
            print_usage(&result);
        }
        "model" => {
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
        "agents" => {
            let result = client
                .call::<moc_protocol::methods::AgentList>(moc_protocol::methods::AgentListParams {
                    parent_session_id: session.id.clone(),
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
        "agent" => {
            let mut parts = argument.splitn(2, ' ');
            let model = parts.next().unwrap_or("inherit");
            let task = parts.next().unwrap_or_default().trim();
            if task.is_empty() {
                bail!("用法：/agent [inherit|default|explore|plan] <任务>");
            }
            let result = client
                .call::<moc_protocol::methods::AgentSpawn>(
                    moc_protocol::methods::AgentSpawnParams {
                        parent_session_id: session.id.clone(),
                        task: task.into(),
                        title: None,
                        model: (model != "inherit").then(|| model.into()),
                    },
                )
                .await?;
            println!("子代理已创建：{}", result.agent.id);
        }
        "steer-agent" => {
            let (agent_id, message) = argument
                .split_once(' ')
                .ok_or_else(|| anyhow::anyhow!("用法：/steer-agent <id> <消息>"))?;
            let result = client
                .call::<moc_protocol::methods::AgentSteer>(
                    moc_protocol::methods::AgentSteerParams {
                        parent_session_id: session.id.clone(),
                        agent_id: agent_id.into(),
                        message: message.trim().into(),
                    },
                )
                .await?;
            println!("steer 已接受：{}", result.accepted);
        }
        "stop-agent" => {
            if argument.is_empty() {
                bail!("用法：/stop-agent <id>");
            }
            let result = client
                .call::<moc_protocol::methods::AgentStop>(moc_protocol::methods::AgentStopParams {
                    parent_session_id: session.id.clone(),
                    agent_id: argument.into(),
                    reason: None,
                })
                .await?;
            println!("停止请求已处理：{}", result.stopped);
        }
        "" => {}
        _ => eprintln!("未知命令 `/{name}`；输入 /help 查看命令。"),
    }
    Ok(CommandResult::Continue)
}

async fn approve_plan(
    client: &ClientHandle,
    command: &str,
    session: &mut SessionInfo,
    mode: &mut SessionMode,
) -> anyhow::Result<()> {
    let load = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id.clone(),
            max_entries: None,
        })
        .await?;
    let plan = load
        .plans
        .iter()
        .rev()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .ok_or_else(|| anyhow::anyhow!("当前会话没有等待批准的计划"))?;
    let action = match command {
        "execute" => PlanAction::Execute,
        "compact" => PlanAction::ExecuteCompacted,
        "new" => PlanAction::ExecuteNewSession,
        "reject" => PlanAction::Reject,
        _ => bail!("用法：/approve <execute|compact|new|reject>"),
    };
    let approved = client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: session.id.clone(),
            plan_id: plan.plan_id.clone(),
            action,
            execution_model: None,
        })
        .await?;
    let next = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: approved.session_id,
            max_entries: None,
        })
        .await?;
    *session = next.session;
    *mode = approved.mode;
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
    mode: Option<SessionMode>,
) -> anyhow::Result<moc_protocol::methods::PromptResult> {
    let pending = client.call::<SessionPrompt>(moc_protocol::methods::PromptParams {
        session_id: session_id.to_owned(),
        input: vec![InputPart::Text { text: prompt }],
        mode,
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
            let recommended = question.recommended == Some(index as u32);
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
            if let Some(index) = question
                .recommended
                .and_then(|index| question.options.get(index as usize))
            {
                selected.push(index.label.clone());
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

fn parse_mode(mode: &str) -> anyhow::Result<SessionMode> {
    match mode {
        "normal" => Ok(SessionMode::Normal),
        "plan" => Ok(SessionMode::Plan),
        "ptc" => Ok(SessionMode::Ptc),
        "clm" | "clm-experimental" => Ok(SessionMode::ClmExperimental),
        _ => bail!(
            "模式必须是 normal、plan、ptc 或 clm-experimental；单条消息的问答请用 /ask <问题>"
        ),
    }
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
            "{}\t{:?}\t{}\t{}\t{}\t{}",
            session.id,
            session.mode,
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
        println!(
            "模型 {}：调用 {} 次，输入 {}，输出 {}，缓存命中 {} ({cache_rate:.1}%)，缓存写入 {}，思考 {}，激活时间 {}，TPS {:.2}",
            summary.model,
            summary.calls,
            summary.input_tokens,
            summary.output_tokens,
            cache,
            summary.cache_creation_input_tokens.unwrap_or(0),
            reasoning,
            format_duration(summary.active_ms),
            tokens_per_second(summary.output_tokens, summary.active_ms),
        );
    }
}

fn tokens_per_second(output_tokens: u64, active_ms: u64) -> f64 {
    if active_ms == 0 {
        0.0
    } else {
        output_tokens as f64 * 1000.0 / active_ms as f64
    }
}

fn format_duration(active_ms: u64) -> String {
    if active_ms < 1_000 {
        format!("{active_ms} ms")
    } else {
        format!("{:.2} s", active_ms as f64 / 1000.0)
    }
}

fn print_help() {
    println!(
        "/mode <normal|plan|ptc|clm-experimental>  切换工作模式\n/clm-experimental          启用 CLM 实验模式；首次请求后本会话固定为 CLM\n/permission <full|workspace> 切换命令权限（workspace 需要 bwrap）\n/plan <提示词>            切换计划模式并发送提示词\n/ask <问题>               仅本条消息按问答模式（只读）处理\n/agents                  列出当前会话的直接子代理\n/agent <role> <任务>      创建子代理（role: inherit|default|explore|plan）\n/steer-agent <id> <消息>  补充运行中子代理\n/stop-agent <id>          停止运行中的子代理\n/approve <execute|compact|new|reject>\n/compact                 压缩当前上下文\n/usage                   查看当前会话模型用量\n/undo                    撤销最近一次编辑\n/new                     创建新会话\n/sessions                列出会话\n/model <selector>        切换模型（装不下当前上下文时先询问再压缩）\n/quit                    退出"
    );
    println!("/cancel                  取消当前提示（运行中按 Ctrl+C）");
}

enum CommandResult {
    Continue,
    Quit,
}

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use moc_core::{Core, CoreConfig, resolve_models};
use moc_protocol::methods::{
    AskAnswer, AskResult, ClientCapabilities, ClientInfo, InputPart, PermissionRequestParams,
    PermissionResult, PlanAction, PlanStatus, SessionCancel, SessionCancelParams, SessionCompact,
    SessionCompactParams, SessionInfo, SessionList, SessionListParams, SessionLoad,
    SessionLoadParams, SessionMode, SessionNew, SessionNewParams, SessionPlanApprove,
    SessionPlanApproveParams, SessionPrompt, SessionRequestAsk, SessionRequestPermission,
    SessionSetMode, SessionSetModeParams, SessionUpdateMethod,
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
    let result = run_embedded(core.clone(), args).await;
    core.shutdown().await;
    result
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
    let result = run_repl(&core, &client, &input, &args, initial_prompt).await;
    client.close().await;
    server.shutdown();
    connection.wait().await;
    result
}

async fn run_repl(
    core: &Core,
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
    if let Some(selector) = &args.model {
        session.model = core.set_model(&session.id, selector).await?;
    }
    let mut mode = session.mode;

    if let Some(prompt) = initial_prompt {
        match send_prompt(client, &session.id, prompt).await {
            Ok(result) if args.print => {
                println!("{}", result.message.text());
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
        "会话 {} · {} · {:?} · {}",
        session.id, session.model, mode, session.cwd
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
        if let Some(command) = line.strip_prefix('/') {
            match run_command(core, client, command, &mut session, &mut mode).await {
                Ok(CommandResult::Continue) => continue,
                Ok(CommandResult::Quit) => break,
                Err(error) => eprintln!("命令失败：{error:#}"),
            }
        } else {
            if let Err(error) = send_prompt(client, &session.id, line.to_owned()).await {
                eprintln!("请求失败：{error:#}");
            }
        }
    }
    Ok(())
}

async fn run_command(
    core: &Core,
    client: &ClientHandle,
    command: &str,
    session: &mut SessionInfo,
    mode: &mut SessionMode,
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
                    "轮次取消请求已发送"
                } else {
                    "当前没有在飞轮次"
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
            send_prompt(client, &session.id, argument.to_owned()).await?;
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
        "undo" => println!("{}", core.undo(&session.id).await?),
        "model" => {
            if argument.is_empty() {
                bail!("用法：/model <selector>");
            }
            session.model = core.set_model(&session.id, argument).await?;
            println!("模型已切换为 {}", session.model);
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
        send_prompt(client, &session.id, prompt).await?;
    }
    Ok(())
}

async fn send_prompt(
    client: &ClientHandle,
    session_id: &str,
    prompt: String,
) -> anyhow::Result<moc_protocol::methods::TurnResult> {
    let pending = client.call::<SessionPrompt>(moc_protocol::methods::PromptParams {
        session_id: session_id.to_owned(),
        input: vec![InputPart::Text { text: prompt }],
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
        "ask" => Ok(SessionMode::Ask),
        "plan" => Ok(SessionMode::Plan),
        _ => bail!("模式必须是 normal、ask 或 plan"),
    }
}

fn print_sessions(sessions: &[SessionInfo]) {
    for session in sessions {
        println!(
            "{}\t{:?}\t{}\t{}\t{}",
            session.id, session.mode, session.model, session.title, session.cwd
        );
    }
}

fn print_help() {
    println!(
        "/mode <normal|ask|plan>  切换工作模式\n/plan <提示词>            切换计划模式并发送提示词\n/approve <execute|compact|new|reject>\n/compact                 压缩当前上下文\n/undo                    撤销最近一次编辑\n/new                     创建新会话\n/sessions                列出会话\n/model <selector>        切换当前模型\n/quit                    退出"
    );
    println!("/cancel                  取消当前轮次（运行中按 Ctrl+C）");
}

enum CommandResult {
    Continue,
    Quit,
}

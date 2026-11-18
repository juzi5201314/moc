use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use moc_core::{Core, CoreConfig, PtcWorkerCommand, resolve_models};
use moc_protocol::features::FeatureStates;
use moc_protocol::methods::{
    ClientCapabilities, ClientInfo, ConfigGet, ConfigGetParams, PlanAction, SessionAutonomy,
    SessionInfo, SessionList, SessionListParams, SessionLoad, SessionLoadParams,
    SessionModelOverflowAction, SessionNew, SessionNewParams, SessionPlanApprove,
    SessionPlanApproveParams, SessionRequestAsk, SessionRequestPermission, SessionSetModel,
    SessionSetModelParams, SessionUpdate, SessionUpdateMethod,
};
use moc_protocol::{ClientBuilder, ClientHandle, RpcError, RpcErrorCode, ServerOptions};
use rust_i18n::t;
use tokio::sync::Mutex as AsyncMutex;

use crate::args::CliArgs;
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
    let core = Core::new(config).context(t!("cli.repl.core_init_failed").to_string())?;
    // SIGTERM / SIGHUP 不会走前端的正常退出路径：先把前端 future 收掉（TUI 的
    // TerminalGuard Drop 恢复终端、连接 Drop 触发收尾），再统一执行 core.shutdown()，
    // 保证 raw mode 与 MCP 子进程都不因信号退出而丢失。
    let result = tokio::select! {
        result = run_frontend(Arc::clone(&core), args, frontend) => result,
        signal = termination_signal() => {
            eprintln!("{}", t!("cli.repl.signal_exit", signal = signal));
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
        bad.push(t!("cli.repl.prompt_word").into_owned());
    }
    if args.resume.is_some() {
        bad.push("--resume".to_owned());
    }
    if args.continue_session {
        bad.push("--continue".to_owned());
    }
    if args.list {
        bad.push("--list".to_owned());
    }
    if args.models {
        bad.push("--models".to_owned());
    }
    if args.print {
        bad.push("--print".to_owned());
    }
    if args.repl {
        bad.push("--repl".to_owned());
    }
    if args.detail.is_some() {
        bad.push("--detail".to_owned());
    }
    if args.hide_thinking {
        bad.push("--hide-thinking".to_owned());
    }
    if args.port.is_some() || args.token.is_some() || args.no_open {
        bad.push(t!("cli.repl.web_opts").into_owned());
    }
    anyhow::ensure!(
        bad.is_empty(),
        "{}",
        t!(
            "cli.repl.acp_bad_args",
            args = bad.join(t!("cli.repl.arg_sep").as_ref())
        )
    );
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
        .with_context(|| t!("cli.repl.no_cwd", path = cwd.display().to_string()).into_owned())?;
    let worker = std::env::current_exe().context(t!("cli.repl.no_exe").into_owned())?;
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
            bail!("{}", t!("cli.repl.no_prompt"));
        }
        initial_prompt = Some(prompt);
    }

    let server = core.server(ServerOptions::default());
    let (frontend_side, connection) = server.connect(64 * 1024);
    let renderer = Renderer::new(
        args.print,
        args.hide_thinking,
        args.output_format,
        args.max_turns,
    );
    let update_renderer = renderer.clone();
    let permission_input = Arc::clone(&input);
    let ask_input = Arc::clone(&input);
    let allow_tools = args.allow_tools.clone();
    let print_mode = args.print;
    // 通知回调在 client 建立前注册；槽位在握手完成后填充。
    let notification_client = Arc::new(std::sync::Mutex::new(Option::<ClientHandle>::None));
    let update_client = Arc::clone(&notification_client);
    let builder = ClientBuilder::new(ClientInfo {
        name: "moc-cli".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        frontend: if args.print { "print" } else { "tui" }.into(),
    })
    .capabilities(ClientCapabilities {
        // print 默认弹不出权限询问（未注册处理器等效拒绝）；给了 --allow-tools
        // 就换成非交互的自动应答，让名单内的工具放行、其余照旧拒绝。
        permission_requests: !args.print || !args.allow_tools.is_empty(),
        ask_requests: !args.print,
    })
    .on_notification::<SessionUpdateMethod, _, _>(move |params, _ctx| {
        // unattended：计划提交后直接自动批准「压缩并执行」，不等 /approve。
        if let SessionUpdate::PlanSubmitted { plan } = &params.update
            && let Some(client) = update_client.lock().expect("notify client slot").clone()
        {
            let session_id = params.session_id.clone();
            let plan_id = plan.plan_id.clone();
            tokio::spawn(async move {
                let unattended = client
                    .call::<SessionLoad>(SessionLoadParams {
                        omit_auxiliary: false,
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
        let allow = allow_tools.clone();
        async move {
            if print_mode {
                return Ok(crate::print::auto_permission(&params, &allow));
            }
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
        bail!(
            "{}: {}",
            t!("cli.repl.not_moc_service"),
            handshake.server.name
        );
    }
    *notification_client.lock().expect("notify client slot") = Some(client.clone());
    let result = run_repl(&client, &input, &args, initial_prompt, &renderer).await;
    // 提示还没跑起来就失败（会话创建 / 模型切换等）时，json 类格式仍给 stdout 一行
    // 可解析的错误汇总；CliExit 的失败已在 finish_print 里输出过结果对象，不重复。
    let result = match result {
        Err(error)
            if args.print
                && args.output_format != crate::args::OutputFormat::Text
                && error.downcast_ref::<crate::CliExit>().is_none() =>
        {
            crate::print::emit_early_error(args.output_format, &error);
            Err(error)
        }
        other => other,
    };
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
                omit_auxiliary: false,
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
            .ok_or_else(|| anyhow::anyhow!("{}", t!("cli.repl.no_resumable")))?
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
                "{}",
                t!(
                    "cli.repl.ctx_warn",
                    msg = capacity_notice(overflow, selector)
                )
            );
        }
    }
    if let Some(prompt) = initial_prompt {
        let started = std::time::Instant::now();
        match send_prompt(client, &session.id, prompt, None).await {
            result if args.print => {
                return finish_print(args, renderer, &session, started, result);
            }
            Err(error) => eprintln!("{}: {error:#}", t!("cli.repl.request_failed")),
            Ok(_) => {}
        }
    }
    if args.print {
        return Ok(());
    }

    println!(
        "{}",
        t!(
            "cli.repl.banner",
            id = session.id,
            model = session.model,
            state = state_label(&session),
            cwd = session.cwd,
            permission = session.permission.as_str()
        )
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
                eprintln!("{}: {error:#}", t!("cli.repl.command_failed"));
            }
            continue;
        }
        // `.` 独占整行时是 `/retry` 的快捷写法。
        if line.trim() == "." {
            if let Err(error) = send_retry(client, &session.id).await {
                eprintln!("{}: {error:#}", t!("cli.repl.request_failed"));
            }
            continue;
        }
        if let Some(command) = line.strip_prefix('/') {
            match run_command(client, &registry, &features, command, &mut session, input).await {
                Ok(CommandResult::Continue) => continue,
                Ok(CommandResult::Quit) => break,
                Err(error) => eprintln!("{}: {error:#}", t!("cli.repl.command_failed")),
            }
        } else {
            if let Err(error) = send_prompt(client, &session.id, line.to_owned(), None).await {
                eprintln!("{}: {error:#}", t!("cli.repl.request_failed"));
            }
        }
    }
    Ok(())
}

/// `--print` 的收尾：按 `--output-format` 落最终输出，并把失败类别折成退出码。
///
/// text 保持旧行为（正文上 stdout、错误冒泡成退出码 1）；json / stream-json
/// 输出 [`crate::print::PrintResult`] 汇总对象；退出码 2/3 经 [`crate::CliExit`] 传出。
fn finish_print(
    args: &CliArgs,
    renderer: &Renderer,
    session: &SessionInfo,
    started: std::time::Instant,
    result: anyhow::Result<moc_protocol::methods::PromptResult>,
) -> anyhow::Result<()> {
    let report = renderer.report();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (code, error) = crate::print::classify(args.max_turns, &report, &result);
    if args.output_format == crate::args::OutputFormat::Text {
        if let Ok(result) = &result {
            println!("{}", result.message.text());
        }
        return match code {
            0 => Ok(()),
            // 一般错误维持旧路径：原文经 anyhow 冒泡，退出码 1。
            1 => match result {
                Err(error) => Err(error),
                Ok(_) => bail!("{}", error.unwrap_or_default()),
            },
            _ => Err(crate::CliExit {
                code,
                message: error.unwrap_or_default(),
            }
            .into()),
        };
    }
    let mut out = crate::print::build_result(&session.id, &report, duration_ms, &result);
    if out.error.is_none() {
        out.error = error;
    }
    out.emit(args.output_format);
    if code == 0 {
        return Ok(());
    }
    Err(crate::CliExit {
        code,
        message: out.error.unwrap_or_default(),
    }
    .into())
}

mod commands;
use commands::run_command;
mod interaction;
use interaction::ask_prompt;
use interaction::permission_prompt;
use interaction::read_line;
use interaction::send_bash;
use interaction::send_prompt;
use interaction::send_retry;
mod display;
use display::capacity_notice;
use display::print_sessions;
use display::state_label;

fn rpc_io_error(error: std::io::Error) -> RpcError {
    RpcError::new(RpcErrorCode::InternalError.code(), error.to_string())
}

enum CommandResult {
    Continue,
    Quit,
}

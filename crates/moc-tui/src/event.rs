//! 主事件循环：终端启动、会话启动、输入/协议事件分派与绘制节拍。
//!
//! 结构是「一个 `tokio::select!` 循环 + 无界 UI 队列」：协议通知入队即返回
//! （不阻塞协议读循环），输入线程独立，绘制按 16ms 帧预算节流（预算窗口内
//! 的事件合并为一帧），空闲时主循环完全 park。

use std::sync::Arc;

use anyhow::Context as _;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use moc_core::Core;
use moc_protocol::{
    ClientHandle, SessionInfo,
    methods::{
        ConfigGet, ConfigGetParams, InputPart, PromptParams, SessionCompact, SessionCompactParams,
        SessionList, SessionListParams, SessionLoad, SessionLoadParams, SessionMode, SessionNew,
        SessionNewParams, SessionPlanApprove, SessionPlanApproveParams, SessionSetMode,
        SessionSetModeParams, SessionSetModel, SessionSetModelParams, SessionUpdate,
    },
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::App;
use crate::input::{InputEvent, InputThread};
use crate::modal::Modal;
use crate::protocol::{AppEvent, EmbeddedConnection, connect_embedded};
use crate::render;
use crate::terminal::TerminalGuard;
use crate::theme::Theme;

/// TUI 启动参数（从 CLI 参数翻译而来）。
#[derive(Debug, Clone, Default)]
pub struct TuiOptions {
    /// 工作目录；`None` 用 core 当前目录。
    pub cwd: Option<String>,
    /// 模型 selector；`None` 用 core 默认。
    pub model: Option<String>,
    /// 会话工作模式。
    pub mode: SessionMode,
    /// `None` = 新会话；`Some(None)` = 恢复当前目录最近会话；`Some(Some(id))` = 指定会话。
    pub resume: Option<Option<String>>,
    /// 启动即发送的提示词（`moc "提示词"`）。
    pub initial_prompt: Option<String>,
}

/// 运行 TUI 直到退出。
pub async fn run(core: Arc<Core>, opts: TuiOptions) -> anyhow::Result<()> {
    init_logging();
    let theme = Theme::global();
    let mut guard = TerminalGuard::enter()?;

    let (events, mut event_rx) = unbounded_channel::<AppEvent>();
    let connection = connect_embedded(Arc::clone(&core), events.clone())
        .await
        .context("连接内嵌 core 失败")?;

    let mut app = App::new(theme, events.clone());
    bootstrap(&connection.client, &mut app, &opts).await;

    // `moc "提示词"`：会话就绪后立即发送首轮
    if let (Some(session), Some(text)) =
        (app.session.as_ref().map(|s| s.id.clone()), opts.initial_prompt.clone())
        && !text.trim().is_empty()
    {
        app.push_user(text.clone());
        send_turn(&connection.client, session, vec![InputPart::Text { text }]);
    }

    let mut input = InputThread::spawn();
    let result = event_loop(&mut guard, &mut app, &mut input, &mut event_rx, &connection).await;

    connection.shutdown().await;
    guard.restore();
    result
}

/// TUI 独占 stdout / stderr，日志只能落文件；初始化失败时静默（日志非关键路径）。
fn init_logging() {
    let home = std::env::var("MOC_HOME")
        .or_else(|_| std::env::var("HOME").map(|home| format!("{home}/.moc")))
        .unwrap_or_else(|_| "/tmp".into());
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{home}/tui.log"))
    else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Arc::new(file))
        .with_ansi(false)
        .with_target(true)
        .with_max_level(tracing::Level::DEBUG)
        .try_init();
    // alt screen 下 panic 消息不可见：同时落到日志
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "TUI panic");
        previous(info);
    }));
}

/// 拉取模型目录并建立/恢复初始会话；失败给出可见错误但继续运行（用户可退出）。
async fn bootstrap(client: &ClientHandle, app: &mut App, opts: &TuiOptions) {
    let outcome: anyhow::Result<SessionInfo> = async {
        let config = client.call::<ConfigGet>(ConfigGetParams {}).await?;
        app.models = config.models;
        app.statusline_template = config.statusline_template;
        let info = match &opts.resume {
            None => {
                client
                    .call::<SessionNew>(SessionNewParams {
                        cwd: opts.cwd.clone(),
                        model: opts.model.clone(),
                        title: None,
                        mode: opts.mode,
                    })
                    .await?
            }
            Some(session_id) => {
                let session_id = match session_id {
                    Some(id) => id.clone(),
                    None => {
                        let list = client.call::<SessionList>(SessionListParams {}).await?;
                        let cwd = opts.cwd.as_deref().unwrap_or_default();
                        list.sessions
                            .into_iter()
                            .find(|session| session.cwd == cwd)
                            .map(|session| session.id)
                            .context("当前目录没有可恢复的会话")?
                    }
                };
                client
                    .call::<SessionLoad>(SessionLoadParams {
                        session_id,
                        max_entries: None,
                    })
                    .await?
                    .session
            }
        };
        Ok(info)
    }
    .await;
    match outcome {
        Ok(info) => {
            refresh_git_branch(app, &info.cwd);
            app.session = Some(info);
        }
        Err(error) => app.set_notice(format!("会话启动失败：{error}")),
    }
}

/// 主循环。
///
/// 绘制节流：帧预算 16ms（~60fps）。事件到达时若已过预算立即绘制，否则
/// 等到预算点——同一预算窗口内的所有事件合并为一帧。
async fn event_loop(
    guard: &mut TerminalGuard,
    app: &mut App,
    input: &mut InputThread,
    event_rx: &mut UnboundedReceiver<AppEvent>,
    connection: &EmbeddedConnection,
) -> anyhow::Result<()> {
    const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(16);
    let mut view = render::ScrollbackView::new();
    let mut next_frame = tokio::time::Instant::now();
    let mut dirty = true;

    loop {
        if app.should_quit {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if dirty && now >= next_frame {
            let draw_result = guard
                .terminal_mut()
                .draw(|frame| render::draw(frame, app, &mut view));
            next_frame = tokio::time::Instant::now() + FRAME_BUDGET;
            dirty = false;
            draw_result?;
        }
        // 空闲（无 dirty）时完全 park 在事件上；dirty 但未到预算点才等待
        let need_frame_wait = dirty && tokio::time::Instant::now() < next_frame;
        // notice 显示中：安排 TTL 到点重绘，擦除 toast
        let notice_deadline = app.notice_visible().then(|| {
            tokio::time::Instant::from_std(app.notice_at.unwrap_or_else(std::time::Instant::now))
                + std::time::Duration::from_secs(3)
        });
        tokio::select! {
            biased;
            // 到达预算点：回循环顶部绘制（期间到达的事件已合并为一帧）
            _ = tokio::time::sleep_until(next_frame), if need_frame_wait => {}
            _ = tokio::time::sleep_until(
                    notice_deadline.unwrap_or_else(tokio::time::Instant::now),
                ), if notice_deadline.is_some() => {
                dirty = true;
            }
            input_event = input.recv() => {
                dirty = true;
                match input_event {
                    Some(InputEvent::Terminal(event)) => {
                        handle_terminal_event(app, connection, event);
                    }
                    // 输入线程结束（终端读取错误）：退出
                    None => return Ok(()),
                }
            }
            app_event = event_rx.recv() => {
                dirty = true;
                match app_event {
                    Some(AppEvent::Update(params)) => apply_update(app, params.update),
                    Some(AppEvent::ClipboardImage(image)) => match image {
                        Some((media_type, bytes)) => {
                            let encoded = crate::clipboard::base64_encode(&bytes);
                            app.prompt.insert_image(media_type, encoded);
                            app.set_notice("已附加剪贴板图片");
                        }
                        None => {
                            app.set_notice("剪贴板中没有图片（需要 wl-paste 或 xclip）");
                        }
                    },
                    Some(AppEvent::Permission { params, reply }) => {
                        app.modal = Some(Modal::Permission {
                            params,
                            selected: 0,
                            reply: Some(reply),
                        });
                        app.focus = crate::app::Focus::Prompt;
                    }
                    Some(AppEvent::Ask { params, reply }) => {
                        let per_question = params.questions.len();
                        // 先算推荐选择（借用 params），再整体 move 进模态
                        let highlight = vec![0; per_question];
                        let selected = params
                            .questions
                            .iter()
                            .map(|question| {
                                question
                                    .recommended
                                    .and_then(|index| question.options.get(index as usize))
                                    .map(|option| vec![option.label.clone()])
                                    .unwrap_or_default()
                            })
                            .collect();
                        app.modal = Some(Modal::Ask {
                            params,
                            question: 0,
                            highlight,
                            selected,
                            custom: String::new(),
                            editing_custom: false,
                            reply: Some(reply),
                        });
                        app.focus = crate::app::Focus::Prompt;
                    }
                    Some(AppEvent::Sessions(sessions)) => {
                        app.modal = Some(Modal::Sessions {
                            sessions,
                            selected: 0,
                        });
                    }
                    Some(AppEvent::Loaded(info, blocks)) => {
                        app.modal = None;
                        app.switch_session(info, blocks);
                    }
                    Some(AppEvent::ModelSwitched) => {
                        app.modal = None;
                    }
                    Some(AppEvent::GitBranch(branch)) => {
                        app.git_branch = Some(branch);
                    }
                    // 协议桥退出：退出 TUI
                    None => return Ok(()),
                }
            }
        }
    }
}

/// 终端事件分派（键盘 / 鼠标 / 粘贴 / resize）。
fn handle_terminal_event(app: &mut App, connection: &EmbeddedConnection, event: Event) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, connection, key.modifiers, key.code);
        }
        Event::Mouse(mouse) => handle_mouse(app, mouse),
        Event::Paste(text) => app.prompt.insert_text(&text),
        Event::Resize(_, _) => {} // 下一次 draw 自动按新尺寸重算
        _ => {}
    }
}

/// 鼠标：滚轮滚动、点击折叠、拖选复制。
fn handle_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) {
    use crossterm::event::{MouseButton, MouseEventKind};
    let position = (mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp => scroll(app, -3),
        MouseEventKind::ScrollDown => scroll(app, 3),
        MouseEventKind::Down(MouseButton::Left) => {
            app.pending_click = Some(position);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(start) = app.pending_click {
                if app.selection.is_none() {
                    app.selection = Some((start, position));
                } else if let Some((_, end)) = app.selection.as_mut() {
                    *end = position;
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let clicked = app.pending_click.take() == Some(position);
            if clicked {
                // 单击：命中折叠块标题则切换折叠
                if let Some(&(index, ..)) = app
                    .mouse_hits
                    .iter()
                    .find(|&&(_, start, end)| position.1 >= start && position.1 < end)
                    && let Some(entry) = app.blocks.get_mut(index)
                    && entry.block.foldable()
                {
                    entry.toggle_fold();
                }
                app.selection = None;
            }
            // 拖选结束：保留高亮，等待 y 复制
        }
        _ => {}
    }
}

fn scroll(app: &mut App, delta: i32) {
    if delta < 0 {
        app.scroll_offset = app.scroll_offset.saturating_sub(delta.unsigned_abs() as usize);
    } else {
        app.scroll_offset = app.scroll_offset.saturating_add(delta as usize);
    }
}

/// 键盘处理：模态优先，其次全局开关，最后按焦点路由。
fn handle_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    // 打开的模态吃掉全部键盘事件
    if app.modal.is_some() {
        handle_modal_key(app, connection, modifiers, code);
        return;
    }
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            if app.turn_running {
                // 第一次中断：取消轮次；再按一次退出
                if app.cancel_pending {
                    app.should_quit = true;
                } else {
                    app.cancel_pending = true;
                    app.set_notice("轮次取消中；再按一次 ctrl+c 退出");
                    cancel_turn(&connection.client, app);
                }
            } else {
                app.should_quit = true;
            }
        }
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            app.toggle_thinking();
            app.set_notice(if app.show_thinking {
                "思考内容：展开"
            } else {
                "思考内容：收起"
            });
        }
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
            app.toggle_tools();
            app.set_notice(if app.tools_collapsed {
                "工具调用：收起"
            } else {
                "工具调用：展开"
            });
        }
        (KeyCode::Char('v'), KeyModifiers::CONTROL) => {
            // 图片粘贴：读系统剪贴板（异步），文本粘贴走 bracketed paste
            spawn_clipboard_image(app.events.clone());
        }
        (KeyCode::Char('m'), KeyModifiers::CONTROL) => app.open_model_picker(),
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => open_session_picker(app, connection),
        (KeyCode::Char('y'), KeyModifiers::NONE) => copy_selection(app),
        (KeyCode::Esc, _) => {
            if app.selection.is_some() {
                app.selection = None;
            } else if !app.prompt_text().is_empty() {
                app.prompt.clear();
            }
        }
        // 终端把 shift+tab 报为 BackTab（ESC[Z）；kitty 协议关闭时 modifiers 为空
        (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::BackTab, _)
            if app.focus == crate::app::Focus::Prompt =>
        {
            // 输入框聚焦时 shift+tab 循环模式（codex/grok 语义）
            cycle_mode(app, connection);
        }
        (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
            app.focus = app.focus.toggle();
        }
        (KeyCode::Enter, KeyModifiers::ALT | KeyModifiers::SHIFT)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.insert_char('\n');
        }
        (KeyCode::Enter, KeyModifiers::NONE) if app.focus == crate::app::Focus::Prompt => {
            commit_prompt(app, connection);
        }
        (KeyCode::Up, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.up();
        }
        (KeyCode::Down, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.down();
        }
        (KeyCode::Backspace, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.backspace();
            sync_completion(app);
        }
        (KeyCode::Delete, _) if app.focus == crate::app::Focus::Prompt => app.prompt.delete(),
        (KeyCode::Left, _) if app.focus == crate::app::Focus::Prompt => app.prompt.left(),
        (KeyCode::Right, _) if app.focus == crate::app::Focus::Prompt => app.prompt.right(),
        (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
            app.prompt.move_line_start();
        }
        (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
            app.prompt.move_line_end();
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL) => app.prompt.clear_line_to_end(),
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => app.prompt.clear(),
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
            app.prompt.delete_word_back();
            sync_completion(app);
        }
        (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
            if app.focus == crate::app::Focus::Prompt && !ch.is_control() =>
        {
            app.prompt_insert_char(ch);
        }
        _ => {}
    }
}

fn sync_completion(app: &mut App) {
    app.completion = crate::widgets::completion::CompletionState::derive(app.prompt_text());
}

/// 提交输入：补全确认 > 斜杠命令 > 普通发送。
fn commit_prompt(app: &mut App, connection: &EmbeddedConnection) {
    if let Some(completion) = app.completion.take() {
        // 确认补全：整行替换为命令 + 空格
        let chosen = completion.chosen();
        app.prompt.clear();
        app.prompt.insert_str_raw(chosen);
        app.prompt.insert_char(' ');
        return;
    }
    let first_line = app.prompt_text().lines().next().unwrap_or_default();
    if first_line.starts_with('/') {
        let line = app.prompt.take_committed_text();
        execute_command(app, connection, line.trim().to_owned());
        return;
    }
    if app.prompt.is_empty() {
        return;
    }
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let parts = app.prompt.build_input_parts();
    app.push_user(display_text_of(&parts));
    send_turn(&connection.client, session_id, parts);
}

/// ctrl+r / `/resume`：拉取会话列表后打开选择器。
fn open_session_picker(app: &mut App, connection: &EmbeddedConnection) {
    let client = connection.client.clone();
    let events = app.events.clone();
    app.modal = Some(Modal::Sessions {
        sessions: Vec::new(),
        selected: 0,
    });
    tokio::spawn(async move {
        match client.call::<SessionList>(SessionListParams {}).await {
            Ok(list) => {
                let _ = events.send(AppEvent::Sessions(list.sessions));
            }
            Err(error) => tracing::warn!(%error, "会话列表拉取失败"),
        }
    });
}

/// 模态键盘路由。
///
/// `app.modal` 以 `as_mut` 借用；需要关闭模态或访问会话的分支通过
/// `close` 标志延后到借用结束后生效。
fn handle_modal_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    let events = app.events.clone();
    let mut close = false;
    let Some(modal) = app.modal.as_mut() else {
        return;
    };
    match modal {
        Modal::Permission {
            params,
            selected,
            reply,
        } => {
            let option_count = params.options.len();
            match (code, modifiers) {
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    *selected = selected.saturating_sub(1);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    *selected = (*selected + 1).min(option_count.saturating_sub(1));
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if let Some(option) = params.options.get(*selected)
                        && let Some(reply) = reply.take()
                    {
                        let _ = reply.send(Ok(moc_protocol::methods::PermissionResult {
                            option_id: option.id.clone(),
                        }));
                        close = true;
                        app.notice = None;
                    }
                }
                (KeyCode::Char(digit), KeyModifiers::NONE)
                    if digit.is_ascii_digit() && digit != '0' =>
                {
                    let index = digit.to_digit(10).expect("ascii digit") as usize - 1;
                    if let Some(option) = params.options.get(index)
                        && let Some(reply) = reply.take()
                    {
                        let _ = reply.send(Ok(moc_protocol::methods::PermissionResult {
                            option_id: option.id.clone(),
                        }));
                        close = true;
                    }
                }
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    // 搁置：core 侧按取消处理（记为未选择 = 拒绝）
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(Err(moc_protocol::RpcError::of(
                            moc_protocol::RpcErrorCode::RequestCancelled,
                        )));
                    }
                    close = true;
                    app.set_notice("授权已搁置（按拒绝处理）");
                }
                _ => {}
            }
        }
        Modal::Ask {
            params,
            question,
            highlight,
            selected,
            custom,
            editing_custom,
            reply,
        } => {
            let Some(current) = params.questions.get(*question) else {
                return;
            };
            let option_count = current.options.len();
            if *editing_custom {
                match (code, modifiers) {
                    (KeyCode::Esc, _) | (KeyCode::Enter, _) => *editing_custom = false,
                    (KeyCode::Backspace, _) => {
                        custom.pop();
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                        if !ch.is_control() =>
                    {
                        custom.push(ch);
                    }
                    _ => {}
                }
                return;
            }
            match (code, modifiers) {
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    highlight[*question] = highlight[*question].saturating_sub(1);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    highlight[*question] = (highlight[*question] + 1).min(option_count);
                }
                (KeyCode::Char('z'), KeyModifiers::NONE) => {
                    highlight[*question] = option_count;
                    *editing_custom = true;
                }
                (KeyCode::Char(' '), KeyModifiers::NONE) => {
                    // 多选切换；自定义行上无效
                    if let Some(option) = current.options.get(highlight[*question]) {
                        let label = option.label.clone();
                        if current.multi {
                            let chosen = &mut selected[*question];
                            if let Some(position) = chosen.iter().position(|s| *s == label) {
                                chosen.remove(position);
                            } else {
                                chosen.push(label);
                            }
                        } else {
                            selected[*question] = vec![label];
                        }
                    }
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if highlight[*question] == option_count {
                        // 自定义行提交并推进
                        *editing_custom = false;
                        close |= advance_ask(params, question, selected, custom, reply);
                    } else if let Some(option) = current.options.get(highlight[*question]) {
                        let label = option.label.clone();
                        if current.multi {
                            let chosen = &mut selected[*question];
                            if !chosen.contains(&label) {
                                chosen.push(label);
                            }
                            close |= advance_ask(params, question, selected, custom, reply);
                        } else {
                            selected[*question] = vec![label];
                            close |= advance_ask(params, question, selected, custom, reply);
                        }
                    }
                }
                (KeyCode::Tab, _) => {
                    close |= advance_ask(params, question, selected, custom, reply);
                }
                (KeyCode::Esc, _) => {
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(Err(moc_protocol::RpcError::of(
                            moc_protocol::RpcErrorCode::RequestCancelled,
                        )));
                    }
                    close = true;
                }
                _ => {}
            }
        }
        Modal::Model {
            models,
            selected,
            ..
        } => {
            match (code, modifiers) {
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    *selected = selected.saturating_sub(1);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    *selected = (*selected + 1).min(models.len().saturating_sub(1));
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if let Some(model) = models.get(*selected)
                        && let Some(session_id) = session_id
                    {
                        let client = connection.client.clone();
                        let events = events.clone();
                        let selector = model.id.clone();
                        tokio::spawn(async move {
                            match client
                                .call::<SessionSetModel>(SessionSetModelParams {
                                    session_id,
                                    selector,
                                })
                                .await
                            {
                                Ok(_) => {
                                    let _ = events.send(AppEvent::ModelSwitched);
                                }
                                Err(error) => tracing::warn!(%error, "切换模型失败"),
                            }
                        });
                    }
                    close = true;
                }
                (KeyCode::Esc, _) => close = true,
                _ => {}
            }
        }
        Modal::Sessions { sessions, selected } => {
            match (code, modifiers) {
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    *selected = selected.saturating_sub(1);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    *selected = (*selected + 1).min(sessions.len().saturating_sub(1));
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if let Some(session) = sessions.get(*selected) {
                        let client = connection.client.clone();
                        let session_id = session.id.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            match client
                                .call::<SessionLoad>(SessionLoadParams {
                                    session_id,
                                    max_entries: None,
                                })
                                .await
                            {
                                Ok(loaded) => {
                                    let _ = events.send(AppEvent::Loaded(
                                        loaded.session,
                                        crate::transcript_restore::blocks_from_entries(
                                            &loaded.entries,
                                        ),
                                    ));
                                }
                                Err(error) => tracing::warn!(%error, "加载会话失败"),
                            }
                        });
                    }
                    close = true;
                }
                (KeyCode::Esc, _) => close = true,
                _ => {}
            }
        }
        Modal::Plan {
            plan_id,
            selected,
            ..
        } => {
            match (code, modifiers) {
                (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::NONE) => *selected = 0,
                (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => *selected = 1,
                (KeyCode::Tab, _) => *selected = 1 - *selected,
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if let Some(session_id) = session_id {
                        let action = if *selected == 0 {
                            moc_protocol::methods::PlanAction::Execute
                        } else {
                            moc_protocol::methods::PlanAction::Reject
                        };
                        let client = connection.client.clone();
                        let plan_id = plan_id.clone();
                        tokio::spawn(async move {
                            match client
                                .call::<SessionPlanApprove>(SessionPlanApproveParams {
                                    session_id,
                                    plan_id,
                                    action,
                                })
                                .await
                            {
                                Ok(result) => {
                                    // 批准后 core 可能给自动跟进提示词，直接作为新一轮发送
                                    if let (Some(session_id), Some(prompt)) =
                                        (Some(result.session_id), result.auto_prompt)
                                        && !prompt.trim().is_empty()
                                    {
                                        let params = PromptParams {
                                            session_id,
                                            input: vec![InputPart::Text { text: prompt }],
                                        };
                                        let _ = client
                                            .call::<moc_protocol::methods::SessionPrompt>(params)
                                            .await;
                                    }
                                }
                                Err(error) => tracing::warn!(%error, "计划审批失败"),
                            }
                        });
                    }
                    close = true;
                }
                _ => {}
            }
        }
        Modal::Help => {
            if matches!(
                (code, modifiers),
                (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE)
            ) {
                close = true;
            }
        }
    }
    if close {
        app.modal = None;
    }
}

/// ask 推进到下一问或在最后一问提交；返回是否已提交（应关闭模态）。
#[allow(clippy::too_many_arguments)]
fn advance_ask(
    params: &moc_protocol::methods::AskParams,
    question: &mut usize,
    selected: &[Vec<String>],
    custom: &str,
    reply: &mut Option<
        tokio::sync::oneshot::Sender<Result<moc_protocol::methods::AskResult, moc_protocol::RpcError>>,
    >,
) -> bool {
    let answers = params
        .questions
        .iter()
        .enumerate()
        .map(|(index, current_question)| moc_protocol::methods::AskAnswer {
            id: current_question.id.clone(),
            selected: selected.get(index).cloned().unwrap_or_default(),
            custom: (!custom.is_empty() && index == *question).then(|| custom.to_owned()),
        })
        .collect();
    if *question + 1 >= params.questions.len() {
        if let Some(reply) = reply.take() {
            let _ = reply.send(Ok(moc_protocol::methods::AskResult { answers }));
        }
        true
    } else {
        *question += 1;
        false
    }
}

/// 把协议更新折叠进块序列：流式增量续写活动块，工具调用按 ID 归组。
fn apply_update(app: &mut App, update: SessionUpdate) {
    use crate::transcript::{Block, Entry};
    match update {
        SessionUpdate::AssistantDelta { delta, .. } => match delta {
            moc_protocol::methods::AssistantDelta::Text { delta } => {
                if !delta.is_empty()
                    && !app.has_streaming(|block| matches!(block, Block::Assistant { .. }))
                {
                    app.blocks
                        .push(Entry::new(Block::Assistant { text: String::new() }));
                }
                app.append_streaming(
                    |block| match block {
                        Block::Assistant { text } => Some(text),
                        _ => None,
                    },
                    &delta,
                );
            }
            moc_protocol::methods::AssistantDelta::Reasoning { delta } => {
                if !delta.is_empty()
                    && !app.has_streaming(|block| matches!(block, Block::Thinking { .. }))
                {
                    app.blocks
                        .push(Entry::new(Block::Thinking { text: String::new() }));
                }
                app.append_streaming(
                    |block| match block {
                        Block::Thinking { text } => Some(text),
                        _ => None,
                    },
                    &delta,
                );
            }
            moc_protocol::methods::AssistantDelta::ToolCallStarted { tool_call_id: _, name: _ } => {
            }
            moc_protocol::methods::AssistantDelta::ToolCallArgs { tool_call_id, delta } => {
                if let Some(entry) = app.tool_entry_mut(&tool_call_id) {
                    entry.push_args(&delta);
                } else {
                    app.push_pending_args(&tool_call_id, delta);
                }
            }
            moc_protocol::methods::AssistantDelta::Usage { usage } => app.apply_usage(usage),
        },
        SessionUpdate::ToolStarted {
            tool_call_id,
            name,
            ..
        } => app.start_tool_block(tool_call_id, name),
        SessionUpdate::ToolFinished {
            tool_call_id,
            status,
            result,
            duration_ms,
            diff,
            ..
        } => app.finish_tool_block(tool_call_id, status, result, duration_ms, diff),
        SessionUpdate::TurnStarted { .. } => {
            app.turn_running = true;
            app.cancel_pending = false;
            app.turn_started_at = Some(std::time::Instant::now());
            app.notice = None;
        }
        SessionUpdate::TurnFinished { .. } => {
            app.turn_running = false;
            app.cancel_pending = false;
        }
        SessionUpdate::TitleChanged { title } => {
            if let Some(session) = app.session.as_mut() {
                session.title = title;
            }
        }
        SessionUpdate::ModeChanged { mode } => {
            if let Some(session) = app.session.as_mut() {
                session.mode = mode;
            }
        }
        SessionUpdate::ModelChanged { model } => {
            if let Some(session) = app.session.as_mut() {
                session.model = model;
            }
        }
        SessionUpdate::TodoChanged { phases } => app.update_todos(phases),
        SessionUpdate::PlanSubmitted {
            plan_id,
            title,
            markdown,
            ..
        } => {
            app.pending_plan = Some((plan_id.clone(), title.clone(), markdown.clone()));
            app.modal = Some(Modal::Plan {
                plan_id,
                title,
                markdown,
                selected: 0,
            });
        }
        SessionUpdate::ContextCompacted {
            method,
            tokens_before,
            tokens_after,
        } => {
            app.push_note(
                format!("上下文已压缩（{method}）：{tokens_before} → {tokens_after}"),
                false,
            );
        }
        SessionUpdate::JobFinished {
            command,
            exit_code,
            ..
        } => {
            app.push_note(
                format!("后台任务结束：{command}（exit {exit_code:?}）"),
                false,
            );
        }
        SessionUpdate::WorkspaceChanged { .. } => {
            if let Some(session) = app.session.as_ref() {
                refresh_git_branch(app, &session.cwd);
            }
        }
        // 等待授权由工具块 ▸ 态与居中弹窗表达，不重复加 Note
        SessionUpdate::PermissionRequested { .. } => {}
        SessionUpdate::PermissionResolved { .. } => {}
        SessionUpdate::LockChanged { .. } => {}
        SessionUpdate::Error {
            message,
            retryable,
            ..
        } => {
            app.push_note(format!("错误：{message}（可重试：{retryable}）"), true);
        }
    }
}

/// 发送轮次；`session/prompt` 是长请求，完成/失败只影响运行状态标记。
fn send_turn(client: &ClientHandle, session_id: String, parts: Vec<InputPart>) {
    let client = client.clone();
    tokio::spawn(async move {
        tracing::info!(%session_id, "发送轮次请求");
        let params = PromptParams {
            session_id,
            input: parts,
        };
        match client.call::<moc_protocol::methods::SessionPrompt>(params).await {
            Ok(result) => tracing::info!(finish = ?result.finish_reason, "轮次完成"),
            Err(error) => tracing::warn!(%error, "轮次请求失败"),
        }
    });
}

/// 发出取消；core 会终止本轮并给未执行的工具调用补 Cancelled 结果。
fn cancel_turn(client: &ClientHandle, app: &App) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let client = client.clone();
    tokio::spawn(async move {
        let _ = client
            .call::<moc_protocol::methods::SessionCancel>(
                moc_protocol::methods::SessionCancelParams { session_id },
            )
            .await;
    });
}

/// ctrl+v：异步读剪贴板图片，完成后经事件队列回主循环。
fn spawn_clipboard_image(events: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let image = crate::clipboard::read_clipboard_image()
            .await
            .map(|image| (image.media_type, image.bytes));
        let _ = events.send(AppEvent::ClipboardImage(image));
    });
}

/// 用户展示文本：图片 chip 显示为占位说明。
fn display_text_of(parts: &[InputPart]) -> String {
    parts
        .iter()
        .map(|part| match part {
            InputPart::Text { text } => text.clone(),
            InputPart::Image { .. } => "[图片]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 执行斜杠命令。
fn execute_command(app: &mut App, connection: &EmbeddedConnection, line: String) {
    let (name, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    match name {
        "/quit" | "/q" => app.should_quit = true,
        "/help" => app.modal = Some(Modal::Help),
        "/compact" => {
            if let Some(session_id) = session_id {
                let client = connection.client.clone();
                let instructions = (!rest.trim().is_empty()).then(|| rest.trim().to_owned());
                tokio::spawn(async move {
                    let result = client
                        .call::<SessionCompact>(SessionCompactParams {
                            session_id,
                            instructions,
                        })
                        .await;
                    tracing::info!(?result, "手动压缩完成");
                });
            }
        }
        "/mode" => cycle_mode(app, connection),
        "/model" => app.open_model_picker(),
        "/resume" => open_session_picker(app, connection),
        other => app.set_notice(format!("未知命令：{other}")),
    }
}

/// 循环切换 Normal → Ask → Plan。
fn cycle_mode(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref() else {
        return;
    };
    let next = match session.mode {
        SessionMode::Normal => SessionMode::Ask,
        SessionMode::Ask => SessionMode::Plan,
        SessionMode::Plan => SessionMode::Normal,
    };
    let client = connection.client.clone();
    let session_id = session.id.clone();
    tokio::spawn(async move {
        let _ = client
            .call::<SessionSetMode>(SessionSetModeParams {
                session_id,
                mode: next,
            })
            .await;
    });
}

/// 异步刷新 git 分支（冷路径：启动与工作区变化时）。
fn refresh_git_branch(app: &App, cwd: &str) {
    let events = app.events.clone();
    let cwd = cwd.to_owned();
    tokio::spawn(async move {
        let Ok(output) = tokio::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&cwd)
            .output()
            .await
        else {
            return;
        };
        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !branch.is_empty() {
                let _ = events.send(AppEvent::GitBranch(branch));
            }
        }
    });
}

/// 把拖选覆盖的视口行文本复制到系统剪贴板。
fn copy_selection(app: &mut App) {
    let Some(((sx, sy), (ex, ey))) = app.selection else {
        return;
    };
    let (start_row, end_row) = if sy <= ey { (sy, ey) } else { (ey, sy) };
    let (start_col, end_col) = if sy == ey && sx > ex { (ex, sx) } else { (sx, ex) };
    let mut text = String::new();
    for &(row, ref line_text) in &app.paint_lines {
        if row < start_row || row > end_row {
            continue;
        }
        // 单行选择按列裁剪；跨行选择取整行
        if start_row == end_row {
            let chars: Vec<char> = line_text.chars().collect();
            let from = (start_col as usize).min(chars.len());
            let to = (end_col as usize).min(chars.len());
            text.push_str(&chars[from..to].iter().collect::<String>());
        } else {
            text.push_str(line_text);
        }
        text.push('\n');
    }
    if text.is_empty() {
        return;
    }
    let events = app.events.clone();
    let text_for_log = text.clone();
    tokio::spawn(async move {
        match crate::clipboard::copy_text_to_clipboard(text_for_log.trim_end()).await {
            Ok(()) => {
                tracing::debug!("选区已复制");
            }
            Err(error) => tracing::debug!(%error, "复制选区失败"),
        }
    });
    let _ = events;
    app.selection = None;
    app.set_notice("已复制选中内容");
}

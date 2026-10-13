//! 主事件循环：终端启动、会话启动、输入/协议事件分派与绘制节拍。
//!
//! 结构是「一个 `tokio::select!` 循环 + 无界 UI 队列」：协议通知入队即返回
//! （不阻塞协议读循环），输入线程独立，绘制按 16ms 帧预算节流（预算窗口内
//! 的事件合并为一帧），空闲时主循环完全 park。

use std::ops::Range;
use std::sync::Arc;

use anyhow::Context as _;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use moc_core::Core;
use moc_protocol::{
    ClientHandle, ConfigGet, ConfigGetParams, ContentPart, ImageSource, InputPart, OpenVikingList,
    OpenVikingListParams, OpenVikingRead, OpenVikingReadParams, OpenVikingWrite,
    OpenVikingWriteParams, ResourceComplete, ResourceCompleteParams, Role, SessionCompact,
    SessionCompactParams, SessionContext, SessionInfo, SessionList, SessionListParams, SessionLoad,
    SessionLoadPage, SessionLoadPageParams, SessionLoadParams, SessionLoadResult, SessionMode,
    SessionModelOverflowAction, SessionNew, SessionNewParams, SessionPermission,
    SessionPlanApprove, SessionPlanApproveParams, SessionRewind, SessionRewindParams,
    SessionSetMode, SessionSetModeParams, SessionSetModel, SessionSetModelParams,
    SessionSetPermission, SessionSetPermissionParams, SessionUpdate, SessionUsage,
    SessionUsageParams, UpstreamField,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::{App, CtrlCPending, Focus, MODES};
use crate::input::{InputEvent, InputThread};
use crate::modal::Modal;
use crate::protocol::{AppEvent, ClipboardPayload, EmbeddedConnection, connect_embedded};
use crate::render;
use crate::terminal::TerminalGuard;
use crate::theme::Theme;
use crate::widgets::prompt::{clamp_cursor, cursor_next, cursor_prev};

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
    pub hide_thinking: bool,
}

/// 运行 TUI 直到退出。
pub async fn run(core: Arc<Core>, opts: TuiOptions) -> anyhow::Result<()> {
    init_logging();
    let theme = Theme::global();
    let (tui_store, preferences) = crate::config::TuiStore::load(&core.config_home())?;
    let mut guard = TerminalGuard::enter()?;

    let (events, mut event_rx) = unbounded_channel::<AppEvent>();
    let mut connection = connect_embedded(Arc::clone(&core), events.clone())
        .await
        .context("连接后台服务失败")?;

    let mut app = App::new(theme, events.clone());
    app.show_thinking = preferences.show_thinking && !opts.hide_thinking;
    app.tools_collapsed = !preferences.expand_tools;
    app.statusline_template = preferences.statusline.clone();
    app.keymap = crate::config::KeyMap::compile(&preferences)?;
    app.preferences = preferences;
    app.tui_store = Some(tui_store);
    app.startup_mode = opts.mode;
    app.startup_pending = opts.initial_prompt.clone();
    bootstrap(&connection.client, &mut app, &opts).await;
    if app.session.is_none() && app.models.is_empty() {
        crate::settings::open(&mut app, &connection);
    }

    if app.session.is_some()
        && let Some(text) = app.startup_pending.take()
    {
        app.prompt.insert_str_raw(&text);
        crate::queue::submit(&mut app, &connection, false);
    }

    let mut input = InputThread::spawn();
    let result = event_loop(
        &mut guard,
        &mut app,
        &mut input,
        &mut event_rx,
        &mut connection,
    )
    .await;

    connection.shutdown().await;
    guard.restore();
    result
}

/// TUI 独占 stdout / stderr，日志只能落文件；初始化失败时静默（日志非关键路径）。
fn init_logging() {
    const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
    let home = std::env::var("MOC_HOME")
        .or_else(|_| std::env::var("HOME").map(|home| format!("{home}/.moc")))
        .unwrap_or_else(|_| "/tmp".into());
    let path = std::path::Path::new(&home).join("tui.log");
    if std::fs::metadata(&path)
        .ok()
        .is_some_and(|metadata| metadata.len() >= MAX_LOG_BYTES)
    {
        let _ = std::fs::remove_file(&path);
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
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
    let outcome: anyhow::Result<
        Option<(
            SessionInfo,
            Vec<crate::transcript::Entry>,
            bool,
            Option<String>,
        )>,
    > = async {
        let config = client.call::<ConfigGet>(ConfigGetParams {}).await?;
        app.models = config.models;
        app.startup_cwd = opts.cwd.clone().unwrap_or(config.cwd);
        if config.default_model.is_none() && opts.model.is_none() {
            app.set_notice("首次启动：请添加供应商并选择至少一个模型");
            return Ok(None);
        }
        let loaded = match &opts.resume {
            None => {
                let info = client
                    .call::<SessionNew>(SessionNewParams {
                        cwd: opts.cwd.clone(),
                        model: opts.model.clone(),
                        title: None,
                        mode: opts.mode,
                    })
                    .await?;
                (info, Vec::new(), false, None)
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
                let loaded = client
                    .call::<SessionLoadPage>(SessionLoadPageParams {
                        session_id,
                        before_entry_id: None,
                        max_entries: 64,
                        max_message_bytes: Some(64 * 1024),
                    })
                    .await?;
                let before_entry_id = loaded.entries.first().map(|entry| entry.id.clone());
                let blocks = crate::transcript_restore::blocks_from_entries_with_children(
                    &loaded.entries,
                    &loaded.child_calls,
                );
                (loaded.session, blocks, loaded.has_more, before_entry_id)
            }
        };
        Ok(Some(loaded))
    }
    .await;
    match outcome {
        Ok(Some((info, blocks, _has_more, _before_entry_id))) => {
            refresh_git_branch(app, &info.cwd);
            app.switch_session(info, blocks);
            load_context(client, app).await;
            load_subagents_for_parent(
                app,
                client,
                app.session.as_ref().map(|session| session.id.clone()),
            );
        }
        Ok(None) => {}
        Err(error) => app.set_notice(format!("会话启动失败：{error}")),
    }
}

async fn load_context(client: &ClientHandle, app: &mut App) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    if let Ok(usage) = client
        .call::<SessionContext>(moc_protocol::SessionContextParams { session_id })
        .await
    {
        app.context_usage = Some(usage);
    }
}

fn refresh_context(app: &App, connection: &EmbeddedConnection, notify_failure: bool) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<SessionContext>(moc_protocol::SessionContextParams { session_id })
            .await
        {
            Ok(usage) => {
                let _ = events.send(AppEvent::ContextUsage(usage));
            }
            Err(error) if notify_failure => {
                let _ = events.send(AppEvent::Notice(format!("读取上下文占用失败：{error}")));
            }
            Err(_) => {}
        }
    });
}

fn load_subagents(app: &App, connection: &EmbeddedConnection) {
    let Some(parent_session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::AgentList>(moc_protocol::methods::AgentListParams {
                parent_session_id,
            })
            .await
            .map(|result| result.agents)
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentsLoaded(result));
    });
}

fn open_subagents(app: &mut App, connection: &EmbeddedConnection) {
    app.modal = Some(Modal::Subagents {
        agents: app
            .subagents
            .values()
            .map(|view| view.info.clone())
            .collect(),
        picker: crate::modal::PickerState::default(),
    });
    load_subagents(app, connection);
}

fn send_agent_operation(
    app: &App,
    connection: &EmbeddedConnection,
    agent_id: String,
    message: String,
    steer: bool,
) {
    let Some(parent_session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = if steer {
            client
                .call::<moc_protocol::methods::AgentSteer>(
                    moc_protocol::methods::AgentSteerParams {
                        parent_session_id,
                        agent_id: agent_id.clone(),
                        message,
                    },
                )
                .await
                .map(|result| crate::subagent::AgentOperationNotice {
                    agent_id,
                    message: format!("steer 已接受：{}", result.accepted),
                })
        } else {
            client
                .call::<moc_protocol::methods::AgentSend>(moc_protocol::methods::AgentSendParams {
                    parent_session_id,
                    agent_id: agent_id.clone(),
                    message,
                })
                .await
                .map(|result| crate::subagent::AgentOperationNotice {
                    agent_id,
                    message: format!("消息已提交：{}", result.accepted),
                })
        }
        .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentOperation(result));
    });
}

fn stop_agent_operation(app: &App, connection: &EmbeddedConnection, agent_id: String) {
    let Some(parent_session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::AgentStop>(moc_protocol::methods::AgentStopParams {
                parent_session_id,
                agent_id: agent_id.clone(),
                reason: None,
            })
            .await
            .map(|result| crate::subagent::AgentOperationNotice {
                agent_id,
                message: format!("停止请求已处理：{}", result.stopped),
            })
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentOperation(result));
    });
}
fn load_subagents_for_parent(
    app: &mut App,
    client: &ClientHandle,
    parent_session_id: Option<String>,
) {
    let Some(parent_session_id) = parent_session_id else {
        app.subagents.clear();
        return;
    };
    let client = client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::AgentList>(moc_protocol::methods::AgentListParams {
                parent_session_id,
            })
            .await
            .map(|result| result.agents)
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentsLoaded(result));
    });
}

fn load_subagent_history(app: &App, connection: &EmbeddedConnection, agent_id: String) {
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionLoadPage>(SessionLoadPageParams {
                session_id: agent_id.clone(),
                before_entry_id: None,
                max_entries: 128,
                max_message_bytes: Some(64 * 1024),
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentHistoryLoaded { agent_id, result });
    });
}
fn open_context(app: &mut App, connection: &EmbeddedConnection) {
    if app.session.is_none() {
        app.set_notice("当前没有活动会话");
        return;
    }
    app.modal = Some(Modal::Context {
        usage: app.context_usage.clone(),
    });
    refresh_context(app, connection, true);
}
fn apply_rewind_loaded(app: &mut App, result: anyhow::Result<moc_protocol::SessionLoadResult>) {
    let Some(Modal::Rewind { state }) = app.modal.as_mut() else {
        return;
    };
    match result {
        Ok(loaded) => state.set_entries(loaded.entries),
        Err(error) => {
            state.loading = false;
            state.error = Some(format!("加载消息历史失败：{error}"));
        }
    }
}

fn apply_rewound(
    app: &mut App,
    connection: &EmbeddedConnection,
    selected: moc_protocol::methods::TranscriptEntry,
    result: anyhow::Result<moc_protocol::SessionLoadResult>,
) {
    let loaded = match result {
        Ok(loaded) => loaded,
        Err(error) => {
            if let Some(Modal::Rewind { state }) = app.modal.as_mut() {
                state.loading = false;
                state.error = Some(format!("消息回退失败：{error}"));
            }
            return;
        }
    };
    let restore_user = selected.message.role == Role::User;
    let selected_message = selected.message;
    app.modal = None;
    app.switch_session(
        loaded.session,
        crate::transcript_restore::blocks_from_entries_with_children(
            &loaded.entries,
            &loaded.child_calls,
        ),
    );
    app.todos = loaded.todos;
    app.prompt.clear();
    if restore_user {
        restore_user_message(&mut app.prompt, &selected_message);
    }
    app.prompt_changed();
    app.focus = Focus::Prompt;
    app.usage = Default::default();
    app.usage_samples.clear();
    app.set_notice("会话已回退");
    refresh_context(app, connection, false);
    load_subagents_for_parent(
        app,
        &connection.client,
        app.session.as_ref().map(|session| session.id.clone()),
    );
}

fn restore_user_message(
    prompt: &mut crate::widgets::prompt::PromptState,
    message: &moc_protocol::Message,
) {
    for part in &message.content {
        match part {
            ContentPart::Text(text) => prompt.insert_str_raw(text),
            ContentPart::Image {
                source: ImageSource::Base64 { media_type, data },
                ..
            } => prompt.insert_image(media_type.clone(), data.clone()),
            ContentPart::Image {
                source: ImageSource::Url(url),
                ..
            } => prompt.insert_str_raw(url),
            _ => {}
        }
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
    connection: &mut EmbeddedConnection,
) -> anyhow::Result<()> {
    const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(16);
    const ACTIVITY_TICK: std::time::Duration = std::time::Duration::from_millis(120);
    let mut view = render::ScrollbackView::new();
    let mut next_frame = tokio::time::Instant::now();
    let mut next_activity_tick = tokio::time::Instant::now() + ACTIVITY_TICK;
    let mut dirty = true;
    let mut deferred_event: Option<AppEvent> = None;
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
            if app.history_load_requested {
                request_history_page(app, connection);
            }
        }
        // 空闲（无 dirty）时完全 park 在事件上；dirty 但未到预算点才等待
        let need_frame_wait = dirty && tokio::time::Instant::now() < next_frame;
        let activity_tick_enabled = app.prompt_running;
        // notice / warning 显示中：安排 TTL 到点重绘，擦除浮层
        let notice_deadline = app.notice_deadline().map(tokio::time::Instant::from_std);
        let warning_deadline = app.warning_deadline().map(tokio::time::Instant::from_std);
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(next_frame), if need_frame_wait => {}
            _ = tokio::time::sleep_until(notice_deadline.unwrap_or_else(tokio::time::Instant::now)), if notice_deadline.is_some() => dirty = true,
            _ = tokio::time::sleep_until(warning_deadline.unwrap_or_else(tokio::time::Instant::now)), if warning_deadline.is_some() => dirty = true,
            _ = tokio::time::sleep_until(next_activity_tick), if activity_tick_enabled => {
                next_activity_tick = tokio::time::Instant::now() + ACTIVITY_TICK;
                dirty = true;
            }
            input_event = input.recv() => {
                dirty = true;
                match input_event {
                    Some(InputEvent::Terminal(event)) => handle_terminal_event(app, connection, event),
                    None => return Ok(()),
                }
            }
            update = connection.updates.recv() => {
                dirty = true;
                if let Some(params) = update {
                    apply_update_and_refresh(app, connection, params.session_id, params.update);
                    for _ in 0..64 {
                        let Some(next) = connection.updates.try_recv().ok() else { break };
                        apply_update_and_refresh(app, connection, next.session_id, next.update);
                    }
                }
            }
            app_event = async {
                if let Some(event) = deferred_event.take() { Some(event) } else { event_rx.recv().await }
            } => {
                dirty = true;
                match app_event {
                    Some(event @ (AppEvent::SettingsResponse {..} | AppEvent::ConfigurationLoaded(_)
                        | AppEvent::PromptReturned {..} | AppEvent::SteerReturned {..}
                        | AppEvent::PlanApproval(_) | AppEvent::PlanExecutionLoaded { .. }
                        | AppEvent::ModelSwitchConfirm { .. }
                        | AppEvent::SessionControlSettled { .. })) => {
                        crate::interaction::handle_event(app, connection, event);
                    }
                    Some(AppEvent::PlanReview(result)) => apply_plan_review(app, result),
                    Some(AppEvent::RewindLoaded(result)) => apply_rewind_loaded(app, result),
                    Some(AppEvent::Rewound { selected, result }) => {
                        apply_rewound(app, connection, selected, result);
                    }
                    Some(AppEvent::Update(params)) => {
                        apply_update_and_refresh(app, connection, params.session_id, params.update);
                        while let Ok(next) = event_rx.try_recv() {
                            match next {
                                AppEvent::Update(params) => {
                                    apply_update_and_refresh(app, connection, params.session_id, params.update)
                                }
                                other => {
                                    deferred_event = Some(other);
                                    break;
                                }
                            }
                        }
                    }
                    Some(AppEvent::HistoryPageLoaded { prepend, result }) => {
                        apply_history_page(app, prepend, result);
                    }
                    Some(AppEvent::ClipboardPaste(payload)) => match payload {
                        Some(ClipboardPayload::Image { media_type, data }) => {
                            app.prompt.insert_image(media_type, data);
                            app.set_notice("已附加剪贴板图片");
                        }
                        Some(ClipboardPayload::Text(text)) => app.prompt.insert_text(&text),
                        None => {
                            app.set_notice("剪贴板中没有可粘贴的图片或文本");
                        }
                    },
                    Some(AppEvent::Permission { params, reply }) => {
                        if !app.terminal_focused && app.preferences.notify_waiting { crate::config::notify("有操作等待你的确认"); }
                        app.modal = Some(Modal::Permission {
                            params,
                            selected: 0,
                            scroll_offset: 0,
                            reply: Some(reply),
                        });
                        app.focus = crate::app::Focus::Prompt;
                    }
                    Some(AppEvent::Ask { params, reply }) => {
                        if !app.terminal_focused && app.preferences.notify_waiting { crate::config::notify("有问题等待你的回答"); }
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
                                    .map_or_else(Vec::new, |option| vec![option.label.clone()])
                            })
                            .collect();
                        app.modal = Some(Modal::Ask {
                            params,
                            question: 0,
                            highlight,
                            selected,
                            custom: String::new(),
                            custom_cursor: 0,
                            editing_custom: false,
                            preview_scroll: 0,
                            reply: Some(reply),
                        });
                        app.focus = crate::app::Focus::Prompt;
                    }
                    Some(AppEvent::Memories(entries)) => {
                        if let Some(Modal::Memories { entries: current, picker, .. }) = app.modal.as_mut() {
                            *current = entries;
                            picker.loading = false;
                            picker.error = None;
                            picker.selected = 0;
                            picker.offset = 0;
                        }
                    }
                    Some(AppEvent::MemoryRead(document)) => {
                        if let Some(Modal::Memories { root_uri, entries, picker }) = app.modal.take() {
                            let mut editor = crate::widgets::prompt::PromptState::default();
                            editor.text = document.content;
                            editor.cursor = editor.text.len();
                            app.modal = Some(Modal::MemoryEditor { uri: document.uri, editor, dirty: false, root_uri, entries, picker });
                        }
                    }
                    Some(AppEvent::MemorySaved(_)) => {
                        let root = match app.modal.take() {
                            Some(Modal::MemoryEditor { root_uri, .. }) => root_uri,
                            other => { app.modal = other; String::new() }
                        };
                        if root.is_empty() { app.set_notice("记忆已保存"); }
                        else {
                            app.modal = Some(Modal::Memories { root_uri: root.clone(), entries: Vec::new(), picker: crate::modal::PickerState { loading: true, ..Default::default() } });
                            let client = connection.client.clone();
                            let events = app.events.clone();
                            tokio::spawn(async move {
                                match client.call::<OpenVikingList>(OpenVikingListParams { uri: root, recursive: false, limit: 128 }).await {
                                    Ok(result) => { let _ = events.send(AppEvent::Memories(result.entries)); }
                                    Err(error) => { let _ = events.send(AppEvent::PickerError(format!("刷新记忆列表失败：{error}"))); }
                                }
                            });
                        }
                    }
                    Some(AppEvent::Usage(result)) => {
                        app.push_note(format_usage(&result), false);
                    }
                    Some(AppEvent::ContextUsage(usage)) => {
                        app.context_usage = Some(usage.clone());
                        if let Some(Modal::Context { usage: current }) = app.modal.as_mut() {
                            *current = Some(usage);
                        }
                    }
                    Some(AppEvent::Sessions(sessions)) => {
                        app.modal = Some(Modal::Sessions {
                            sessions,
                            picker: crate::modal::PickerState::default(),
                        });
                    }
                    Some(AppEvent::SubagentsLoaded(result)) => match result {
                        Ok(agents) => {
                            app.subagents = agents
                                .into_iter()
                                .map(|info| {
                                    let id = info.id.clone();
                                    (id, crate::subagent::SubagentView::new(info))
                                })
                                .collect();
                            app.subagent_generation = app.subagent_generation.wrapping_add(1);
                            if let Some(Modal::Subagents { agents: current, .. }) = app.modal.as_mut() {
                                *current = app.subagents.values().map(|view| view.info.clone()).collect();
                            }
                            for agent_id in app.subagents.keys().cloned().collect::<Vec<_>>() {
                                load_subagent_history(app, connection, agent_id);
                            }
                        }
                        Err(error) => app.set_notice(format!("读取子代理列表失败：{error}")),
                    },
                    Some(AppEvent::SubagentHistoryLoaded { agent_id, result }) => match result {
                        Ok(page) => {
                            if let Some(view) = app.subagents.get_mut(&agent_id) {
                                view.restore(crate::transcript_restore::blocks_from_entries_with_children(&page.entries, &page.child_calls));
                                app.subagent_generation = app.subagent_generation.wrapping_add(1);
                            }
                        }
                        Err(error) => app.set_notice(format!("加载子代理历史失败：{error}")),
                    },
                    Some(AppEvent::SubagentOperation(result)) => match result {
                        Ok(notice) => app.set_notice(notice.message),
                        Err(error) => app.set_notice(format!("子代理操作失败：{error}")),
                    },
                    Some(AppEvent::PickerError(error)) => {
                        let shown = app
                            .modal
                            .as_mut()
                            .is_some_and(|modal| modal.set_error(&error));
                        if !shown {
                            app.set_notice(error);
                        }
                    }
                    Some(AppEvent::ResourceCompletion {
                        session_id,
                        revision,
                        prefix,
                        result,
                    }) => {
                        let current_session = app.session.as_ref().map(|session| session.id.as_str());
                        let current_prefix = app.completion_prefix.as_deref();
                        if current_session == Some(session_id.as_str())
                            && app.prompt_revision == revision
                            && current_prefix == Some(prefix.as_str())
                        {
                            match result {
                                Ok(candidates) => {
                                    app.completion = crate::widgets::completion::CompletionState::from_resources(
                                        candidates,
                                        current_token_range(&app.prompt),
                                    );
                                }
                                Err(_) => {
                                    app.completion = crate::widgets::completion::CompletionState::derive(app.prompt_text());
                                }
                            }
                        }
                    }
                    Some(AppEvent::Notice(message)) => app.set_notice(message),
                    Some(AppEvent::Loaded(info, blocks, has_more, before_entry_id)) => {
                        app.modal = None;
                        let cwd = info.cwd.clone();
                        let id = info.id.clone();
                        app.switch_session(info, blocks);
                        app.set_history_cursor(has_more, before_entry_id);
                        refresh_git_branch(app, &cwd);
                        refresh_context(app, connection, false);
                        load_subagents_for_parent(app, &connection.client, Some(id));
                        if let Some(text) = app.startup_pending.take() {
                            app.prompt.insert_str_raw(&text);
                            crate::queue::submit(app, connection, false);
                        }
                        crate::queue::dispatch(app, connection);
                    }
                    Some(AppEvent::GitBranch(branch)) => {
                        app.git_branch = Some(branch);
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

/// 终端事件分派（键盘、鼠标、粘贴与窗口变化）。
fn handle_terminal_event(app: &mut App, connection: &EmbeddedConnection, event: Event) {
    if app.settings.is_some() && app.modal.is_none() {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                crate::settings::handle_key(app, connection, key.modifiers, key.code)
            }
            Event::Mouse(mouse) => crate::settings::mouse(app, connection, mouse),
            Event::Paste(text) => crate::settings::paste(app, &text),
            Event::FocusGained => app.terminal_focused = true,
            Event::FocusLost => app.terminal_focused = false,
            _ => {}
        }
        return;
    }
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let previous_revision = app.prompt_revision;
            handle_key(app, connection, key.modifiers, key.code);
            if app.focus == Focus::Prompt && app.prompt_revision != previous_revision {
                request_resource_completion(app, connection);
            }
        }
        Event::Mouse(mouse) => handle_mouse_with_connection(app, connection, mouse),
        Event::Paste(text) if text.is_empty() => spawn_clipboard_paste(app.events.clone()),
        Event::Paste(text) => {
            app.prompt.insert_text(&text);
            app.prompt_changed();
            request_resource_completion(app, connection);
        }
        Event::Resize(_, _) => {}
        Event::FocusGained => app.terminal_focused = true,
        Event::FocusLost => app.terminal_focused = false,
        _ => {}
    }
}
fn current_token_range(prompt: &crate::widgets::prompt::PromptState) -> Range<usize> {
    let line_start = prompt.text[..prompt.cursor]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let token_start = prompt.text[line_start..prompt.cursor]
        .rfind(char::is_whitespace)
        .map_or(line_start, |index| line_start + index + 1);
    let token_end = prompt.text[prompt.cursor..]
        .find(char::is_whitespace)
        .map_or(prompt.text.len(), |index| prompt.cursor + index);
    token_start..token_end
}

fn resource_prefix(prompt: &crate::widgets::prompt::PromptState) -> Option<(String, Range<usize>)> {
    let range = current_token_range(prompt);
    let prefix = prompt.text[range.start..prompt.cursor].to_owned();
    let is_uri = prefix == "//" || prefix.ends_with(":/") || prefix.contains("://");
    is_uri.then_some((prefix, range))
}

fn request_resource_completion(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.completion_prefix = None;
        return;
    };
    let Some((prefix, _range)) = resource_prefix(&app.prompt) else {
        app.completion_prefix = None;
        return;
    };
    let revision = app.prompt_revision;
    app.completion_prefix = Some(prefix.clone());
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<ResourceComplete>(ResourceCompleteParams {
                session_id: session_id.clone(),
                prefix: prefix.clone(),
            })
            .await
            .map(|result| result.candidates)
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::ResourceCompletion {
            session_id,
            revision,
            prefix,
            result,
        });
    });
}

/// 鼠标事件：上下文点击需要连接，其余交互保持无连接处理。
fn handle_mouse_with_connection(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: crossterm::event::MouseEvent,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    if app.modal.is_some() {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && app
                .plan_model_area
                .contains((mouse.column, mouse.row).into())
        {
            app.open_plan_model_picker();
        } else if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            handle_mouse(app, mouse);
        }
        return;
    }
    if handle_mode_menu_mouse(app, connection, &mouse) {
        return;
    }
    if handle_completion_mouse(app, &mouse) {
        return;
    }
    if app.queue_area.contains((mouse.column, mouse.row).into()) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                app.queue_offset = app.queue_offset.saturating_sub(1);
                return;
            }
            MouseEventKind::ScrollDown => {
                app.queue_offset = app.queue_offset.saturating_add(1);
                return;
            }
            _ => {}
        }
    }
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        let position = (mouse.column, mouse.row);
        if let Some((agent_id, _)) = app
            .subagent_hits
            .iter()
            .find(|(_, area)| area.contains(position.into()))
        {
            app.subagent_selected = Some(agent_id.clone());
            app.modal = Some(Modal::SubagentDetail {
                agent_id: agent_id.clone(),
                editor: crate::widgets::prompt::PromptState::default(),
                scroll_offset: 0,
            });
            return;
        }
        if app.modal.is_none() && app.mode_area.contains(position.into()) {
            app.open_mode_menu();
            return;
        }
        if let Some((id, _, cancel)) = app
            .queue_hits
            .iter()
            .find(|(_, row, cancel)| {
                row.contains(position.into()) || cancel.contains(position.into())
            })
            .cloned()
        {
            if cancel.contains(position.into()) {
                crate::queue::remove(app, id);
            } else {
                crate::queue::edit(app, id);
            }
            return;
        }
        let context = app.context_area;
        if position.0 >= context.x
            && position.0 < context.x.saturating_add(context.width)
            && position.1 >= context.y
            && position.1 < context.y.saturating_add(context.height)
        {
            open_context(app, connection);
            return;
        }
    }
    handle_mouse(app, mouse);
}

/// 就地模式菜单的鼠标处理：命中条目即切换模式，点到菜单外（含再点一次图例）关闭。
///
/// 返回 `true` 表示事件已被菜单消费，不再落到队列、滚动区或输入框。
fn handle_mode_menu_mouse(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: &crossterm::event::MouseEvent,
) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    if app.mode_menu.is_none() || app.modal.is_some() {
        return false;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let entry = app
                .mode_menu
                .as_ref()
                .and_then(|menu| menu.entry_at(mouse.column, mouse.row));
            app.mode_menu = None;
            if let Some(index) = entry {
                set_mode(app, connection, MODES[index]);
            }
            true
        }
        MouseEventKind::Moved => {
            if let Some(menu) = app.mode_menu.as_mut()
                && let Some(index) = menu.entry_at(mouse.column, mouse.row)
            {
                menu.selected = index;
            }
            true
        }
        _ => false,
    }
}

/// 斜杠与资源补全菜单的鼠标处理：命中条目即选中并确认，点到外部关闭菜单。
fn handle_completion_mouse(app: &mut App, mouse: &crossterm::event::MouseEvent) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    if app.completion.is_none() || app.modal.is_some() {
        return false;
    }
    let Some(completion) = &app.completion else {
        return false;
    };
    let in_area = completion.area.contains((mouse.column, mouse.row).into());
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if in_area {
                if let Some(index) = completion.entry_at(mouse.column, mouse.row)
                    && let Some(comp) = app.completion.as_mut()
                {
                    comp.selected = index;
                    accept_completion(app);
                }
                true
            } else {
                app.completion = None;
                false
            }
        }
        MouseEventKind::Moved => {
            if in_area {
                if let Some(index) = completion.entry_at(mouse.column, mouse.row)
                    && let Some(comp) = app.completion.as_mut()
                {
                    comp.selected = index;
                }
                true
            } else {
                false
            }
        }
        MouseEventKind::ScrollUp if in_area => {
            if let Some(comp) = app.completion.as_mut() {
                comp.select_previous();
            }
            true
        }
        MouseEventKind::ScrollDown if in_area => {
            if let Some(comp) = app.completion.as_mut() {
                comp.select_next();
            }
            true
        }
        _ => false,
    }
}

/// 鼠标：滚轮滚动、点击折叠、拖选复制。
fn handle_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) {
    use crossterm::event::{MouseButton, MouseEventKind};
    let position = (mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.scroll_offset = state.scroll_offset.saturating_sub(3);
                return;
            }
            if let Some(Modal::Permission { scroll_offset, .. }) = app.modal.as_mut() {
                *scroll_offset = scroll_offset.saturating_sub(3);
                return;
            }
            scroll(app, 3);
        }
        MouseEventKind::ScrollDown => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.scroll_offset = state.scroll_offset.saturating_add(3);
                return;
            }
            if let Some(Modal::Permission { scroll_offset, .. }) = app.modal.as_mut() {
                *scroll_offset = scroll_offset.saturating_add(3);
                return;
            }
            scroll(app, -3);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let area = app.prompt_area;
            if position.0 >= area.x
                && position.0 < area.x.saturating_add(area.width)
                && position.1 >= area.y
                && position.1 < area.y.saturating_add(area.height)
            {
                app.focus = Focus::Prompt;
                if let Some(cursor) = crate::render::prompt_cursor_from_point(
                    &app.prompt.text,
                    area,
                    position.0,
                    position.1,
                ) {
                    app.prompt.cursor = cursor;
                }
            }
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
                    .find(|&&(_, start, _)| position.1 == start)
                    && let Some(entry) = app.blocks.get_mut(index)
                    && entry.block.foldable()
                {
                    entry.toggle_fold();
                }
                app.selection = None;
            } else if app.selection.is_some() {
                app.set_notice("按 y 复制选中内容");
            }
        }
        _ => {}
    }
}

/// 滚动视口：`delta > 0` 向历史方向推进（更早的行），`delta < 0` 回到尾部。
///
/// [`App::scroll_offset`] 的语义是「距尾部行数」，因此正向 delta 直接增大偏移。
fn request_history_page(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let Some(before_entry_id) = app.history_before_entry_id.clone() else {
        app.history_load_requested = false;
        return;
    };
    if !app.begin_history_load() {
        app.history_load_requested = false;
        return;
    }
    app.history_load_requested = false;
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionLoadPage>(SessionLoadPageParams {
                session_id,
                before_entry_id: Some(before_entry_id),
                max_entries: 64,
                max_message_bytes: Some(64 * 1024),
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::HistoryPageLoaded {
            prepend: true,
            result,
        });
    });
}

fn apply_history_page(
    app: &mut App,
    prepend: bool,
    result: anyhow::Result<moc_protocol::methods::SessionLoadPageResult>,
) {
    let Ok(page) = result else {
        app.history_loading = false;
        return;
    };
    let before_entry_id = page.entries.first().map(|entry| entry.id.clone());
    let blocks = crate::transcript_restore::blocks_from_entries_with_children(
        &page.entries,
        &page.child_calls,
    );
    if prepend {
        app.prepend_history(blocks);
    } else {
        app.switch_session(page.session, blocks);
    }
    app.set_history_cursor(page.has_more, before_entry_id);
}

fn scroll(app: &mut App, delta: i32) {
    if delta < 0 {
        app.scroll_offset = app
            .scroll_offset
            .saturating_sub(delta.unsigned_abs() as usize);
    } else {
        app.scroll_offset = app.scroll_offset.saturating_add(delta as usize);
        if app.scroll_offset >= app.scroll_max_offset && app.history_has_more {
            app.history_load_requested = true;
        }
    }
}

/// 键盘处理：模态优先，其次全局开关，最后按焦点路由。
fn handle_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    if !matches!(
        (code, modifiers),
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
    ) {
        app.ctrl_c_pending = None;
    }
    if !matches!(code, KeyCode::Esc) {
        app.clear_pending = false;
    }
    if app.modal.is_some() {
        handle_modal_key(app, connection, modifiers, code);
        return;
    }
    if handle_mode_menu_key(app, connection, modifiers, code) {
        return;
    }
    if matches!(code, KeyCode::Esc) && app.completion.is_some() {
        app.completion = None;
        return;
    }
    if handle_completion_key(app, modifiers, code) {
        return;
    }
    let translated = app.keymap.action(modifiers, code);
    match translated {
        Some("config") => {
            crate::settings::open(app, connection);
            return;
        }
        Some("agents") => {
            open_subagents(app, connection);
            return;
        }
        Some("mode") => {
            cycle_mode(app, connection);
            return;
        }
        Some("queue") => {
            crate::queue::submit(app, connection, true);
            return;
        }
        Some("queue_edit") => {
            if let Some(id) = app
                .session
                .as_ref()
                .and_then(|session| app.queues.get(&session.id))
                .and_then(|queue| queue.front())
                .map(|entry| entry.id)
            {
                crate::queue::edit(app, id);
            }
            return;
        }
        _ => {}
    }
    let (modifiers, code) = if let Some(key) = app.keymap.translated(modifiers, code) {
        key
    } else if app.keymap.reserved(modifiers, code) {
        return;
    } else {
        (modifiers, code)
    };
    if app.focus == Focus::Scrollback {
        match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                scroll(app, 3);
                return;
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                scroll(app, -3);
                return;
            }
            (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                scroll(app, 10);
                return;
            }
            (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                scroll(app, -10);
                return;
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                app.scroll_offset = usize::MAX;
                return;
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::SHIFT) => {
                app.scroll_offset = 0;
                return;
            }
            _ => {}
        }
    }
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            if app.prompt_running {
                if app.take_ctrl_c_pending(CtrlCPending::Quit) {
                    app.should_quit = true;
                } else {
                    app.ctrl_c_pending = Some((CtrlCPending::Quit, std::time::Instant::now()));
                    app.cancel_requested = true;
                    app.set_notice("Stopping... Press Ctrl+C again to exit");
                    cancel_prompt(&connection.client, app);
                }
            } else if !app.prompt_text().is_empty() {
                if app.take_ctrl_c_pending(CtrlCPending::ClearPrompt) {
                    app.prompt.clear();
                    app.notice = None;
                    app.notice_at = None;
                } else {
                    app.ctrl_c_pending =
                        Some((CtrlCPending::ClearPrompt, std::time::Instant::now()));
                    app.set_notice("clear input");
                }
            } else if app.take_ctrl_c_pending(CtrlCPending::Quit) {
                app.should_quit = true;
            } else {
                app.ctrl_c_pending = Some((CtrlCPending::Quit, std::time::Instant::now()));
                app.set_notice("Press Ctrl+C again to exit");
            }
        }
        (KeyCode::Char('h'), KeyModifiers::CONTROL) => app.modal = Some(Modal::Help),
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            app.toggle_thinking();
            app.set_notice(if app.show_thinking {
                "思考内容：展开"
            } else {
                "思考内容：折叠"
            });
        }
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
            app.toggle_tools();
            app.set_notice(if app.tools_collapsed {
                "工具调用：折叠"
            } else {
                "工具调用：展开"
            });
        }
        (KeyCode::Char('v'), KeyModifiers::CONTROL) => spawn_clipboard_paste(app.events.clone()),
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => app.open_model_picker(),
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => open_session_picker(app, connection),
        (KeyCode::Char('y'), KeyModifiers::NONE) => copy_selection(app),
        (KeyCode::Esc, _) => {
            if app.selection.is_some() {
                app.selection = None;
            } else if app.prompt_running {
                if !app.cancel_requested {
                    app.cancel_requested = true;
                    app.set_notice("Stopping...");
                    cancel_prompt(&connection.client, app);
                }
            } else if app.focus == Focus::Prompt && !app.prompt_text().is_empty() {
                if app.clear_pending {
                    app.prompt.clear();
                    app.clear_pending = false;
                } else {
                    app.clear_pending = true;
                    app.set_notice("再次按 Esc 清空输入");
                }
            } else if app.focus == Focus::Prompt && app.clear_pending {
                app.clear_pending = false;
                open_rewind(app, connection);
            } else if app.focus == Focus::Prompt {
                app.clear_pending = true;
                app.set_notice("再次按 Esc 打开消息回退");
            }
        }
        (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::BackTab, _) => {
            cycle_thinking_level(app, connection)
        }
        (KeyCode::Tab, _) => app.focus = app.focus.toggle(),
        (KeyCode::Enter, KeyModifiers::ALT | KeyModifiers::SHIFT)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.insert_char('\n');
            app.prompt_changed();
        }
        (KeyCode::Enter, KeyModifiers::NONE) if app.focus == crate::app::Focus::Prompt => {
            commit_prompt(app, connection)
        }
        (KeyCode::Up, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.up();
        }
        (KeyCode::Down, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.down();
        }
        (KeyCode::Backspace, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.backspace();
            app.prompt_changed();
        }
        (KeyCode::Delete, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.delete();
            app.prompt_changed();
        }
        (KeyCode::Left, KeyModifiers::CONTROL)
        | (KeyCode::Left, KeyModifiers::ALT)
        | (KeyCode::Char('b'), KeyModifiers::ALT)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.move_word_back();
        }
        (KeyCode::Right, KeyModifiers::CONTROL)
        | (KeyCode::Right, KeyModifiers::ALT)
        | (KeyCode::Char('f'), KeyModifiers::ALT)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.move_word_forward();
        }
        (KeyCode::Left, _) if app.focus == crate::app::Focus::Prompt => app.prompt.left(),
        (KeyCode::Right, _) if app.focus == crate::app::Focus::Prompt => app.prompt.right(),
        (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.move_line_start()
        }
        (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.move_line_end()
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.clear_line_to_end();
            app.prompt_changed();
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.clear();
            app.prompt_changed();
        }
        (KeyCode::Char('w'), KeyModifiers::CONTROL) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.delete_word_back();
            app.prompt_changed();
        }
        (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
            if app.focus == crate::app::Focus::Prompt && !ch.is_control() =>
        {
            app.prompt_insert_char(ch);
        }
        _ => {}
    }
}

fn handle_completion_key(app: &mut App, modifiers: KeyModifiers, code: KeyCode) -> bool {
    if app.focus != Focus::Prompt || app.completion.is_none() {
        return false;
    }
    match (code, modifiers) {
        (KeyCode::Tab, KeyModifiers::SHIFT) => false,
        (KeyCode::Tab, KeyModifiers::NONE) | (KeyCode::Enter, KeyModifiers::NONE) => {
            accept_completion(app)
        }
        (KeyCode::Up, _) => {
            if let Some(completion) = app.completion.as_mut() {
                completion.select_previous();
            }
            true
        }
        (KeyCode::Down, _) => {
            if let Some(completion) = app.completion.as_mut() {
                completion.select_next();
            }
            true
        }
        _ => false,
    }
}

fn accept_completion(app: &mut App) -> bool {
    let Some(completion) = app.completion.take() else {
        return false;
    };
    let chosen = completion.chosen().clone();
    app.prompt
        .replace_range(completion.replace_range, &chosen.value, chosen.append_space);
    app.prompt_changed();
    true
}

/// 提交输入：补全确认 > 斜杠命令 > Bash 命令或普通发送。
fn commit_prompt(app: &mut App, connection: &EmbeddedConnection) {
    if accept_completion(app) {
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
    crate::queue::submit(app, connection, false);
}
/// 空输入框双击 Esc：加载当前会话的 transcript 回退选择器。
fn open_rewind(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.set_notice("当前没有活动会话");
        return;
    };
    app.open_rewind();
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionLoadPage>(SessionLoadPageParams {
                session_id,
                before_entry_id: None,
                max_entries: 128,
                max_message_bytes: Some(64 * 1024),
            })
            .await
            .map(|loaded| SessionLoadResult {
                session: loaded.session,
                entries: loaded.entries,
                truncated: loaded.has_more,
                todos: loaded.todos,
                plans: loaded.plans,
                child_calls: loaded.child_calls,
            })
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::RewindLoaded(result));
    });
}

/// ctrl+r / `/resume`：拉取会话列表后打开选择器。
fn open_session_picker(app: &mut App, connection: &EmbeddedConnection) {
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

/// 提问预览每次翻页滚动的行数；渲染层会夹到实际内容高度。
const ASK_PREVIEW_SCROLL_STEP: usize = 8;

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
    if let Some(Modal::Plan { state }) = app.modal.as_ref() {
        if state.submitting {
            return;
        }
        if (code, modifiers) == (KeyCode::Char('p'), KeyModifiers::CONTROL) {
            app.open_plan_model_picker();
            return;
        }
    }
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    let events = app.events.clone();
    let mut close = false;
    let mut return_memory_list = false;
    let mut return_plan_review = false;
    let plan_review_open = matches!(app.modal, Some(Modal::Plan { .. }));
    let Some(modal) = app.modal.as_mut() else {
        return;
    };
    app.ctrl_c_pending = None;
    match modal {
        Modal::Permission {
            params,
            selected,
            scroll_offset,
            reply,
        } => {
            let option_count = params.options.len();
            match (code, modifiers) {
                (KeyCode::PageUp, _) => {
                    *scroll_offset = scroll_offset.saturating_sub(6);
                }
                (KeyCode::PageDown, _) => {
                    *scroll_offset = scroll_offset.saturating_add(6);
                }
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
                    app.set_notice("未授权，已按拒绝处理");
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
            custom_cursor,
            editing_custom,
            preview_scroll,
            reply,
        } => {
            let Some(current) = params.questions.get(*question) else {
                return;
            };
            let option_count = current.options.len();
            if *editing_custom {
                match (code, modifiers) {
                    (KeyCode::Esc, _) => *editing_custom = false,
                    (KeyCode::Enter, _) => {
                        *editing_custom = false;
                        close |= advance_ask(params, question, selected, custom, reply);
                    }
                    // 光标与删除一律按字符步进：`custom` 是 UTF-8 文本，按字节移动会切进多字节字符。
                    (KeyCode::Left, _) => *custom_cursor = cursor_prev(custom, *custom_cursor),
                    (KeyCode::Right, _) => *custom_cursor = cursor_next(custom, *custom_cursor),
                    (KeyCode::Home, _) => *custom_cursor = 0,
                    (KeyCode::End, _) => *custom_cursor = custom.len(),
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        custom.clear();
                        *custom_cursor = 0;
                    }
                    (KeyCode::Backspace, _) => {
                        let previous = cursor_prev(custom, *custom_cursor);
                        if previous != *custom_cursor {
                            custom.remove(previous);
                        }
                        *custom_cursor = previous;
                    }
                    (KeyCode::Delete, _) => {
                        let cursor = clamp_cursor(custom, *custom_cursor);
                        if custom[cursor..].chars().next().is_some() {
                            custom.remove(cursor);
                        }
                        *custom_cursor = cursor;
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                        if !ch.is_control() =>
                    {
                        let pos = clamp_cursor(custom, *custom_cursor);
                        custom.insert(pos, ch);
                        *custom_cursor = pos + ch.len_utf8();
                    }
                    _ => {}
                }
                if close {
                    app.modal = None;
                }
                return;
            }
            match (code, modifiers) {
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    let next = highlight[*question].saturating_sub(1);
                    if next != highlight[*question] {
                        highlight[*question] = next;
                        *preview_scroll = 0;
                    }
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    let next = (highlight[*question] + 1).min(option_count);
                    if next != highlight[*question] {
                        highlight[*question] = next;
                        *preview_scroll = 0;
                    }
                }
                // 聚焦项预览滚动：只改偏移，越界由渲染层夹到实际行数。
                (KeyCode::PageUp, _) => {
                    *preview_scroll = preview_scroll.saturating_sub(ASK_PREVIEW_SCROLL_STEP);
                }
                (KeyCode::PageDown, _) => {
                    *preview_scroll = preview_scroll.saturating_add(ASK_PREVIEW_SCROLL_STEP);
                }
                (KeyCode::Char('z'), KeyModifiers::NONE) => {
                    highlight[*question] = option_count;
                    *editing_custom = true;
                    *custom_cursor = custom.len();
                    *preview_scroll = 0;
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
                        *editing_custom = true;
                        *custom_cursor = custom.len();
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
                (KeyCode::Tab, KeyModifiers::NONE) => {
                    *preview_scroll = 0;
                    close |= advance_ask(params, question, selected, custom, reply);
                }
                (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::BackTab, _) => {
                    if *question > 0 {
                        *question -= 1;
                        *preview_scroll = 0;
                    }
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
        Modal::Rewind { state } => match (code, modifiers) {
            (KeyCode::Up, _) => state.move_vertical(-1),
            (KeyCode::Down, _) => state.move_vertical(1),
            (KeyCode::Left, _) => state.move_horizontal(-1),
            (KeyCode::Right, _) => state.move_horizontal(1),
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if state.loading {
                    return;
                }
                let Some(selected) = state.selected().cloned() else {
                    state.error = Some("当前会话没有可回退的消息".into());
                    return;
                };
                let Some(session_id) = session_id.clone() else {
                    state.error = Some("当前没有活动会话".into());
                    return;
                };
                state.loading = true;
                state.error = None;
                let client = connection.client.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let result = client
                        .call::<SessionRewind>(SessionRewindParams {
                            session_id,
                            entry_id: selected.id.clone(),
                        })
                        .await
                        .map_err(anyhow::Error::from);
                    let _ = events.send(AppEvent::Rewound { selected, result });
                });
            }
            (KeyCode::Esc, _) if state.loading && !state.prompts.is_empty() => {}
            (KeyCode::Esc, _) => close = true,
            _ => {}
        },
        Modal::Model {
            models,
            picker,
            target,
            ..
        } => {
            let filtered = crate::modal::filtered_models(models, &picker.query);
            match (code, modifiers) {
                (KeyCode::Up, _) => picker.move_selection(-1, filtered.len(), 8),
                (KeyCode::Down, _) => picker.move_selection(1, filtered.len(), 8),
                (KeyCode::PageUp, _) => picker.move_selection(-8, filtered.len(), 8),
                (KeyCode::PageDown, _) => picker.move_selection(8, filtered.len(), 8),
                (KeyCode::Home, _) => {
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::End, _) => {
                    picker.selected = filtered.len().saturating_sub(1);
                    picker.keep_visible(8, filtered.len());
                }
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    picker.query.clear();
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                    while picker.query.ends_with(' ') {
                        picker.query.pop();
                    }
                    while !picker.query.is_empty() && !picker.query.ends_with(' ') {
                        picker.query.pop();
                    }
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Backspace, _) => {
                    picker.query.pop();
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if !ch.is_control() =>
                {
                    picker.query.push(ch);
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if picker.loading {
                        return;
                    }
                    let Some((_, model)) = filtered.get(picker.selected) else {
                        return;
                    };
                    if let crate::modal::ModelTarget::PlanReview(state) = target {
                        state.execution_model = model.id.clone();
                        return_plan_review = true;
                    } else {
                        let Some(session_id) = session_id.clone() else {
                            picker.error = Some("当前没有活动会话".into());
                            return;
                        };
                        picker.loading = true;
                        picker.error = None;
                        let client = connection.client.clone();
                        let events = events.clone();
                        let selector = model.id.clone();
                        let request_selector = selector.clone();
                        app.inflight_session_ops.insert(session_id.clone());
                        tokio::spawn(async move {
                            let outcome = client
                                .call::<SessionSetModel>(SessionSetModelParams {
                                    session_id: session_id.clone(),
                                    selector: request_selector,
                                    overflow: SessionModelOverflowAction::Ask,
                                })
                                .await;
                            let _ = events.send(match outcome {
                                Ok(result) => match result.overflow {
                                    Some(overflow) => AppEvent::ModelSwitchConfirm {
                                        session_id,
                                        selector,
                                        overflow,
                                    },
                                    None => AppEvent::SessionControlSettled {
                                        session_id,
                                        notice: None,
                                        result: Ok(result.compaction_error),
                                    },
                                },
                                Err(error) => AppEvent::SessionControlSettled {
                                    session_id,
                                    notice: None,
                                    result: Err(format!("切换模型失败：{error}")),
                                },
                            });
                        });
                    }
                }
                (KeyCode::Esc, _) => {
                    if matches!(target, crate::modal::ModelTarget::PlanReview(_)) {
                        return_plan_review = true;
                    } else {
                        close = true;
                    }
                }
                _ => {}
            }
        }
        Modal::SwitchModel {
            session_id: card_session,
            selector,
            option,
            loading,
            error,
            ..
        } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Left, _) => *option = option.saturating_sub(1),
            (KeyCode::Down, _) | (KeyCode::Right, _) => *option = (*option + 1).min(1),
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if *option == 1 {
                    close = true;
                } else if !*loading {
                    *loading = true;
                    *error = None;
                    let session_id = card_session.clone();
                    let selector = selector.clone();
                    let client = connection.client.clone();
                    let events = events.clone();
                    app.inflight_session_ops.insert(session_id.clone());
                    tokio::spawn(async move {
                        let outcome = client
                            .call::<SessionSetModel>(SessionSetModelParams {
                                session_id: session_id.clone(),
                                selector,
                                overflow: SessionModelOverflowAction::Compact,
                            })
                            .await;
                        let _ = events.send(AppEvent::SessionControlSettled {
                            session_id,
                            notice: None,
                            result: outcome
                                .map(|result| result.compaction_error)
                                .map_err(|error| format!("切换模型失败：{error}")),
                        });
                    });
                }
            }
            (KeyCode::Esc, _) => close = true,
            _ => {}
        },
        Modal::Memories {
            root_uri,
            entries,
            picker,
        } => {
            let query = picker.query.to_lowercase();
            let filtered: Vec<_> = entries
                .iter()
                .filter(|entry| entry.name.to_lowercase().contains(&query))
                .collect();
            match (code, modifiers) {
                (KeyCode::Up, _) => picker.move_selection(-1, filtered.len(), 8),
                (KeyCode::Down, _) => picker.move_selection(1, filtered.len(), 8),
                (KeyCode::PageUp, _) => picker.move_selection(-8, filtered.len(), 8),
                (KeyCode::PageDown, _) => picker.move_selection(8, filtered.len(), 8),
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    picker.query.clear();
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                    while picker.query.ends_with(' ') {
                        picker.query.pop();
                    }
                    while !picker.query.is_empty() && !picker.query.ends_with(' ') {
                        picker.query.pop();
                    }
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Backspace, _) => {
                    picker.query.pop();
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if !ch.is_control() =>
                {
                    picker.query.push(ch);
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    let Some(entry) = filtered.get(picker.selected) else {
                        return;
                    };
                    let uri = entry.uri.clone();
                    let client = connection.client.clone();
                    let events = events.clone();
                    if entry.is_dir {
                        *root_uri = uri.clone();
                        picker.loading = true;
                        picker.error = None;
                        tokio::spawn(async move {
                            match client
                                .call::<OpenVikingList>(OpenVikingListParams {
                                    uri,
                                    recursive: false,
                                    limit: 128,
                                })
                                .await
                            {
                                Ok(result) => {
                                    let _ = events.send(AppEvent::Memories(result.entries));
                                }
                                Err(error) => {
                                    let _ = events.send(AppEvent::PickerError(format!(
                                        "读取 OpenViking 目录失败：{error}"
                                    )));
                                }
                            }
                        });
                    } else {
                        picker.loading = true;
                        picker.error = None;
                        tokio::spawn(async move {
                            match client
                                .call::<OpenVikingRead>(OpenVikingReadParams {
                                    uri,
                                    offset: 0,
                                    limit: 32_000,
                                })
                                .await
                            {
                                Ok(result) => {
                                    let _ = events.send(AppEvent::MemoryRead(result));
                                }
                                Err(error) => {
                                    let _ = events.send(AppEvent::PickerError(format!(
                                        "读取 OpenViking 正文失败：{error}"
                                    )));
                                }
                            }
                        });
                    }
                }
                (KeyCode::Esc, _) => close = true,
                _ => {}
            }
        }
        Modal::MemoryEditor {
            uri, editor, dirty, ..
        } => match (code, modifiers) {
            (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                let uri = uri.clone();
                let content = editor.text.clone();
                let client = connection.client.clone();
                let events = events.clone();
                *dirty = false;
                tokio::spawn(async move {
                    match client
                        .call::<OpenVikingWrite>(OpenVikingWriteParams {
                            uri: uri.clone(),
                            content,
                            mode: "replace".into(),
                        })
                        .await
                    {
                        Ok(_) => {
                            let _ = events.send(AppEvent::MemorySaved(uri));
                        }
                        Err(error) => {
                            let _ = events
                                .send(AppEvent::PickerError(format!("保存记忆失败：{error}")));
                        }
                    }
                });
            }
            (KeyCode::Esc, _) => return_memory_list = true,
            (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) if !ch.is_control() => {
                editor.insert_char(ch);
                *dirty = true;
            }
            (KeyCode::Backspace, _) => {
                editor.backspace();
                *dirty = true;
            }
            (KeyCode::Enter, KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                editor.insert_char('\n');
                *dirty = true;
            }
            (KeyCode::Left, _) => editor.left(),
            (KeyCode::Right, _) => editor.right(),
            (KeyCode::Up, _) => {
                editor.up();
            }
            (KeyCode::Down, _) => {
                editor.down();
            }
            _ => {}
        },
        Modal::Sessions { sessions, picker } => {
            let filtered = crate::modal::filtered_sessions(sessions, &picker.query);
            match (code, modifiers) {
                (KeyCode::Up, _) => picker.move_selection(-1, filtered.len(), 8),
                (KeyCode::Down, _) => picker.move_selection(1, filtered.len(), 8),
                (KeyCode::PageUp, _) => picker.move_selection(-8, filtered.len(), 8),
                (KeyCode::PageDown, _) => picker.move_selection(8, filtered.len(), 8),
                (KeyCode::Home, _) => {
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::End, _) => {
                    picker.selected = filtered.len().saturating_sub(1);
                    picker.keep_visible(8, filtered.len());
                }
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    picker.query.clear();
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                    while picker.query.ends_with(' ') {
                        picker.query.pop();
                    }
                    while !picker.query.is_empty() && !picker.query.ends_with(' ') {
                        picker.query.pop();
                    }
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Backspace, _) => {
                    picker.query.pop();
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if !ch.is_control() =>
                {
                    picker.query.push(ch);
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if picker.loading {
                        return;
                    }
                    let Some((_, session)) = filtered.get(picker.selected) else {
                        return;
                    };
                    picker.loading = true;
                    picker.error = None;
                    let client = connection.client.clone();
                    let session_id = session.id.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        match client
                            .call::<SessionLoad>(SessionLoadParams {
                                session_id,
                                max_entries: Some(64),
                            })
                            .await
                        {
                            Ok(loaded) => {
                                let before_entry_id =
                                    loaded.entries.first().map(|entry| entry.id.clone());
                                let _ = events.send(AppEvent::Loaded(
                                    loaded.session,
                                    crate::transcript_restore::blocks_from_entries_with_children(
                                        &loaded.entries,
                                        &loaded.child_calls,
                                    ),
                                    loaded.truncated,
                                    before_entry_id,
                                ));
                            }
                            Err(error) => {
                                let _ = events
                                    .send(AppEvent::PickerError(format!("加载会话失败：{error}")));
                            }
                        }
                    });
                }
                (KeyCode::Esc, _) => close = true,
                _ => {}
            }
        }
        Modal::Plan { state } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
            }
            (KeyCode::PageUp, _) => state.scroll_offset = state.scroll_offset.saturating_sub(10),
            (KeyCode::PageDown, _) => state.scroll_offset = state.scroll_offset.saturating_add(10),
            (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
                state.selected = state.selected.saturating_sub(1);
            }
            (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
                state.selected = (state.selected + 1).min(3);
            }
            (KeyCode::Tab, _) => state.selected = (state.selected + 1) % 4,
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if state.selected == 3 {
                    close = true;
                } else if !state.submitting
                    && let Some(session_id) = session_id
                {
                    let action = match state.selected {
                        0 => moc_protocol::methods::PlanAction::Execute,
                        1 => moc_protocol::methods::PlanAction::ExecuteCompacted,
                        2 => moc_protocol::methods::PlanAction::ExecuteNewSession,
                        _ => unreachable!("计划审阅选项已限制为四项"),
                    };
                    state.submitting = true;
                    state.error = None;
                    let client = connection.client.clone();
                    let plan_id = state.plan_id.clone();
                    let execution_model = state.execution_model.clone();
                    let events = app.events.clone();
                    tokio::spawn(async move {
                        let result = client
                            .call::<SessionPlanApprove>(SessionPlanApproveParams {
                                session_id,
                                plan_id,
                                action,
                                execution_model: Some(execution_model),
                            })
                            .await
                            .map_err(anyhow::Error::from);
                        let _ = events.send(AppEvent::PlanApproval(result));
                    });
                }
            }
            (KeyCode::Esc, _) => close = true,
            _ => {}
        },
        Modal::Help | Modal::Context { .. } => {
            if matches!(
                (code, modifiers),
                (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE)
            ) {
                close = true;
            }
        }
        Modal::Subagents { agents, picker } => {
            let filtered: Vec<_> = agents
                .iter()
                .filter(|agent| {
                    let query = picker.query.to_lowercase();
                    agent.title.to_lowercase().contains(&query) || agent.id.contains(&query)
                })
                .collect();
            match (code, modifiers) {
                (KeyCode::Up, _) => picker.move_selection(-1, filtered.len(), 8),
                (KeyCode::Down, _) => picker.move_selection(1, filtered.len(), 8),
                (KeyCode::Backspace, _) => {
                    picker.query.pop();
                    picker.selected = 0;
                }
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if !ch.is_control() =>
                {
                    picker.query.push(ch);
                    picker.selected = 0;
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if let Some(agent) = filtered.get(picker.selected) {
                        app.subagent_selected = Some(agent.id.clone());
                        app.modal = Some(Modal::SubagentDetail {
                            agent_id: agent.id.clone(),
                            editor: crate::widgets::prompt::PromptState::default(),
                            scroll_offset: 0,
                        });
                    }
                }
                (KeyCode::Char('x'), KeyModifiers::NONE) => {
                    if let Some(agent) = filtered.get(picker.selected) {
                        let agent_id = agent.id.clone();
                        let client = connection.client.clone();
                        let parent_session_id = session_id.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            let result = match parent_session_id {
                                Some(parent_session_id) => client
                                    .call::<moc_protocol::methods::AgentStop>(
                                        moc_protocol::methods::AgentStopParams {
                                            parent_session_id,
                                            agent_id: agent_id.clone(),
                                            reason: None,
                                        },
                                    )
                                    .await
                                    .map(|result| crate::subagent::AgentOperationNotice {
                                        agent_id,
                                        message: format!("停止请求已处理：{}", result.stopped),
                                    })
                                    .map_err(anyhow::Error::from),
                                None => Err(anyhow::anyhow!("当前没有活动会话")),
                            };
                            let _ = events.send(AppEvent::SubagentOperation(result));
                        });
                    }
                }
                (KeyCode::Esc, _) => close = true,
                _ => {}
            }
        }
        Modal::SubagentDetail {
            agent_id,
            editor,
            scroll_offset,
        } => match (code, modifiers) {
            (KeyCode::Esc, _) => {
                app.modal = Some(Modal::Subagents {
                    agents: app
                        .subagents
                        .values()
                        .map(|view| view.info.clone())
                        .collect(),
                    picker: crate::modal::PickerState::default(),
                });
            }
            (KeyCode::Enter, KeyModifiers::ALT | KeyModifiers::SHIFT) => editor.insert_char('\n'),
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let message = editor.text.trim().to_owned();
                if !message.is_empty() {
                    let running = app.subagents.get(agent_id).is_some_and(|view| view.running);
                    let parent_session_id = session_id.clone();
                    let target_agent_id = agent_id.clone();
                    let client = connection.client.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        let result = match parent_session_id {
                            Some(parent_session_id) if running => client
                                .call::<moc_protocol::methods::AgentSteer>(
                                    moc_protocol::methods::AgentSteerParams {
                                        parent_session_id,
                                        agent_id: target_agent_id.clone(),
                                        message,
                                    },
                                )
                                .await
                                .map(|result| crate::subagent::AgentOperationNotice {
                                    agent_id: target_agent_id,
                                    message: format!("steer 已接受：{}", result.accepted),
                                })
                                .map_err(anyhow::Error::from),
                            Some(parent_session_id) => client
                                .call::<moc_protocol::methods::AgentSend>(
                                    moc_protocol::methods::AgentSendParams {
                                        parent_session_id,
                                        agent_id: target_agent_id.clone(),
                                        message,
                                    },
                                )
                                .await
                                .map(|result| crate::subagent::AgentOperationNotice {
                                    agent_id: target_agent_id,
                                    message: format!("消息已提交：{}", result.accepted),
                                })
                                .map_err(anyhow::Error::from),
                            None => Err(anyhow::anyhow!("当前没有活动会话")),
                        };
                        let _ = events.send(AppEvent::SubagentOperation(result));
                    });
                    editor.clear();
                }
            }
            (KeyCode::Char('x'), KeyModifiers::NONE)
            | (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                let target_agent_id = agent_id.clone();
                let parent_session_id = session_id.clone();
                let client = connection.client.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let result = match parent_session_id {
                        Some(parent_session_id) => client
                            .call::<moc_protocol::methods::AgentStop>(
                                moc_protocol::methods::AgentStopParams {
                                    parent_session_id,
                                    agent_id: target_agent_id.clone(),
                                    reason: None,
                                },
                            )
                            .await
                            .map(|result| crate::subagent::AgentOperationNotice {
                                agent_id: target_agent_id,
                                message: format!("停止请求已处理：{}", result.stopped),
                            })
                            .map_err(anyhow::Error::from),
                        None => Err(anyhow::anyhow!("当前没有活动会话")),
                    };
                    let _ = events.send(AppEvent::SubagentOperation(result));
                });
            }
            (KeyCode::Backspace, _) => editor.backspace(),
            (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) if !ch.is_control() => {
                editor.insert_char(ch)
            }
            (KeyCode::Up, _) => *scroll_offset = scroll_offset.saturating_add(1),
            (KeyCode::Down, _) => *scroll_offset = scroll_offset.saturating_sub(1),
            _ => {}
        },
    }
    if return_plan_review {
        if let Some(Modal::Model {
            target: crate::modal::ModelTarget::PlanReview(state),
            ..
        }) = app.modal.take()
        {
            app.modal = Some(Modal::Plan { state });
        }
    } else if return_memory_list {
        if let Some(Modal::MemoryEditor {
            root_uri,
            entries,
            picker,
            ..
        }) = app.modal.take()
        {
            app.modal = Some(Modal::Memories {
                root_uri,
                entries,
                picker,
            });
        }
    } else if close {
        app.modal = None;
        if !plan_review_open {
            crate::queue::dispatch(app, connection);
        }
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
        tokio::sync::oneshot::Sender<
            Result<moc_protocol::methods::AskResult, moc_protocol::RpcError>,
        >,
    >,
) -> bool {
    let answers = params
        .questions
        .iter()
        .enumerate()
        .map(
            |(index, current_question)| moc_protocol::methods::AskAnswer {
                id: current_question.id.clone(),
                selected: selected.get(index).cloned().unwrap_or_default(),
                custom: (!custom.is_empty() && index == *question).then(|| custom.to_owned()),
            },
        )
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

fn confirm_plan(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    if session.mode != SessionMode::Plan {
        app.set_notice("只有计划模式可以重新审阅计划");
        return;
    }
    let session_id = session.id.clone();
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = async {
            let loaded = client
                .call::<SessionLoad>(SessionLoadParams {
                    session_id,
                    max_entries: None,
                })
                .await?;
            anyhow::ensure!(
                loaded.session.mode == SessionMode::Plan,
                "会话已离开计划模式"
            );
            let plan = loaded
                .plans
                .into_iter()
                .rev()
                .find(|plan| plan.status == moc_protocol::methods::PlanStatus::Submitted)
                .ok_or_else(|| anyhow::anyhow!("当前会话没有已写入的待审计划"))?;
            anyhow::ensure!(
                !plan.markdown.trim().is_empty()
                    && plan.path
                        == format!("local://{}/plans/{}.md", loaded.session.id, plan.plan_id),
                "待审计划的正文或实体 URI 无效"
            );
            Ok(plan)
        }
        .await;
        let _ = events.send(AppEvent::PlanReview(result));
    });
}

fn open_plan_review(app: &mut App, plan: moc_protocol::methods::PlanInfo) {
    let Some(session) = app.session.as_ref() else {
        return;
    };
    app.modal = Some(Modal::Plan {
        state: crate::modal::PlanReviewState::new(plan, session.model.clone()),
    });
}

fn apply_plan_review(app: &mut App, result: anyhow::Result<moc_protocol::methods::PlanInfo>) {
    match result {
        Ok(plan)
            if app.session.as_ref().is_some_and(|session| {
                session.mode == SessionMode::Plan
                    && plan.path == format!("local://{}/plans/{}.md", session.id, plan.plan_id)
            }) =>
        {
            open_plan_review(app, plan)
        }
        Ok(_) => app.set_notice("当前会话已变化，不能打开该计划"),
        Err(error) => app.set_notice(format!("无法审阅计划：{error}")),
    }
}

pub(crate) fn apply_plan_approval(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<moc_protocol::methods::SessionPlanApproveResult>,
) {
    match result {
        Ok(result) => {
            app.modal = None;
            if app
                .session
                .as_ref()
                .is_some_and(|session| session.id == result.session_id)
            {
                start_plan_execution(app, connection, result.auto_prompt);
            } else {
                let client = connection.client.clone();
                let events = app.events.clone();
                tokio::spawn(async move {
                    let loaded = client
                        .call::<SessionLoad>(SessionLoadParams {
                            session_id: result.session_id,
                            max_entries: None,
                        })
                        .await
                        .map_err(anyhow::Error::from);
                    let _ = events.send(AppEvent::PlanExecutionLoaded {
                        result: loaded,
                        auto_prompt: result.auto_prompt,
                    });
                });
            }
        }
        Err(error) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.submitting = false;
                state.error = Some(error.to_string());
            } else {
                app.set_notice(format!("计划审批失败：{error}"));
            }
        }
    }
}

pub(crate) fn apply_plan_execution_loaded(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<SessionLoadResult>,
    auto_prompt: Option<String>,
) {
    match result {
        Ok(loaded) => {
            let cwd = loaded.session.cwd.clone();
            let id = loaded.session.id.clone();
            let before = loaded.entries.first().map(|entry| entry.id.clone());
            app.switch_session(
                loaded.session,
                crate::transcript_restore::blocks_from_entries_with_children(
                    &loaded.entries,
                    &loaded.child_calls,
                ),
            );
            app.todos = loaded.todos;
            app.set_history_cursor(loaded.truncated, before);
            refresh_git_branch(app, &cwd);
            refresh_context(app, connection, false);
            load_subagents_for_parent(app, &connection.client, Some(id));
            start_plan_execution(app, connection, auto_prompt);
        }
        Err(error) => app.set_notice(format!(
            "计划已批准，但加载执行会话失败：{error}；可通过 /resume 恢复"
        )),
    }
}

fn start_plan_execution(
    app: &mut App,
    connection: &EmbeddedConnection,
    auto_prompt: Option<String>,
) {
    let Some(prompt) = auto_prompt else {
        return;
    };
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    app.push_user(prompt.clone());
    app.inflight_prompts.insert(session_id.clone());
    app.prompt_running = true;
    crate::queue::send_prompt(
        &connection.client,
        session_id,
        vec![InputPart::Text { text: prompt }],
        None,
        app.events.clone(),
    );
}

fn apply_update_and_refresh(
    app: &mut App,
    connection: &EmbeddedConnection,
    session_id: String,
    update: SessionUpdate,
) {
    if app
        .session
        .as_ref()
        .is_some_and(|session| session.id != session_id)
        && let Some(view) = app.subagents.get_mut(&session_id)
    {
        if let SessionUpdate::AgentStatusChanged {
            status,
            last_message,
            ..
        } = &update
        {
            view.set_status(*status, last_message.clone());
        } else {
            view.apply(update);
        }
        app.subagent_generation = app.subagent_generation.wrapping_add(1);
        return;
    }
    if let SessionUpdate::AgentStarted { agent } = &update {
        app.subagents
            .entry(agent.id.clone())
            .or_insert_with(|| crate::subagent::SubagentView::new(agent.clone()));
        app.subagent_generation = app.subagent_generation.wrapping_add(1);
    }
    if let SessionUpdate::AgentStatusChanged {
        agent_id,
        status,
        last_message,
    } = &update
        && let Some(view) = app.subagents.get_mut(agent_id)
    {
        view.set_status(*status, last_message.clone());
        app.subagent_generation = app.subagent_generation.wrapping_add(1);
    }
    let prompt_started = matches!(
        &update,
        SessionUpdate::PromptStarted { .. } | SessionUpdate::BashStarted { .. }
    );
    let refresh = matches!(
        &update,
        SessionUpdate::PromptFinished { .. }
            | SessionUpdate::ModeChanged { .. }
            | SessionUpdate::ModelChanged { .. }
            | SessionUpdate::PermissionChanged { .. }
            | SessionUpdate::ContextCompacted { .. }
    ) || matches!(&update, SessionUpdate::ToolFinished { prompt_id, .. } if prompt_id.starts_with("bash_"));
    apply_update(app, update);
    if prompt_started && app.cancel_requested {
        cancel_prompt(&connection.client, app);
    }
    if refresh {
        refresh_context(app, connection, false);
    }
}

/// 把协议更新折叠进块序列：流式增量续写活动块，工具调用按 ID 归组。
fn apply_update(app: &mut App, update: SessionUpdate) {
    use crate::transcript::Block;
    match update {
        SessionUpdate::AgentStarted { .. } | SessionUpdate::AgentStatusChanged { .. } => {}
        SessionUpdate::ToolBatchStarted {
            batch_id, calls, ..
        } => {
            app.register_tool_batch(batch_id, calls.into_iter().map(|call| call.tool_call_id));
        }
        SessionUpdate::GenerationStarted { .. } => app.start_generation(),
        SessionUpdate::GuardNotice {
            message, terminal, ..
        } => {
            if terminal {
                app.discard_generation();
            }
            app.push_note(message.clone(), terminal);
            app.set_warning(message);
        }
        SessionUpdate::AssistantDelta { delta, .. } => match delta {
            moc_protocol::methods::AssistantDelta::Text { delta } => {
                app.finish_thinking();
                if !delta.is_empty()
                    && !app.has_streaming(|block| matches!(block, Block::Assistant { .. }))
                {
                    app.push_block(Block::Assistant {
                        text: String::new(),
                    });
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
                    app.thinking_started_at = Some(std::time::Instant::now());
                    app.push_block(Block::Thinking {
                        text: String::new(),
                        reasoning_tokens: None,
                        duration_ms: None,
                    });
                }
                app.append_streaming_limited(
                    |block| match block {
                        Block::Thinking { text, .. } => Some(text),
                        _ => None,
                    },
                    &delta,
                    128 * 1024,
                    "\n\n[思考内容过长，已停止在界面展开]\n",
                );
            }
            moc_protocol::methods::AssistantDelta::ToolCallStarted {
                tool_call_id: _,
                name: _,
            } => {}
            moc_protocol::methods::AssistantDelta::ToolCallArgs {
                tool_call_id,
                delta,
            } => {
                if let Some(entry) = app.tool_entry_mut(&tool_call_id) {
                    entry.push_args(&delta);
                } else {
                    app.push_pending_args(&tool_call_id, delta);
                }
            }
            moc_protocol::methods::AssistantDelta::Usage { usage } => app.apply_usage(usage),
        },
        SessionUpdate::PtcChildStarted {
            tool_call_id,
            name,
            parent_tool_call_id,
            ..
        } => app.start_child_tool_block(tool_call_id, name, parent_tool_call_id),
        SessionUpdate::BashStarted {
            tool_call_id,
            command,
        } => {
            app.start_tool_block(tool_call_id.clone(), "bash".into());
            if let Some(entry) = app.tool_entry_mut(&tool_call_id) {
                entry.push_args(&serde_json::to_string(&command).expect("Bash 参数可序列化"));
            }
        }
        SessionUpdate::PtcChildFinished {
            tool_call_id,
            status,
            result,
            duration_ms,
            diff,
            ..
        } => app.finish_tool_block(tool_call_id, status, result, duration_ms, diff),
        SessionUpdate::ToolStarted {
            tool_call_id, name, ..
        } => {
            app.commit_generation();
            app.start_tool_block(tool_call_id, name);
        }
        SessionUpdate::ToolFinished {
            tool_call_id,
            status,
            result,
            duration_ms,
            diff,
            ..
        } => app.finish_tool_block(tool_call_id, status, result, duration_ms, diff),
        SessionUpdate::PromptStarted { .. } => {
            app.prompt_running = true;
            app.ctrl_c_pending = None;
            app.prompt_started_at = Some(std::time::Instant::now());
            if !app.cancel_requested {
                app.notice = None;
                app.warning = None;
            }
        }
        SessionUpdate::PromptFinished { .. } => app.finish_prompt(),
        SessionUpdate::TitleChanged { title } => {
            if let Some(session) = app.session.as_mut() {
                session.title = title;
            }
        }
        SessionUpdate::ModeChanged { mode } => {
            if let Some(session) = app.session.as_mut() {
                session.mode = mode;
            }
            if mode == SessionMode::ClmExperimental {
                app.set_notice(
                    "CLM 实验模式已启用：模型将维护实时上下文；本会话使用后不可退出 CLM",
                );
            }
        }
        SessionUpdate::ModelChanged { model } => {
            if let Some(session) = app.session.as_mut() {
                session.model = model;
            }
        }
        SessionUpdate::PermissionChanged { permission } => {
            if let Some(session) = app.session.as_mut() {
                session.permission = permission;
            }
        }
        SessionUpdate::TodoChanged { phases } => app.update_todos(phases),
        SessionUpdate::PlanSubmitted {
            plan_id,
            title,
            path,
            markdown,
        } => {
            open_plan_review(
                app,
                moc_protocol::methods::PlanInfo {
                    plan_id,
                    title,
                    path,
                    markdown,
                    status: moc_protocol::methods::PlanStatus::Submitted,
                },
            );
        }
        SessionUpdate::ContextCompacted {
            tokens_before,
            tokens_after,
            ..
        } => {
            app.push_note(
                format!(
                    "上下文已压缩：{} → {}",
                    crate::render::tokens_k(tokens_before),
                    crate::render::tokens_k(tokens_after)
                ),
                false,
            );
        }
        SessionUpdate::JobFinished {
            command, exit_code, ..
        } => {
            let code = exit_code.map_or_else(String::new, |code| format!("（退出码 {code}）"));
            app.push_note(format!("后台任务结束：{command}{code}"), false);
        }
        SessionUpdate::WorkspaceChanged { .. } => {
            if let Some(session) = app.session.as_ref() {
                refresh_git_branch(app, &session.cwd);
            }
        }
        SessionUpdate::PermissionRequested { .. } => {}
        SessionUpdate::PermissionResolved { .. } => {}
        SessionUpdate::LockChanged { .. } => {}
        SessionUpdate::UpstreamMismatch { fields, .. } => {
            let detail = fields
                .iter()
                .map(|mismatch| match mismatch.field {
                    UpstreamField::Model => {
                        format!("model {} → {}", mismatch.requested, mismatch.reported)
                    }
                    UpstreamField::Effort => {
                        format!("effort {} → {}", mismatch.requested, mismatch.reported)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ｜ ");
            app.set_warning(detail);
        }
        SessionUpdate::Error {
            message, retryable, ..
        } => {
            app.push_note(
                format!(
                    "错误：{message}{}",
                    if retryable { "（可以重试）" } else { "" }
                ),
                true,
            );
        }
    }
}

/// 发出取消；core 会终止本次提示并给未执行的工具调用补 Cancelled 结果。
pub(crate) fn cancel_prompt(client: &ClientHandle, app: &App) {
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

/// ctrl+v：异步读剪贴板（图片优先，其次文本），完成后经事件队列回主循环。
/// 两条通道并行探测，图片命中时不留文本读取的等待。
fn spawn_clipboard_paste(events: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let (image, text) = tokio::join!(
            crate::clipboard::read_clipboard_image(),
            crate::clipboard::read_clipboard_text()
        );
        let payload = match image {
            Some(image) => Some(ClipboardPayload::Image {
                media_type: image.media_type,
                data: crate::clipboard::base64_encode(&image.bytes),
            }),
            None => text.map(ClipboardPayload::Text),
        };
        let _ = events.send(AppEvent::ClipboardPaste(payload));
    });
}

fn format_usage(result: &moc_protocol::SessionUsageResult) -> String {
    result
        .models
        .iter()
        .chain(std::iter::once(&result.total))
        .map(|summary| {
            let cache = summary.cached_input_tokens.unwrap_or(0);
            let cache_rate = if summary.input_tokens == 0 {
                0.0
            } else {
                cache as f64 * 100.0 / summary.input_tokens as f64
            };
            format!(
                "{}：{} 次，输入 {}，输出 {}，缓存 {:.1}%，思考 {}，激活 {}，TPS {:.2}",
                summary.model,
                summary.calls,
                summary.input_tokens,
                summary.output_tokens,
                cache_rate,
                summary
                    .reasoning_tokens
                    .map_or_else(|| "未知".into(), |value| value.to_string()),
                format_duration(summary.active_ms),
                tokens_per_second(summary.output_tokens, summary.active_ms),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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

/// 用户展示文本：图片 chip 显示为占位说明。
pub(crate) fn display_text_of(parts: &[InputPart]) -> String {
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
        "/confirm-plan" => confirm_plan(app, connection),
        "/agents" => open_subagents(app, connection),
        "/agent" => {
            let mut parts = rest.trim().splitn(2, ' ');
            let model = parts.next().unwrap_or("inherit");
            let task = parts.next().unwrap_or_default().trim();
            if task.is_empty() {
                app.set_notice("用法：/agent [inherit|default|explore|plan] <task>");
                return;
            }
            let Some(parent_session_id) = session_id.clone() else {
                app.set_notice("当前没有活动会话");
                return;
            };
            let client = connection.client.clone();
            let events = app.events.clone();
            let task = task.to_owned();
            let model = (model != "inherit").then(|| model.to_owned());
            tokio::spawn(async move {
                let result = client
                    .call::<moc_protocol::methods::AgentSpawn>(
                        moc_protocol::methods::AgentSpawnParams {
                            parent_session_id,
                            task,
                            title: None,
                            model,
                        },
                    )
                    .await
                    .map(|result| crate::subagent::AgentOperationNotice {
                        agent_id: result.agent.id,
                        message: "子代理已创建".into(),
                    })
                    .map_err(anyhow::Error::from);
                let _ = events.send(AppEvent::SubagentOperation(result));
            });
        }
        "/steer-agent" | "/stop-agent" => {
            let mut parts = rest.trim().splitn(2, ' ');
            let Some(agent_id) = parts.next().filter(|value| !value.is_empty()) else {
                app.set_notice("用法：/steer-agent <id> <message> 或 /stop-agent <id>");
                return;
            };
            if name == "/stop-agent" {
                stop_agent_operation(app, connection, agent_id.to_owned());
            } else if let Some(message) = parts.next().filter(|value| !value.trim().is_empty()) {
                send_agent_operation(
                    app,
                    connection,
                    agent_id.to_owned(),
                    message.to_owned(),
                    true,
                );
            } else {
                app.set_notice("用法：/steer-agent <id> <message>");
            }
        }
        "/config" | "/settings" => crate::settings::open(app, connection),
        "/new" => {
            let client = connection.client.clone();
            let events = app.events.clone();
            let (cwd, model, mode) = app
                .session
                .as_ref()
                .map(|session| {
                    (
                        Some(session.cwd.clone()),
                        Some(session.model.clone()),
                        session.mode,
                    )
                })
                .unwrap_or((None, None, SessionMode::Normal));
            tokio::spawn(async move {
                match client
                    .call::<SessionNew>(SessionNewParams {
                        cwd,
                        model,
                        title: None,
                        mode,
                    })
                    .await
                {
                    Ok(info) => {
                        let _ = events.send(AppEvent::Loaded(info, Vec::new(), false, None));
                    }
                    Err(error) => {
                        let _ = events.send(AppEvent::Notice(format!("新建会话失败：{error}")));
                    }
                }
            });
        }
        "/memories" => {
            let root_uri = if rest.trim().is_empty() {
                "viking://~/memories/".to_owned()
            } else {
                rest.trim().replace("ov://", "viking://")
            };
            app.modal = Some(Modal::Memories {
                root_uri: root_uri.clone(),
                entries: Vec::new(),
                picker: crate::modal::PickerState {
                    loading: true,
                    ..Default::default()
                },
            });
            let client = connection.client.clone();
            let events = app.events.clone();
            tokio::spawn(async move {
                match client
                    .call::<OpenVikingList>(OpenVikingListParams {
                        uri: root_uri,
                        recursive: false,
                        limit: 128,
                    })
                    .await
                {
                    Ok(result) => {
                        let _ = events.send(AppEvent::Memories(result.entries));
                    }
                    Err(error) => {
                        let _ =
                            events.send(AppEvent::PickerError(format!("读取记忆失败：{error}")));
                    }
                }
            });
        }
        "/usage" => {
            let Some(session_id) = session_id else {
                app.set_notice("当前没有活动会话");
                return;
            };
            let client = connection.client.clone();
            let events = app.events.clone();
            tokio::spawn(async move {
                match client
                    .call::<SessionUsage>(SessionUsageParams { session_id })
                    .await
                {
                    Ok(result) => {
                        let _ = events.send(AppEvent::Usage(result));
                    }
                    Err(error) => {
                        let _ = events.send(AppEvent::Notice(format!("读取用量失败：{error}")));
                    }
                }
            });
        }
        "/help" => app.modal = Some(Modal::Help),
        "/compact" => {
            if let Some(session_id) = session_id {
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
            } else {
                app.set_notice("当前没有活动会话");
            }
        }
        "/mode" => cycle_mode(app, connection),
        "/clm-experimental" => set_mode(app, connection, SessionMode::ClmExperimental),
        "/permission" => set_permission(app, connection, rest),
        "/ask" => ask_once(app, connection, rest),
        "/model" => app.open_model_picker(),
        "/resume" => open_session_picker(app, connection),
        other => app.set_notice(format!("未知命令：{other}")),
    }
}

/// 循环切换模式：Normal → Plan → PTC。
fn cycle_mode(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    let index = MODES
        .iter()
        .position(|mode| *mode == session.mode)
        .unwrap_or(0);
    set_mode(app, connection, MODES[(index + 1) % MODES.len()]);
}

/// `/ask <问题>`：仅本条消息进入问答模式（只读），不改变会话模式。
///
/// 该次提示无论当前是否空闲都排进队列：模式覆盖是提示级属性，无法注入正在飞行的提示。
fn ask_once(app: &mut App, connection: &EmbeddedConnection, rest: &str) {
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
        SessionMode::Ask,
    );
}

/// 请求 core 切换会话模式（模式循环与就地菜单共用）。
fn set_mode(app: &mut App, connection: &EmbeddedConnection, mode: SessionMode) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<SessionSetMode>(SessionSetModeParams { session_id, mode })
            .await
        {
            Ok(_) => {}
            Err(error) => {
                let _ = events.send(AppEvent::Notice(format!("切换模式失败：{error}")));
            }
        }
    });
}

fn set_permission(app: &mut App, connection: &EmbeddedConnection, rest: &str) {
    let permission = match rest.trim() {
        "full" => SessionPermission::Full,
        "workspace" => SessionPermission::Workspace,
        _ => {
            app.set_notice("用法：/permission <full|workspace>");
            return;
        }
    };
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
            })
            .await
        {
            Ok(_) => {}
            Err(error) => {
                let _ = events.send(AppEvent::Notice(format!("切换权限失败：{error}")));
            }
        }
    });
}

/// 就地模式菜单的键盘处理：`↑↓` 选择、`Enter` 应用、`Esc` 关闭。
///
/// 与斜杠补全菜单同一套约定：只认方向键与确认键，其他按键先关掉菜单再照常处理，
/// 菜单不会吞掉正常输入与快捷键。
fn handle_mode_menu_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) -> bool {
    let Some(menu) = app.mode_menu.as_ref() else {
        return false;
    };
    let count = MODES.len();
    let selected = menu.selected.min(count - 1);
    match (code, modifiers) {
        (KeyCode::Esc, _) => {
            app.mode_menu = None;
            true
        }
        (KeyCode::Up, _) => {
            if let Some(menu) = app.mode_menu.as_mut() {
                menu.selected = (selected + count - 1) % count;
            }
            true
        }
        (KeyCode::Down, _) => {
            if let Some(menu) = app.mode_menu.as_mut() {
                menu.selected = (selected + 1) % count;
            }
            true
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            app.mode_menu = None;
            set_mode(app, connection, MODES[selected]);
            true
        }
        _ => {
            app.mode_menu = None;
            false
        }
    }
}

/// 按当前模型声明的思考等级循环切换。
fn cycle_thinking_level(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    let (base, current) = session
        .model
        .rsplit_once(':')
        .unwrap_or((session.model.as_str(), "off"));
    let levels = app
        .models
        .iter()
        .find(|model| model.id == base)
        .and_then(|model| model.thinking_levels.clone())
        .unwrap_or_else(|| {
            ["off", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        });
    let index = levels
        .iter()
        .position(|level| level == current)
        .unwrap_or(0);
    let next = levels[(index + 1) % levels.len()].clone();
    let selector = format!("{base}:{next}");
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let events = app.events.clone();
    app.inflight_session_ops.insert(session_id.clone());
    tokio::spawn(async move {
        let outcome = client
            .call::<SessionSetModel>(SessionSetModelParams {
                session_id: session_id.clone(),
                selector,
                overflow: SessionModelOverflowAction::Switch,
            })
            .await;
        let _ = events.send(match outcome {
            Ok(_) => AppEvent::SessionControlSettled {
                session_id,
                notice: Some(format!("思考等级：{next}")),
                result: Ok(None),
            },
            Err(error) => AppEvent::SessionControlSettled {
                session_id,
                notice: None,
                result: Err(format!("切换思考等级失败：{error}")),
            },
        });
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
    let (start_row, end_row, start_col, end_col) = if sy < ey {
        (sy, ey, sx, ex)
    } else if sy > ey {
        (ey, sy, ex, sx)
    } else if sx <= ex {
        (sy, ey, sx, ex)
    } else {
        (sy, ey, ex, sx)
    };
    let area_x = app.scrollback_area.x;
    let start_col = usize::from(start_col.saturating_sub(area_x));
    let end_col = usize::from(end_col.saturating_sub(area_x));
    let mut text = String::new();
    for &(row, ref line_text) in &app.paint_lines {
        if row < start_row || row > end_row {
            continue;
        }
        if start_row == end_row {
            text.push_str(&slice_display_columns(line_text, start_col, end_col));
        } else if row == start_row {
            text.push_str(&slice_display_columns(line_text, start_col, usize::MAX));
        } else if row == end_row {
            text.push_str(&slice_display_columns(line_text, 0, end_col));
        } else {
            text.push_str(line_text);
        }
        text.push('\n');
    }
    let body = text.trim_end().to_owned();
    if body.is_empty() {
        return;
    }
    write_osc52(&body);
    tokio::spawn(async move {
        match crate::clipboard::copy_text_to_clipboard(&body).await {
            Ok(()) => tracing::debug!("选区已复制"),
            Err(error) => tracing::debug!(%error, "复制选区失败"),
        }
    });
    app.selection = None;
    app.set_notice("已复制选中内容");
}

/// 通过 OSC52 把文本交给终端自身的剪贴板；与绘制同线程写出，不会与帧交错。
fn write_osc52(text: &str) {
    use std::io::{IsTerminal, Write};
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return;
    }
    let _ = stdout.write_all(crate::clipboard::osc52_sequence(text).as_bytes());
    let _ = stdout.flush();
}

fn slice_display_columns(text: &str, from: usize, to: usize) -> String {
    let mut output = String::new();
    let mut column = 0usize;
    for ch in text.chars() {
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        let next = column.saturating_add(width);
        if next > from && column < to {
            output.push(ch);
        }
        column = next;
        if column >= to {
            break;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use moc_protocol::methods::UpstreamFieldMismatch;
    use tokio::sync::mpsc::unbounded_channel;

    fn app() -> App {
        let (events, _receiver) = unbounded_channel::<AppEvent>();
        App::new(Theme::global(), events)
    }

    #[tokio::test]
    async fn input_bang_commands_render_results_and_remain_out_of_agent_context_when_private() {
        use crate::transcript::Block;
        use moc_core::{Core, CoreConfig};
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config.toml"), "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:1/v1'\n[providers.test.models.model]\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n").unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
        })
        .unwrap();
        let (events, mut receiver) = unbounded_channel();
        let mut connection = connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                mode: SessionMode::Normal,
            })
            .await
            .unwrap();
        let id = session.id.clone();
        let mut app = App::new(Theme::global(), events);
        app.session = Some(session);
        app.tools_collapsed = false;
        // 长粘贴经 chip 展开，再作为多行 Bash 脚本执行。
        app.prompt.insert_str_raw("!");
        app.prompt.insert_text("printf 'TUI_PUBLIC\\n'\nprintf 'TUI_SECOND\\n'\nprintf 'TUI_THIRD\\n'\nprintf 'TUI_FOURTH\\n'");
        assert!(app.prompt.text.contains("[paste#"));
        commit_prompt(&mut app, &connection);
        // 命令在运行时再次提交 `!!` 必须排队，不能作为 steering 注入模型。
        app.prompt
            .insert_text("!!printf TUI_PRIVATE\nprintf done > tui-marker\n:\n:");
        commit_prompt(&mut app, &connection);
        assert_eq!(app.queues[&id].len(), 1);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut completed = 0;
            while completed < 2 {
                let event = receiver.recv().await.unwrap();
                if let AppEvent::PromptReturned { result, .. } = &event {
                    result.as_ref().unwrap();
                    completed += 1;
                }
                while let Ok(update) = connection.updates.try_recv() {
                    apply_update(&mut app, update.update);
                }
                match event {
                    event @ (AppEvent::PromptReturned { .. } | AppEvent::SteerReturned { .. }) => {
                        crate::interaction::handle_event(&mut app, &connection, event)
                    }
                    _ => {}
                }
            }
            while let Ok(update) = connection.updates.try_recv() {
                apply_update(&mut app, update.update);
            }
        })
        .await
        .unwrap();
        assert!(!app.prompt_running);
        assert_eq!(
            std::fs::read_to_string(root.path().join("tui-marker")).unwrap(),
            "done"
        );
        assert!(app.blocks.iter().any(|entry| matches!(&entry.block, Block::Tool(tool) if tool.result.as_deref().is_some_and(|result| result.contains("TUI_FOURTH")))));
        assert!(app.blocks.iter().any(|entry| matches!(&entry.block, Block::Tool(tool) if tool.result.as_deref() == Some("TUI_PRIVATE"))));
        assert!(
            !app.blocks
                .iter()
                .any(|entry| matches!(entry.block, Block::User { .. } | Block::Assistant { .. }))
        );
        let loaded = connection
            .client
            .call::<SessionLoad>(SessionLoadParams {
                session_id: id,
                max_entries: None,
            })
            .await
            .unwrap();
        assert!(loaded.entries.iter().any(|entry| {
            entry
                .context_message()
                .is_some_and(|message| message.text().contains("TUI_PUBLIC"))
        }));
        assert!(
            !loaded
                .entries
                .iter()
                .filter_map(|entry| entry.context_message())
                .any(|message| message.text().contains("TUI_PRIVATE"))
        );
        app.blocks = crate::transcript_restore::blocks_from_entries(&loaded.entries);
        app.view_gen += 1;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 34)).unwrap();
        let mut view = render::ScrollbackView::new();
        terminal
            .draw(|frame| render::draw(frame, &mut app, &mut view))
            .unwrap();
        let surface = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(surface.contains("! bash"));
        assert!(surface.contains("!! bash"));
        assert!(surface.contains("TUI_PUBLIC"));
        assert!(surface.contains("TUI_PRIVATE"));
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 上游改写折叠成告警文案：模型与推理强度各成一段、` ｜ ` 连接；再次不一致只替换。
    #[test]
    fn upstream_mismatch_folds_into_warning_text() {
        let mut app = app();
        apply_update(
            &mut app,
            SessionUpdate::UpstreamMismatch {
                prompt_id: "prompt_1".into(),
                fields: vec![
                    UpstreamFieldMismatch {
                        field: UpstreamField::Model,
                        requested: "glm-5.3".into(),
                        reported: "glm-5.3-air".into(),
                    },
                    UpstreamFieldMismatch {
                        field: UpstreamField::Effort,
                        requested: "max".into(),
                        reported: "high".into(),
                    },
                ],
            },
        );
        assert_eq!(
            app.warning.as_deref(),
            Some("model glm-5.3 → glm-5.3-air ｜ effort max → high")
        );
        assert!(app.warning_visible());

        // 同一次提示的下一次上游请求再次不一致：覆盖而非堆叠。
        apply_update(
            &mut app,
            SessionUpdate::UpstreamMismatch {
                prompt_id: "prompt_1".into(),
                fields: vec![UpstreamFieldMismatch {
                    field: UpstreamField::Effort,
                    requested: "high".into(),
                    reported: "medium".into(),
                }],
            },
        );
        assert_eq!(app.warning.as_deref(), Some("effort high → medium"));
    }

    #[tokio::test]
    async fn closing_modal_resumes_queue_after_turn_finishes() {
        use moc_core::{Core, CoreConfig};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "data: {\"id\":\"reply\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"队列已恢复\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                "text/event-stream",
            ))
            .expect(1)
            .mount(&upstream)
            .await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config.toml"), format!(
            "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='{}'\n[providers.test.models.model]\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
            upstream.uri(),
        )).unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
        })
        .unwrap();
        let (events, mut receiver) = unbounded_channel();
        let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                mode: SessionMode::Normal,
            })
            .await
            .unwrap();
        let id = session.id.clone();
        app.session = Some(session);
        app.prompt_running = true;
        app.inflight_prompts.insert(id.clone());
        app.modal = Some(Modal::Help);
        app.prompt.insert_str_raw("弹窗关闭后继续执行");
        crate::queue::submit(&mut app, &connection, true);
        crate::interaction::handle_event(
            &mut app,
            &connection,
            AppEvent::PromptReturned {
                session: id.clone(),
                result: Ok(()),
            },
        );
        assert_eq!(app.queues[&id].len(), 1, "弹窗打开期间应保留队列");
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let AppEvent::PromptReturned { result, .. } = receiver.recv().await.unwrap() {
                    result.unwrap();
                    break;
                }
            }
        })
        .await
        .expect("关闭弹窗应派发队列，而不是等待下一次用户输入");
        let loaded = connection
            .client
            .call::<SessionLoad>(SessionLoadParams {
                session_id: id,
                max_entries: None,
            })
            .await
            .unwrap();
        assert_eq!(loaded.entries[0].message.text(), "弹窗关闭后继续执行");
        assert_eq!(loaded.entries[1].message.text(), "队列已恢复");
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 模型选择器回报容量缺口时渲染压缩确认卡片，确认后先切换再由新模型压缩。
    #[tokio::test]
    async fn model_picker_asks_before_compacting() {
        use moc_core::{Core, CoreConfig};
        use moc_protocol::{PromptParams, SessionPrompt};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(|request: &wiremock::Request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("请求 JSON");
                if body["stream"] == true {
                    ResponseTemplate::new(200).set_body_raw(
                        "data: {\"id\":\"reply\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"答复\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    )
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "id": "summary",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "检查点"},
                            "finish_reason": "stop"
                        }]
                    }))
                }
            })
            .mount(&upstream)
            .await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config.toml"), format!(
            "[models.roles]\ndefault='test/model:off'\n[providers.test]\napi='openai_chat'\nbase_url='{}'\n[providers.test.models.model]\ncontext_window=100000\n[providers.test.models.tiny]\ncontext_window=1000\nmax_output_tokens=4096\n[compaction]\nmethod_order=['soft']\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
            upstream.uri(),
        )).unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
        })
        .unwrap();
        let (events, mut receiver) = unbounded_channel();
        let mut connection = connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                mode: SessionMode::Normal,
            })
            .await
            .unwrap();
        let id = session.id.clone();
        app.session = Some(session);
        app.models = connection
            .client
            .call::<ConfigGet>(ConfigGetParams {})
            .await
            .unwrap()
            .models;
        // 压缩需要可摘要的历史：先跑一轮普通提示。
        connection
            .client
            .call::<SessionPrompt>(PromptParams {
                session_id: id,
                input: vec![InputPart::Text {
                    text: "你好".into(),
                }],
                mode: None,
            })
            .await
            .unwrap();

        app.open_model_picker();
        let tiny = app
            .models
            .iter()
            .position(|model| model.id == "test/tiny")
            .expect("配置里有 tiny 模型");
        let Some(Modal::Model { picker, .. }) = app.modal.as_mut() else {
            panic!("模型选择器应打开");
        };
        picker.selected = tiny;
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);

        let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(event @ AppEvent::ModelSwitchConfirm { .. }) = receiver.recv().await {
                    break event;
                }
            }
        })
        .await
        .expect("选择模型后应回报容量缺口");
        let AppEvent::ModelSwitchConfirm { overflow, .. } = &event else {
            unreachable!("循环只返回确认事件");
        };
        assert_eq!(overflow.context_window, 1000);
        crate::interaction::handle_event(&mut app, &connection, event);
        assert!(
            matches!(app.modal, Some(Modal::SwitchModel { option: 0, .. })),
            "确认卡片应打开并默认停在切换项"
        );

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).unwrap();
        let mut view = render::ScrollbackView::new();
        terminal
            .draw(|frame| render::draw(frame, &mut app, &mut view))
            .unwrap();
        let surface = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        // 宽字符在缓冲区里占两个单元，拼接后每个汉字后面留一个占位空格。
        let compact: String = surface.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(compact.contains("超过test/tiny的窗口"), "{surface}");

        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(event @ AppEvent::SessionControlSettled { .. }) = receiver.recv().await
                {
                    break event;
                }
            }
        })
        .await
        .expect("确认后应回报切换结果");
        let AppEvent::SessionControlSettled { result, .. } = &event else {
            unreachable!("循环只返回收尾事件");
        };
        assert_eq!(result.as_ref().ok().cloned().flatten(), None, "{result:?}");
        crate::interaction::handle_event(&mut app, &connection, event);
        assert!(app.modal.is_none(), "切换完成应关闭确认卡片");
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while app.session.as_ref().unwrap().model != "test/tiny:off" {
                let update = connection.updates.recv().await.unwrap();
                apply_update(&mut app, update.update);
            }
        })
        .await
        .expect("模型变化通知应写回会话模型");
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 「切换并压缩 / 手动压缩」流程的测试内核：摘要响应延迟到 300ms，
    /// 让压缩在飞期间有可观测窗口。
    struct SlowCompactionHarness {
        upstream: wiremock::MockServer,
        _root: tempfile::TempDir,
        core: std::sync::Arc<moc_core::Core>,
        connection: EmbeddedConnection,
        events: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        app: App,
        session: String,
    }

    impl SlowCompactionHarness {
        async fn new() -> Self {
            use moc_core::{Core, CoreConfig};
            use wiremock::{
                Mock, MockServer, ResponseTemplate,
                matchers::{method, path},
            };

            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(|request: &wiremock::Request| {
                    let body: serde_json::Value =
                        serde_json::from_slice(&request.body).expect("请求 JSON");
                    if body["stream"] == true {
                        ResponseTemplate::new(200).set_body_raw(
                            "data: {\"id\":\"reply\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"答复\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                            "text/event-stream",
                        )
                    } else {
                        ResponseTemplate::new(200)
                            .set_delay(std::time::Duration::from_millis(300))
                            .set_body_json(serde_json::json!({
                                "id": "summary",
                                "choices": [{
                                    "index": 0,
                                    "message": {"role": "assistant", "content": "检查点"},
                                    "finish_reason": "stop"
                                }]
                            }))
                    }
                })
                .mount(&upstream)
                .await;
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("config.toml"), format!(
                "[models.roles]\ndefault='test/model:off'\n[providers.test]\napi='openai_chat'\nbase_url='{}'\n[providers.test.models.model]\ncontext_window=100000\n[providers.test.models.tiny]\ncontext_window=1000\nmax_output_tokens=4096\n[compaction]\nauto=false\nmethod_order=['soft']\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
                upstream.uri(),
            )).unwrap();
            let core = Core::new(CoreConfig {
                home: root.path().to_owned(),
                cwd: root.path().to_owned(),
                overlays: Vec::new(),
                session_dir: Some(root.path().join("sessions")),
                client: Default::default(),
            })
            .unwrap();
            let (events, events_rx) = unbounded_channel();
            let connection = connect_embedded(core.clone(), events.clone())
                .await
                .unwrap();
            let mut app = App::new(Theme::global(), events);
            let session = connection
                .client
                .call::<SessionNew>(SessionNewParams {
                    cwd: None,
                    model: None,
                    title: None,
                    mode: SessionMode::Normal,
                })
                .await
                .unwrap();
            let id = session.id.clone();
            app.session = Some(session);
            app.models = connection
                .client
                .call::<ConfigGet>(ConfigGetParams {})
                .await
                .unwrap()
                .models;
            // 压缩需要可摘要的历史。
            connection
                .client
                .call::<moc_protocol::SessionPrompt>(moc_protocol::PromptParams {
                    session_id: id.clone(),
                    input: vec![InputPart::Text {
                        text: "你好".into(),
                    }],
                    mode: None,
                })
                .await
                .unwrap();
            Self {
                upstream,
                _root: root,
                core,
                connection,
                events: events_rx,
                app,
                session: id,
            }
        }

        /// 等到第一个满足条件的事件，丢弃无关事件。
        async fn next_event(&mut self, matches: impl Fn(&AppEvent) -> bool) -> AppEvent {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let event = self.events.recv().await.expect("事件流未关闭");
                    if matches(&event) {
                        break event;
                    }
                }
            })
            .await
            .expect("等待事件超时")
        }

        async fn settle(&mut self) -> AppEvent {
            self.next_event(|event| matches!(event, AppEvent::SessionControlSettled { .. }))
                .await
        }

        /// 提交一条普通消息；返回它是否停在队列里（而不是立刻发出）。
        fn submit_message(&mut self, text: &str) -> bool {
            self.app.prompt.insert_str_raw(text);
            crate::queue::submit(&mut self.app, &self.connection, false);
            !self.app.prompt_running
                && !self.app.inflight_prompts.contains(&self.session)
                && self
                    .app
                    .queues
                    .get(&self.session)
                    .is_some_and(|queue| !queue.is_empty())
        }

        async fn transcript_contains(&self, needle: &str) -> bool {
            let loaded = self
                .connection
                .client
                .call::<SessionLoad>(SessionLoadParams {
                    session_id: self.session.clone(),
                    max_entries: None,
                })
                .await
                .unwrap();
            loaded.entries.iter().any(|entry| {
                entry
                    .context_message()
                    .is_some_and(|message| message.text().contains(needle))
            })
        }

        async fn shutdown(self) {
            self.connection.shutdown().await;
            self.core.shutdown().await;
        }
    }

    /// 压缩在飞时发出的消息排队等待，切换收尾后打到新模型，而不是撞出 `-32011`。
    #[tokio::test]
    async fn model_switch_confirmation_queues_messages_until_switch_settles() {
        let mut h = SlowCompactionHarness::new().await;
        h.app.open_model_picker();
        let tiny = h
            .app
            .models
            .iter()
            .position(|model| model.id == "test/tiny")
            .expect("配置里有 tiny 模型");
        let Some(Modal::Model { picker, .. }) = h.app.modal.as_mut() else {
            panic!("模型选择器应打开");
        };
        picker.selected = tiny;
        handle_modal_key(
            &mut h.app,
            &h.connection,
            KeyModifiers::NONE,
            KeyCode::Enter,
        );

        let event = h
            .next_event(|event| matches!(event, AppEvent::ModelSwitchConfirm { .. }))
            .await;
        crate::interaction::handle_event(&mut h.app, &h.connection, event);
        assert!(matches!(
            h.app.modal,
            Some(Modal::SwitchModel { option: 0, .. })
        ));
        handle_modal_key(
            &mut h.app,
            &h.connection,
            KeyModifiers::NONE,
            KeyCode::Enter,
        );
        assert!(
            h.app.inflight_session_ops.contains(&h.session),
            "切换压缩在飞时应标记会话忙"
        );

        // 关掉卡片再发消息：会话仍忙，消息必须留在队列里。
        handle_modal_key(&mut h.app, &h.connection, KeyModifiers::NONE, KeyCode::Esc);
        assert!(h.app.modal.is_none(), "Esc 关闭确认卡片");
        assert!(
            h.submit_message("压缩期间的消息"),
            "压缩在飞时消息应排队而不是报错"
        );
        assert!(
            !h.transcript_contains("压缩期间的消息").await,
            "压缩结束前不应发送该消息"
        );

        let event = h.settle().await;
        crate::interaction::handle_event(&mut h.app, &h.connection, event);
        assert!(
            h.app
                .queues
                .get(&h.session)
                .is_none_or(|queue| queue.is_empty()),
            "收尾后应立即派发队列"
        );
        assert!(h.app.prompt_running && h.app.inflight_prompts.contains(&h.session));
        let event = h
            .next_event(|event| matches!(event, AppEvent::PromptReturned { .. }))
            .await;
        crate::interaction::handle_event(&mut h.app, &h.connection, event);
        assert!(!h.app.prompt_running);
        assert!(h.transcript_contains("压缩期间的消息").await);

        let requests = h.upstream.received_requests().await.unwrap();
        assert!(
            requests.iter().any(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                body["model"] == "tiny"
                    && body["stream"] == true
                    && body.to_string().contains("压缩期间的消息")
            }),
            "排队消息应打到切换后的模型"
        );
        h.shutdown().await;
    }

    /// `/compact` 在飞时同样让消息排队，压缩收尾后再派发。
    #[tokio::test]
    async fn manual_compaction_queues_messages_until_settled() {
        let mut h = SlowCompactionHarness::new().await;
        execute_command(&mut h.app, &h.connection, "/compact".into());
        assert!(
            h.app.inflight_session_ops.contains(&h.session),
            "手动压缩在飞时应标记会话忙"
        );
        assert!(
            h.submit_message("压缩期间的消息"),
            "手动压缩在飞时消息应排队而不是报错"
        );

        let event = h.settle().await;
        crate::interaction::handle_event(&mut h.app, &h.connection, event);
        assert_eq!(h.app.notice.as_deref(), Some("上下文压缩已完成"));
        assert!(h.app.prompt_running && h.app.inflight_prompts.contains(&h.session));
        let event = h
            .next_event(|event| matches!(event, AppEvent::PromptReturned { .. }))
            .await;
        crate::interaction::handle_event(&mut h.app, &h.connection, event);
        assert!(h.transcript_contains("压缩期间的消息").await);
        h.shutdown().await;
    }

    /// 提问预览的滚轮键位：PgUp/PgDn 改预览偏移，切选项或切问题归零。
    #[tokio::test]
    async fn ask_preview_keys_scroll_and_reset_on_focus_change() {
        use moc_core::{Core, CoreConfig};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
        })
        .unwrap();
        let (events, _receiver) = unbounded_channel();
        let connection = connect_embedded(core.clone(), events).await.unwrap();
        let mut app = app();
        let question = || moc_protocol::methods::AskQuestion {
            id: "layout".into(),
            question: "选哪个布局？".into(),
            header: None,
            multi: false,
            recommended: None,
            options: (0..2)
                .map(|index| moc_protocol::methods::AskOption {
                    label: format!("选项{index}"),
                    description: None,
                    preview: Some("```\n+--+\n```".into()),
                })
                .collect(),
        };
        app.modal = Some(Modal::Ask {
            params: moc_protocol::methods::AskParams {
                session_id: "ses_1".into(),
                prompt_id: "prompt_1".into(),
                request_id: "ask_1".into(),
                questions: vec![question(), question()],
            },
            question: 0,
            highlight: vec![0, 0],
            selected: vec![Vec::new(), Vec::new()],
            custom: String::new(),
            custom_cursor: 0,
            editing_custom: false,
            preview_scroll: 0,
            reply: None,
        });

        let preview_scroll = |app: &App| match app.modal.as_ref() {
            Some(Modal::Ask { preview_scroll, .. }) => *preview_scroll,
            other => panic!(
                "提问模态应保持打开：{}",
                other.map_or_else(|| "无模态".to_owned(), Modal::title)
            ),
        };

        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageDown);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageDown);
        assert_eq!(preview_scroll(&app), 2 * ASK_PREVIEW_SCROLL_STEP);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageUp);
        assert_eq!(preview_scroll(&app), ASK_PREVIEW_SCROLL_STEP);

        // 切聚焦项 / 切问题：偏移归零，避免把上一项的滚动位置带到下一项。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        assert_eq!(preview_scroll(&app), 0);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageDown);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
        assert_eq!(preview_scroll(&app), 0);
        assert!(matches!(
            app.modal.as_ref(),
            Some(Modal::Ask { question: 1, .. })
        ));
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageDown);
        handle_modal_key(&mut app, &connection, KeyModifiers::SHIFT, KeyCode::BackTab);
        assert_eq!(preview_scroll(&app), 0);
        assert!(matches!(
            app.modal.as_ref(),
            Some(Modal::Ask { question: 0, .. })
        ));

        connection.shutdown().await;
        core.shutdown().await;
    }

    fn app_with_completion() -> App {
        let mut app = app();
        app.prompt.insert_str_raw("/m");
        app.completion = crate::widgets::completion::CompletionState::derive(app.prompt_text());
        app
    }

    #[test]
    fn completion_tab_accepts_the_selected_command() {
        // 初始选中第 0 项：Tab 直接接受，而不是切换焦点。
        let mut app = app_with_completion();
        assert!(handle_completion_key(
            &mut app,
            KeyModifiers::NONE,
            KeyCode::Tab
        ));
        assert_eq!(app.prompt.text, "/model ");
        assert!(app.completion.is_none());
        assert_eq!(app.focus, Focus::Prompt);

        // 方向键切换选中项后，Tab 接受当前项。
        let mut app = app_with_completion();
        assert!(handle_completion_key(
            &mut app,
            KeyModifiers::NONE,
            KeyCode::Down
        ));
        assert_eq!(
            app.completion
                .as_ref()
                .map(|completion| completion.chosen().value.as_str()),
            Some("/mode")
        );
        assert!(handle_completion_key(
            &mut app,
            KeyModifiers::NONE,
            KeyCode::Tab
        ));
        assert_eq!(app.prompt.text, "/mode ");
        assert!(app.completion.is_none());
    }

    #[test]
    fn completion_navigation_wraps_and_accepts_with_enter() {
        let mut app = app_with_completion();
        assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(0));

        // 向上循环到最后一项
        assert!(handle_completion_key(
            &mut app,
            KeyModifiers::NONE,
            KeyCode::Up
        ));
        assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(2));

        // 向下循环回到第一项
        assert!(handle_completion_key(
            &mut app,
            KeyModifiers::NONE,
            KeyCode::Down
        ));
        assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(0));

        // Enter 键确认当前选中的补全
        assert!(handle_completion_key(
            &mut app,
            KeyModifiers::NONE,
            KeyCode::Enter
        ));
        assert_eq!(app.prompt.text, "/model ");
        assert!(app.completion.is_none());
    }

    async fn setup_test_app() -> (std::sync::Arc<moc_core::Core>, EmbeddedConnection, App) {
        use moc_core::{Core, CoreConfig};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
        })
        .unwrap();
        let (events, _receiver) = unbounded_channel();
        let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                mode: SessionMode::Normal,
            })
            .await
            .unwrap();
        app.session = Some(session);
        (core, connection, app)
    }

    #[tokio::test]
    async fn terminal_event_arrows_preserve_completion_selection() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_str_raw("/co");
        app.completion = crate::widgets::completion::CompletionState::derive(app.prompt_text());
        assert!(app.completion.is_some());
        assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(0));

        // 经由终端事件向下移动选中项，不应被 prompt_changed 重置
        handle_terminal_event(
            &mut app,
            &connection,
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )),
        );
        assert_eq!(
            app.completion.as_ref().map(|c| c.selected),
            Some(1),
            "下方向键应切换到下一个候选而不被重置"
        );

        // 再次向下
        handle_terminal_event(
            &mut app,
            &connection,
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )),
        );
        assert_eq!(
            app.completion.as_ref().map(|c| c.selected),
            Some(2),
            "再次按下方向键继续切换"
        );

        // 向上移动
        handle_terminal_event(
            &mut app,
            &connection,
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )),
        );
        assert_eq!(
            app.completion.as_ref().map(|c| c.selected),
            Some(1),
            "上方向键应切换回上一项"
        );

        connection.shutdown().await;
        core.shutdown().await;
    }

    #[tokio::test]
    async fn completion_mouse_click_selects_and_accepts_candidate() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_str_raw("/co");
        let mut completion =
            crate::widgets::completion::CompletionState::derive(app.prompt_text()).unwrap();
        let menu_area = ratatui::layout::Rect::new(2, 10, 40, 6);
        completion.area = menu_area;
        completion.visible_count = 3;
        app.completion = Some(completion);

        let mouse = |kind: MouseEventKind, column: u16, row: u16| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // 点击第 2 个候选（/compact，row = menu_area.y + 2 = 12）
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 10, 12),
        );
        assert_eq!(app.prompt.text, "/compact ");
        assert!(app.completion.is_none(), "点击条目应接受补全并关闭菜单");

        connection.shutdown().await;
        core.shutdown().await;
    }

    #[tokio::test]
    async fn completion_mouse_hover_and_scroll_and_dismiss() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_str_raw("/co");
        let mut completion =
            crate::widgets::completion::CompletionState::derive(app.prompt_text()).unwrap();
        let menu_area = ratatui::layout::Rect::new(2, 10, 40, 6);
        completion.area = menu_area;
        completion.visible_count = 3;
        app.completion = Some(completion);

        let mouse = |kind: MouseEventKind, column: u16, row: u16| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // 鼠标悬停到第 3 个候选（row = 13）
        handle_mouse_with_connection(&mut app, &connection, mouse(MouseEventKind::Moved, 10, 13));
        assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(2));

        // 鼠标滚轮在菜单区域向上滚动
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::ScrollUp, 10, 12),
        );
        assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(1));

        // 鼠标滚轮在菜单区域向下滚动
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::ScrollDown, 10, 12),
        );
        assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(2));

        // 点击菜单外部（比如 row = 20）
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 10, 20),
        );
        assert!(app.completion.is_none(), "点击外部应关闭补全菜单");

        connection.shutdown().await;
        core.shutdown().await;
    }

    #[test]
    fn clicking_the_prompt_restores_focus_only_inside_its_area() {
        let mut app = app();
        app.prompt_area = ratatui::layout::Rect::new(4, 10, 40, 3);
        app.focus = Focus::Scrollback;

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: 11,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.focus, Focus::Scrollback);

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row: 11,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[tokio::test]
    async fn alt_m_cycles_the_session_mode() {
        use moc_core::{Core, CoreConfig};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
        })
        .unwrap();
        let (events, _receiver) = unbounded_channel();
        let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                mode: SessionMode::Normal,
            })
            .await
            .unwrap();
        let session_id = session.id.clone();
        app.session = Some(session);

        handle_key(&mut app, &connection, KeyModifiers::ALT, KeyCode::Char('m'));

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let session = connection
                    .client
                    .call::<SessionLoad>(SessionLoadParams {
                        session_id: session_id.clone(),
                        max_entries: None,
                    })
                    .await
                    .unwrap()
                    .session;
                if session.mode == SessionMode::Plan {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("Alt+M 应切换到 Plan 模式");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// `/ask <问题>`：本次提示带问答模式覆盖排队，不改变会话模式；缺参数只提示。
    #[tokio::test]
    async fn ask_command_queues_a_read_only_prompt() {
        use moc_core::{Core, CoreConfig};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
        })
        .unwrap();
        let (events, _receiver) = unbounded_channel();
        let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                mode: SessionMode::Normal,
            })
            .await
            .unwrap();
        let session_id = session.id.clone();
        app.session = Some(session);

        // 进行中的提示期间只排队：模式覆盖是提示级属性，无法注入正在飞行的提示。
        app.prompt_running = true;
        execute_command(&mut app, &connection, "/ask 有没有遗留的临时文件".into());
        let queued = app.queues.get(&session_id).expect("命令排进待发送队列");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].mode, Some(SessionMode::Ask));
        assert_eq!(queued[0].draft.text, "有没有遗留的临时文件");
        assert_eq!(
            app.session.as_ref().map(|session| session.mode),
            Some(SessionMode::Normal),
            "临时问答不得改写会话模式"
        );

        execute_command(&mut app, &connection, "/ask".into());
        assert_eq!(app.notice.as_deref(), Some("用法：/ask <问题>"));
        assert_eq!(
            app.queues.get(&session_id).map(|queue| queue.len()),
            Some(1),
            "缺参数不得产生新队列项"
        );

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 点图例弹出就地菜单：悬停高亮、点条目切换模式、再点图例关闭，键位同样可用。
    #[tokio::test]
    async fn mode_menu_switches_the_session_mode() {
        use moc_core::{Core, CoreConfig};

        async fn wait_for_mode(
            connection: &EmbeddedConnection,
            session_id: &str,
            expected: SessionMode,
        ) {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let session = connection
                        .client
                        .call::<SessionLoad>(SessionLoadParams {
                            session_id: session_id.to_owned(),
                            max_entries: None,
                        })
                        .await
                        .unwrap()
                        .session;
                    if session.mode == expected {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("模式应切换为期望值");
        }

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
        })
        .unwrap();
        let (events, _receiver) = unbounded_channel();
        let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                mode: SessionMode::Normal,
            })
            .await
            .unwrap();
        let session_id = session.id.clone();
        app.session = Some(session);

        // 图例命中区与菜单区域由绘制回填；这里摆出与渲染一致的坐标。
        app.mode_area = ratatui::layout::Rect::new(60, 20, 4, 1);
        let menu_area = ratatui::layout::Rect::new(44, 14, 24, 5);
        let mouse = |kind: MouseEventKind, column: u16, row: u16| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // 点图例开菜单：默认高亮当前模式。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
        );
        assert!(app.mode_menu.is_some(), "点模式图例应弹出菜单");
        assert_eq!(app.mode_menu.as_ref().map(|menu| menu.selected), Some(0));

        // 悬停在条目上即高亮该条，移出菜单不改选择。
        app.mode_menu.as_mut().unwrap().area = menu_area;
        handle_mouse_with_connection(&mut app, &connection, mouse(MouseEventKind::Moved, 45, 17));
        assert_eq!(app.mode_menu.as_ref().map(|menu| menu.selected), Some(2));
        handle_mouse_with_connection(&mut app, &connection, mouse(MouseEventKind::Moved, 10, 5));
        assert_eq!(app.mode_menu.as_ref().map(|menu| menu.selected), Some(2));

        // 点第二条（plan）直达该模式。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 45, 16),
        );
        assert!(app.mode_menu.is_none(), "选中条目后菜单关闭");
        wait_for_mode(&connection, &session_id, SessionMode::Plan).await;
        // core 通过 `ModeChanged` 推送新模式，事件循环据此更新本地会话；此处手动对齐。
        app.session.as_mut().unwrap().mode = SessionMode::Plan;

        // 再点图例开菜单，再点一次图例应关闭而不是重开。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
        );
        assert!(app.mode_menu.is_some());
        assert_eq!(app.mode_menu.as_ref().map(|menu| menu.selected), Some(1));
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
        );
        assert!(app.mode_menu.is_none(), "再次点图例关闭菜单");

        // 键盘路径：开菜单后 ↓ 选到 ptc，Enter 应用。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
        );
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        assert_eq!(app.mode_menu.as_ref().map(|menu| menu.selected), Some(2));
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        assert!(app.mode_menu.is_none());
        wait_for_mode(&connection, &session_id, SessionMode::Ptc).await;

        // 普通输入先关掉菜单再照常处理：菜单不会吞键。
        app.session.as_mut().unwrap().mode = SessionMode::Plan;
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
        );
        assert_eq!(app.mode_menu.as_ref().map(|menu| menu.selected), Some(1));
        let revision = app.prompt_revision;
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('h'),
        );
        assert!(app.mode_menu.is_none(), "输入字符只关菜单");
        assert_eq!(app.prompt.text, "h", "字符照常进入输入框");
        assert_ne!(app.prompt_revision, revision);

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 滚轮方向：上滚进入更早的历史，下滚回到尾部（offset 语义为距尾部行数）。
    #[test]
    fn wheel_scrolls_toward_and_away_from_tail() {
        fn mouse(kind: MouseEventKind) -> MouseEvent {
            MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }
        }

        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.scroll_offset = 10;

        handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_offset, 13, "上滚应查看更早的历史");
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_offset, 10, "下滚应回到尾部");

        app.scroll_offset = 1;
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_offset, 0, "已在尾部时下滚饱和为 0");
    }

    #[tokio::test]
    async fn ctrl_c_clears_input_when_nonempty_within_three_seconds() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_text("hello moc");
        assert_eq!(app.prompt_text(), "hello moc");

        // 第一次 Ctrl+C：提示 "clear input"，不应清空
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.prompt_text(), "hello moc");
        assert_eq!(app.notice.as_deref(), Some("clear input"));
        assert!(!app.should_quit);
        assert!(app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

        // 3 秒内第二次 Ctrl+C：清空输入框
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.prompt_text(), "");
        assert!(!app.should_quit);
        assert!(!app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

        // 此时输入框变空，再次按 Ctrl+C：应提示退出
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.notice.as_deref(), Some("Press Ctrl+C again to exit"));
        assert!(!app.should_quit);
        assert!(app.has_ctrl_c_pending(CtrlCPending::Quit));

        // 3 秒内第四次 Ctrl+C：退出
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert!(app.should_quit);

        connection.shutdown().await;
        core.shutdown().await;
    }

    #[tokio::test]
    async fn ctrl_c_times_out_after_three_seconds() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_text("some text");

        // 非空输入框：第一次按 Ctrl+C
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.notice.as_deref(), Some("clear input"));

        // 模拟超过 3 秒
        if let Some((_, at)) = app.ctrl_c_pending.as_mut() {
            *at = *at - std::time::Duration::from_secs(4);
        }

        // 再次按 Ctrl+C：超时，不应清空，应重新触发第一次提示
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.prompt_text(), "some text");
        assert_eq!(app.notice.as_deref(), Some("clear input"));
        assert!(app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

        // 清空后测试空输入框超时
        app.prompt.clear();
        app.ctrl_c_pending = None;

        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.notice.as_deref(), Some("Press Ctrl+C again to exit"));
        assert!(!app.should_quit);

        // 模拟超过 3 秒
        if let Some((_, at)) = app.ctrl_c_pending.as_mut() {
            *at = *at - std::time::Duration::from_secs(4);
        }

        // 再次按 Ctrl+C：超时，不应退出
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert!(!app.should_quit);
        assert_eq!(app.notice.as_deref(), Some("Press Ctrl+C again to exit"));

        connection.shutdown().await;
        core.shutdown().await;
    }

    #[tokio::test]
    async fn other_key_resets_ctrl_c_pending() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_text("hello");

        // 第一次 Ctrl+C
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert!(app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

        // 按其他键（例如向右光标键）
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
        assert!(!app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

        // 再次按 Ctrl+C：应当重新作为第一次按，提示而不是清空
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.prompt_text(), "hello");
        assert_eq!(app.notice.as_deref(), Some("clear input"));

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 单问 ask 模态：选项一个 + 自定义行（`z` 进入编辑态）。
    fn ask_modal() -> Modal {
        use moc_protocol::methods::{AskOption, AskParams, AskQuestion};
        Modal::Ask {
            params: AskParams {
                session_id: "ses_test".into(),
                prompt_id: "prompt_test".into(),
                request_id: "ask_test".into(),
                questions: vec![AskQuestion {
                    id: "q1".into(),
                    question: "选哪个？".into(),
                    header: None,
                    multi: false,
                    recommended: None,
                    options: vec![AskOption {
                        label: "选项一".into(),
                        description: None,
                        preview: None,
                    }],
                }],
            },
            question: 0,
            highlight: vec![0],
            selected: vec![Vec::new()],
            custom: String::new(),
            custom_cursor: 0,
            editing_custom: false,
            preview_scroll: 0,
            reply: None,
        }
    }

    /// ask 自定义输入的当前文本与光标。
    fn ask_custom(app: &App) -> (String, usize) {
        match app.modal.as_ref() {
            Some(Modal::Ask {
                custom,
                custom_cursor,
                ..
            }) => (custom.clone(), *custom_cursor),
            _ => unreachable!("测试期间应保持 ask 模态"),
        }
    }

    /// 走一遍模态渲染路径（越界切分会在这里 panic）。
    fn draw_ask(app: &mut App) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        terminal
            .draw(|frame| crate::render::modal::draw_modal(frame, app))
            .expect("绘制提问模态");
    }

    /// C7：ask 自定义输入的光标按字符移动，多字节字符不得被切在中间。
    #[tokio::test]
    async fn ask_custom_cursor_steps_by_char() {
        let (core, connection, mut app) = setup_test_app().await;
        app.modal = Some(ask_modal());

        // z 进入自定义编辑态，输入两个汉字。
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char('z'));
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char('中'));
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char('文'));
        assert_eq!(ask_custom(&app), ("中文".to_owned(), "中文".len()));

        // ← 回退一个字符后重绘：按字节移动会在 split_at 处 panic。
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
        assert_eq!(ask_custom(&app).1, "中".len(), "光标停在字符边界");
        draw_ask(&mut app);

        // Delete 删光标处字符、Backspace 删前一个字符，光标随之回退。
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Delete);
        assert_eq!(ask_custom(&app), ("中".to_owned(), "中".len()));
        draw_ask(&mut app);
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Backspace,
        );
        assert_eq!(ask_custom(&app), (String::new(), 0));
        draw_ask(&mut app);

        // → 到行尾饱和；行首 ← 与空串 Backspace/Delete 都不越界。
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char('中'));
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
        assert_eq!(ask_custom(&app).1, "中".len());
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
        assert_eq!(ask_custom(&app).1, 0, "行首左移饱和");
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Backspace,
        );
        assert_eq!(ask_custom(&app), ("中".to_owned(), 0), "行首无前驱可删");
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Delete);
        assert_eq!(ask_custom(&app), (String::new(), 0), "Delete 删光标处字符");
        draw_ask(&mut app);

        connection.shutdown().await;
        core.shutdown().await;
    }
}

#[cfg(test)]
#[path = "event_plan_tests.rs"]
mod plan_tests;

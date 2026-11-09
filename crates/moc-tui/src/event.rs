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
    ClientHandle, ConfigGet, ConfigGetParams, ContentPart, ImageSource, InputPart, PluginList,
    PluginListParams, PluginReload, PluginReloadParams, PluginRunCommand, PluginRunCommandParams,
    ResourceComplete, ResourceCompleteParams, Role, SessionCompact, SessionCompactParams,
    SessionContext, SessionContextContent, SessionContextContentParams, SessionEngine, SessionInfo,
    SessionList, SessionListParams, SessionLoad, SessionLoadPageParams, SessionLoadParams,
    SessionLoadResult, SessionModelOverflowAction, SessionNew, SessionNewParams, SessionPermission,
    SessionPlanApprove, SessionPlanApproveParams, SessionPlanDiff, SessionPlanDiffParams,
    SessionPlanDiffResult, SessionPlanMode, SessionPlanModeParams, SessionPlanRevise,
    SessionPlanReviseParams, SessionReask, SessionReaskParams, SessionRedo, SessionRedoParams,
    SessionRewind, SessionRewindParams, SessionSetEngine, SessionSetEngineParams,
    SessionSetFastMode, SessionSetFastModeParams, SessionSetModel, SessionSetModelParams,
    SessionSetPermission, SessionSetPermissionParams, SessionUpdate, SessionUsage,
    SessionUsageParams, UpstreamField,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::{App, ChartHit, CtrlCPending, Focus, LegendMenu, ScrollbackDrag};
use crate::commands::CommandId;
use crate::history_page::HistoryClient as _;
use crate::input::{InputEvent, InputThread};
use crate::modal::{ContextContentState, ContextDrag, Modal, PlanPane, SessionScope};
use crate::protocol::{AppEvent, ClipboardPayload, EmbeddedConnection, connect_embedded};
use crate::render;
use crate::render::wrap::str_width;
use crate::terminal::TerminalGuard;
use crate::theme::Theme;
use crate::transcript::{DetailLevel, ToolCallFinish, ToolCallStart};
use crate::widgets::prompt::{clamp_cursor, cursor_next, cursor_prev};
use moc_protocol::commands::{CommandRef, CommandSpec};

/// TUI 启动参数（从 CLI 参数翻译而来）。
#[derive(Debug, Clone, Default)]
pub struct TuiOptions {
    /// 工作目录；`None` 用 core 当前目录。
    pub cwd: Option<String>,
    /// 模型 selector；`None` 用 core 默认。
    pub model: Option<String>,
    /// 新会话的引擎。
    pub engine: SessionEngine,
    /// 新会话直接进入 plan 叠加态。
    pub plan: bool,
    /// `None` = 新会话；`Some(None)` = 恢复当前目录最近会话；`Some(Some(id))` = 指定会话。
    pub resume: Option<Option<String>>,
    /// 启动即发送的提示词（`moc "提示词"`）。
    pub initial_prompt: Option<String>,
    /// 启动覆盖显示档位（思考与工具同时生效）；`None` 用偏好设置。
    pub detail: Option<DetailLevel>,
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
    app.sandbox_modes = connection.sandbox_modes.clone();
    // 终端能力（图形协议 + DEC 2026 同步输出）只探测一次：必须早于
    // 输入线程启动（探测要直接读 fd 0 的应答）
    let caps = crate::render::graphics::probe();
    app.graphics = caps.graphics;
    app.sync_output = caps.sync_output;
    app.keyboard_enhanced = guard.keyboard_enhanced();
    guard.set_sync_output(caps.sync_output);
    app.thinking_detail = preferences.thinking_detail;
    app.tool_detail = preferences.tool_detail;
    if let Some(level) = opts.detail {
        app.thinking_detail = level;
        app.tool_detail = level;
    }
    app.statusline_template = preferences.statusline.clone();
    app.keymap = crate::config::KeyMap::compile(&preferences)?;
    app.preferences = preferences;
    app.recent_models = crate::model_history::RecentModels::load(&core.config_home())?;
    app.load_input_history(&core.config_home());
    app.tui_store = Some(tui_store);
    app.startup_engine = opts.engine;
    app.startup_plan = opts.plan;
    app.startup_pending = opts.initial_prompt.clone();
    bootstrap(&connection.client, &mut app, &opts).await;
    // 启动时把会话累计用量拉回来，第一帧的右下角就是整个会话的 total。
    refresh_usage(&mut app, &connection, false);
    // 启动目录在 bootstrap 才拿到；无会话时按它过滤「按项目」历史。
    app.refresh_input_history();
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

/// `bootstrap` 成功时恢复的会话与转写块；`bool` 是 `has_more`，末项是首条历史 ID。
type BootstrapLoad = (
    SessionInfo,
    Vec<crate::transcript::Entry>,
    bool,
    Option<String>,
);

/// 拉取模型目录并建立/恢复初始会话；失败给出可见错误但继续运行（用户可退出）。
async fn bootstrap(client: &ClientHandle, app: &mut App, opts: &TuiOptions) {
    let outcome: anyhow::Result<Option<BootstrapLoad>> = async {
        let config = client.call::<ConfigGet>(ConfigGetParams {}).await?;
        // core 的 cwd 已规范化，会话 header 里存的也是同一形式；用它比较才不受
        // `--cwd` 的相对路径或符号链接影响。
        app.startup_cwd = config.cwd.clone();
        app.models = config.models;
        app.fusion_defaults = config.fusion.clone();
        app.features = config.features;
        if opts.resume.is_none() && config.default_model.is_none() && opts.model.is_none() {
            app.set_notice("请选择本次使用的模型，之后会自动记住");
            if !app.models.is_empty() {
                app.open_model_picker();
            }
            return Ok(None);
        }
        let loaded = match &opts.resume {
            None => {
                let info = client
                    .call::<SessionNew>(SessionNewParams {
                        cwd: opts.cwd.clone(),
                        model: opts.model.clone(),
                        title: None,
                        engine: opts.engine,
                        plan_active: opts.plan,
                        fast_mode: false,
                    })
                    .await?;
                (info, Vec::new(), false, None)
            }
            Some(session_id) => {
                let session_id = match session_id {
                    Some(id) => id.clone(),
                    None => {
                        let list = client.call::<SessionList>(SessionListParams {}).await?;
                        list.sessions
                            .into_iter()
                            .find(|session| session.cwd == config.cwd)
                            .map(|session| session.id)
                            .context("当前目录没有可恢复的会话")?
                    }
                };
                let loaded = client
                    .history_page(SessionLoadPageParams {
                        full_messages: true,
                        plan_page: Some(Default::default()),
                        session_id,
                        before_entry_id: None,
                        max_entries: 64,
                        max_message_bytes: Some(64 * 1024),
                        redo_page: Some(Default::default()),
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
        Err(error) => app.set_error_notice(format!("会话启动失败：{error}")),
    }
}

/// 显示会话只取一页正文；租约请求不携带可能超帧的消息。
async fn load_display_session(
    client: &ClientHandle,
    session_id: String,
    read_only: bool,
) -> Result<SessionLoadResult, moc_protocol::RpcCallError> {
    client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: true,
            session_id: session_id.clone(),
            max_entries: Some(0),
            read_only,
        })
        .await?;
    load_display_page(client, session_id).await
}

async fn load_display_page(
    client: &ClientHandle,
    session_id: String,
) -> Result<SessionLoadResult, moc_protocol::RpcCallError> {
    use moc_protocol::methods::session::PlanPageParams;
    let mut page = client
        .history_page(SessionLoadPageParams {
            full_messages: true,
            plan_page: Some(PlanPageParams {
                before: None,
                max_entries: 32,
            }),
            session_id: session_id.clone(),
            before_entry_id: None,
            max_entries: 64,
            max_message_bytes: Some(64 * 1024),
            redo_page: Some(Default::default()),
        })
        .await?;
    let mut before = page.plans_before;
    let mut plan_pages = vec![std::mem::take(&mut page.plans)];
    while let Some(cursor) = before {
        let older = client
            .history_page(SessionLoadPageParams {
                full_messages: true,
                session_id: session_id.clone(),
                before_entry_id: None,
                max_entries: 0,
                max_message_bytes: None,
                redo_page: Some(Default::default()),
                plan_page: Some(PlanPageParams {
                    before: Some(cursor),
                    max_entries: 32,
                }),
            })
            .await?;
        before = older.plans_before;
        plan_pages.push(older.plans);
    }
    Ok(SessionLoadResult {
        session: page.session,
        entries: page.entries,
        truncated: page.has_more,
        todos: page.todos,
        plans: plan_pages.into_iter().rev().flatten().collect(),
        child_calls: page.child_calls,
        goal: page.goal,
    })
}

async fn load_mutation_page(
    client: &ClientHandle,
    result: Result<SessionLoadResult, moc_protocol::RpcCallError>,
) -> anyhow::Result<SessionLoadResult> {
    let loaded = result?;
    Ok(load_display_page(client, loaded.session.id).await?)
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

/// 刷新会话累计用量：状态栏与 `/usage` 读同一份 core replay 出的统计。
///
/// `show` 为真时把明细写进转写（`/usage`），否则只后台更新状态栏数字。
fn refresh_usage(app: &mut App, connection: &EmbeddedConnection, show: bool) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let request = app.begin_usage_request();
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<SessionUsage>(SessionUsageParams { session_id })
            .await
        {
            Ok(result) => {
                let _ = events.send(AppEvent::Usage {
                    request,
                    result,
                    show,
                });
            }
            Err(error) if show => {
                let _ = events.send(AppEvent::Notice(format!("读取用量失败：{error}")));
            }
            Err(_) => {}
        }
    });
}

/// 刷新插件清单：`show` 为真时把清单写进转写（`/plugins`），否则只更新命令补全；
/// `reload` 为真时先丢弃插件宿主并重新加载（`/plugins reload`）。
fn refresh_plugins(app: &App, connection: &EmbeddedConnection, show: bool, reload: bool) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let outcome = if reload {
            client
                .call::<PluginReload>(PluginReloadParams { session_id })
                .await
                .map(|result| result.plugins)
                .map_err(|error| format!("重载插件失败：{error}"))
        } else {
            client
                .call::<PluginList>(PluginListParams { session_id })
                .await
                .map(|result| result.plugins)
                .map_err(|error| format!("读取插件失败：{error}"))
        };
        match outcome {
            Ok(plugins) => {
                let text = show.then(|| {
                    if reload {
                        reload_plugins_note(&plugins)
                    } else {
                        format_plugins(&plugins)
                    }
                });
                let _ = events.send(AppEvent::Plugins(plugins));
                if let Some(text) = text {
                    let _ = events.send(AppEvent::PluginOutput {
                        name: "plugins".into(),
                        text,
                        error: false,
                    });
                }
            }
            Err(text) => {
                if show {
                    let _ = events.send(AppEvent::PluginOutput {
                        name: "plugins".into(),
                        text,
                        error: true,
                    });
                }
            }
        }
    });
}

/// `/plugins` 的正文：首行是总数，随后每个插件一行来源与目录，再列出它注册的工具和命令。
fn format_plugins(plugins: &[moc_protocol::methods::PluginInfo]) -> String {
    if plugins.is_empty() {
        return "当前工作区没有插件。把插件放进 .moc/plugins/<name>/（或 $MOC_HOME/plugins）即可自动启用。"
            .into();
    }
    format!(
        "已加载 {} 个插件：\n{}",
        plugins.len(),
        format_plugin_lines(plugins)
    )
}

/// `/plugins reload` 的正文：重载结果的可用计数，后接每个插件的明细。
fn reload_plugins_note(plugins: &[moc_protocol::methods::PluginInfo]) -> String {
    if plugins.is_empty() {
        return "插件已重载：当前工作区没有插件".into();
    }
    let disabled = plugins
        .iter()
        .filter(|plugin| plugin.error.is_some())
        .count();
    format!(
        "插件已重载：{} 个可用 / {disabled} 个仍不可用\n{}",
        plugins.len() - disabled,
        format_plugin_lines(plugins)
    )
}

/// 插件明细：来源、目录、不可用原因，以及它注册的工具与命令。
fn format_plugin_lines(plugins: &[moc_protocol::methods::PluginInfo]) -> String {
    let mut lines = Vec::new();
    for plugin in plugins {
        let version = plugin
            .version
            .as_deref()
            .map(|version| format!(" v{version}"))
            .unwrap_or_default();
        lines.push(format!(
            "- {}{version}（{}）{}",
            plugin.name,
            plugin.source,
            plugin.description.as_deref().unwrap_or_default()
        ));
        lines.push(format!("  {}", plugin.dir));
        if let Some(error) = &plugin.error {
            lines.push(format!("  不可用：{error}"));
            continue;
        }
        for tool in &plugin.tools {
            lines.push(format!("  工具 {} [{}]", tool.name, tool.modes.join(",")));
        }
        for command in &plugin.commands {
            lines.push(format!("  命令 /{}", command.name));
        }
    }
    lines.join("\n")
}

/// 插件停用提示：保留失败原因（可能是多行 JS 调用栈）与恢复方式。
fn plugin_notice_note(plugin: &str, code: &str, message: &str) -> String {
    format!("插件 `{plugin}` 已停用（{code}）：{message}\n修复后 /plugins reload 可重载")
}

/// 刷新代理面：名册与侧栏始终挂靠 [`App::agents_root_id`]（聚焦时是主会话）。
fn load_subagents(app: &App, connection: &EmbeddedConnection) {
    let Some(parent_session_id) = app.agents_root_id() else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::AgentList>(moc_protocol::methods::AgentListParams {
                session_id: parent_session_id,
            })
            .await
            .map(|result| result.agents)
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentsLoaded(result));
    });
}

/// 唤出子代理侧栏：手动覆盖显示（名册曾以居中模态呈现，已收敛为右侧停靠）。
///
/// 顺手向 core 刷新一次名册；迟到的回执照常并入 `app.subagents`，
/// 有代理时侧栏随之现身。名册为空时不强开面板，只提示。
fn open_subagents(app: &mut App, connection: &EmbeddedConnection) {
    app.subagent_sidebar = Some(true);
    load_subagents(app, connection);
    if app.subagents.is_empty() {
        app.set_notice("当前没有子代理");
    }
}

/// 停止一个子代理：聚焦时的 `Ctrl+S` 用它。
fn stop_agent_operation(app: &App, connection: &EmbeddedConnection, agent_id: String) {
    // 停止请求的父会话永远是代理面挂靠的那个：聚焦子代理时 `app.session` 是子代理自己。
    let Some(parent_session_id) = app.agents_root_id() else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::AgentStop>(moc_protocol::methods::AgentStopParams {
                session_id: parent_session_id,
                agent_id: agent_id.clone(),
                reason: None,
            })
            .await
            .map(|result| crate::subagent::AgentOperationNotice {
                target: agent_id,
                message: format!("停止请求已处理：{}", result.stopped),
            })
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentOperation(result));
    });
}

/// 聚焦进入或返回：按 `/resume` 同路加载目标会话。
///
/// 聚焦期间另一侧的更新被忽略（见 `apply_update_and_refresh` 的代理面分支），
/// 所以返回主会话时同样以磁盘真相重载，而不是自己缓存一份转写。
fn load_focus(app: &mut App, connection: &EmbeddedConnection, session_id: String, returning: bool) {
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .history_page(SessionLoadPageParams {
                full_messages: true,
                plan_page: Some(Default::default()),
                session_id,
                before_entry_id: None,
                max_entries: 128,
                max_message_bytes: Some(64 * 1024),
                redo_page: Some(Default::default()),
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::FocusLoaded { returning, result });
    });
}

/// 聚焦一个子代理：主 TUI 整体切到它的会话。
///
/// 已聚焦时再聚焦另一个代理只换焦点：主会话仍是原来那一个。
fn focus_agent(app: &mut App, connection: &EmbeddedConnection, agent_id: String) {
    let Some(view) = app.subagents.get(&agent_id) else {
        return;
    };
    let target = view.info.clone();
    if app.focused.is_none() {
        let Some(home) = app.session.clone() else {
            return;
        };
        if home.id == target.id {
            return;
        }
        app.focused = Some(crate::app::FocusedAgent {
            home,
            home_draft: app.prompt_text().to_owned(),
            home_scroll: app.scroll_offset,
        });
    } else if app
        .session
        .as_ref()
        .is_some_and(|session| session.id == target.id)
    {
        return;
    }
    app.subagent_selected = Some(target.id.clone());
    app.modal = None;
    load_focus(app, connection, target.id, false);
}

/// 返回主会话：还原草稿与滚动位置，并按磁盘真相重载。
fn leave_focus(app: &mut App, connection: &EmbeddedConnection) {
    let Some(focused) = app.focused.take() else {
        return;
    };
    app.focus_restore = Some((focused.home_draft, focused.home_scroll));
    load_focus(app, connection, focused.home.id, true);
}

/// 刷新子代理名册；`parent_session_id` 为 `None` 时清空名册。
fn load_subagents_for_parent(
    app: &mut App,
    client: &ClientHandle,
    parent_session_id: Option<String>,
) {
    let Some(parent_session_id) = parent_session_id else {
        app.subagents.clear();
        app.sync_subagent_sidebar();
        return;
    };
    let client = client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::AgentList>(moc_protocol::methods::AgentListParams {
                session_id: parent_session_id,
            })
            .await
            .map(|result| result.agents)
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentsLoaded(result));
    });
}

/// 应用一次聚焦加载结果：`returning` 时还原草稿与滚动位置。
fn apply_focus_loaded(
    app: &mut App,
    connection: &EmbeddedConnection,
    returning: bool,
    result: anyhow::Result<moc_protocol::methods::SessionLoadPageResult>,
) {
    let page = match result {
        Ok(page) => page,
        Err(error) => {
            app.focus_restore = None;
            app.focused = None;
            app.set_error_notice(format!("加载代理会话失败：{error}"));
            return;
        }
    };
    let blocks = crate::transcript_restore::blocks_from_entries_with_children(
        &page.entries,
        &page.child_calls,
    );
    let has_more = page.has_more;
    let before_entry_id = page.entries.first().map(|entry| entry.id.clone());
    let cwd = page.session.cwd.clone();
    let todos = page.todos;
    app.switch_session(page.session, blocks);
    app.todos = todos;
    app.goal = page.goal;
    if returning && let Some((draft, scroll)) = app.focus_restore.take() {
        if !draft.is_empty() {
            app.prompt.insert_str_raw(&draft);
            app.prompt_changed();
        }
        app.scroll_offset = scroll;
    }
    app.set_history_cursor(has_more, before_entry_id);
    refresh_git_branch(app, &cwd);
    refresh_context(app, connection, false);
    refresh_usage(app, connection, false);
    load_subagents(app, connection);
}

fn open_tools(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.set_notice("当前没有活动会话");
        return;
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let request = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    app.modal = Some(Modal::Tools {
        request,
        tools: None,
        error: None,
        scroll: 0,
    });
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::SessionTools>(
                moc_protocol::methods::SessionToolsParams { session_id },
            )
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::Tools { request, result });
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

/// `/context`：拉取上下文里的真实正文，开着窗等结果（拉取失败在窗内显示）。
fn open_context_content(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.set_notice("当前没有活动会话");
        return;
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let request = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    app.modal = Some(Modal::ContextContent {
        state: ContextContentState {
            request,
            session_id: session_id.clone(),
            ..Default::default()
        },
    });
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionContextContent>(SessionContextContentParams {
                session_id: session_id.clone(),
                ..Default::default()
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::ContextContent {
            request,
            session_id,
            result,
        });
    });
}

/// 上下文正文回执：只落进仍然开着的正文窗（用户可能已经关掉或换了模态）。
fn apply_context_content(
    app: &mut App,
    request: u64,
    result: anyhow::Result<moc_protocol::methods::ContextContentResult>,
) {
    if let Some(Modal::ContextContent { state }) = app.modal.as_mut() {
        if state.request != request {
            return;
        }
        match result {
            Ok(mut content)
                if state
                    .content
                    .as_ref()
                    .is_some_and(|old| old.view_id == content.view_id) =>
            {
                state
                    .expanded
                    .extend(std::iter::repeat_n(false, content.sections.len()));
                state
                    .depths
                    .extend(std::iter::repeat_n(0, content.sections.len()));
                let old = state.content.as_mut().unwrap();
                old.sections.append(&mut content.sections);
                old.next_cursor = content.next_cursor;
                state.pending.remove("");
                state.layout = None;
            }
            Ok(content) => state.set_content(content),
            Err(error) => state.set_error(error.to_string()),
        }
    }
}

fn apply_rewind_loaded(
    app: &mut App,
    result: anyhow::Result<moc_protocol::methods::SessionLoadPageResult>,
) {
    let Some(Modal::Rewind { state }) = app.modal.as_mut() else {
        return;
    };
    match result {
        Ok(loaded) => state.set_entries(loaded.entries, loaded.redo_entries),
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
    is_redo: bool,
    result: anyhow::Result<moc_protocol::SessionLoadResult>,
) {
    let loaded = match result {
        Ok(loaded) => loaded,
        Err(error) => {
            if let Some(Modal::Rewind { state }) = app.modal.as_mut() {
                state.loading = false;
                state.error = Some(format!(
                    "消息{}失败：{error}",
                    if is_redo { "前滚" } else { "回退" }
                ));
            }
            return;
        }
    };
    // 代理消息与运行时提醒都不是用户输入，不能回填到输入框；前滚选中的条目本来就在历史里。
    let restore_user = !is_redo
        && selected.message.role == Role::User
        && selected.agent.is_none()
        && selected.reminder.is_none();
    let selected_message = selected.message;
    app.modal = None;
    app.switch_session(
        loaded.session,
        crate::transcript_restore::blocks_from_entries_with_children(
            &loaded.entries,
            &loaded.child_calls,
        ),
    );
    app.set_history_cursor(
        loaded.truncated,
        loaded.entries.first().map(|entry| entry.id.clone()),
    );
    app.todos = loaded.todos;
    app.goal = loaded.goal;
    app.prompt.clear();
    if restore_user {
        restore_user_message(&mut app.prompt, &selected_message);
    }
    app.prompt_changed();
    app.focus = Focus::Prompt;
    app.usage = Default::default();
    app.set_notice(if is_redo {
        "已向前滚到该节点".to_owned()
    } else {
        "会话已回退".to_owned()
    });
    refresh_context(app, connection, false);
    // 回退会截断历史，重放出来的累计用量随之缩水，必须重新取一次。
    refresh_usage(app, connection, false);
    load_subagents_for_parent(
        app,
        &connection.client,
        app.session.as_ref().map(|session| session.id.clone()),
    );
}

/// 重新作答收尾：视图切到截断后的历史（含新答案的工具结果），随即
/// `session/retry` 让模型带着新答案续跑。
fn apply_reasked(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<moc_protocol::SessionLoadResult>,
) {
    let loaded = match result {
        Ok(loaded) => loaded,
        Err(error) => {
            if let Some(session) = app.session.as_ref() {
                app.inflight_session_ops.remove(&session.id);
            }
            app.set_notice(format!("重新作答失败：{error}"));
            return;
        }
    };
    let session_id = loaded.session.id.clone();
    app.inflight_session_ops.remove(&session_id);
    app.switch_session(
        loaded.session,
        crate::transcript_restore::blocks_from_entries_with_children(
            &loaded.entries,
            &loaded.child_calls,
        ),
    );
    app.set_history_cursor(
        loaded.truncated,
        loaded.entries.first().map(|entry| entry.id.clone()),
    );
    app.todos = loaded.todos;
    app.goal = loaded.goal;
    app.usage = Default::default();
    app.set_notice("已重新作答，正在继续".to_owned());
    refresh_context(app, connection, false);
    refresh_usage(app, connection, false);
    load_subagents_for_parent(
        app,
        &connection.client,
        app.session.as_ref().map(|session| session.id.clone()),
    );
    crate::queue::send_retry(&connection.client, session_id, app.events.clone());
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

/// 取出问题默认高亮的选项下标：推荐项优先，没有推荐停在第一项——
/// 用户直接回车选中的就是模型建议的答案。
fn recommended_highlight(question: &moc_protocol::AskQuestion) -> usize {
    question
        .recommended
        .first()
        .map_or(0, |index| *index as usize)
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
    // 全量重绘逃生门：终端声称支持 2026 但实际撕裂时，用 `MOC_FULL_REPAINT=1`
    // 强制每帧清屏重画（代价是整屏写入与位图重传，仅作排障手段）。
    let full_repaint =
        std::env::var("MOC_FULL_REPAINT").is_ok_and(|value| !value.is_empty() && value != "0");
    let mut view = render::ScrollbackView::background();
    let mut next_frame = tokio::time::Instant::now();
    let mut next_activity_tick = tokio::time::Instant::now() + ACTIVITY_TICK;
    let mut next_marquee_tick = tokio::time::Instant::now() + render::todo_hud::MARQUEE_TICK;
    let mut dirty = true;
    let mut memory = moc_core::memory::IdleReclaim::new(std::time::Instant::now());
    loop {
        crate::btw::flush_cancel(app, connection);
        if app.should_quit {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if dirty && now >= next_frame {
            // sixel / iTerm2 的旧图只能靠覆盖像素擦除：先清屏并重置 back buffer，
            // 再让 ratatui 完整重画一帧；剩余图表由 emit 整批重放。
            if app.image_repaint || full_repaint {
                app.image_repaint = false;
                app.image_resume = true;
                guard.reset_screen()?;
                // 整屏清空把终端里的图像数据一并作废：下一次放置必须重新传输，
                // 否则贴上去的是一张终端已经没有的图。
                app.images.forget_all_transmissions();
            }
            let draw_result = guard.draw(|frame| render::draw(frame, app, &mut view));
            next_frame = tokio::time::Instant::now() + FRAME_BUDGET;
            dirty = view.has_deferred_render();
            draw_result?;
            // 位图只能画在帧之后：先让 ratatui 把文本与底色写完，再按格坐标贴图
            crate::render::graphics::emit(app, &mut std::io::stdout())?;
            guard.set_title(&render::terminal_title(app));
            memory.activity(std::time::Instant::now());
            // emit 在帧后才发现“上一张图消失了”：立刻再排一帧清屏。
            if app.image_repaint {
                dirty = true;
            }
            if app.history_load_requested {
                request_history_page(app, connection);
            }
        }
        // 空闲（无 dirty）时完全 park 在事件上；dirty 但未到预算点才等待
        let need_frame_wait = dirty && tokio::time::Instant::now() < next_frame;
        let activity_tick_enabled = app.prompt_running
            || app.compaction_progress.is_some()
            || matches!(app.modal, Some(Modal::Monitor));
        // notice / warning 显示中：安排 TTL 到点重绘，擦除浮层
        let notice_deadline = app.notice_deadline().map(tokio::time::Instant::from_std);
        let warning_deadline = app.warning_deadline().map(tokio::time::Instant::from_std);
        let marquee_tick_enabled = app.todo_hud.marquee_active;
        let memory_deadline = if !dirty && !activity_tick_enabled && !marquee_tick_enabled {
            memory.deadline().map(tokio::time::Instant::from_std)
        } else {
            None
        };
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(memory_deadline.unwrap_or_else(tokio::time::Instant::now)), if memory_deadline.is_some() => {
                memory.reclaim(std::time::Instant::now());
            }
            _ = tokio::time::sleep_until(next_frame), if need_frame_wait => {}
            _ = tokio::time::sleep_until(notice_deadline.unwrap_or_else(tokio::time::Instant::now)), if notice_deadline.is_some() => dirty = true,
            _ = tokio::time::sleep_until(warning_deadline.unwrap_or_else(tokio::time::Instant::now)), if warning_deadline.is_some() => dirty = true,
            _ = tokio::time::sleep_until(next_marquee_tick), if marquee_tick_enabled => {
                next_marquee_tick = tokio::time::Instant::now() + render::todo_hud::MARQUEE_TICK;
                dirty = true;
            }
            _ = tokio::time::sleep_until(next_activity_tick), if activity_tick_enabled => {
                next_activity_tick = tokio::time::Instant::now() + ACTIVITY_TICK;
                if app.prompt_running || matches!(app.modal, Some(Modal::Monitor)) {
                    dirty = true;
                }
                if app.tick_compaction() {
                    dirty = true;
                }
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
                    apply_ui_update(app, connection, params);
                    for _ in 0..64 {
                        let Some(next) = connection.updates.try_recv().ok() else { break };
                        apply_ui_update(app, connection, next);
                    }
                }
            }
            app_event = event_rx.recv() => {
                dirty = true;
                match app_event {
                    Some(AppEvent::BtwReturned { session_id, request_id, result }) => {
                        app.btw.finish(&session_id, request_id, result);
                    }
                    Some(AppEvent::SessionClosed { session_id }) => {
                        app.btw.clear(&session_id);
                        if app.session.as_ref().is_some_and(|session| session.id == session_id) {
                            app.btw.expanded = false;
                            if app.focus == Focus::Btw { app.focus = Focus::Prompt; }
                            app.set_notice("会话已卸载，写租约已释放；旁路内存已清空");
                        }
                    }
                    Some(event @ (AppEvent::SettingsResponse {..} | AppEvent::ConfigurationLoaded(_)
                        | AppEvent::PromptReturned {..} | AppEvent::SteerReturned {..}
                        | AppEvent::UnsteerReturned { .. }
                        | AppEvent::PlanApproval(_) | AppEvent::PlanExecutionLoaded { .. }
                        | AppEvent::PlanRevised(_) | AppEvent::PlanDiff(_)
                        | AppEvent::ModelSwitchConfirm { .. }
                        | AppEvent::SessionControlSettled { .. })) => {
                        crate::interaction::handle_event(app, connection, event);
                    }
                    Some(AppEvent::PlanReview(result)) => {
                        apply_plan_review(app, connection, result)
                    }
                    Some(AppEvent::RewindLoaded(result)) => apply_rewind_loaded(app, result),
                    Some(AppEvent::Rewound {
                        selected,
                        is_redo,
                        result,
                    }) => {
                        apply_rewound(app, connection, selected, is_redo, *result);
                    }
                    Some(AppEvent::Reasked { result }) => {
                        apply_reasked(app, connection, *result);
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
                        // 高亮默认落在推荐项上；勾选集永远从空开始——
                        // 推荐只是徽章提示，答案必须由用户亲自勾选。
                        let highlight = params
                            .questions
                            .iter()
                            .map(recommended_highlight)
                            .collect();
                        let selected = vec![Vec::new(); per_question];
                        app.modal = Some(Modal::Ask {
                            params,
                            highlight,
                            selected,
                            custom: vec![crate::modal::CustomAnswer::default(); per_question],
                            question: 0,
                            preview_scroll: 0,
                            question_scroll: 0,
                            parked: false,
                            reply: Some(reply),
                        });
                        app.focus = crate::app::Focus::Prompt;
                    }
                    Some(AppEvent::Plugins(plugins)) => {
                        app.commands.set_plugins(&plugins);
                        app.plugins = plugins;
                    }
                    Some(AppEvent::PluginOutput { name, text, error }) => {
                        let text = if text.trim().is_empty() {
                            format!("插件命令 /{name} 没有输出")
                        } else {
                            text
                        };
                        app.push_note(text, error);
                    }
                    Some(AppEvent::Usage {
                        request,
                        result,
                        show,
                    }) => {
                        // 表格按当前转写区宽度排版，免得 80 列终端里被折行拆散。
                        let width = usize::from(app.scrollback_area.width);
                        let note = show.then(|| format_usage(&result, width));
                        app.apply_session_usage(request, result);
                        if let Some(note) = note {
                            app.push_note(note, false);
                        }
                    }
                    Some(AppEvent::Tools { request, result }) => {
                        if let Some(Modal::Tools { request: current, tools, error, .. }) = app.modal.as_mut()
                            && *current == request
                        {
                            match result {
                                Ok(result) => *tools = Some(result),
                                Err(failure) => *error = Some(failure.to_string()),
                            }
                        }
                    }
                    Some(AppEvent::ContextUsage(usage)) => {
                        app.context_usage = Some(usage.clone());
                        if let Some(Modal::Context { usage: current }) = app.modal.as_mut() {
                            *current = Some(usage);
                        }
                    }
                    Some(AppEvent::ContextContent { request, session_id, result }) => {
                        if !matches!(&app.modal, Some(Modal::ContextContent{state}) if state.request==request) {
                            if let Ok(content)=result {
                                let client=connection.client.clone();
                                tokio::spawn(async move {let _=client.call::<moc_protocol::methods::SessionContextClose>(moc_protocol::methods::SessionContextCloseParams{session_id,view_id:content.view_id}).await;});
                            }
                        } else {apply_context_content(app, request, result);}
                    }
                    Some(AppEvent::ContextRead { request, view_id, node_id, result }) => {
                        if let Some(Modal::ContextContent { state })=app.modal.as_mut()
                            && state.request==request && state.content.as_ref().is_some_and(|c|c.view_id==view_id) {
                            state.pending.remove(&node_id);
                            match result { Ok(page)=>state.apply_page(page), Err(error)=>{state.node_errors.insert(node_id,error.to_string());state.layout=None;} }
                        }
                    }
                    Some(AppEvent::Sessions(sessions)) => {
                        app.modal = Some(Modal::Sessions {
                            sessions,
                            cwd: app.startup_cwd.clone(),
                            scope: SessionScope::default(),
                            picker: crate::modal::PickerState::default(),
                        });
                    }
                    Some(AppEvent::SubagentsLoaded(result)) => match result {
                        Ok(agents) => {
                            let selected = app.subagent_selected.clone();
                            app.subagents = agents
                                .into_iter()
                                .map(|info| {
                                    let id = info.id.clone();
                                    (id, crate::subagent::SubagentView::new(info))
                                })
                                .collect();
                            if let Some(selected) = selected
                                && !app.subagents.contains_key(&selected)
                            {
                                app.subagent_selected = None;
                            }
                            app.subagent_generation = app.subagent_generation.wrapping_add(1);
                            app.sync_subagent_sidebar();
                        }
                        Err(error) => app.set_error_notice(format!("读取子代理列表失败：{error}")),
                    },
                    Some(AppEvent::FocusLoaded { returning, result }) => {
                        apply_focus_loaded(app, connection, returning, result);
                    }
                    Some(AppEvent::SubagentOperation(result)) => match result {
                        Ok(notice) => app.set_notice(notice.message),
                        Err(error) => app.set_error_notice(format!("子代理操作失败：{error}")),
                    },
                    Some(AppEvent::ImagePrepared { key, result }) => {
                        app.images.complete(key.clone(), result);
                        // 只让真正含这张图的块失效：全局 view_gen 会让最近 64 块
                        // 全部重新解析 markdown，一张图一张图地叠加就是明显卡顿。
                        if !app.touch_blocks_with_image(&key) {
                            // markdown 图片的键在渲染期才算出来，无法反查，只能全局失效
                            app.view_gen = app.view_gen.wrapping_add(1);
                        }
                    }
                    // 编码完成不改变布局（占位行已经按版式留好了），
                    // 下一次 emit 就能取到载荷；事件本身已经把 dirty 置上了。
                    Some(AppEvent::ImageEncoded { .. } | AppEvent::LayoutReady) => {}
                    Some(AppEvent::PickerError(error)) => {
                        let shown = app
                            .modal
                            .as_mut()
                            .is_some_and(|modal| modal.set_error(&error));
                        if !shown {
                            app.set_error_notice(error);
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
                                    app.completion =
                        crate::widgets::completion::CompletionState::derive(app);
                                }
                            }
                        }
                    }
                    Some(AppEvent::Notice(message)) => app.set_notice(message),
                    Some(AppEvent::GoalResult { goal, text }) => {
                        app.goal = goal;
                        app.push_note(text, false);
                    }
                    Some(AppEvent::OpenJobsModal(jobs)) => {
                        app.modal = Some(Modal::Jobs {
                            jobs,
                            selected: 0,
                            pending_kill: None,
                            offset: 0,
                            scroll: crate::modal::ModelScroll::default(),
                            pointer: None,
                        });
                    }
                    Some(AppEvent::OpenWorkflowApproval(request)) => {
                        app.workflow
                            .queued_approvals
                            .retain(|a| a.request_id != request.request_id);
                        app.modal = Some(Modal::WorkflowApproval {
                            state: Box::new(crate::workflow::WorkflowApprovalState::new(
                                request,
                            )),
                        });
                    }
                    Some(AppEvent::OpenWorkflowRun(snapshot)) => {
                        let state = crate::workflow::WorkflowRunState {
                            run_id: snapshot.summary.run_id.clone(),
                            title: snapshot
                                .summary
                                .title
                                .clone()
                                .unwrap_or_else(|| snapshot.summary.run_id.clone()),
                            status: snapshot.summary.status,
                            plan: snapshot.plan.clone().unwrap_or(serde_json::Value::Null),
                            phases: snapshot.phases.clone(),
                            roles: snapshot.roles.clone(),
                            commands: snapshot.commands.clone(),
                            reports: snapshot.reports.clone(),
                            artifacts: snapshot.artifacts.clone(),
                            holes: snapshot.holes.clone(),
                            usage: snapshot.usage.clone(),
                            log: Vec::new(),
                            artifact_versions: std::collections::BTreeMap::new(),
                            artifact_view: None,
                            focus: 0,
                            body_scroll: 0,
                            list_scroll: 0,
                            scroll: crate::modal::ModelScroll::default(),
                            pointer: None,
                        };
                        app.modal = Some(Modal::WorkflowRun {
                            state: Box::new(state),
                        });
                    }
                    Some(AppEvent::WorkflowArtifactView {
                        artifact_id,
                        content,
                    }) => {
                        if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut() {
                            // markdown 复用渲染管线；文本按行；结构化内容 pretty JSON。
                            let theme = app.theme;
                            let width = 100usize;
                            let view = if let Some(text) = content.as_str() {
                                if state
                                    .artifacts
                                    .iter()
                                    .find(|a| {
                                        a.get("artifact_id").and_then(|v| v.as_str())
                                            == Some(artifact_id.as_str())
                                    })
                                    .and_then(|a| a.get("member"))
                                    .and_then(|v| v.as_str())
                                    == Some("markdown")
                                {
                                    crate::render::markdown::render_markdown(text, width, theme)
                                } else {
                                    text.lines()
                                        .map(|line| ratatui::text::Line::raw(line.to_string()))
                                        .collect()
                                }
                            } else {
                                serde_json::to_string_pretty(&content)
                                    .unwrap_or_default()
                                    .lines()
                                    .map(|line| ratatui::text::Line::raw(line.to_string()))
                                    .collect()
                            };
                            state.artifact_view = Some((artifact_id, view));
                        }
                    }
                    Some(AppEvent::OpenWorkflowRuns(runs)) => {
                        app.modal = Some(Modal::WorkflowRuns {
                            runs,
                            picker: crate::modal::PickerState::default(),
                        });
                    }
                    Some(AppEvent::Loaded(
                        info,
                        blocks,
                        has_more,
                        before_entry_id,
                        todos,
                        read_only,
                        goal,
                    )) => {
                        app.modal = None;
                        let cwd = info.cwd.clone();
                        let id = info.id.clone();
                        let lock_holder = info.lock.clone();
                        app.switch_session(info, blocks);
                        app.goal = goal;
                        app.read_only = read_only;
                        if read_only {
                            let holder = lock_holder
                                .map(|lock| format!("{} (pid {})", lock.frontend, lock.pid))
                                .unwrap_or_else(|| "另一个 moc 进程".into());
                            app.set_notice(format!("会话被 {holder} 占用，已只读打开"));
                        }
                        // 恢复会话的 todo 快照：HUD 立刻可见，不必等下一次 TodoChanged。
                        app.todos = todos;
                        app.set_history_cursor(has_more, before_entry_id);
                        refresh_git_branch(app, &cwd);
                        refresh_context(app, connection, false);
                        refresh_usage(app, connection, false);
                        refresh_plugins(app, connection, false, false);
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
    let previous_context = match &app.modal {
        Some(Modal::ContextContent { state }) => state
            .content
            .as_ref()
            .map(|c| (state.request, state.session_id.clone(), c.view_id.clone())),
        _ => None,
    };
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
            // 提问的自定义答案正在编辑时，粘贴进那一行而不是主输入框——
            // 落到背后的输入框里，提交前用户根本看不见。
            let editing_ask = matches!(
                app.modal.as_mut(),
                Some(Modal::Ask {
                    custom,
                    question,
                    parked: false,
                    ..
                }) if custom
                    .get(*question)
                    .is_some_and(|answer| answer.editing)
            );
            if editing_ask {
                let Some(Modal::Ask {
                    custom, question, ..
                }) = app.modal.as_mut()
                else {
                    unreachable!("上面刚判过提问模态");
                };
                if let Some(answer) = custom.get_mut(*question) {
                    let pos = clamp_cursor(&answer.text, answer.cursor);
                    answer.text.insert_str(pos, &text);
                    answer.cursor = pos + text.len();
                }
            } else {
                app.prompt.insert_text(&text);
                app.prompt_changed();
                request_resource_completion(app, connection);
            }
        }
        Event::Resize(_, _) => app.todo_hud.on_resize(),
        Event::FocusGained => app.terminal_focused = true,
        Event::FocusLost => {
            app.terminal_focused = false;
            app.todo_hud.on_focus_lost();
        }
        _ => {}
    }
    flush_context_loads(app, connection);
    if let Some((request, session_id, view_id)) = previous_context
        && !matches!(&app.modal,Some(Modal::ContextContent{state}) if state.request==request)
    {
        let client = connection.client.clone();
        tokio::spawn(async move {
            let _ = client
                .call::<moc_protocol::methods::SessionContextClose>(
                    moc_protocol::methods::SessionContextCloseParams {
                        session_id,
                        view_id,
                    },
                )
                .await;
        });
    }
}

fn flush_context_loads(app: &mut App, connection: &EmbeddedConnection) {
    let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
        return;
    };
    if state.refresh {
        open_context_content(app, connection);
        return;
    }
    let Some(content) = &state.content else {
        return;
    };
    let view_id = content.view_id.clone();
    let request = state.request;
    let session_id = state.session_id.clone();
    if std::mem::take(&mut state.root_more)
        && content.next_cursor.is_some()
        && state.pending.insert(String::new())
    {
        let cursor = content.next_cursor;
        let view_id = view_id.clone();
        let session_id = session_id.clone();
        let client = connection.client.clone();
        let events = app.events.clone();
        tokio::spawn(async move {
            let result = client
                .call::<SessionContextContent>(SessionContextContentParams {
                    session_id: session_id.clone(),
                    view_id: Some(view_id),
                    cursor,
                    limit: None,
                })
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::ContextContent {
                request,
                session_id,
                result,
            });
        });
    }
    for (node_id, cursor) in std::mem::take(&mut state.loads) {
        let client = connection.client.clone();
        let events = app.events.clone();
        let view_id = view_id.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let result = client
                .call::<moc_protocol::methods::SessionContextRead>(
                    moc_protocol::methods::SessionContextReadParams {
                        session_id,
                        view_id: view_id.clone(),
                        node_id: node_id.clone(),
                        cursor,
                        limit: None,
                    },
                )
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::ContextRead {
                request,
                view_id,
                node_id,
                result,
            });
        });
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
    let position = (mouse.column, mouse.row);
    let left_down = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
    if app.modal.is_some() {
        // 提示条上的键位（footer、回退 dock、提问细条）：点中 = 按下该键。
        if left_down && let Some(hit) = key_hit_at(app, position) {
            handle_key(app, connection, hit.modifiers, hit.code);
            return;
        }
        // 浏览/选择类模态点卡片外即关闭（与 Esc 同义）；授权、提问、计划审阅、
        // 切换确认这类需要明确答复的卡片不因误点背景而消失。
        // 模型选择器的「外」是列表卡、详情卡、下拉卡三块浮层之外。
        let pos: ratatui::layout::Position = position.into();
        let outside = !app.modal_card_area.is_empty()
            && !app.modal_card_area.contains(pos)
            && !app.modal_detail_area.contains(pos)
            && !app.modal_menu_area.contains(pos);
        if left_down && outside && matches!(app.modal, Some(Modal::Model { .. })) {
            // popover 语义：一次点击即关——先收起下拉与详情焦点，再走列表层 Esc
            // （关闭；WorkflowRole 目标回到 workflow 窗）。
            if let Some(Modal::Model { detail, .. }) = app.modal.as_mut() {
                detail.menu = None;
                detail.focus = None;
            }
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Esc);
            // 点在触发标签上只收起（否则会立刻又打开）；Workflow 回程仍有模态
            // 时同样吞掉这次点击。
            if app.model_tag_area.contains(pos) || app.modal.is_some() {
                return;
            }
            // 已彻底关闭：把这次点击当作无模态时的普通点击重新派发。
            handle_mouse_with_connection(app, connection, mouse);
            return;
        } else if left_down && outside && app.modal.as_ref().is_some_and(modal_dismissable) {
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Esc);
            return;
        }
        if matches!(app.modal, Some(Modal::ContextContent { .. })) {
            handle_context_content_mouse(app, mouse);
            return;
        }
        if matches!(app.modal, Some(Modal::Rewind { .. })) {
            handle_rewind_mouse(app, connection, mouse);
            return;
        }
        if matches!(app.modal, Some(Modal::Ask { .. })) {
            handle_ask_mouse(app, connection, mouse);
            return;
        }
        if matches!(app.modal, Some(Modal::Plan { .. })) {
            handle_plan_mouse(app, connection, mouse);
            return;
        }
        if matches!(app.modal, Some(Modal::Model { .. })) {
            handle_model_picker_mouse(app, connection, mouse);
            return;
        }
        // 列表/面板类模态只记鼠标坐标：悬停高亮由渲染层按最终命中矩形计算，
        // 键盘导航时清空（handle_modal_key 开头），不与选中态抢视觉。
        {
            let pos: ratatui::layout::Position = position.into();
            match app.modal.as_mut() {
                Some(Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. }) => {
                    picker.pointer = Some(pos)
                }
                Some(Modal::Jobs { pointer, .. }) => *pointer = Some(pos),
                Some(Modal::WorkflowApproval { state }) => state.pointer = Some(pos),
                Some(Modal::WorkflowRun { state }) => state.pointer = Some(pos),
                _ => {}
            }
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // 命中矩形由渲染层按卡片正文行号每帧写入。
                if let Some((index, _)) = app
                    .modal_item_hits
                    .iter()
                    .find(|(_, area)| area.contains(position.into()))
                    .copied()
                {
                    click_modal_item(app, connection, index);
                }
            }
            MouseEventKind::Moved => {
                if let Some((index, _)) = app
                    .modal_item_hits
                    .iter()
                    .find(|(_, area)| area.contains(position.into()))
                    .copied()
                {
                    hover_modal_item(app, index);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                let delta: isize = if up { -3 } else { 3 };
                // 列表/面板类模态：滚轮只滚 viewport，不移动选中项，也不滚背后的转写。
                let scrolled = match app.modal.as_mut() {
                    Some(Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. }) => {
                        picker.scroll.scroll_by(&mut picker.offset, delta);
                        true
                    }
                    Some(Modal::Jobs { offset, scroll, .. }) => {
                        scroll.scroll_by(offset, delta);
                        true
                    }
                    Some(Modal::WorkflowApproval { state }) => {
                        state.scroll.scroll_by(&mut state.body_scroll, delta);
                        true
                    }
                    Some(Modal::WorkflowRun { state }) => {
                        state.scroll.scroll_by(&mut state.body_scroll, delta);
                        true
                    }
                    // 占用窗与监控没有可滚内容，也不该滚背后的转写。
                    Some(Modal::Context { .. } | Modal::Monitor) => true,
                    _ => false,
                };
                if !scrolled {
                    // 切换确认是二选一菜单：保留滚轮 = 换项的菜单语义。
                    if matches!(app.modal, Some(Modal::SwitchModel { .. })) {
                        handle_modal_key(
                            app,
                            connection,
                            KeyModifiers::NONE,
                            if up { KeyCode::Up } else { KeyCode::Down },
                        );
                    } else {
                        // 授权预览与帮助：滚各自的正文。
                        handle_mouse(app, mouse);
                    }
                }
            }
            _ => {}
        }
        return;
    }
    if crate::btw::handle_mouse(app, mouse) {
        return;
    }
    if handle_legend_menu_mouse(app, connection, &mouse) {
        return;
    }
    // 补全菜单等浮层提示条上的键位。
    if left_down && let Some(hit) = key_hit_at(app, position) {
        handle_key(app, connection, hit.modifiers, hit.code);
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
    if app.subagent_area.contains((mouse.column, mouse.row).into()) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if app
                    .subagent_close_area
                    .contains((mouse.column, mouse.row).into()) =>
            {
                app.subagent_sidebar = Some(false);
                return;
            }
            MouseEventKind::ScrollUp => {
                app.subagent_scroll = app.subagent_scroll.saturating_sub(1);
                return;
            }
            MouseEventKind::ScrollDown => {
                app.subagent_scroll = app.subagent_scroll.saturating_add(1);
                return;
            }
            _ => {}
        }
    }
    if handle_todo_hud_mouse(app, &mouse) {
        return;
    }
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        let position = (mouse.column, mouse.row);
        if let Some((agent_id, _)) = app
            .subagent_hits
            .iter()
            .find(|(_, area)| area.contains(position.into()))
        {
            let agent_id = agent_id.clone();
            focus_agent(app, connection, agent_id);
            return;
        }
        if app.modal.is_none() && app.engine_area.contains(position.into()) {
            app.open_engine_menu();
            return;
        }
        if app.modal.is_none() && app.model_tag_area.contains(position.into()) {
            app.open_model_picker();
            return;
        }
        if app.modal.is_none() && app.permission_tag_area.contains(position.into()) {
            app.open_permission_menu();
            return;
        }
        // workflow 胶囊：最小化审批优先重开，其次弹出排队审批，否则打开 run 列表。
        if app.modal.is_none() && app.workflow_capsule_area.contains(position.into()) {
            if let Some(state) = take_next_approval(app) {
                app.modal = Some(Modal::WorkflowApproval { state });
            } else {
                open_workflow_runs(app, connection);
            }
            return;
        }
        // 输入框图例左端：运行中是「停止」，空闲是「快捷键」，点中与对应按键同义。
        if app.legend_tip_area.contains(position.into()) {
            if app.prompt_running {
                request_stop(app, connection);
            } else {
                app.modal = Some(Modal::Help { scroll: 0 });
            }
            return;
        }
        if app.focused.is_some() && app.header_back_area.contains(position.into()) {
            leave_focus(app, connection);
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
                crate::queue::remove(app, connection, id);
            } else {
                crate::queue::edit(app, connection, id);
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
        // statusline 右侧用量统计段：点击与输入 /usage 同义。
        if app.usage_area.contains(position.into()) {
            execute_command(app, connection, "/usage".to_owned());
            return;
        }
        // 离底时输入框上方的「回到底部」胶囊：点击贴回最新消息。
        if app.jump_bottom_area.contains(position.into()) {
            app.scroll_offset = 0;
            return;
        }
    }
    handle_mouse(app, mouse);
}

/// 坐标命中的提示条键位（上一帧绘制登记）。
fn key_hit_at(app: &App, position: (u16, u16)) -> Option<crate::render::labels::KeyHit> {
    app.key_hits
        .iter()
        .copied()
        .find(|hit| hit.area.contains(position.into()))
}

/// 点卡片外可关闭的模态：只浏览或选择、关闭不丢答复的那些。
fn modal_dismissable(modal: &Modal) -> bool {
    matches!(
        modal,
        Modal::Help { .. }
            | Modal::Tools { .. }
            | Modal::Context { .. }
            | Modal::ContextContent { .. }
            | Modal::Model { .. }
            | Modal::Sessions { .. }
            | Modal::Jobs { .. }
            | Modal::Rewind { .. }
            | Modal::Monitor
    )
}

/// 点中通用模态的第 `index` 项。
///
/// 按钮式选项（授权、切换确认）点中即执行；候选列表（会话、子代理）先选中、
/// 再点已选中的一项才确认。模型选择器有独立的三浮层处理，不走这里。
fn click_modal_item(app: &mut App, connection: &EmbeddedConnection, index: usize) {
    let mut job_kill: Option<String> = None;
    let confirm = match app.modal.as_mut() {
        Some(Modal::Permission { selected, .. }) => {
            *selected = index;
            true
        }
        Some(Modal::SwitchModel { option, .. }) => {
            *option = index;
            true
        }
        Some(Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. }) => {
            let again = picker.selected == index;
            picker.selected = index;
            again
        }
        // workflow 审批窗：单击选中 role/ref 行，点已选行 = 打开模型选择器。
        Some(Modal::WorkflowApproval { state }) => {
            let again = state.focus == index;
            state.focus = index;
            again
        }
        // workflow run 面板：同上（点已选 role 行开选择器改模型）。
        Some(Modal::WorkflowRun { state }) => {
            let again = state.focus == index;
            state.focus = index;
            again
        }
        // /jobs：点行选中；点已选中行与连按两次 x 同义——第二次点击本身就是确认。
        Some(Modal::Jobs {
            jobs,
            selected,
            pending_kill,
            ..
        }) => {
            let again = *selected == index;
            *selected = index;
            *pending_kill = None;
            if again {
                job_kill = jobs.get(index).map(|job| job.job_id.clone());
            }
            false
        }
        _ => false,
    };
    if let Some(job_id) = job_kill {
        if let Some(session_id) = app.session.as_ref().map(|s| s.id.clone()) {
            let client = connection.client.clone();
            tokio::spawn(async move {
                let _ = client
                    .call::<moc_protocol::methods::JobKill>(moc_protocol::methods::JobTarget {
                        session_id,
                        job_id,
                    })
                    .await;
            });
        }
        // 与键盘路径一致：杀任务后关闭弹窗。
        app.modal = None;
        return;
    }
    if confirm {
        handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
    }
}

/// 鼠标悬浮移到按钮式选项（授权、切换确认）：与 ↑↓ 换行同语义，只改高亮不确认。
/// 列表类模态的悬停不写选中态——pointer 已记录，渲染层只铺悬停底色。
fn hover_modal_item(app: &mut App, index: usize) {
    match app.modal.as_mut() {
        Some(Modal::Permission { selected, .. }) => *selected = index,
        Some(Modal::SwitchModel { option, .. }) => *option = index,
        _ => {}
    }
}

/// 模型选择器：滚轮只滚动光标所在列表，悬停只记录坐标。
/// 列表行先选中再点确认；详情行和下拉候选点中即执行。
fn handle_model_picker_mouse(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: crossterm::event::MouseEvent,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    let position: ratatui::layout::Position = (mouse.column, mouse.row).into();
    let Some(Modal::Model {
        entries,
        picker,
        level,
        detail,
        ..
    }) = app.modal.as_mut()
    else {
        return;
    };
    detail.pointer = Some(position);
    if matches!(
        mouse.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) {
        let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
            -3
        } else {
            3
        };
        if app.modal_menu_area.contains(position) {
            if let Some(menu) = detail.menu.as_mut() {
                menu.scroll.scroll_by(&mut menu.offset, delta);
            }
        } else if !app.modal_detail_area.contains(position)
            && app.modal_card_area.contains(position)
        {
            detail.list_scroll.scroll_by(&mut picker.offset, delta);
        }
        return;
    }
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    if let Some(menu) = detail.menu.as_mut() {
        if let Some((index, _)) = app
            .modal_menu_hits
            .iter()
            .find(|(_, area)| area.contains(position))
        {
            menu.selected = *index;
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
        } else {
            // 点下拉以外：收起下拉，再执行同一次点击。
            detail.menu = None;
            handle_model_picker_mouse(app, connection, mouse);
        }
        return;
    }
    if let Some((row, _)) = app
        .modal_detail_hits
        .iter()
        .find(|(_, area)| area.contains(position))
    {
        detail.focus = Some(*row);
        handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
    } else if app.modal_detail_area.contains(position) || app.modal_menu_area.contains(position) {
        // 详情卡/下拉卡内非控制行不穿透到候选列表。
    } else if let Some((index, _)) = app
        .modal_item_hits
        .iter()
        .find(|(_, area)| area.contains(position))
    {
        let again = picker.selected == *index;
        if !again {
            let filtered = crate::modal::filtered_entries(entries, &picker.query);
            level.change_entry(
                filtered.get(picker.selected).map(|(_, entry)| *entry),
                filtered.get(*index).map(|(_, entry)| *entry),
            );
            picker.selected = *index;
        }
        detail.focus = None;
        if again {
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
        }
    }
}

/// 请求停止当前提示（`Esc` 与图例「停止」共用）；已在停止中时不重复发送。
fn request_stop(app: &mut App, connection: &EmbeddedConnection) {
    if !app.cancel_requested {
        app.cancel_requested = true;
        app.set_notice("正在停止...");
        cancel_prompt(&connection.client, app);
    }
}

/// todo HUD：单击在展开卡片与折叠胶囊间切换、按住卡片拖动。
fn handle_todo_hud_mouse(app: &mut App, mouse: &crossterm::event::MouseEvent) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    let position = (mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Moved => {
            let inside = app.todo_hud.pointer_moved(position);
            if inside {
                // 卡片盖住图表入口时，别留下过期的行高亮。
                app.hover_chart = None;
            }
            inside
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if app.todo_hud.pointer_down(position) {
                // 点击 HUD 不落到转写：不进入点击/拖选状态。
                app.pending_click = None;
                app.selection = None;
                true
            } else {
                false
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            app.todo_hud.pointer_drag(position, app.scrollback_area)
        }
        MouseEventKind::Up(MouseButton::Left) => app.todo_hud.pointer_up(position),
        _ => false,
    }
}

/// 上下文正文窗的鼠标：滚轮滚动、点击分块展开收起、拖拽滚动条。
///
/// 几何全部来自上一帧绘制（轨道、滑块、行区间），因此这里只做换算，不重排正文。
fn handle_context_content_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) {
    use crossterm::event::{MouseButton, MouseEventKind};
    let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
        return;
    };
    match mouse.kind {
        MouseEventKind::ScrollUp => state.scroll_by(-3),
        MouseEventKind::ScrollDown => state.scroll_by(3),
        MouseEventKind::Down(MouseButton::Left) => {
            let scrollbar = state.scrollbar;
            let track = scrollbar.track;
            let on_track = mouse.column == track.x
                && mouse.row >= track.y
                && mouse.row < track.y.saturating_add(track.height);
            if on_track {
                // 按在滑块上就保持抓取点，按在轨道空白处把滑块中线挪到光标。
                let top = track.y.saturating_add(scrollbar.thumb_top(state.scroll));
                let grab = if mouse.row >= top && mouse.row < top.saturating_add(scrollbar.thumb) {
                    i32::from(mouse.row - top)
                } else {
                    i32::from(scrollbar.thumb) / 2
                };
                state.drag = Some(ContextDrag { grab });
                state.scroll = scrollbar.offset_at(mouse.row, grab);
            } else if let Some(index) = state.block_at(mouse.row) {
                state.focus = Some(index);
                state.toggle(index);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(drag) = state.drag {
                state.scroll = state.scrollbar.offset_at(mouse.row, drag.grab);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => state.drag = None,
        _ => {}
    }
}

/// 计划审阅的鼠标交互：执行模型与动作行可点选；批注视图按住拖动即可选行。
/// 批注编辑器盖在上面时只响应它自己的提示条键位（由调用方先行分派）。
fn handle_plan_mouse(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: crossterm::event::MouseEvent,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    if matches!(&app.modal, Some(Modal::Plan { state }) if state.editor.is_some()) {
        return;
    }
    let height = usize::from(app.plan_body_area.height).max(1);
    let position = (mouse.column, mouse.row).into();
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        // 动作行（执行 / 压缩后执行 / 新会话执行 / 继续计划）：点中即选中并确认。
        if let Some((index, _)) = app
            .modal_item_hits
            .iter()
            .find(|(_, area)| area.contains(position))
            .copied()
        {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                if state.submitting {
                    return;
                }
                state.selected = index;
            }
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
            return;
        }
    }
    let body_area = app.plan_body_area;
    let row_map = app.plan_row_map.clone();
    let Some(Modal::Plan { state }) = app.modal.as_mut() else {
        return;
    };
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if state.pane == PlanPane::Read {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
            } else {
                state.move_cursor(-1, false);
                state.ensure_cursor_visible(height);
            }
        }
        MouseEventKind::ScrollDown => {
            if state.pane == PlanPane::Read {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
            } else {
                state.move_cursor(1, false);
                state.ensure_cursor_visible(height);
            }
        }
        MouseEventKind::Down(MouseButton::Left)
            if state.pane == PlanPane::Annotate
                && body_area.contains((mouse.column, mouse.row).into()) =>
        {
            let row = usize::from(mouse.row.saturating_sub(body_area.y));
            if let Some(index) = row_map.get(row).copied() {
                state.cursor = index;
                state.anchor = Some(index);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if state.pane == PlanPane::Annotate => {
            if row_map.is_empty() {
                return;
            }
            if mouse.row < body_area.y {
                state.cursor = row_map.first().copied().unwrap_or_default();
            } else if mouse.row >= body_area.y.saturating_add(body_area.height) {
                state.cursor = row_map.last().copied().unwrap_or_default();
            } else {
                let row = usize::from(mouse.row.saturating_sub(body_area.y));
                if let Some(index) = row_map.get(row).copied() {
                    state.cursor = index;
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) if state.pane == PlanPane::Annotate => {
            state.ensure_cursor_visible(height);
        }
        _ => {}
    }
}

/// 消息回退视图的鼠标处理：滚动时间线列表或详情预览；点击选择时间线节点，
/// 再点已选中的节点即确认（与 Enter 同义）。
fn handle_rewind_mouse(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: crossterm::event::MouseEvent,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    let timeline_area = app.rewind_timeline_area;
    let detail_area = app.rewind_detail_area;
    let Some(Modal::Rewind { state }) = app.modal.as_mut() else {
        return;
    };
    let pos = (mouse.column, mouse.row).into();
    // 只记坐标：悬停高亮由渲染层按最终布局计算，不写选中态。
    state.pointer = Some(pos);
    let in_detail = detail_area.contains(pos);
    let in_timeline = timeline_area.contains(pos);

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if in_detail {
                state.scroll_detail(-3);
            } else if in_timeline {
                // 滚轮只滚时间线 viewport，不移动选中项（键盘/点击才动 selected）。
                state.scroll.scroll_by(&mut state.list_scroll, -3);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_detail {
                state.scroll_detail(3);
            } else if in_timeline {
                state.scroll.scroll_by(&mut state.list_scroll, 3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) if in_timeline && timeline_area.height > 2 => {
            let inner_y = timeline_area.y + 1;
            if mouse.row >= inner_y {
                let rel_row = (mouse.row - inner_y) as usize;
                let item_height = if timeline_area.height.saturating_sub(2) >= 14 {
                    2
                } else {
                    1
                };
                let item_click = rel_row / item_height + state.list_scroll;
                let matches = state.filtered_indices();
                if item_click < matches.len() {
                    if state.selected == matches[item_click] {
                        handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
                        return;
                    }
                    state.selected = matches[item_click];
                    state.sync_prompt_item_from_selected();
                }
            }
        }
        _ => {}
    }
}

/// 提问卡片的鼠标处理。
/// 作答态：点选项即选择（单选直接推进、多选切换勾选），点自定义行进入编辑，
/// 点导航条跳问（「确认」项落确认页、确认页上再点即提交），点转写区或按
/// 滚动条折叠；滚轮按悬停区分流——落在可滚文本区（溢出的问题正文窗/详情区）
/// 滚该区，其余位置与 ↑↓ 同语义移动选项光标。
/// 折叠态：点细条展开回作答，其余鼠标行为与转写区一致（滚动/拖选/滚动条）。
/// 命中矩形由渲染层每帧写入 `ask_option_hits`/`ask_nav_hits`/`ask_card_area`/
/// `ask_question_area`/`ask_detail_area`/`ask_strip_area`；未命中区域不响应。
fn handle_ask_mouse(
    app: &mut App,
    _connection: &EmbeddedConnection,
    mouse: crossterm::event::MouseEvent,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    let position = (mouse.column, mouse.row).into();
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
            let Some(Modal::Ask {
                params,
                question,
                highlight,
                preview_scroll,
                question_scroll,
                parked,
                ..
            }) = app.modal.as_mut()
            else {
                return;
            };
            if *parked {
                // 折叠态滚轮与 ↑↓ 同语义：滚转写区。
                scroll(app, if up { 3 } else { -3 });
                return;
            }
            let Some(current) = params.questions.get(*question) else {
                // 确认页：滚轮滚动答案列表（复用 preview_scroll 当滚动偏移）。
                *preview_scroll = if up {
                    preview_scroll.saturating_sub(ASK_PREVIEW_SCROLL_STEP)
                } else {
                    preview_scroll.saturating_add(ASK_PREVIEW_SCROLL_STEP)
                };
                return;
            };
            // 悬停分流：滚轮落在可滚文本区（问题正文/详情区）就滚该区内容；
            // 其余位置与 ↑↓ 同语义，移动选项光标——命中区域只在溢出时才登记，
            // 所以落到这里的悬停要么在选项上、要么悬停区并不需要滚。
            if app.ask_detail_area.contains(position) {
                *preview_scroll = if up {
                    preview_scroll.saturating_sub(ASK_PREVIEW_SCROLL_STEP)
                } else {
                    preview_scroll.saturating_add(ASK_PREVIEW_SCROLL_STEP)
                };
                return;
            }
            if app.ask_question_area.contains(position) {
                *question_scroll = if up {
                    question_scroll.saturating_sub(ASK_PREVIEW_SCROLL_STEP)
                } else {
                    question_scroll.saturating_add(ASK_PREVIEW_SCROLL_STEP)
                };
                return;
            }
            let option_count = current.options.len();
            let next = if up {
                highlight[*question].saturating_sub(1)
            } else {
                (highlight[*question] + 1).min(option_count)
            };
            if next != highlight[*question] {
                highlight[*question] = next;
                *preview_scroll = 0;
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if matches!(app.modal, Some(Modal::Ask { parked: true, .. })) {
                // 折叠态细条点中即展开；其余位置与转写区一致：
                // 滚动条拖拽、拖选复制、块折叠/图表切换照常可用。
                if app.ask_strip_area.contains(position) {
                    if let Some(Modal::Ask { parked, .. }) = app.modal.as_mut() {
                        *parked = false;
                    }
                } else {
                    handle_mouse(app, mouse);
                }
                return;
            }
            // 导航条：点问题项跳问（与 ←→ 同语义），点「确认」项是提交链路的鼠标入口。
            if let Some((target, _)) = app
                .ask_nav_hits
                .iter()
                .find(|(_, area)| area.contains(position))
                .cloned()
            {
                let mut submitted = false;
                if let Some(Modal::Ask {
                    params,
                    question,
                    selected,
                    custom,
                    preview_scroll,
                    question_scroll,
                    reply,
                    ..
                }) = app.modal.as_mut()
                {
                    if target == params.questions.len() && *question == params.questions.len() {
                        // 确认页上再点「确认」= 提交（纯鼠标的提交入口）。
                        submitted = submit_ask(params, selected, custom, reply);
                    } else if target <= params.questions.len() {
                        // 点问题项跳问，点「确认」落到确认页；
                        // 编辑中的文本留在原问，跳页即结束编辑态。
                        if let Some(answer) = custom.get_mut(*question) {
                            answer.editing = false;
                        }
                        *question = target;
                        *preview_scroll = 0;
                        *question_scroll = 0;
                    }
                }
                if submitted {
                    app.modal = None;
                }
                return;
            }
            // 滚动条轨道在转写区外（终端右缘），按下同样算「翻上下文」：
            // 折叠并把按下交给拖拽逻辑，后续 Drag/Up 走折叠态的通用路径。
            if handle_scrollbar_mouse(app, &mouse) {
                if let Some(Modal::Ask { parked, .. }) = app.modal.as_mut() {
                    *parked = true;
                }
                return;
            }
            // 转写区点击 = 折叠（与 Esc 同语义，可再展开）：点上下文才是想查阅；
            // 输入框/状态栏等死区不误判，几何未建立（首帧渲染前）同样不误判。
            if !app.ask_card_area.is_empty()
                && !app.ask_card_area.contains(position)
                && app.scrollback_area.contains(position)
            {
                if let Some(Modal::Ask { parked, .. }) = app.modal.as_mut() {
                    *parked = true;
                }
                return;
            }
            let Some((option, _)) = app
                .ask_option_hits
                .iter()
                .find(|(_, area)| area.contains(position))
                .cloned()
            else {
                return;
            };
            let Some(Modal::Ask {
                params,
                question,
                highlight,
                selected,
                custom,
                preview_scroll,
                question_scroll,
                ..
            }) = app.modal.as_mut()
            else {
                return;
            };
            let count = params
                .questions
                .get(*question)
                .map(|current| current.options.len())
                .unwrap_or(0);
            *preview_scroll = 0;
            // 编辑中的文本留在原处，焦点交给点击的行。
            if let Some(answer) = custom.get_mut(*question) {
                answer.editing = false;
            }
            highlight[*question] = option;
            if option >= count {
                if let Some(answer) = custom.get_mut(*question) {
                    answer.editing = true;
                    answer.cursor = answer.text.len();
                }
                return;
            }
            // 单选点中即推进；越过最后一问落到确认页，由确认页统一提交。
            if let Some(current) = params.questions.get(*question)
                && pick_ask_option(current, *question, option, selected)
            {
                advance_ask(question, preview_scroll, question_scroll);
            }
        }
        // 折叠态其余鼠标事件（拖动/抬起/移动/右键）与转写区通用路径一致。
        _ if matches!(app.modal, Some(Modal::Ask { parked: true, .. })) => {
            handle_mouse(app, mouse);
        }
        _ => {}
    }
}

/// 图例菜单对一个输入事件的处置：是否消费、是否带选中值应用。
enum LegendAction<T> {
    /// 事件照常处理：无菜单，或菜单因普通键关闭（键位不丢）。
    Passthrough,
    /// 事件已被菜单吃掉。
    Consumed,
    /// 确认选中项：应用并关闭。
    Apply(T),
}

/// 图例菜单的键盘语义：`↑↓` 换高亮、`Enter` 应用、`Esc` 关闭、其他键关菜单后照常处理。
fn legend_menu_key<T: Copy>(
    menu: &mut Option<LegendMenu<T>>,
    modifiers: KeyModifiers,
    code: KeyCode,
) -> LegendAction<T> {
    let Some(state) = menu.as_ref() else {
        return LegendAction::Passthrough;
    };
    let count = state.items.len();
    let selected = state.selected.min(count - 1);
    match (code, modifiers) {
        (KeyCode::Esc, _) => {
            *menu = None;
            LegendAction::Consumed
        }
        (KeyCode::Up, _) => {
            if let Some(state) = menu.as_mut() {
                state.selected = (selected + count - 1) % count;
            }
            LegendAction::Consumed
        }
        (KeyCode::Down, _) => {
            if let Some(state) = menu.as_mut() {
                state.selected = (selected + 1) % count;
            }
            LegendAction::Consumed
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            let item = state.items.get(selected).copied();
            *menu = None;
            match item {
                Some(item) => LegendAction::Apply(item),
                None => LegendAction::Consumed,
            }
        }
        _ => {
            *menu = None;
            LegendAction::Passthrough
        }
    }
}

/// 图例菜单的鼠标语义：悬停换高亮、点条目应用、点菜单外（含再点一次图例）关闭。
fn legend_menu_mouse<T: Copy>(
    menu: &mut Option<LegendMenu<T>>,
    mouse: &crossterm::event::MouseEvent,
) -> LegendAction<T> {
    use crossterm::event::{MouseButton, MouseEventKind};
    let Some(state) = menu.as_mut() else {
        return LegendAction::Passthrough;
    };
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let item = state
                .entry_at(mouse.column, mouse.row)
                .and_then(|index| state.items.get(index).copied());
            *menu = None;
            match item {
                Some(item) => LegendAction::Apply(item),
                None => LegendAction::Consumed,
            }
        }
        MouseEventKind::Moved => {
            if let Some(index) = state.entry_at(mouse.column, mouse.row) {
                state.selected = index;
            }
            LegendAction::Consumed
        }
        // 菜单内滚轮与 ↑↓ 同语义循环换高亮（同补全菜单约定）；菜单外照旧穿透滚转写。
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            if state.area.contains((mouse.column, mouse.row).into())
                && !state.items.is_empty() =>
        {
            let count = state.items.len();
            let selected = state.selected.min(count - 1);
            state.selected = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                (selected + count - 1) % count
            } else {
                (selected + 1) % count
            };
            LegendAction::Consumed
        }
        _ => LegendAction::Passthrough,
    }
}

/// 就地图例菜单（引擎 / 权限）的键盘处理。
///
/// 与斜杠补全菜单同一套约定：只认方向键与确认键，其他按键先关掉菜单再照常处理，
/// 菜单不会吞掉正常输入与快捷键。
fn handle_legend_menu_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) -> bool {
    if app.engine_menu.is_some() {
        return match legend_menu_key(&mut app.engine_menu, modifiers, code) {
            LegendAction::Passthrough => false,
            LegendAction::Consumed => true,
            LegendAction::Apply(engine) => {
                set_engine(app, connection, engine);
                true
            }
        };
    }
    if app.permission_menu.is_some() {
        return match legend_menu_key(&mut app.permission_menu, modifiers, code) {
            LegendAction::Passthrough => false,
            LegendAction::Consumed => true,
            LegendAction::Apply(mode) => {
                apply_permission(app, connection, mode, false);
                true
            }
        };
    }
    false
}

/// 就地图例菜单（引擎 / 权限）的鼠标处理：命中条目即应用，点到菜单外关闭。
/// 返回 `true` 表示事件已被菜单消费，不再落到队列、滚动区或输入框。
fn handle_legend_menu_mouse(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: &crossterm::event::MouseEvent,
) -> bool {
    if app.modal.is_some() {
        return false;
    }
    if app.engine_menu.is_some() {
        return match legend_menu_mouse(&mut app.engine_menu, mouse) {
            LegendAction::Passthrough => false,
            LegendAction::Consumed => true,
            LegendAction::Apply(engine) => {
                set_engine(app, connection, engine);
                true
            }
        };
    }
    if app.permission_menu.is_some() {
        return match legend_menu_mouse(&mut app.permission_menu, mouse) {
            LegendAction::Passthrough => false,
            LegendAction::Consumed => true,
            LegendAction::Apply(mode) => {
                apply_permission(app, connection, mode, false);
                true
            }
        };
    }
    false
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

/// 转写区右缘滚动条：按在滑块上保持抓取点拖动，按在轨道空白处把滑块中线挪过来。
///
/// 几何来自上一帧绘制（[`App::scrollbar`]），这里只做换算；命中滚动条的按下与
/// 拖动不进入文本选区。
fn handle_scrollbar_mouse(app: &mut App, mouse: &crossterm::event::MouseEvent) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    let bar = app.scrollbar;
    let on_track = bar.max_offset > 0
        && mouse.column == bar.track.x
        && mouse.row >= bar.track.y
        && mouse.row < bar.track.y.saturating_add(bar.track.height);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) if on_track => {
            let top = bar.track.y.saturating_add(bar.thumb_top(app.scroll_offset));
            let on_thumb = mouse.row >= top && mouse.row < top.saturating_add(bar.thumb);
            // 按在轨道空白处时滑块中线跟到光标，和正文窗的滚动条一致。
            let grab = if on_thumb {
                i32::from(mouse.row - top)
            } else {
                i32::from(bar.thumb) / 2
            };
            app.scrollbar_drag = Some(ScrollbackDrag { grab });
            if !on_thumb {
                drag_scroll(app, mouse.row, grab);
            }
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(drag) = app.scrollbar_drag else {
                return false;
            };
            drag_scroll(app, mouse.row, drag.grab);
            true
        }
        MouseEventKind::Up(MouseButton::Left) => app.scrollbar_drag.take().is_some(),
        _ => false,
    }
}

/// 滚动条拖拽：光标行换算成滚动偏移，贴到历史顶端时请求更早的一页。
fn drag_scroll(app: &mut App, row: u16, grab: i32) {
    app.scroll_offset = app.scrollbar.offset_at(row, grab);
    request_history_if_at_edge(app);
}

/// 鼠标：滚轮滚动、点击折叠、拖选复制。
fn handle_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) {
    use crossterm::event::{MouseButton, MouseEventKind};
    // 新的一次按下总是结束上一轮滚动条拖拽：拖拽途中弹出模态时抬起事件会被吞掉。
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        app.scrollbar_drag = None;
    }
    if handle_scrollbar_mouse(app, &mouse) {
        return;
    }
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
            if let Some(Modal::Help { scroll } | Modal::Tools { scroll, .. }) = app.modal.as_mut() {
                *scroll = scroll.saturating_sub(3);
                return;
            }
            // 模态打开时兜底不滚背景转写，新模态漏分组也不会误伤。
            if app.modal.is_none() {
                scroll(app, 3);
            }
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
            if let Some(Modal::Help { scroll } | Modal::Tools { scroll, .. }) = app.modal.as_mut() {
                *scroll = scroll.saturating_add(3);
                return;
            }
            if app.modal.is_none() {
                scroll(app, -3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let area = app.prompt_area;
            if position.0 >= area.x
                && position.0 < area.x.saturating_add(area.width)
                && position.1 >= area.y
                && position.1 < area.y.saturating_add(area.height)
            {
                app.focus = Focus::Prompt;
                if let Some(cursor) =
                    crate::render::prompt_cursor_from_point(app, area, position.0, position.1)
                {
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
                // 表格入口优先：它独占一行，若同时命中块标题会点出两件事
                if let Some(hit) = chart_hit_at(app, position) {
                    app.toggle_table_chart(hit.entry, hit.table);
                    app.selection = None;
                    return;
                }
                if let Some(hit) = link_hit_at(app, position) {
                    app.toggle_links(hit.entry);
                    app.selection = None;
                    return;
                }
                // 单击：命中折叠块标题则切换该块档位（完整 ↔ 收起形态）
                if let Some(&(index, ..)) = app
                    .mouse_hits
                    .iter()
                    .find(|&&(_, start, _)| position.1 == start)
                {
                    let axis = app
                        .blocks
                        .get(index)
                        .map_or(DetailLevel::Brief, |entry| app.block_level(&entry.block));
                    if let Some(entry) = app.blocks.get_mut(index) {
                        entry.toggle_level(axis);
                    }
                }
                app.selection = None;
            } else if app.selection.is_some() {
                app.set_notice("Ctrl+C / 右键 / y 复制选中内容");
            }
        }
        // 与 Windows Terminal 一致：有选区时右键直接复制，不再需要额外按键。
        MouseEventKind::Up(MouseButton::Right) if app.selection.is_some() => {
            copy_selection(app);
        }
        MouseEventKind::Moved => {
            // 悬停只做视觉反馈：命中的入口整行提亮
            app.hover_chart = chart_hit_at(app, position).map(|hit| (hit.entry, hit.table));
            app.hover_link = link_hit_at(app, position).map(|hit| hit.entry);
        }
        _ => {}
    }
}

/// 坐标命中的表格入口（列区间限定在按钮文字上，行号必须完全一致）。
fn chart_hit_at(app: &App, position: (u16, u16)) -> Option<ChartHit> {
    app.chart_hits.iter().copied().find(|hit| {
        hit.row == position.1 && position.0 >= hit.x && position.0 < hit.x.saturating_add(hit.w)
    })
}

/// 坐标命中的参考链接折叠入口（列区间限定在按钮文字上）。
fn link_hit_at(app: &App, position: (u16, u16)) -> Option<crate::app::LinkHit> {
    app.link_hits.iter().copied().find(|hit| {
        hit.row == position.1 && position.0 >= hit.x && position.0 < hit.x.saturating_add(hit.w)
    })
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
            .history_page(SessionLoadPageParams {
                full_messages: true,
                plan_page: Some(Default::default()),
                session_id,
                before_entry_id: Some(before_entry_id),
                max_entries: 64,
                max_message_bytes: Some(64 * 1024),
                redo_page: Some(Default::default()),
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
        app.goal = page.goal;
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
        request_history_if_at_edge(app);
    }
}

/// 滚到历史边缘且还有未加载的页时，请求下一页。
fn request_history_if_at_edge(app: &mut App) {
    if app.scroll_offset >= app.scroll_max_offset && app.history_has_more {
        app.history_load_requested = true;
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
    if crate::btw::handle_key(app, modifiers, code) {
        return;
    }
    if handle_legend_menu_key(app, connection, modifiers, code) {
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
        Some("sidebar") => {
            if app.subagents.is_empty() {
                app.set_notice("当前没有子代理");
            } else {
                let visible = app.subagent_sidebar_visible();
                app.subagent_sidebar = Some(!visible);
            }
            return;
        }
        Some("engine") => {
            cycle_engine(app, connection);
            return;
        }
        Some("queue") => {
            crate::queue::submit(app, connection, true);
            return;
        }
        Some("queue_edit") => {
            let Some(session) = app.session.as_ref().map(|session| session.id.clone()) else {
                return;
            };
            let steer_id = app.steers.get(&session).and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|entry| entry.withdrawal.is_none())
                    .map(|entry| entry.id)
            });
            let queue_id = app
                .queues
                .get(&session)
                .and_then(|queue| queue.front())
                .map(|entry| entry.id);
            if let Some(id) = steer_id.or(queue_id) {
                crate::queue::edit(app, connection, id);
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
    // 原生终端语义：有选区时 Ctrl+C 先复制，没有选区才走取消/清空输入。
    if app.selection.is_some()
        && matches!(code, KeyCode::Char('c' | 'C'))
        && (modifiers == KeyModifiers::CONTROL
            || modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT))
        && copy_selection(app)
    {
        app.ctrl_c_pending = None;
        return;
    }
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
                    app.set_notice("正在停止... 再次按 Ctrl+C 退出");
                    cancel_prompt(&connection.client, app);
                }
            } else if !app.prompt_text().is_empty() {
                if app.take_ctrl_c_pending(CtrlCPending::ClearPrompt) {
                    app.prompt.clear();
                    app.prompt_changed();
                    app.notice = None;
                    app.notice_at = None;
                } else {
                    app.ctrl_c_pending =
                        Some((CtrlCPending::ClearPrompt, std::time::Instant::now()));
                    app.set_notice("再次按 Ctrl+C 清空输入");
                }
            } else if app.take_ctrl_c_pending(CtrlCPending::Quit) {
                app.should_quit = true;
            } else {
                app.ctrl_c_pending = Some((CtrlCPending::Quit, std::time::Instant::now()));
                app.set_notice("再次按 Ctrl+C 退出");
            }
        }
        (KeyCode::Char('h'), KeyModifiers::CONTROL) => app.modal = Some(Modal::Help { scroll: 0 }),
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            app.cycle_thinking_detail();
            app.set_notice(format!("思考显示：{}", app.thinking_detail.label()));
        }
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
            app.cycle_tool_detail();
            app.set_notice(format!("工具显示：{}", app.tool_detail.label()));
        }
        (KeyCode::Char('v'), KeyModifiers::CONTROL) => spawn_clipboard_paste(app.events.clone()),
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => app.open_model_picker(),
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => open_session_picker(app, connection),
        (KeyCode::Char('s'), KeyModifiers::CONTROL) if app.focused.is_some() => {
            // 聚焦时 ctrl+s 停止的就是眼前这个子代理。
            let agent_id = app.session.as_ref().map(|session| session.id.clone());
            if let Some(agent_id) = agent_id {
                stop_agent_operation(app, connection, agent_id);
            }
        }
        (KeyCode::Left, _)
            if app.focused.is_some()
                && app.focus == Focus::Prompt
                && app.prompt_text().is_empty() =>
        {
            // 双击 `←` 返回主会话（omp Hub 的手势）；单次按键不吞掉。
            if app.take_left_tap_pending(std::time::Instant::now()) {
                leave_focus(app, connection);
            } else {
                app.left_tap_pending = Some(std::time::Instant::now());
                app.prompt.left();
            }
        }
        (KeyCode::Char('y'), KeyModifiers::NONE) if app.selection.is_some() => {
            copy_selection(app);
        }
        (KeyCode::Esc, _) => {
            if app.selection.is_some() {
                app.selection = None;
            } else if app.focused.is_some()
                && app.focus == Focus::Prompt
                && app.prompt_text().is_empty()
            {
                // 聚焦子代理时的 `Esc` 只返回主会话，不打断子代理（omp Hub 的语义）；
                // 取消仍在运行的那个提示交给 `Ctrl+C`。
                leave_focus(app, connection);
            } else if app.prompt_running {
                request_stop(app, connection);
            } else if app.focus == Focus::Prompt && !app.prompt_text().is_empty() {
                if app.clear_pending {
                    app.prompt.clear();
                    app.prompt_changed();
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
        (KeyCode::Tab, _) => {
            app.focus = if app.focus == Focus::Prompt && app.btw.expanded {
                app.selection = None;
                Focus::Btw
            } else {
                app.focus.toggle()
            };
        }
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
    if crate::btw::submit(app, connection) {
        return;
    }
    let name = app
        .prompt_text()
        .split_whitespace()
        .next()
        .unwrap_or_default();
    // 只有已注册的命令才分流；绝对路径和其他斜杠文本按普通消息发送。
    if app.prompt_text().starts_with('/') && app.commands.resolve(name).is_some() {
        if app.prompt.has_attached_images() {
            app.set_notice("斜杠命令不支持图片，请移除图片后执行");
            return;
        }
        let (line, history) = app.prompt.take_committed_text();
        if let Some(text) = history {
            app.record_input_history(&text);
        }
        execute_command(app, connection, line.trim().to_owned());
        return;
    }
    // `.` 独占整条输入时是 `/retry` 的快捷写法：走同一分派，忙时门控与提示完全复用。
    if app.prompt_text().trim() == "." {
        let (_, history) = app.prompt.take_committed_text();
        if let Some(text) = history {
            app.record_input_history(&text);
        }
        execute_command(app, connection, "/retry".to_owned());
        return;
    }
    if app.prompt.is_empty() {
        return;
    }
    crate::queue::submit(app, connection, false);
}
async fn load_rewind_pages(
    client: &ClientHandle,
    session_id: String,
) -> anyhow::Result<moc_protocol::SessionLoadPageResult> {
    use moc_protocol::methods::session::RedoPageParams;
    let mut result = client
        .history_page(SessionLoadPageParams {
            full_messages: true,
            plan_page: Some(Default::default()),
            session_id: session_id.clone(),
            before_entry_id: None,
            max_entries: 128,
            max_message_bytes: Some(64 * 1024),
            redo_page: Some(RedoPageParams::default()),
        })
        .await?;
    let mut pages = Vec::new();
    let mut has_more = result.redo_has_more;
    let mut before = None;
    while has_more {
        let page = client
            .history_page(SessionLoadPageParams {
                full_messages: true,
                plan_page: Some(Default::default()),
                session_id: session_id.clone(),
                before_entry_id: None,
                max_entries: 0,
                max_message_bytes: Some(64 * 1024),
                redo_page: Some(RedoPageParams {
                    before_entry_id: before,
                    max_entries: 128,
                }),
            })
            .await?;
        has_more = page.redo_has_more;
        before = if has_more {
            Some(
                page.redo_entries
                    .first()
                    .context("前滚分页未返回可继续的游标")?
                    .id
                    .clone(),
            )
        } else {
            None
        };
        pages.push(page.redo_entries);
    }
    result.redo_entries = pages.into_iter().rev().flatten().collect();
    result.redo_has_more = false;
    Ok(result)
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
        let result = load_rewind_pages(&client, session_id).await;
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
/// 帮助页 PgUp/PgDn 每次滚动的行数；渲染层会夹到实际内容高度。
const HELP_PAGE: usize = 8;

/// Fusion 组合的应用：两侧已齐时，规范组合串等于 `current` 就关闭选择器（返回 `true`），
/// 否则置 loading 并延后发起 `session/set_model`。调用方先处理两侧有空的情况。
fn apply_fusion_pick(
    picker: &mut crate::modal::PickerState,
    fusion: &crate::modal::FusionDraft,
    current: &str,
    session_id: &Option<String>,
    session_apply: &mut Option<(Option<String>, String)>,
) -> bool {
    let selector = crate::modal::fusion_selector(&fusion.lead, &fusion.sidekick);
    if selector == current {
        true
    } else {
        picker.loading = true;
        picker.error = None;
        *session_apply = Some((session_id.clone(), selector));
        false
    }
}

/// `session/set_model` 的异步发起：普通模型与 Fusion 组合的应用共用；
/// 需要压缩时照旧走 `ModelSwitchConfirm` 确认流。
fn request_session_model(
    app: &mut App,
    connection: &EmbeddedConnection,
    session_id: Option<String>,
    selector: String,
) {
    let Some(session_id) = session_id else {
        let client = connection.client.clone();
        let events = app.events.clone();
        let cwd = app.startup_cwd.clone();
        let engine = app.startup_engine;
        let plan_active = app.startup_plan;
        tokio::spawn(async move {
            match client
                .call::<SessionNew>(SessionNewParams {
                    cwd: Some(cwd),
                    model: Some(selector),
                    title: None,
                    engine,
                    plan_active,
                    fast_mode: false,
                })
                .await
            {
                Ok(session) => {
                    let _ = events.send(AppEvent::Loaded(
                        session,
                        Vec::new(),
                        false,
                        None,
                        Vec::new(),
                        false,
                        None,
                    ));
                }
                Err(error) => {
                    let _ = events.send(AppEvent::Notice(format!("创建会话失败：{error}")));
                }
            }
        });
        app.modal = None;
        return;
    };
    app.inflight_session_ops.insert(session_id.clone());
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let outcome = client
            .call::<SessionSetModel>(SessionSetModelParams {
                session_id: session_id.clone(),
                selector: selector.clone(),
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

/// 模态键盘路由。
///
/// `app.modal` 以 `as_mut` 借用；需要关闭模态或访问会话的分支通过
/// `close` 标志延后到借用结束后生效。
/// 计划审阅窗口的按键：编辑器 → 批注视图 → 阅读视图逐级分派。
fn plan_modal_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let editing = matches!(&app.modal, Some(Modal::Plan { state }) if state.editor.is_some());
    if editing {
        plan_editor_key(app, modifiers, code);
        return;
    }
    let pane = match &app.modal {
        Some(Modal::Plan { state }) => state.pane,
        _ => return,
    };
    match pane {
        PlanPane::Annotate => plan_annotate_key(app, connection, modifiers, code),
        PlanPane::Read => plan_read_key(app, connection, modifiers, code),
    }
}

fn plan_editor_key(app: &mut App, modifiers: KeyModifiers, code: KeyCode) {
    if code == KeyCode::Esc {
        if let Some(Modal::Plan { state }) = app.modal.as_mut() {
            state.editor = None;
        }
        return;
    }
    if code == KeyCode::Char('s') && modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(Modal::Plan { state }) = app.modal.as_mut() {
            state.save_editor();
        }
        return;
    }
    if code == KeyCode::Char('g') && modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(Modal::Plan { state }) = app.modal.as_mut() {
            insert_suggestion_block(state);
        }
        return;
    }
    if let Some(Modal::Plan { state }) = app.modal.as_mut()
        && let Some(editor) = state.editor.as_mut()
    {
        crate::widgets::prompt::edit_input(&mut editor.body, modifiers, code);
    }
}

/// 在批注正文里插入 suggestion 围栏，并预填选中的源码行。
fn insert_suggestion_block(state: &mut crate::modal::PlanReviewState) {
    let prefill = match state.editor.as_ref() {
        Some(editor) => {
            let lines = state.source_lines();
            if lines.is_empty() {
                return;
            }
            let start = editor.start_line.saturating_sub(1).min(lines.len() - 1);
            let end = editor.end_line.saturating_sub(1).min(lines.len() - 1);
            lines[start..=end].join("\n")
        }
        None => return,
    };
    let Some(editor) = state.editor.as_mut() else {
        return;
    };
    let mut block = String::new();
    if !editor.body.text.is_empty() && !editor.body.text.ends_with('\n') {
        block.push('\n');
    }
    block.push_str("```suggestion\n");
    block.push_str(&prefill);
    block.push_str("\n```\n");
    for character in block.chars() {
        editor.body.insert_char(character);
    }
}

fn plan_annotate_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let height = usize::from(app.plan_body_area.height).max(1);
    // 提交与视图切换先处理，避免与 `state` 的借用冲突。
    match (code, modifiers) {
        (KeyCode::Char('s'), KeyModifiers::NONE) => {
            submit_plan_review(app, connection);
            return;
        }
        (KeyCode::Esc, _) | (KeyCode::Char('a'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.pane = PlanPane::Read;
                state.anchor = None;
                state.scroll_offset = 0;
            }
            return;
        }
        (KeyCode::Char('d'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.pane = PlanPane::Read;
                state.anchor = None;
                state.scroll_offset = 0;
                if state.previous_plan_id.is_some() {
                    state.show_diff = !state.show_diff;
                }
            }
            return;
        }
        _ => {}
    }
    let Some(Modal::Plan { state }) = app.modal.as_mut() else {
        return;
    };
    match (code, modifiers) {
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
            // 选行模式下方向键延长选区（类 vim visual），否则只移动光标。
            let select = state.anchor.is_some();
            state.move_cursor(-1, select);
            state.ensure_cursor_visible(height);
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
            let select = state.anchor.is_some();
            state.move_cursor(1, select);
            state.ensure_cursor_visible(height);
        }
        (KeyCode::PageUp, _) => {
            let select = state.anchor.is_some();
            state.move_cursor(-10, select);
            state.ensure_cursor_visible(height);
        }
        (KeyCode::PageDown, _) => {
            let select = state.anchor.is_some();
            state.move_cursor(10, select);
            state.ensure_cursor_visible(height);
        }
        (KeyCode::Char('v'), KeyModifiers::NONE) => {
            state.anchor = if state.anchor.is_some() {
                None
            } else {
                Some(state.cursor)
            };
        }
        (KeyCode::Char('c'), KeyModifiers::NONE) | (KeyCode::Enter, KeyModifiers::NONE) => {
            state.open_editor();
        }
        (KeyCode::Char('x'), KeyModifiers::NONE) => state.remove_draft_at_cursor(),
        _ => {}
    }
}

fn plan_read_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    let (plan_id, execution_engine, selected, status) = match app.modal.as_ref() {
        Some(Modal::Plan { state }) => (
            state.plan_id.clone(),
            state.execution_engine,
            state.selected,
            state.status,
        ),
        _ => return,
    };
    match (code, modifiers) {
        (KeyCode::Esc, _) => app.modal = None,
        (KeyCode::Char('a'), KeyModifiers::NONE) => plan_toggle_annotate(app),
        (KeyCode::Char('d'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut()
                && state.previous_plan_id.is_some()
            {
                state.show_diff = !state.show_diff;
                state.scroll_offset = 0;
            }
        }
        // 「在新会话执行」的目标引擎循环；其他动作忽略该字段。
        (KeyCode::Char('m'), KeyModifiers::NONE) => {
            let engines: Vec<SessionEngine> = app.engines().collect();
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                let index = engines
                    .iter()
                    .position(|engine| *engine == state.execution_engine)
                    .unwrap_or(0);
                state.execution_engine = engines[(index + 1) % engines.len()];
            }
        }
        (KeyCode::Char('s'), KeyModifiers::NONE) => submit_plan_review(app, connection),
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => plan_scroll(app, -1),
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => plan_scroll(app, 1),
        (KeyCode::PageUp, _) => plan_scroll(app, -10),
        (KeyCode::PageDown, _) => plan_scroll(app, 10),
        (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.selected = state.selected.saturating_sub(1);
            }
        }
        (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.selected = (state.selected + 1).min(3);
            }
        }
        (KeyCode::Tab, _) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.selected = (state.selected + 1) % 4;
            }
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            if status != moc_protocol::methods::PlanStatus::Submitted {
                return;
            }
            if selected == 3 {
                app.modal = None;
                return;
            }
            let Some(session_id) = session_id else {
                return;
            };
            let action = match selected {
                0 => moc_protocol::methods::PlanAction::Execute,
                1 => moc_protocol::methods::PlanAction::ExecuteCompacted,
                2 => moc_protocol::methods::PlanAction::ExecuteNewSession,
                _ => return,
            };
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.submitting = true;
                state.error = None;
            }
            let client = connection.client.clone();
            let events = app.events.clone();
            tokio::spawn(async move {
                let result = client
                    .call::<SessionPlanApprove>(SessionPlanApproveParams {
                        session_id,
                        plan_id,
                        action,
                        execution_engine: Some(execution_engine),
                    })
                    .await
                    .map_err(anyhow::Error::from);
                let _ = events.send(AppEvent::PlanApproval(result));
            });
        }
        _ => {}
    }
}

fn plan_scroll(app: &mut App, delta: isize) {
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        state.scroll_offset = state.scroll_offset.saturating_add_signed(delta);
    }
}

fn plan_toggle_annotate(app: &mut App) {
    let Some(Modal::Plan { state }) = app.modal.as_mut() else {
        return;
    };
    if !state.can_annotate() {
        state.error = Some("只有待审阅的计划可以批注".into());
        return;
    }
    state.pane = PlanPane::Annotate;
    state.cursor = 0;
    state.anchor = None;
    state.scroll_offset = 0;
    state.error = None;
}

/// 把草稿批注提交给 core；回执里的修订提示随后当普通提示发送。
fn submit_plan_review(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let (plan_id, annotations) = match app.modal.as_ref() {
        Some(Modal::Plan { state }) => {
            if !state.can_annotate() || state.drafts.is_empty() {
                return;
            }
            (state.plan_id.clone(), state.annotations())
        }
        _ => return,
    };
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        state.submitting = true;
        state.error = None;
    }
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionPlanRevise>(SessionPlanReviseParams {
                session_id,
                plan_id,
                annotations,
                note: None,
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::PlanRevised(result));
    });
}

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
        // 计划审阅有自己的编辑/选区语义，整体交给 `plan_modal_key`。
        plan_modal_key(app, connection, modifiers, code);
        return;
    }
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    let events = app.events.clone();
    let mut close = false;
    // 模型角色字段选定的值：延迟到模态借用结束后写回配置。
    let mut config_pick: Option<(Vec<String>, String, String)> = None;
    // `session/set_model` 的发起也延后：模态借用期拿不走 `&mut App`。
    let mut session_apply: Option<(Option<String>, String)> = None;
    // 详情卡 Fast Mode 行：切换请求延后到借用结束后发出。
    let mut fast_mode_apply: Option<bool> = None;
    let session_fast = app
        .session
        .as_ref()
        .is_some_and(|session| session.fast_mode);
    // workflow：模型选择后还原的 modal、审批决策、待打开 run 面板/选择器。
    let mut workflow_back: Option<crate::workflow::WorkflowRoleBack> = None;
    let mut workflow_decision: Option<(String, moc_protocol::methods::WorkflowDecision)> = None;
    let mut workflow_picker: Option<(crate::workflow::FocusTarget, String)> = None;
    let mut workflow_action: Option<crate::workflow::WorkflowAction> = None;
    let plan_review_open = matches!(app.modal, Some(Modal::Plan { .. }));
    // 键盘接管：丢掉悬停坐标，不与选中高亮抢视觉（模型选择器在其分支内自处理）。
    match app.modal.as_mut() {
        Some(Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. }) => {
            picker.pointer = None
        }
        Some(Modal::Jobs { pointer, .. }) => *pointer = None,
        Some(Modal::WorkflowApproval { state }) => state.pointer = None,
        Some(Modal::WorkflowRun { state }) => state.pointer = None,
        Some(Modal::Rewind { state }) => state.pointer = None,
        _ => {}
    }
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
            preview_scroll,
            question_scroll,
            parked,
            reply,
        } => {
            // Ctrl+C 与权限弹窗同语义：任何状态（聚焦/折叠/编辑/确认页）下直接取消整单，
            // 不让肌肉记忆落在被吞掉的键上。
            if matches!(
                (code, modifiers),
                (KeyCode::Char('c'), KeyModifiers::CONTROL)
            ) {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(moc_protocol::RpcError::of(
                        moc_protocol::RpcErrorCode::RequestCancelled,
                    )));
                }
                close = true;
            } else if *parked {
                // 折叠态：键盘交给转写区（与 Focus::Scrollback 同一套滚动键），
                // 卡片收成输入框位的细条，Tab 随时回来继续作答。
                match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => scroll(app, 3),
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        scroll(app, -3)
                    }
                    (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        scroll(app, 10)
                    }
                    (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        scroll(app, -10)
                    }
                    (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                        app.scroll_offset = usize::MAX
                    }
                    (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::SHIFT) => {
                        app.scroll_offset = 0
                    }
                    (KeyCode::Tab, _)
                    | (KeyCode::Enter, KeyModifiers::NONE)
                    | (KeyCode::Char(' '), KeyModifiers::NONE) => *parked = false,
                    (KeyCode::Char('z'), KeyModifiers::NONE) => {
                        *parked = false;
                        if let Some(current) = params.questions.get(*question) {
                            highlight[*question] = current.options.len();
                            if let Some(answer) = custom.get_mut(*question) {
                                answer.editing = true;
                                answer.cursor = answer.text.len();
                            }
                        }
                        *preview_scroll = 0;
                    }
                    // ←→ 翻看问题与作答态同一语义：只动指针，不提交；确认页也算一页。
                    (KeyCode::Left, _) => {
                        if *question > 0 {
                            *question -= 1;
                            *preview_scroll = 0;
                            *question_scroll = 0;
                        }
                    }
                    (KeyCode::Right, _) => {
                        if *question < params.questions.len() {
                            *question += 1;
                            *preview_scroll = 0;
                            *question_scroll = 0;
                        }
                    }
                    (KeyCode::Char('X'), KeyModifiers::SHIFT) => {
                        close |= dismiss_ask(reply);
                    }
                    (KeyCode::Esc, _) => {
                        // 折叠后再退一次才取消整单（core 侧按「用户没有回答」收场）。
                        if let Some(reply) = reply.take() {
                            let _ = reply.send(Err(moc_protocol::RpcError::of(
                                moc_protocol::RpcErrorCode::RequestCancelled,
                            )));
                        }
                        close = true;
                    }
                    _ => {}
                }
            } else if *question >= params.questions.len() {
                // 确认页：越过最后一问强制经过，提交前最后一次通览全部回答。
                // enter/tab 确认提交；←/shift+tab 回上一问；↑↓ 滚动答案列表（复用
                // preview_scroll 当滚动偏移）；esc 折叠；shift+X 整单跳过。
                match (code, modifiers) {
                    (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Tab, KeyModifiers::NONE) => {
                        close |= submit_ask(params, selected, custom, reply);
                    }
                    (KeyCode::Tab, KeyModifiers::SHIFT)
                    | (KeyCode::BackTab, _)
                    | (KeyCode::Left, _) => {
                        *question = params.questions.len().saturating_sub(1);
                        *preview_scroll = 0;
                        *question_scroll = 0;
                    }
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        *preview_scroll = preview_scroll.saturating_sub(1);
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        *preview_scroll = preview_scroll.saturating_add(1);
                    }
                    (KeyCode::PageUp, _) => {
                        *preview_scroll = preview_scroll.saturating_sub(ASK_PREVIEW_SCROLL_STEP);
                    }
                    (KeyCode::PageDown, _) => {
                        *preview_scroll = preview_scroll.saturating_add(ASK_PREVIEW_SCROLL_STEP);
                    }
                    // 跳过整单：所有问题按未回答提交，模型收到明确语义而非错误。
                    (KeyCode::Char('X'), KeyModifiers::SHIFT) => {
                        close |= dismiss_ask(reply);
                    }
                    (KeyCode::Esc, _) => *parked = true,
                    _ => {}
                }
            } else if let Some(answer) = custom.get_mut(*question) {
                let current = &params.questions[*question];
                let option_count = current.options.len();
                if answer.editing {
                    let mut submit = false;
                    {
                        match (code, modifiers) {
                            (KeyCode::Esc, _) => answer.editing = false,
                            (KeyCode::Enter, _) => {
                                answer.editing = false;
                                submit = true;
                            }
                            // 光标与删除一律按字符步进：正文是 UTF-8，按字节移动会切进多字节字符。
                            (KeyCode::Left, _) => {
                                answer.cursor = cursor_prev(&answer.text, answer.cursor)
                            }
                            (KeyCode::Right, _) => {
                                answer.cursor = cursor_next(&answer.text, answer.cursor)
                            }
                            (KeyCode::Home, _) => answer.cursor = 0,
                            (KeyCode::End, _) => answer.cursor = answer.text.len(),
                            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                                answer.text.clear();
                                answer.cursor = 0;
                            }
                            (KeyCode::Backspace, _) => {
                                let previous = cursor_prev(&answer.text, answer.cursor);
                                if previous != answer.cursor {
                                    answer.text.remove(previous);
                                }
                                answer.cursor = previous;
                            }
                            (KeyCode::Delete, _) => {
                                let cursor = clamp_cursor(&answer.text, answer.cursor);
                                if answer.text[cursor..].chars().next().is_some() {
                                    answer.text.remove(cursor);
                                }
                                answer.cursor = cursor;
                            }
                            (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                                if !ch.is_control() =>
                            {
                                let pos = clamp_cursor(&answer.text, answer.cursor);
                                answer.text.insert(pos, ch);
                                answer.cursor = pos + ch.len_utf8();
                            }
                            _ => {}
                        }
                    }
                    if submit {
                        advance_ask(question, preview_scroll, question_scroll);
                    }
                } else {
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
                        // 长文区滚动（问题正文窗 + 详情区）：只改偏移，越界由渲染层夹。
                        (KeyCode::PageUp, _) => {
                            *preview_scroll =
                                preview_scroll.saturating_sub(ASK_PREVIEW_SCROLL_STEP);
                            *question_scroll =
                                question_scroll.saturating_sub(ASK_PREVIEW_SCROLL_STEP);
                        }
                        (KeyCode::PageDown, _) => {
                            *preview_scroll =
                                preview_scroll.saturating_add(ASK_PREVIEW_SCROLL_STEP);
                            *question_scroll =
                                question_scroll.saturating_add(ASK_PREVIEW_SCROLL_STEP);
                        }
                        (KeyCode::Char('z'), KeyModifiers::NONE) => {
                            highlight[*question] = option_count;
                            if let Some(answer) = custom.get_mut(*question) {
                                answer.editing = true;
                                answer.cursor = answer.text.len();
                            }
                            *preview_scroll = 0;
                        }
                        (KeyCode::Char(' '), KeyModifiers::NONE) => {
                            // 多选切换；单选只写入聚焦项，自定义行上无效
                            pick_ask_option(current, *question, highlight[*question], selected);
                        }
                        // 数字直达：单选选中即推进；多选切换勾选，enter 才推进。
                        (KeyCode::Char(digit), KeyModifiers::NONE)
                            if digit.is_ascii_digit() && digit != '0' =>
                        {
                            let index = digit.to_digit(10).expect("ascii digit") as usize - 1;
                            if pick_ask_option(current, *question, index, selected) {
                                advance_ask(question, preview_scroll, question_scroll);
                            }
                        }
                        // 复制聚焦项：选项给 label，自定义行给已输入的正文。
                        (KeyCode::Char('y'), KeyModifiers::NONE) => {
                            let text = current
                                .options
                                .get(highlight[*question])
                                .map(|option| option.label.clone())
                                .or_else(|| {
                                    custom
                                        .get(*question)
                                        .filter(|answer| !answer.text.is_empty())
                                        .map(|answer| answer.text.clone())
                                });
                            if let Some(text) = text {
                                write_osc52(&text);
                                tokio::spawn(async move {
                                    match crate::clipboard::copy_text_to_clipboard(&text).await {
                                        Ok(()) => tracing::debug!("选项已复制"),
                                        Err(error) => tracing::debug!(%error, "复制选项失败"),
                                    }
                                });
                                app.set_notice("已复制选项");
                            }
                        }
                        (KeyCode::Enter, KeyModifiers::NONE) => {
                            if highlight[*question] == option_count {
                                if let Some(answer) = custom.get_mut(*question) {
                                    answer.editing = true;
                                    answer.cursor = answer.text.len();
                                }
                            } else if current.multi {
                                if *question + 1 < params.questions.len() {
                                    // 多选 enter = 空格：只切换聚焦项，
                                    // 跳问走 tab/方向键，不在 enter 上兼差。
                                    pick_ask_option(
                                        current,
                                        *question,
                                        highlight[*question],
                                        selected,
                                    );
                                } else {
                                    // 最后一问没有下一题：enter 在此是通往确认页的手势。
                                    advance_ask(question, preview_scroll, question_scroll);
                                }
                            } else if let Some(option) = current.options.get(highlight[*question]) {
                                selected[*question] = vec![option.label.clone()];
                                advance_ask(question, preview_scroll, question_scroll);
                            }
                        }
                        (KeyCode::Tab, KeyModifiers::NONE) => {
                            // tab 只在问题间循环：最后一问绕回第一问，
                            // 通往确认页的手势是最后一问的 enter。
                            *question = (*question + 1) % params.questions.len();
                            *preview_scroll = 0;
                            *question_scroll = 0;
                        }
                        (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::BackTab, _) => {
                            // shift+tab 反向循环：第一问绕回最后一问。
                            *question = question
                                .checked_sub(1)
                                .unwrap_or_else(|| params.questions.len() - 1);
                            *preview_scroll = 0;
                            *question_scroll = 0;
                        }
                        // ←→ 只翻看问题指针：不写入选择也不提交，回看时误触不会推进；
                        // 最后一问再 → 进入确认页，确认提交始终是确认页上 enter 的事。
                        (KeyCode::Left, _) => {
                            if *question > 0 {
                                *question -= 1;
                                *preview_scroll = 0;
                                *question_scroll = 0;
                            }
                        }
                        (KeyCode::Right, _) => {
                            if *question < params.questions.len() {
                                *question += 1;
                                *preview_scroll = 0;
                                *question_scroll = 0;
                            }
                        }
                        // 跳过整单：所有问题按未回答提交，模型收到明确语义而非错误。
                        (KeyCode::Char('X'), KeyModifiers::SHIFT) => {
                            close |= dismiss_ask(reply);
                        }
                        (KeyCode::Esc, _) => {
                            // 先折叠去翻提问背后的上下文；折叠态再 Esc 才取消整单。
                            *parked = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        Modal::Rewind { state } => {
            if state.filtering {
                match (code, modifiers) {
                    (KeyCode::Esc, _) => {
                        if state.query.is_empty() {
                            state.filtering = false;
                        } else {
                            state.clear_filter();
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        state.backspace();
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        state.input_char(c);
                    }
                    (KeyCode::Up, _) => state.step(-1),
                    (KeyCode::Down, _) => state.step(1),
                    (KeyCode::Left, _) => state.jump_user(-1),
                    (KeyCode::Right, _) => state.jump_user(1),
                    (KeyCode::PageUp, _) => state.scroll_detail(-6),
                    (KeyCode::PageDown, _) => state.scroll_detail(6),
                    (KeyCode::Enter, KeyModifiers::NONE) if !state.loading => {
                        let is_redo = state.selected_item_ref().is_some_and(|item| item.future);
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
                            let result = if is_redo {
                                client
                                    .call::<SessionRedo>(SessionRedoParams {
                                        omit_auxiliary: true,
                                        session_id,
                                        entry_id: selected.id.clone(),
                                        max_entries: Some(0),
                                    })
                                    .await
                            } else {
                                client
                                    .call::<SessionRewind>(SessionRewindParams {
                                        omit_auxiliary: true,
                                        session_id,
                                        entry_id: selected.id.clone(),
                                        max_entries: Some(0),
                                    })
                                    .await
                            };
                            let result = load_mutation_page(&client, result).await;
                            let _ = events.send(AppEvent::Rewound {
                                selected,
                                is_redo,
                                result: Box::new(result),
                            });
                        });
                    }
                    _ => {}
                }
            } else {
                match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Char('k' | 'K'), KeyModifiers::NONE) => {
                        state.step(-1);
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j' | 'J'), KeyModifiers::NONE) => {
                        state.step(1);
                    }
                    (KeyCode::Left, _) | (KeyCode::Char('h' | 'H'), KeyModifiers::NONE) => {
                        state.jump_user(-1);
                    }
                    (KeyCode::Right, _) | (KeyCode::Char('l' | 'L'), KeyModifiers::NONE) => {
                        state.jump_user(1);
                    }
                    (KeyCode::Char('f' | 'F' | '/'), KeyModifiers::NONE) => {
                        state.start_filter();
                    }
                    // 选中含 ask 调用的条目时重新作答：core 截回未回答状态、按原参数重弹提问卡片。
                    (KeyCode::Char('r' | 'R'), KeyModifiers::NONE) => {
                        if !state.loading {
                            let reask_call = state.selected_item_ref().and_then(|item| {
                                (!item.future).then(|| {
                                    item.entry
                                        .message
                                        .tool_calls()
                                        .find(|call| call.name == "ask")
                                        .map(|call| call.id.clone())
                                })?
                            });
                            let Some(tool_call_id) = reask_call else {
                                state.error = Some("选中的条目没有可重新作答的提问".into());
                                return;
                            };
                            let Some(session_id) = session_id.clone() else {
                                state.error = Some("当前没有活动会话".into());
                                return;
                            };
                            // 先关模态：重放的提问卡片要在主界面弹出作答。
                            // 作答期间会话对外表现为「操作在飞」，用户输入进队列
                            // 而不是撞 SessionBusy。
                            close = true;
                            app.inflight_session_ops.insert(session_id.clone());
                            let client = connection.client.clone();
                            let events = events.clone();
                            tokio::spawn(async move {
                                let result = client
                                    .call::<SessionReask>(SessionReaskParams {
                                        omit_auxiliary: true,
                                        session_id,
                                        tool_call_id,
                                        max_entries: Some(0),
                                    })
                                    .await;
                                let result = load_mutation_page(&client, result).await;
                                let _ = events.send(AppEvent::Reasked {
                                    result: Box::new(result),
                                });
                            });
                        }
                    }
                    (KeyCode::PageUp, _) => state.scroll_detail(-6),
                    (KeyCode::PageDown, _) => state.scroll_detail(6),
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        if !state.loading {
                            let is_redo = state.selected_item_ref().is_some_and(|item| item.future);
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
                                let result = if is_redo {
                                    client
                                        .call::<SessionRedo>(SessionRedoParams {
                                            omit_auxiliary: true,
                                            session_id,
                                            entry_id: selected.id.clone(),
                                            max_entries: Some(0),
                                        })
                                        .await
                                } else {
                                    client
                                        .call::<SessionRewind>(SessionRewindParams {
                                            omit_auxiliary: true,
                                            session_id,
                                            entry_id: selected.id.clone(),
                                            max_entries: Some(0),
                                        })
                                        .await
                                };
                                let result = load_mutation_page(&client, result).await;
                                let _ = events.send(AppEvent::Rewound {
                                    selected,
                                    is_redo,
                                    result: Box::new(result),
                                });
                            });
                        }
                    }
                    (KeyCode::Esc, _) if state.loading && !state.items.is_empty() => {}
                    (KeyCode::Esc, _) => close = true,
                    _ => {}
                }
            }
        }
        Modal::Model {
            entries,
            current,
            picker,
            level,
            detail,
            target,
        } => {
            use crate::modal::{DetailRow, FUSION_ENTRY_SELECTOR, MenuItems, MenuState};
            detail.pointer = None;
            let filtered = crate::modal::filtered_entries(entries, &picker.query);
            let highlighted = |picker: &crate::modal::PickerState| {
                crate::modal::filtered_entries(entries, &picker.query)
                    .get(picker.selected)
                    .map(|(index, _)| *index)
            };
            let entry = filtered.get(picker.selected).map(|(_, entry)| *entry);
            let before = highlighted(picker);
            let rows = crate::modal::detail_rows(entry, entries, target, &detail.fusion);
            // 展开某详情行的下拉：selected 指向当前生效值，query 从头开始。
            let open_menu =
                |detail: &mut crate::modal::DetailState, row: DetailRow, current: Option<usize>| {
                    detail.menu = Some(MenuState {
                        row,
                        selected: current.unwrap_or(0),
                        offset: 0,
                        scroll: Default::default(),
                        query: String::new(),
                    });
                };
            if let Some(menu) = detail.menu.as_mut() {
                // 下拉打开：候选内导航；模型下拉支持输入过滤，等级下拉忽略输入。
                let items = crate::modal::menu_items(
                    menu.row,
                    entry,
                    entries,
                    level,
                    &detail.fusion,
                    &menu.query,
                );
                let count = items.len();
                let model_menu = matches!(menu.row, DetailRow::Lead | DetailRow::Sidekick);
                match (code, modifiers) {
                    (KeyCode::Up, _) => menu.move_selection(-1, count),
                    (KeyCode::Down, _) => menu.move_selection(1, count),
                    (KeyCode::PageUp, _) => menu.move_selection(-8, count),
                    (KeyCode::PageDown, _) => menu.move_selection(8, count),
                    (KeyCode::Home, _) => {
                        menu.selected = 0;
                        menu.offset = 0;
                        menu.scroll.reveal_selection = true;
                    }
                    (KeyCode::End, _) => {
                        menu.selected = count.saturating_sub(1);
                        menu.keep_visible(count);
                    }
                    (KeyCode::Esc, _) | (KeyCode::Left, _) => detail.menu = None,
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        // 会话目标下切换进行中：Level 提交是 no-op，下拉保持打开。
                        if picker.loading
                            && menu.row == DetailRow::Level
                            && matches!(target, crate::modal::ModelTarget::Session)
                        {
                            return;
                        }
                        let chosen = match &items {
                            MenuItems::Levels { levels, .. } => levels.get(menu.selected).cloned(),
                            MenuItems::Models {
                                entries: candidates,
                                ..
                            } => {
                                candidates
                                    .get(menu.selected)
                                    .map(|(_, candidate)| match menu.row {
                                        DetailRow::Lead => crate::modal::fusion_side_pick(
                                            &detail.fusion.lead,
                                            candidate,
                                        ),
                                        _ => crate::modal::fusion_side_pick(
                                            &detail.fusion.sidekick,
                                            candidate,
                                        ),
                                    })
                            }
                        };
                        if let Some(chosen) = chosen {
                            match menu.row {
                                DetailRow::Level => {
                                    level.manual = Some(chosen.clone());
                                    // 会话目标：改等级立即切换且不关闭选择器。
                                    if matches!(target, crate::modal::ModelTarget::Session)
                                        && let Some(entry) = entry
                                    {
                                        let selector = entry.selector_with(Some(&chosen));
                                        if selector != *current {
                                            picker.loading = true;
                                            picker.error = None;
                                            detail.pending = Some(selector.clone());
                                            session_apply = Some((session_id.clone(), selector));
                                        }
                                    }
                                }
                                DetailRow::Lead => detail.fusion.lead = chosen,
                                DetailRow::Sidekick => detail.fusion.sidekick = chosen,
                                DetailRow::LeadLevel => {
                                    let next = crate::modal::fusion_side_level(
                                        &detail.fusion.lead,
                                        &chosen,
                                    );
                                    detail.fusion.lead = next;
                                }
                                DetailRow::SidekickLevel => {
                                    let next = crate::modal::fusion_side_level(
                                        &detail.fusion.sidekick,
                                        &chosen,
                                    );
                                    detail.fusion.sidekick = next;
                                }
                                _ => {}
                            }
                        }
                        detail.menu = None;
                    }
                    (KeyCode::Backspace, _) if model_menu => {
                        menu.query.pop();
                        menu.selected = 0;
                        menu.offset = 0;
                        menu.scroll.reveal_selection = true;
                    }
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) if model_menu => {
                        menu.query.clear();
                        menu.selected = 0;
                        menu.offset = 0;
                        menu.scroll.reveal_selection = true;
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                        if model_menu && !ch.is_control() =>
                    {
                        menu.query.push(ch);
                        menu.selected = 0;
                        menu.offset = 0;
                        menu.scroll.reveal_selection = true;
                    }
                    _ => {}
                }
            } else if let Some(focus) = detail.focus {
                // 详情卡焦点：↑↓ 在控制行间移动（夹紧不循环），Enter/Space 按行动作。
                match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Down, _) if !rows.is_empty() => {
                        let position = rows.iter().position(|row| *row == focus).unwrap_or(0);
                        let next = position
                            .saturating_add_signed(if matches!(code, KeyCode::Up) { -1 } else { 1 })
                            .min(rows.len() - 1);
                        detail.focus = Some(rows[next]);
                    }
                    (KeyCode::Left, _) | (KeyCode::Esc, _) => detail.focus = None,
                    (KeyCode::Enter, KeyModifiers::NONE)
                    | (KeyCode::Char(' '), KeyModifiers::NONE) => match focus {
                        DetailRow::Fast => fast_mode_apply = Some(!session_fast),
                        DetailRow::Apply => {
                            if picker.loading {
                                return;
                            }
                            if detail.fusion.lead.is_empty() || detail.fusion.sidekick.is_empty() {
                                picker.error = Some("请先选择 Lead 与 Sidekick".into());
                            } else if apply_fusion_pick(
                                picker,
                                &detail.fusion,
                                current,
                                &session_id,
                                &mut session_apply,
                            ) {
                                close = true;
                            }
                        }
                        row => {
                            let items = crate::modal::menu_items(
                                row,
                                entry,
                                entries,
                                level,
                                &detail.fusion,
                                "",
                            );
                            open_menu(detail, row, items.current());
                        }
                    },
                    _ => {}
                }
            } else {
                match (code, modifiers) {
                    (KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown, _) => {
                        let delta = match code {
                            KeyCode::Up => -1,
                            KeyCode::Down => 1,
                            KeyCode::PageUp => -8,
                            _ => 8,
                        };
                        picker.selected = picker
                            .selected
                            .saturating_add_signed(delta)
                            .min(filtered.len().saturating_sub(1));
                        detail.list_scroll.reveal_selection = true;
                    }
                    // Tab：普通模型展开思考等级下拉（焦点仍在列表）；高亮 Fusion 项时等同 →。
                    (KeyCode::Tab, KeyModifiers::NONE) => {
                        if entry.is_some_and(|entry| entry.selector == FUSION_ENTRY_SELECTOR) {
                            if let Some(first) = rows.first() {
                                detail.focus = Some(*first);
                            }
                        } else if entry.is_some_and(|entry| !entry.levels.is_empty()) {
                            let items = crate::modal::menu_items(
                                DetailRow::Level,
                                entry,
                                entries,
                                level,
                                &detail.fusion,
                                "",
                            );
                            open_menu(detail, DetailRow::Level, items.current());
                        }
                    }
                    (KeyCode::Right, _) => {
                        if let Some(first) = rows.first() {
                            detail.focus = Some(*first);
                        }
                    }
                    (KeyCode::Home, _) => {
                        picker.selected = 0;
                        picker.offset = 0;
                        detail.list_scroll.reveal_selection = true;
                    }

                    (KeyCode::End, _) => {
                        picker.selected = filtered.len().saturating_sub(1);
                        detail.list_scroll.reveal_selection = true;
                    }
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        picker.query.clear();
                        picker.selected = 0;
                        picker.offset = 0;
                        detail.list_scroll.reveal_selection = true;
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
                        detail.list_scroll.reveal_selection = true;
                    }
                    (KeyCode::Backspace, _) => {
                        picker.query.pop();
                        picker.selected = 0;
                        picker.offset = 0;
                        detail.list_scroll.reveal_selection = true;
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                        if !ch.is_control() =>
                    {
                        picker.query.push(ch);
                        picker.selected = 0;
                        picker.offset = 0;
                        detail.list_scroll.reveal_selection = true;
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        if picker.loading {
                            return;
                        }
                        let Some((_, entry)) = filtered.get(picker.selected).copied() else {
                            return;
                        };
                        if entry.selector == FUSION_ENTRY_SELECTOR {
                            // Fusion 特殊项：两侧都已选则应用；否则聚焦到第一个
                            // 空槽位行并直接展开它的模型下拉。
                            if detail.fusion.lead.is_empty() || detail.fusion.sidekick.is_empty() {
                                let row = if detail.fusion.lead.is_empty() {
                                    DetailRow::Lead
                                } else {
                                    DetailRow::Sidekick
                                };
                                detail.focus = Some(row);
                                let items = crate::modal::menu_items(
                                    row,
                                    Some(entry),
                                    entries,
                                    level,
                                    &detail.fusion,
                                    "",
                                );
                                open_menu(detail, row, items.current());
                            } else if apply_fusion_pick(
                                picker,
                                &detail.fusion,
                                current,
                                &session_id,
                                &mut session_apply,
                            ) {
                                close = true;
                            }
                        } else {
                            let selector =
                                entry.selector_with(level.resolve(&entry.levels).as_deref());
                            match target {
                                crate::modal::ModelTarget::Config { path, label } => {
                                    config_pick = Some((path.clone(), label.clone(), selector));
                                }
                                crate::modal::ModelTarget::WorkflowRole { .. } => {
                                    // 先整体取出（借用期拿不走 back 箱），写回后统一还原。
                                    let owned = std::mem::replace(
                                        target,
                                        crate::modal::ModelTarget::Session,
                                    );
                                    if let crate::modal::ModelTarget::WorkflowRole { key, back } =
                                        owned
                                    {
                                        let (back, action) = crate::workflow::apply_workflow_pick(
                                            back, &key, selector,
                                        );
                                        workflow_back = Some(back);
                                        workflow_action = action;
                                    }
                                }
                                crate::modal::ModelTarget::Session => {
                                    picker.loading = true;
                                    picker.error = None;
                                    session_apply = Some((session_id.clone(), selector));
                                }
                            }
                        }
                    }

                    (KeyCode::Esc, _) => {
                        if matches!(target, crate::modal::ModelTarget::WorkflowRole { .. }) {
                            // 取消选择：原样放回回程票，不写覆盖。
                            let owned =
                                std::mem::replace(target, crate::modal::ModelTarget::Session);
                            if let crate::modal::ModelTarget::WorkflowRole { back, .. } = owned {
                                workflow_back = Some(*back);
                            }
                        } else {
                            close = true;
                        }
                    }
                    _ => {}
                }
                // 高亮条目变化：等级跨条目记忆照旧，详情焦点与下拉一并复位。
                let after = highlighted(picker);
                if before != after {
                    detail.menu = None;
                    detail.focus = None;
                }
                level.change_entry(
                    before.and_then(|index| entries.get(index)),
                    after.and_then(|index| entries.get(index)),
                );
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
        Modal::Sessions {
            sessions,
            cwd,
            scope,
            picker,
        } => {
            let filtered = crate::modal::filtered_sessions(sessions, &picker.query, *scope, cwd);
            match (code, modifiers) {
                (KeyCode::Up, _) => picker.move_selection(-1, filtered.len(), 8),
                (KeyCode::Down, _) => picker.move_selection(1, filtered.len(), 8),
                (KeyCode::PageUp, _) => picker.move_selection(-8, filtered.len(), 8),
                (KeyCode::PageDown, _) => picker.move_selection(8, filtered.len(), 8),
                (KeyCode::Tab, _) => {
                    *scope = scope.toggled();
                    picker.selected = 0;
                    picker.offset = 0;
                }
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
                        let first = load_display_session(&client, session_id.clone(), false).await;
                        // 被另一个 core 进程持有写租约时降级为只读打开，让用户至少能看历史。
                        let (loaded, read_only) = match first {
                            Ok(loaded) => (Ok(loaded), false),
                            Err(error)
                                if error.code()
                                    == Some(moc_protocol::RpcErrorCode::SessionLocked.code()) =>
                            {
                                let retry = load_display_session(&client, session_id, true).await;
                                (retry, true)
                            }
                            Err(error) => (Err(error), false),
                        };
                        match loaded {
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
                                    loaded.todos,
                                    read_only,
                                    loaded.goal,
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
        // 计划审阅在 `handle_modal_key` 开头整体分派，这里不会走到。
        Modal::Plan { .. } => {}
        Modal::Help { scroll } | Modal::Tools { scroll, .. } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                *scroll = scroll.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                *scroll = scroll.saturating_add(1);
            }
            (KeyCode::PageUp, _) => *scroll = scroll.saturating_sub(HELP_PAGE),
            (KeyCode::PageDown, _) => *scroll = scroll.saturating_add(HELP_PAGE),
            (KeyCode::Home, _) => *scroll = 0,
            // 上界由渲染按可见行数收紧，这里只表达「到底」。
            (KeyCode::End, _) => *scroll = usize::MAX,
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => close = true,
            _ => {}
        },
        Modal::Context { .. } | Modal::Monitor => {
            if matches!(
                (code, modifiers),
                (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE)
            ) {
                close = true;
            }
        }
        Modal::ContextContent { state } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => state.scroll_by(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => state.scroll_by(1),
            (KeyCode::PageUp, _) => state.scroll_by(-(state.viewport as i64)),
            (KeyCode::PageDown, _) => state.scroll_by(state.viewport as i64),
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                state.scroll_to_edge(false);
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::SHIFT) => {
                state.scroll_to_edge(true);
            }
            (KeyCode::Tab, KeyModifiers::NONE) => state.focus_step(1),
            (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => state.focus_step(-1),
            (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Char(' '), KeyModifiers::NONE) => {
                if let Some(index) = state.focus {
                    state.toggle(index);
                }
            }
            (KeyCode::Char('p'), KeyModifiers::NONE) => state.previous_page(),
            (KeyCode::Char('n'), KeyModifiers::NONE) => state.next_page(),
            (KeyCode::Char('m'), KeyModifiers::NONE) => state.root_more = true,
            (KeyCode::Char('r'), KeyModifiers::NONE) => state.refresh = true,
            (KeyCode::Char('e'), KeyModifiers::NONE) => state.set_all_expanded(true),
            (KeyCode::Char('c'), KeyModifiers::NONE) => state.set_all_expanded(false),
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => close = true,
            _ => {}
        },
        Modal::Jobs {
            jobs,
            selected,
            pending_kill,
            scroll,
            ..
        } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                *selected = selected.saturating_sub(1);
                *pending_kill = None;
                scroll.reveal_selection = true;
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                *selected = (*selected + 1).min(jobs.len().saturating_sub(1));
                *pending_kill = None;
                scroll.reveal_selection = true;
            }
            (KeyCode::Char('x'), KeyModifiers::NONE) => {
                if *pending_kill == Some(*selected) {
                    // 二次确认：杀任务
                    if let Some(job) = jobs.get(*selected) {
                        let job_id = job.job_id.clone();
                        let client = connection.client.clone();
                        let session_id = app.session.as_ref().map(|s| s.id.clone());
                        tokio::spawn(async move {
                            if let Some(session_id) = session_id {
                                let _ = client
                                    .call::<moc_protocol::methods::JobKill>(
                                        moc_protocol::methods::JobTarget { session_id, job_id },
                                    )
                                    .await;
                            }
                        });
                        close = true;
                    }
                } else {
                    // 首次按 x：标记待确认
                    *pending_kill = Some(*selected);
                }
            }
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => close = true,
            _ => {
                *pending_kill = None;
            }
        },
        Modal::WorkflowApproval { state } => {
            if let Some(feedback) = state.feedback.as_mut() {
                // 反馈输入态：全部键位归文本行。
                match (code, modifiers) {
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        let text = state.feedback.take().unwrap_or_default();
                        workflow_decision = Some((
                            state.request.request_id.clone(),
                            moc_protocol::methods::WorkflowDecision::Reject {
                                feedback: (!text.trim().is_empty()).then_some(text),
                            },
                        ));
                        close = true;
                    }
                    (KeyCode::Esc, _) => {
                        state.feedback = None;
                    }
                    (KeyCode::Backspace, _) => {
                        feedback.pop();
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        feedback.push(ch);
                    }
                    _ => {}
                }
            } else {
                match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        state.focus = state.focus.saturating_sub(1);
                        state.scroll.reveal_selection = true;
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        state.focus = (state.focus + 1).min(state.rows().saturating_sub(1));
                        state.scroll.reveal_selection = true;
                    }
                    (KeyCode::PageUp, _) => {
                        state.body_scroll = state.body_scroll.saturating_sub(6);
                        state.scroll.reveal_selection = false;
                    }
                    (KeyCode::PageDown, _) => {
                        state.body_scroll = state.body_scroll.saturating_add(6);
                        state.scroll.reveal_selection = false;
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        // 选中 role/ref → 打开模型选择器；回程票带回整个审批状态。
                        if let Some(target) = state.focus_target() {
                            let current = match &target {
                                crate::workflow::FocusTarget::Role(id) => {
                                    state.role_models.get(id).cloned().unwrap_or_default()
                                }
                                crate::workflow::FocusTarget::ModelRef(id) => {
                                    state.model_ref_map.get(id).cloned().unwrap_or_default()
                                }
                            };
                            workflow_picker = Some((target, current));
                        }
                    }
                    (KeyCode::Char('a'), KeyModifiers::NONE) => {
                        workflow_decision = Some((
                            state.request.request_id.clone(),
                            moc_protocol::methods::WorkflowDecision::Approve {
                                role_models: Some(state.role_models.clone()),
                                model_ref_map: Some(state.model_ref_map.clone()),
                            },
                        ));
                        close = true;
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) => {
                        state.feedback = Some(String::new());
                    }
                    (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                        // 最小化：审批仍在等待，状态栏胶囊可重开。
                        state.parked = true;
                        close = true;
                    }
                    _ => {}
                }
            }
        }
        Modal::WorkflowRuns { runs, picker } => {
            let filtered: Vec<_> = runs
                .iter()
                .filter(|run| {
                    let query = picker.query.to_lowercase();
                    run.run_id.to_lowercase().contains(&query)
                        || run
                            .title
                            .as_deref()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&query)
                })
                .collect();
            match (code, modifiers) {
                (KeyCode::Up, _) => picker.move_selection(-1, filtered.len(), 8),
                (KeyCode::Down, _) => picker.move_selection(1, filtered.len(), 8),
                (KeyCode::Backspace, _) => {
                    picker.query.pop();
                    picker.selected = 0;
                    picker.scroll.reveal_selection = true;
                }
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if !ch.is_control() =>
                {
                    picker.query.push(ch);
                    picker.selected = 0;
                    picker.scroll.reveal_selection = true;
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if let Some(run) = filtered.get(picker.selected) {
                        workflow_action =
                            Some(crate::workflow::WorkflowAction::OpenRun(run.run_id.clone()));
                    }
                    close = true;
                }
                (KeyCode::Esc, _) => close = true,
                _ => {}
            }
        }
        Modal::WorkflowRun { state } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                state.focus = state.focus.saturating_sub(1);
                state.scroll.reveal_selection = true;
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                state.focus = (state.focus + 1).min(state.roles.len().saturating_sub(1));
                state.scroll.reveal_selection = true;
            }
            (KeyCode::PageUp, _) => {
                state.body_scroll = state.body_scroll.saturating_sub(6);
                state.scroll.reveal_selection = false;
            }
            (KeyCode::PageDown, _) => {
                state.body_scroll = state.body_scroll.saturating_add(6);
                state.scroll.reveal_selection = false;
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if let Some(role) = state.roles.get(state.focus) {
                    workflow_picker = Some((
                        crate::workflow::FocusTarget::Role(role.role_id.clone()),
                        role.model.clone().unwrap_or_default(),
                    ));
                }
            }
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                workflow_action = Some(crate::workflow::WorkflowAction::Stop(state.run_id.clone()));
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                workflow_action = Some(crate::workflow::WorkflowAction::Resume(
                    state.run_id.clone(),
                ));
            }
            (KeyCode::Char('v'), KeyModifiers::NONE) => {
                // 多版本切换：所有有多版本的 artifact 各进一版（wrap）。
                let mut ids: Vec<String> = Vec::new();
                let mut counts: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                for artifact in &state.artifacts {
                    if let Some(id) = artifact.get("artifact_id").and_then(|v| v.as_str()) {
                        *counts.entry(id.to_string()).or_insert(0) += 1;
                        if !ids.contains(&id.to_string()) {
                            ids.push(id.to_string());
                        }
                    }
                }
                for id in ids {
                    let n = counts[&id];
                    if n > 1 {
                        let cur = *state.artifact_versions.get(&id).unwrap_or(&n);
                        state.artifact_versions.insert(id, cur % n + 1);
                    }
                }
            }
            (KeyCode::Char('o'), KeyModifiers::NONE) => {
                if state.artifact_view.is_some() {
                    state.artifact_view = None;
                } else if let Some(id) = state
                    .artifacts
                    .iter()
                    .find(|a| {
                        matches!(
                            a.get("member").and_then(|v| v.as_str()),
                            Some("file") | Some("markdown")
                        )
                    })
                    .and_then(|a| a.get("artifact_id").and_then(|v| v.as_str()))
                {
                    workflow_action = Some(crate::workflow::WorkflowAction::ViewArtifact(
                        state.run_id.clone(),
                        id.to_string(),
                    ));
                }
            }
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                if state.artifact_view.is_some() {
                    state.artifact_view = None;
                } else {
                    close = true;
                }
            }
            _ => {}
        },
    }
    if let Some((id, selector)) = session_apply {
        request_session_model(app, connection, id, selector);
    }
    // workflow：模型选择器回程（审批窗/run 面板）。
    if let Some(back) = workflow_back {
        app.modal = Some(match back {
            crate::workflow::WorkflowRoleBack::Approval(state) => Modal::WorkflowApproval { state },
            crate::workflow::WorkflowRoleBack::Run(state) => Modal::WorkflowRun { state },
        });
    }
    // workflow：审批决策回传 core。
    if let Some((request_id, decision)) = workflow_decision
        && let Some(session_id) = app.session.as_ref().map(|session| session.id.clone())
    {
        let client = connection.client.clone();
        let events = app.events.clone();
        tokio::spawn(async move {
            let result = client
                .call::<moc_protocol::methods::SessionWorkflowApprove>(
                    moc_protocol::methods::SessionWorkflowApproveParams {
                        session_id,
                        request_id,
                        decision,
                    },
                )
                .await;
            if let Err(error) = result {
                let _ = events.send(AppEvent::Notice(format!("workflow 审批回传失败：{error}")));
            }
        });
    }
    // workflow：role/ref 焦点进模型选择器（带回程票）。
    if let Some((key, current)) = workflow_picker
        && let Some(modal) = app.modal.take()
    {
        let back = match modal {
            Modal::WorkflowApproval { state } => crate::workflow::WorkflowRoleBack::Approval(state),
            Modal::WorkflowRun { state } => crate::workflow::WorkflowRoleBack::Run(state),
            other => {
                app.modal = Some(other);
                return;
            }
        };
        app.open_model_picker_for(
            current,
            &[],
            crate::modal::ModelTarget::WorkflowRole {
                key,
                back: Box::new(back),
            },
        );
    }
    // workflow：面板动作 RPC。
    if let Some(action) = workflow_action
        && let Some(session_id) = app.session.as_ref().map(|session| session.id.clone())
    {
        let client = connection.client.clone();
        let events = app.events.clone();
        tokio::spawn(async move {
            match action {
                crate::workflow::WorkflowAction::Stop(run_id) => {
                    if let Err(error) = client
                        .call::<moc_protocol::methods::SessionWorkflowStop>(
                            moc_protocol::methods::SessionWorkflowRunParams { session_id, run_id },
                        )
                        .await
                    {
                        let _ =
                            events.send(AppEvent::Notice(format!("workflow 停止失败：{error}")));
                    }
                }
                crate::workflow::WorkflowAction::Resume(run_id) => {
                    if let Err(error) = client
                        .call::<moc_protocol::methods::SessionWorkflowResume>(
                            moc_protocol::methods::SessionWorkflowRunParams { session_id, run_id },
                        )
                        .await
                    {
                        let _ =
                            events.send(AppEvent::Notice(format!("workflow 续跑失败：{error}")));
                    }
                }
                crate::workflow::WorkflowAction::OpenRun(run_id) => {
                    match client
                        .call::<moc_protocol::methods::SessionWorkflowRun>(
                            moc_protocol::methods::SessionWorkflowRunParams { session_id, run_id },
                        )
                        .await
                    {
                        Ok(snapshot) => {
                            let _ = events.send(AppEvent::OpenWorkflowRun(Box::new(snapshot)));
                        }
                        Err(error) => {
                            let _ =
                                events.send(AppEvent::Notice(format!("读取 run 失败：{error}")));
                        }
                    }
                }
                crate::workflow::WorkflowAction::SetRoleModel(run_id, role_id, model) => {
                    if let Err(error) = client
                        .call::<moc_protocol::methods::SessionWorkflowSetRoleModel>(
                            moc_protocol::methods::SessionWorkflowSetRoleModelParams {
                                session_id,
                                run_id,
                                role_id,
                                model,
                            },
                        )
                        .await
                    {
                        let _ =
                            events.send(AppEvent::Notice(format!("role 模型热改失败：{error}")));
                    }
                }
                crate::workflow::WorkflowAction::ViewArtifact(run_id, artifact_id) => {
                    match client
                        .call::<moc_protocol::methods::SessionWorkflowArtifact>(
                            moc_protocol::methods::SessionWorkflowArtifactParams {
                                session_id,
                                run_id,
                                artifact_id,
                                version: None,
                            },
                        )
                        .await
                    {
                        Ok(result) => {
                            let _ = events.send(AppEvent::WorkflowArtifactView {
                                artifact_id: result.artifact_id,
                                content: result.content,
                            });
                        }
                        Err(error) => {
                            let _ = events
                                .send(AppEvent::Notice(format!("读取 artifact 失败：{error}")));
                        }
                    }
                }
            }
        });
    }
    if let Some(enabled) = fast_mode_apply {
        set_fast_mode(app, connection, enabled);
    }

    if let Some((path, label, value)) = config_pick {
        app.modal = None;
        crate::settings::apply_model_role(app, connection, path, &label, value);
    }

    if close {
        // 最小化的 workflow 审批：状态移进状态栏胶囊（点击重开），不算已决。
        if let Some(Modal::WorkflowApproval { state }) = app.modal.take()
            && state.parked
        {
            app.workflow.parked_approval = Some(state);
        } else {
            app.modal = None;
        }
        if !plan_review_open {
            crate::queue::dispatch(app, connection);
        }
    }
}

/// 写入一次选项选择：多选切换勾选，单选整体覆盖（返回 true 表示已就绪，调用方据此推进）。
/// 空格、数字直达与鼠标点选共用这一份语义。
fn pick_ask_option(
    current: &moc_protocol::methods::AskQuestion,
    question: usize,
    index: usize,
    selected: &mut [Vec<String>],
) -> bool {
    let Some(option) = current.options.get(index) else {
        return false;
    };
    let label = option.label.clone();
    if current.multi {
        let chosen = &mut selected[question];
        if let Some(position) = chosen.iter().position(|s| *s == label) {
            chosen.remove(position);
        } else {
            chosen.push(label);
        }
        false
    } else {
        selected[question] = vec![label];
        true
    }
}

/// 跳过整单提问：提交空答案，core 侧按「用户没有回答」输出给模型。
fn dismiss_ask(
    reply: &mut Option<
        tokio::sync::oneshot::Sender<
            Result<moc_protocol::methods::AskResult, moc_protocol::RpcError>,
        >,
    >,
) -> bool {
    if let Some(reply) = reply.take() {
        let _ = reply.send(Ok(moc_protocol::methods::AskResult {
            answers: Vec::new(),
        }));
    }
    true
}

/// ask 推进到下一页：越过最后一问即落在确认页（`question == questions.len()`），
/// 真正的提交只发生在确认页上。
fn advance_ask(question: &mut usize, preview_scroll: &mut usize, question_scroll: &mut usize) {
    *question += 1;
    *preview_scroll = 0;
    *question_scroll = 0;
}

/// 确认页提交：按各问勾选与自定义文本组装 AskResult 回给 core；返回是否应关闭模态。
fn submit_ask(
    params: &moc_protocol::methods::AskParams,
    selected: &[Vec<String>],
    custom: &[crate::modal::CustomAnswer],
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
                // 自定义答案各归各问：取该问自己的输入，不共用一份缓冲
                custom: custom
                    .get(index)
                    .filter(|answer| !answer.text.is_empty())
                    .map(|answer| answer.text.clone()),
            },
        )
        .collect();
    if let Some(reply) = reply.take() {
        let _ = reply.send(Ok(moc_protocol::methods::AskResult { answers }));
    }
    true
}

fn confirm_plan(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    if !session.plan_active {
        app.set_notice("只有计划状态可以重新审阅计划");
        return;
    }
    let session_id = session.id.clone();
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result =
            async {
                let loaded = client
                    .call::<SessionLoad>(SessionLoadParams {
                        omit_auxiliary: true,
                        session_id: session_id.clone(),
                        max_entries: Some(0),
                        read_only: false,
                    })
                    .await?;
                anyhow::ensure!(loaded.session.plan_active, "会话已离开计划状态");
                let mut before = None;
                let plan =
                    loop {
                        let page = client
                            .history_page(SessionLoadPageParams {
                                full_messages: true,
                                session_id: session_id.clone(),
                                before_entry_id: None,
                                max_entries: 0,
                                max_message_bytes: None,
                                redo_page: Some(Default::default()),
                                plan_page: Some(moc_protocol::methods::session::PlanPageParams {
                                    before,
                                    max_entries: 32,
                                }),
                            })
                            .await?;
                        anyhow::ensure!(page.session.plan_active, "会话已离开计划状态");
                        if let Some(plan) = page.plans.into_iter().rev().find(|plan| {
                            plan.status == moc_protocol::methods::PlanStatus::Submitted
                        }) {
                            break plan;
                        }
                        before = page.plans_before;
                        anyhow::ensure!(before.is_some(), "当前会话没有已写入的待审计划");
                    };
                anyhow::ensure!(
                    !plan.markdown.trim().is_empty()
                        && plan.path == format!("local://{}.md", plan.plan_id),
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
        state: Box::new(crate::modal::PlanReviewState::new(plan, session.engine)),
    });
}

/// 为当前计划版本拉取与上一版的差异。
fn request_plan_diff(app: &mut App, connection: &EmbeddedConnection, plan_id: String) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    if let Some(Modal::Plan { state }) = app.modal.as_mut()
        && state.plan_id == plan_id
    {
        state.diff_loading = true;
        state.diff_error = None;
    }
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionPlanDiff>(SessionPlanDiffParams {
                session_id,
                plan_id,
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::PlanDiff(result));
    });
}

pub(crate) fn apply_plan_diff(app: &mut App, result: anyhow::Result<SessionPlanDiffResult>) {
    let Some(Modal::Plan { state }) = app.modal.as_mut() else {
        return;
    };
    state.diff_loading = false;
    match result {
        Ok(diff) => {
            if diff.plan_id == state.plan_id {
                state.diff = Some(diff.diff);
            }
        }
        Err(error) => state.diff_error = Some(error.to_string()),
    }
}

fn apply_plan_review(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<moc_protocol::methods::PlanInfo>,
) {
    match result {
        Ok(plan)
            if app.session.as_ref().is_some_and(|session| {
                session.plan_active && plan.path == format!("local://{}.md", plan.plan_id)
            }) =>
        {
            let needs_diff = plan.previous_plan_id.is_some();
            let plan_id = plan.plan_id.clone();
            open_plan_review(app, plan);
            if needs_diff {
                request_plan_diff(app, connection, plan_id);
            }
        }
        Ok(_) => app.set_notice("当前会话已变化，不能打开该计划"),
        Err(error) => app.set_notice(format!("无法审阅计划：{error}")),
    }
}

/// 批注已落库：关闭审阅窗口并发送 core 生成的修订提示。
pub(crate) fn apply_plan_revised(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<moc_protocol::methods::SessionPlanReviseResult>,
) {
    match result {
        Ok(result) => {
            app.modal = None;
            start_auto_prompt(app, connection, Some(result.prompt));
        }
        Err(error) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.submitting = false;
                state.error = Some(error.to_string());
            } else {
                app.set_notice(format!("提交批注失败：{error}"));
            }
        }
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
                start_auto_prompt(app, connection, result.auto_prompt);
            } else {
                let client = connection.client.clone();
                let events = app.events.clone();
                tokio::spawn(async move {
                    let loaded = load_display_session(&client, result.session_id, false)
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
            app.goal = loaded.goal;
            app.set_history_cursor(loaded.truncated, before);
            refresh_git_branch(app, &cwd);
            refresh_context(app, connection, false);
            refresh_usage(app, connection, false);
            load_subagents_for_parent(app, &connection.client, Some(id));
            start_auto_prompt(app, connection, auto_prompt);
        }
        Err(error) => app.set_notice(format!(
            "计划已批准，但加载执行会话失败：{error}；可通过 /resume 恢复"
        )),
    }
}

fn start_auto_prompt(app: &mut App, connection: &EmbeddedConnection, auto_prompt: Option<String>) {
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

/// 把一条会话更新折进界面；只接受当前会话与已知子代理的更新。
///
/// - 当前会话：正常折叠进转写与状态；
/// - 已知子代理（其会话 ID 就是代理 ID）：折进侧栏的子代理视图；
/// - 其余来源（运行中切走后的旧会话、别的会话）：内容一律丢弃，只按
///   `PromptFinished` 回收本前端的在飞记账——旧会话的 delta/工具块写进新会话就是串台。
fn apply_ui_update(
    app: &mut App,
    connection: &EmbeddedConnection,
    update: crate::protocol::UiUpdate,
) {
    match update {
        crate::protocol::UiUpdate::Session(params) => {
            apply_update_and_refresh(app, connection, params.session_id, params.update);
        }
        crate::protocol::UiUpdate::Btw(params) => {
            app.btw.apply_update(params);
            crate::btw::flush_cancel(app, connection);
        }
    }
}

fn apply_update_and_refresh(
    app: &mut App,
    connection: &EmbeddedConnection,
    session_id: String,
    update: SessionUpdate,
) {
    let is_current = app
        .session
        .as_ref()
        .is_some_and(|session| session.id == session_id);
    // 补充消息的落位事件对所有会话都要处理，否则切走期间回来的回执会留下幽灵项。
    if let SessionUpdate::SteeringApplied { id, text } = &update {
        crate::queue::steering_applied(app, &session_id, *id, text, is_current);
        if is_current {
            // pending 清空后才能安全启动下一条提示，保证用户块先于新提示落地。
            crate::queue::dispatch(app, connection);
        }
        return;
    }
    if !is_current {
        // 聚焦子代理时主会话不再是当前会话，但代理面仍挂靠在它上面：
        // 主会话发出的状态事件与新建代理照旧更新名册，避免侧栏在聚焦期间僵住。
        if app.agents_root_id().as_deref() == Some(session_id.as_str()) {
            if let SessionUpdate::AgentStatusChanged {
                agent_id,
                status,
                last_message,
            } = &update
                && let Some(view) = app.subagents.get_mut(agent_id)
            {
                view.set_status(*status, last_message.clone());
                app.subagent_generation = app.subagent_generation.wrapping_add(1);
                app.sync_subagent_sidebar();
            }
            if matches!(&update, SessionUpdate::AgentStarted { .. }) {
                load_subagents(app, connection);
            }
            if matches!(&update, SessionUpdate::PromptFinished { .. }) {
                app.inflight_prompts.remove(&session_id);
                app.interrupt_pending.remove(&session_id);
            }
            return;
        }
        // 子代理自身的流：TUI 不再另存一份转写（聚焦时它才是当前会话），
        // 只按在飞记账收尾。
        if matches!(&update, SessionUpdate::PromptFinished { .. }) {
            app.inflight_prompts.remove(&session_id);
            app.interrupt_pending.remove(&session_id);
        }
        return;
    }
    if let SessionUpdate::AgentStarted { agent } = &update {
        app.subagents
            .entry(agent.id.clone())
            .or_insert_with(|| crate::subagent::SubagentView::new(agent.clone()));
        app.subagent_generation = app.subagent_generation.wrapping_add(1);
        app.sync_subagent_sidebar();
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
        app.sync_subagent_sidebar();
        // 终态在主流里留一行：侧栏未必打开，用户仍要知道子代理已经结束。
        if matches!(
            status,
            moc_protocol::methods::AgentStatus::Completed
                | moc_protocol::methods::AgentStatus::Stopped
                | moc_protocol::methods::AgentStatus::Failed
        ) {
            let title = app
                .subagents
                .get(agent_id)
                .map_or(agent_id.as_str(), |view| view.info.title.as_str())
                .to_owned();
            let label = crate::render::scrollback::agent_status_label(*status);
            app.push_note(
                format!("子代理「{title}」{}{label}", crate::render::labels::SEP),
                false,
            );
        }
    }
    let prompt_started = matches!(
        &update,
        SessionUpdate::PromptStarted { .. } | SessionUpdate::BashStarted { .. }
    );
    let refresh = matches!(
        &update,
        SessionUpdate::PromptFinished { .. }
            | SessionUpdate::EngineChanged { .. }
            | SessionUpdate::AutonomyChanged { .. }
            | SessionUpdate::PlanModeChanged { .. }
            | SessionUpdate::ModelChanged { .. }
            | SessionUpdate::PermissionChanged { .. }
            | SessionUpdate::ContextCompacted { .. }
    ) || matches!(&update, SessionUpdate::ToolFinished { prompt_id, .. } if prompt_id.starts_with("bash_"));
    // 用量只在这些事件后变化：一次提示（含工具轮次）结束，或压缩本身花掉一次调用；
    // 非阻塞的 sidekick/子代理到达终态（完成/停止/失败/空闲）时同样刷新——
    // 它们的用量记在后代会话上，不等下一次提示。
    let usage_changed = matches!(
        &update,
        SessionUpdate::PromptFinished { .. } | SessionUpdate::ContextCompacted { .. }
    ) || (is_current
        && matches!(
            &update,
            SessionUpdate::AgentStatusChanged { status, .. }
                if matches!(
                    status,
                    moc_protocol::methods::AgentStatus::Completed
                        | moc_protocol::methods::AgentStatus::Stopped
                        | moc_protocol::methods::AgentStatus::Failed
                        | moc_protocol::methods::AgentStatus::Idle
                )
        ));
    // unattended 自主程度下计划提交不弹审阅：直接自动批准「压缩并执行」。
    let unattended_plan = match &update {
        SessionUpdate::PlanSubmitted { plan }
            if app.session.as_ref().is_some_and(|session| {
                session.autonomy == moc_protocol::SessionAutonomy::Unattended
            }) =>
        {
            Some(plan.plan_id.clone())
        }
        _ => None,
    };
    // 有上一版时，提交后立刻把「本版改动」拉下来；审阅窗口默认展示它。
    let revised_plan = match &update {
        SessionUpdate::PlanSubmitted { plan }
            if unattended_plan.is_none() && plan.previous_plan_id.is_some() =>
        {
            Some(plan.plan_id.clone())
        }
        _ => None,
    };
    apply_update(app, update);
    if let Some(plan_id) = unattended_plan
        && let Some(session) = app.session.as_ref()
    {
        let client = connection.client.clone();
        let events = app.events.clone();
        let (session_id, engine) = (session.id.clone(), session.engine);
        app.set_notice("unattended：已自动批准计划（压缩并执行）");
        tokio::spawn(async move {
            let result = client
                .call::<SessionPlanApprove>(SessionPlanApproveParams {
                    session_id,
                    plan_id,
                    action: moc_protocol::methods::PlanAction::ExecuteCompacted,
                    execution_engine: Some(engine),
                })
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::PlanApproval(result));
        });
    }
    if let Some(plan_id) = revised_plan {
        request_plan_diff(app, connection, plan_id);
    }
    if prompt_started && app.cancel_requested {
        cancel_prompt(&connection.client, app);
    }
    if refresh {
        refresh_context(app, connection, false);
    }
    if usage_changed {
        refresh_usage(app, connection, false);
    }
}

/// 压缩完成提示：全量摘要与仅裁剪用不同措辞，避免把裁剪说成完整压缩。
fn compaction_done_note(full: bool, tokens_before: u64, tokens_after: u64) -> String {
    let before = crate::render::format_tokens(tokens_before);
    let after = crate::render::format_tokens(tokens_after);
    if full {
        format!("上下文已压缩：{before} → {after}")
    } else {
        format!("上下文已裁剪：{before} → {after}（未生成摘要）")
    }
}

/// 把协议更新折叠进块序列：流式增量续写活动块，工具调用按 ID 归组。
fn apply_update(app: &mut App, update: SessionUpdate) {
    use crate::transcript::Block;
    match update {
        SessionUpdate::AgentStarted { .. } | SessionUpdate::AgentStatusChanged { .. } => {}
        // 在主循环的 apply_update_and_refresh 里单独处理（需要会话上下文）。
        SessionUpdate::SteeringApplied { .. } => {}
        // workflow 事件走 `apply_workflow_update`：审批弹窗、run 面板实时着色、
        // 状态栏胶囊与终态通知都在那里更新，不进转写块序列。
        SessionUpdate::Workflow { run_id, event } => {
            apply_workflow_update(app, run_id, event);
        }
        SessionUpdate::AgentMessage { meta, body } => {
            app.push_block(Block::AgentMessage { meta, body })
        }
        SessionUpdate::GoalRoundMessage { meta, body } => {
            app.push_block(Block::GoalRound { meta, body })
        }
        SessionUpdate::GoalChanged { goal } => {
            app.goal = goal;
        }

        SessionUpdate::ToolProgress {
            tool_call_id, rows, ..
        } => app.set_tool_progress(&tool_call_id, rows),
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
        // 插件失败即在本进程内停用：转写留下原因与恢复方式，状态栏给一行摘要。
        SessionUpdate::PluginNotice {
            plugin,
            code,
            message,
        } => {
            app.push_note(plugin_notice_note(&plugin, &code, &message), true);
            app.set_warning(format!("插件 `{plugin}` 已停用（{code}）"));
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
                // 只有仍在流式中的思考块才续写：已定稿的块属于上一响应，
                // 新响应要另起一块并重新计时。
                if !delta.is_empty()
                    && !app.has_streaming(|block| {
                        matches!(
                            block,
                            Block::Thinking {
                                duration_ms: None,
                                ..
                            }
                        )
                    })
                {
                    app.start_thinking();
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
            moc_protocol::methods::AssistantDelta::Usage { usage } => app.apply_usage(usage),
        },
        SessionUpdate::BashStarted {
            tool_call_id,
            command,
        } => app.start_tool_block(ToolCallStart {
            id: tool_call_id,
            name: "bash".into(),
            parent_id: None,
            arguments: serde_json::to_string(&command).expect("Bash 参数可序列化"),
        }),
        SessionUpdate::ToolStarted {
            tool_call_id,
            name,
            parent_tool_call_id,
            arguments,
            prompt_id: _,
        } => {
            app.commit_generation();
            app.start_tool_block(ToolCallStart {
                id: tool_call_id,
                name,
                parent_id: parent_tool_call_id,
                arguments,
            });
        }
        SessionUpdate::ToolFinished {
            tool_call_id,
            name,
            parent_tool_call_id,
            arguments,
            status,
            result,
            duration_ms,
            diff,
            prompt_id: _,
        } => app.finish_tool_block(ToolCallFinish {
            id: tool_call_id,
            name,
            parent_id: parent_tool_call_id,
            arguments,
            status,
            result,
            duration_ms,
            diff,
        }),
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
        SessionUpdate::EngineChanged { engine } => {
            if let Some(session) = app.session.as_mut() {
                session.engine = engine;
            }
            if engine == SessionEngine::ClmExperimental {
                app.set_notice(
                    "CLM 实验引擎已启用：模型将维护实时上下文；本会话首次请求后锁定引擎",
                );
            }
        }
        SessionUpdate::AutonomyChanged { autonomy } => {
            if let Some(session) = app.session.as_mut() {
                session.autonomy = autonomy;
            }
        }
        SessionUpdate::FastModeChanged { enabled } => {
            if let Some(session) = app.session.as_mut() {
                session.fast_mode = enabled;
            }
        }
        SessionUpdate::PlanModeChanged { active } => {
            if let Some(session) = app.session.as_mut() {
                session.plan_active = active;
            }
        }
        SessionUpdate::ModelChanged { model } => {
            app.remember_model(&model);
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
        SessionUpdate::TodoReminder { .. } => app.push_todo_reminder(),
        // unattended 时不弹审阅；自动批准由 apply_update_and_refresh 发起。
        SessionUpdate::PlanSubmitted { plan } => {
            if !app.session.as_ref().is_some_and(|session| {
                session.autonomy == moc_protocol::SessionAutonomy::Unattended
            }) {
                open_plan_review(app, plan);
            }
        }
        SessionUpdate::PlanUpdated { plan } => {
            // 同一次审阅里计划被改写（收到批注、被取代、批准）：只更新投影，保留草稿。
            if let Some(Modal::Plan { state }) = app.modal.as_mut()
                && state.plan_id == plan.plan_id
            {
                state.status = plan.status;
                state.version = plan.version;
                state.previous_plan_id = plan.previous_plan_id;
                state.submitted = plan.annotations;
            }
        }
        SessionUpdate::ContextCompactionStarted => app.start_compaction(),
        SessionUpdate::ContextCompacted {
            method: _,
            tokens_before,
            tokens_after,
            full,
        } => app.finish_compaction(
            compaction_done_note(full, tokens_before, tokens_after),
            false,
        ),
        SessionUpdate::ContextCompactionFailed { message } => {
            app.finish_compaction(format!("上下文压缩失败：{message}"), true);
        }
        SessionUpdate::JobStarted { job } | SessionUpdate::JobUpdated { job } => {
            // 前台 bash 任务会同时发送 JobStarted 和 ToolStarted；
            // 后台任务（explicit、自动或手动转后台）只发任务更新。
            // job_id 只在任务确为后台来源时挂上：前台命令不挂「后台」徽标，
            // 转后台时 origin 已在 core 侧改写，这里直接按当前值判断。
            if job.origin.is_background()
                && let Some(entry) = app.tool_entry_mut(&job.tool_call_id)
                && let Block::Tool(tool) = &mut entry.block
            {
                tool.job_id = Some(job.job_id.clone());
                entry.touch();
            }
        }
        SessionUpdate::JobOutput {
            job_id: _,
            tool_call_id,
            tail,
        } => {
            // 更新前台任务的实时输出
            if let Some(entry) = app.tool_entry_mut(&tool_call_id)
                && let Block::Tool(tool) = &mut entry.block
            {
                tool.live_output = Some(tail);
                entry.touch();
            }
        }
        SessionUpdate::JobFinished { job } => {
            // 任务脱离「后台运行中」：摘除徽标。只有后台任务结束才推便签——
            // 前台命令的结果已经随工具卡片落地，再提示就是把前台误报成后台。
            if let Some(entry) = app.tool_entry_mut(&job.tool_call_id)
                && let Block::Tool(tool) = &mut entry.block
            {
                tool.job_id = None;
                entry.touch();
            }
            if job.origin.is_background() {
                let code = job
                    .exit_code
                    .map_or_else(String::new, |code| format!("（退出码 {code}）"));
                app.push_note(format!("后台任务结束：{}{code}", job.command), false);
            }
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
            // 请求失败了：这段没落库的流式预览不属于任何一次对话，撤掉。
            app.discard_generation();
            app.push_note(
                format!(
                    "错误：{message}{}",
                    if retryable { "（可以重试）" } else { "" }
                ),
                true,
            );
        }
        SessionUpdate::PromptRewound { replayed_tools } => {
            app.rewind_failed_turn(replayed_tools);
        }
    }
}

/// workflow 事件：审批弹窗（modal 占用时退化为胶囊计数）、run 面板实时投影、
/// 活跃 run 胶囊与终态通知。
fn apply_workflow_update(
    app: &mut App,
    run_id: String,
    event: moc_protocol::methods::WorkflowEvent,
) {
    use moc_protocol::methods::WorkflowEvent as E;
    match event {
        E::ApprovalRequested { request } => {
            // modal 占用时完整载荷进队列（丢载荷该 run 的审批永远到不了，run 挂死）；
            // 空档时直接弹窗。
            if app.modal.is_some() {
                app.workflow.queued_approvals.push_back(request);
            } else {
                app.modal = Some(Modal::WorkflowApproval {
                    state: Box::new(crate::workflow::WorkflowApprovalState::new(request)),
                });
            }
        }
        E::ApprovalResolved { request_id, .. } => {
            app.workflow
                .queued_approvals
                .retain(|a| a.request_id != request_id);
            // 打开的审批窗或最小化票正是该 request → 一并关掉/清掉。
            let modal_is_request = matches!(
                app.modal.as_ref(),
                Some(Modal::WorkflowApproval { state }) if state.request.request_id == request_id
            );
            if modal_is_request {
                app.modal = None;
            }
            if app
                .workflow
                .parked_approval
                .as_ref()
                .is_some_and(|s| s.request.request_id == request_id)
            {
                app.workflow.parked_approval = None;
            }
        }
        E::RunStatus { status } => {
            let active = matches!(
                status,
                moc_protocol::methods::WorkflowRunStatus::Running
                    | moc_protocol::methods::WorkflowRunStatus::Pending
            );
            if active {
                app.workflow
                    .active_runs
                    .entry(run_id.clone())
                    .or_insert_with(|| run_id.clone());
            } else {
                let title = app.workflow.active_runs.remove(&run_id);
                app.set_notice(format!(
                    "workflow run {} {}",
                    title.unwrap_or_else(|| run_id.clone()),
                    crate::workflow::status_label(status)
                ));
            }
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.status = status;
            }
        }
        E::PhaseEntered { phase } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.log_line(format!("phase → {phase}"));
            }
        }
        E::NodeUpdated { phases, roles } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.phases = phases;
                state.roles = roles;
            }
        }
        E::Log { level, message } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.log_line(format!("[{level}] {message}"));
            }
        }
        E::Report { item, .. } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.reports.push(item);
            }
        }
        E::Artifact { artifact } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.artifacts.push(artifact);
            }
        }
        E::HoleOpened { name, prompt, .. } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state
                    .holes
                    .push(serde_json::json!({"name": name, "prompt": prompt}));
            }
            app.set_notice(format!("workflow hole 打开：{name}"));
        }
        E::HoleFilled { name } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.holes.retain(|hole| {
                    hole.get("name").and_then(|v| v.as_str()) != Some(name.as_str())
                });
            }
        }
        E::Usage { usage } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.usage = usage;
            }
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

/// `/usage` 明细：会话统计 + 模型 × 指标的对齐表格，末行是全部模型的合计。
///
/// 表格宽度按转写区收敛（去掉 note 的行首缩进与标记）：模型名过长时截断，
/// 数字列右对齐；token 数走全 UI 统一的 k/m/b 分档，时间按时长量级换单位。
/// 窄终端按优先级丢列（思考 → 缓存率 → TTFT），至少保住模型、调用、输入、
/// 输出、生成与 TPS。`area_width` 是转写区宽度，首次绘制前为 0 时按 80 列终端估算。
fn format_usage(result: &moc_protocol::SessionUsageResult, area_width: usize) -> String {
    // note 每行扣掉 1 格缩进与 2 格行首标记（标记或对齐空白）。
    let content_width = if area_width == 0 {
        71
    } else {
        area_width.saturating_sub(3)
    };
    // 再窄也先保住模型列，超出部分交给 note 折行（极窄终端才会发生）。
    let max_width = content_width.max(40);

    let mut lines = usage_summary(result);
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.extend(usage_table(result, max_width));
    lines.join("\n")
}

/// 会话级统计：模型用时（生成 + 网络等待）、工具调用用时、平均 TTFT 与输出速度。
///
/// 没有模型调用（新会话）时不输出整块，表格自己会给出零值。
fn usage_summary(result: &moc_protocol::SessionUsageResult) -> Vec<String> {
    let total = &result.total;
    if total.calls == 0 {
        return Vec::new();
    }
    let mut rows: Vec<(&str, String)> = Vec::with_capacity(4);
    let split = if total.gen_ms > 0 {
        let waiting = total.active_ms.saturating_sub(total.gen_ms);
        format!(
            "（生成 {} · 网络等待 {}）",
            format_active(total.gen_ms),
            format_active(waiting)
        )
    } else {
        String::new()
    };
    rows.push((
        "模型用时",
        format!("{}{split}", format_active(total.active_ms)),
    ));
    if result.tool_ms > 0 {
        rows.push(("工具调用用时", format_active(result.tool_ms)));
    }
    if let Some(ttft) = average_ttft(total) {
        rows.push(("首 token 平均（TTFT）", format_active(ttft)));
    }
    if let Some(tps) = usage_tps(total) {
        rows.push(("输出速度", format!("{tps:.1} tok/s")));
    }
    let label_width = rows
        .iter()
        .map(|(label, _)| str_width(label))
        .max()
        .unwrap_or(0);
    let mut lines = vec!["会话统计".to_owned()];
    lines.extend(rows.into_iter().map(|(label, value)| {
        let pad = label_width.saturating_sub(str_width(label));
        format!("  {label}{}{value}", " ".repeat(pad + 2))
    }));
    lines
}

/// 模型明细表；列宽按 `max_width` 收敛，空间不够时按优先级丢列。
fn usage_table(result: &moc_protocol::SessionUsageResult, max_width: usize) -> Vec<String> {
    const COLUMNS: [&str; 9] = [
        "模型",
        "调用",
        "输入",
        "输出",
        "缓存率",
        "思考",
        "生成",
        "TTFT",
        "TPS",
    ];
    // 窄终端按顺序丢列：思考 → 缓存率 → TTFT（会话统计里还有平均 TTFT）。
    const DROP_ORDER: [usize; 3] = [5, 4, 7];
    const MIN_MODEL_WIDTH: usize = 14;

    let mut rows: Vec<[String; 9]> = result
        .models
        .iter()
        .map(|summary| {
            let label = crate::app::model_selector_label(&summary.model);
            // 后代会话的用量行标出来源：Fusion sidekick 与普通子代理。
            let label = match summary.source {
                moc_protocol::methods::UsageSource::Sidekick => format!("{label} (sidekick)"),
                moc_protocol::methods::UsageSource::Subagent => format!("{label} (子代理)"),
                moc_protocol::methods::UsageSource::Session => label,
            };
            usage_row(summary, &label)
        })
        .collect();
    rows.push(usage_row(&result.total, "合计"));

    let mut widths = COLUMNS.map(str_width);
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(str_width(cell));
        }
    }
    let mut hidden = [false; COLUMNS.len()];
    for _ in 0..DROP_ORDER.len() {
        let fixed: usize = (1..COLUMNS.len())
            .filter(|index| !hidden[*index])
            .map(|index| widths[index] + 2)
            .sum();
        if max_width.saturating_sub(fixed) >= MIN_MODEL_WIDTH {
            break;
        }
        let Some(index) = DROP_ORDER.iter().copied().find(|&index| !hidden[index]) else {
            break;
        };
        hidden[index] = true;
    }
    let visible: Vec<usize> = (0..COLUMNS.len()).filter(|index| !hidden[*index]).collect();
    // 模型列吃掉剩余预算，其余列按内容宽定宽。
    let fixed: usize = visible
        .iter()
        .copied()
        .filter(|&index| index != 0)
        .map(|index| widths[index] + 2)
        .sum();
    widths[0] = widths[0]
        .min(max_width.saturating_sub(fixed))
        .max(str_width(COLUMNS[0]));
    for row in &mut rows {
        row[0] = crate::render::wrap::truncate(row[0].clone(), widths[0]).into_owned();
    }

    let line = |cells: &[String; 9]| {
        visible
            .iter()
            .copied()
            .map(|index| {
                let cell = &cells[index];
                let gap = widths[index].saturating_sub(str_width(cell));
                // 模型列左对齐，数字列右对齐。
                if index == 0 {
                    format!("{cell}{}", " ".repeat(gap))
                } else {
                    format!("{}{cell}", " ".repeat(gap))
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    let header_cells = COLUMNS.map(str::to_owned);
    let header = line(&header_cells);
    let rule = "─".repeat(str_width(&header));
    let mut lines = vec![header, rule.clone()];
    let total = rows.pop().expect("合计行总会入列");
    for row in rows {
        lines.push(line(&row));
    }
    if !result.models.is_empty() {
        lines.push(rule);
    }
    lines.push(line(&total));
    lines
}

/// 表格里的一行：`[模型, 调用, 输入, 输出, 缓存率, 思考, 生成, TTFT, TPS]`。
fn usage_row(summary: &moc_protocol::ModelUsageSummary, label: &str) -> [String; 9] {
    let cached = summary.cached_input_tokens.unwrap_or(0);
    let cache_rate = if summary.input_tokens == 0 {
        0.0
    } else {
        cached as f64 * 100.0 / summary.input_tokens as f64
    };
    [
        label.to_owned(),
        summary.calls.to_string(),
        crate::render::format_tokens(summary.input_tokens),
        crate::render::format_tokens(summary.output_tokens),
        format!("{cache_rate:.1}%"),
        summary
            .reasoning_tokens
            .map_or_else(|| "未知".to_owned(), crate::render::format_tokens),
        if summary.gen_ms > 0 {
            format_active(summary.gen_ms)
        } else {
            "—".to_owned()
        },
        average_ttft(summary).map_or_else(|| "—".to_owned(), format_active),
        usage_tps(summary).map_or_else(|| "—".to_owned(), |tps| format!("{tps:.1}")),
    ]
}

/// 平均首 token 等待；没有请求上报过首 token 时返回 `None`（旧会话或全部失败）。
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

/// 时长：毫秒级给毫秒，分钟以上用 `1m02s` / `1h05m`，
/// 免得长会话在表格里写成 `3723.00 s` 撑爆列宽。
fn format_active(active_ms: u64) -> String {
    let secs = active_ms / 1_000;
    if active_ms < 1_000 {
        format!("{active_ms} ms")
    } else if secs < 60 {
        format!("{secs}.{}s", active_ms % 1_000 / 100)
    } else if secs < 3_600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3_600, secs % 3_600 / 60)
    }
}

/// `execute_command` 的解析结果：先把借用 `app.commands` 的部分取出成自有值，
/// 分派阶段才能对 `app` 做可变操作。
enum ResolvedCommand {
    Builtin(&'static CommandSpec<CommandId>),
    Plugin(moc_protocol::commands::PluginCommand),
    Unknown,
}

fn execute_command(app: &mut App, connection: &EmbeddedConnection, line: String) {
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
fn dispatch_command(app: &mut App, connection: &EmbeddedConnection, id: CommandId, rest: &str) {
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
        CommandId::Doctor => app.push_note(crate::doctor::report(app), false),
        CommandId::Help => app.modal = Some(Modal::Help { scroll: 0 }),
        CommandId::Context => open_context_content(app, connection),
        CommandId::Tools => open_tools(app, connection),
        CommandId::Quit => app.should_quit = true,
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
fn show_features(app: &mut App) {
    app.push_note(
        moc_protocol::features::format_features(&app.features),
        false,
    );
}

/// 循环切换引擎：按特性 flag 过滤后的清单循环。会话跑过第一条提示后引擎锁定。
fn cycle_engine(app: &mut App, connection: &EmbeddedConnection) {
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
fn toggle_plan(app: &mut App, connection: &EmbeddedConnection) {
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
fn parse_goal_action(rest: &str) -> Result<moc_protocol::methods::GoalAction, String> {
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
fn render_goal_note(goal: Option<&moc_protocol::methods::GoalView>) -> String {
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
fn goal_command(app: &mut App, connection: &EmbeddedConnection, rest: &str) {
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
        moc_protocol::methods::PromptMode::Ask,
    );
}

/// `/retry`：不追加任何用户消息，按现有上下文重新请求一次；只有空闲时可用。
fn retry(app: &mut App, connection: &EmbeddedConnection) {
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
fn set_engine(app: &mut App, connection: &EmbeddedConnection, engine: SessionEngine) {
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

/// 请求 core 切换会话 Fast Mode；生效后由 `SessionUpdate::FastModeChanged` 写回。
fn set_fast_mode(app: &mut App, connection: &EmbeddedConnection, enabled: bool) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        if let Err(error) = client
            .call::<SessionSetFastMode>(SessionSetFastModeParams {
                session_id,
                enabled,
            })
            .await
        {
            let _ = events.send(AppEvent::Notice(format!("切换 Fast Mode 失败：{error}")));
        }
    });
}

/// `/autonomy [default|dont_ask|unattended]`：无参数时循环切换自主程度，带参数直接设置。
fn set_autonomy(app: &mut App, connection: &EmbeddedConnection, rest: &str) {
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

fn set_permission(app: &mut App, connection: &EmbeddedConnection, rest: &str) {
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
fn apply_permission(
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

/// 按当前模型声明的思考等级循环切换。
fn cycle_thinking_level(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice("当前没有活动会话");
        return;
    };
    // Fusion 会话切换的是 lead 侧的思考等级，sidekick 侧原样保留。
    let (base, current, sidekick) = match crate::modal::parse_fusion(&session.model) {
        Some((lead, sidekick)) => {
            let (base, level) = lead.rsplit_once(':').unwrap_or((lead, "off"));
            (base.to_owned(), level.to_owned(), Some(sidekick.to_owned()))
        }
        None => {
            let (base, level) = session
                .model
                .rsplit_once(':')
                .unwrap_or((session.model.as_str(), "off"));
            (base.to_owned(), level.to_owned(), None)
        }
    };
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
        .position(|level| level == &current)
        .unwrap_or(0);
    let next = levels[(index + 1) % levels.len()].clone();
    let selector = match &sidekick {
        Some(sidekick) => crate::modal::fusion_selector(&format!("{base}:{next}"), sidekick),
        None => format!("{base}:{next}"),
    };
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

/// 把拖选覆盖的视口行文本复制到系统剪贴板；没有可复制内容时返回 `false`。
fn copy_selection(app: &mut App) -> bool {
    let body = selection_text(app);
    if body.is_empty() {
        return false;
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
    true
}

/// 拖选覆盖的纯文本：端点占的格子都算在选区内，跨行时首尾行按列切片、
/// 中间行整行保留。
fn selection_text(app: &App) -> String {
    let Some(((start_col, start_row), (end_col, end_row))) =
        app.selection.map(crate::app::ordered_selection)
    else {
        return String::new();
    };
    let area_x = app.scrollback_area.x;
    let start_col = usize::from(start_col.saturating_sub(area_x));
    // 端点列是格子坐标，切片的右开上界因此取「末列 + 1」。
    let end_col = usize::from(end_col.saturating_sub(area_x)).saturating_add(1);
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
    text.trim_end().to_owned()
}

/// 通过 OSC52 把文本交给终端自身的剪贴板；与绘制同线程写出，不会与帧交错。
pub(crate) fn write_osc52(text: &str) {
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
    use moc_protocol::methods::{AskOption, AskParams, AskQuestion, AskResult};
    use tokio::sync::mpsc::unbounded_channel;

    fn app() -> App {
        let (events, _receiver) = unbounded_channel::<AppEvent>();
        App::new(Theme::global(), events)
    }

    /// 前台 bash 也登记为 job：其开始/结束更新不得挂「后台」徽标或推「后台任务结束」
    /// 便签——结果已随工具卡片落地。转后台后 origin 改写，徽标挂上、结束才提示。
    #[test]
    fn foreground_job_updates_do_not_masquerade_as_background() {
        use crate::transcript::Block;
        use moc_protocol::methods::{JobInfo, JobLifecycle, JobOrigin, JobStatus};

        let job = |origin: JobOrigin, status: JobStatus, exit_code: Option<i32>| JobInfo {
            job_id: "job_1".into(),
            command: "cargo build".into(),
            description: None,
            tool_call_id: "call_1".into(),
            status,
            exit_code,
            lifecycle: JobLifecycle::Monitored,
            origin,
            stalled: false,
            output_elided: false,
            started_at_ms: 0,
            duration_ms: Some(1_600),
            log_uri: "artifact://jobs/job_1.log".into(),
        };
        let tool_job_id = |app: &App| match &app.blocks[0].block {
            Block::Tool(tool) => tool.job_id.clone(),
            other => panic!("首个块应为工具块：{other:?}"),
        };

        let mut app = app();
        apply_update(
            &mut app,
            SessionUpdate::ToolStarted {
                prompt_id: "prompt_1".into(),
                tool_call_id: "call_1".into(),
                name: "bash".into(),
                parent_tool_call_id: None,
                arguments: "{}".into(),
            },
        );

        // 前台来源：不挂徽标，结束也不推便签。
        apply_update(
            &mut app,
            SessionUpdate::JobStarted {
                job: job(JobOrigin::Foreground, JobStatus::Running, None),
            },
        );
        assert_eq!(tool_job_id(&app), None);
        apply_update(
            &mut app,
            SessionUpdate::JobFinished {
                job: job(JobOrigin::Foreground, JobStatus::Exited, Some(0)),
            },
        );
        assert_eq!(app.blocks.len(), 1, "前台任务结束不得追加便签");

        // 自动转后台：徽标挂上；结束推便签并摘除徽标。
        apply_update(
            &mut app,
            SessionUpdate::JobUpdated {
                job: job(JobOrigin::AutoBackground, JobStatus::Running, None),
            },
        );
        assert_eq!(tool_job_id(&app), Some("job_1".into()));
        apply_update(
            &mut app,
            SessionUpdate::JobFinished {
                job: job(JobOrigin::AutoBackground, JobStatus::Exited, Some(0)),
            },
        );
        assert_eq!(tool_job_id(&app), None, "任务结束后摘除徽标");
        let Some(Block::Note { text, error }) = app.blocks.last().map(|entry| &entry.block) else {
            panic!("后台任务结束应追加便签");
        };
        assert!(!*error);
        assert_eq!(text, "后台任务结束：cargo build（退出码 0）");
    }

    /// `/usage` 输出会话统计与对齐表格：列宽一致、数字右对齐、token 走 k/m/b 单位，
    /// 超宽模型名截断，末行是全部模型的合计。
    #[test]
    fn usage_table_aligns_columns_with_units() {
        use moc_protocol::{ModelUsageSummary, SessionUsageResult};

        let summary = |model: &str, calls| ModelUsageSummary {
            model: model.into(),
            source: moc_protocol::methods::UsageSource::Session,
            calls,
            input_tokens: 1_200_000,
            output_tokens: 12_345,
            cached_input_tokens: Some(1_100_000),
            cache_creation_input_tokens: None,
            reasoning_tokens: Some(1_200),
            active_ms: 62_000,
            ttft_ms: 12_000,
            ttft_calls: 2,
            gen_ms: 50_000,
        };
        let result = SessionUsageResult {
            models: vec![
                summary("cpa-chat/deepseek-v4.1-flash:max", 12),
                summary("p/m", 3),
            ],
            total: summary("total", 15),
            tool_ms: 248_000,
        };
        let table = format_usage(&result, 74);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(
            lines.len(),
            12,
            "会话统计标题 + 四行统计 + 空行 + 表头/分隔线/两个模型/分隔线/合计：\n{table}"
        );
        assert_eq!(lines[0], "会话统计");
        assert!(
            lines[1].contains("1m02s")
                && lines[1].contains("生成 50.0s")
                && lines[1].contains("网络等待 12.0s"),
            "模型用时必须拆成生成与网络等待：{table}"
        );
        assert!(lines[2].contains("4m08s"), "工具调用用时：{table}");
        assert!(lines[3].contains("6.0s"), "平均 TTFT = 12s / 2 次：{table}");
        assert!(
            lines[4].contains("246.9 tok/s"),
            "TPS 只除生成时间：{table}"
        );
        assert_eq!(lines[5], "", "统计和表格之间留一个空行");
        let table_lines = &lines[6..];
        let width = str_width(table_lines[0]);
        for line in table_lines {
            assert_eq!(str_width(line), width, "列宽必须一致：{table}");
            assert!(
                str_width(line) <= 71,
                "表格不得超过 80 列终端里 note 的可用宽度：{table}"
            );
        }
        assert!(
            table_lines[0].contains("模型")
                && table_lines[0].contains("生成")
                && table_lines[0].contains("TTFT")
                && table_lines[0].contains("TPS")
        );
        assert!(
            table.contains("1.2m") && table.contains("12.35k"),
            "{table}"
        );
        assert!(table.contains('…'), "超宽模型名应截断：{table}");
        let total = table_lines.last().expect("合计行");
        assert!(total.trim_start().starts_with("合计"), "{table}");
        assert!(total.contains("15"), "合计行用 total 的调用次数：{table}");
        // note 渲染时按 71 列折行：换行必须是硬断行，表格才能逐行保持对齐。
        assert_eq!(
            crate::render::wrap::wrap_text(&table, 71),
            lines
                .iter()
                .map(|line| (*line).to_owned())
                .collect::<Vec<_>>(),
            "表格在 note 里不得被折行拆散：{table}"
        );

        // 窄终端按优先级丢列：先丢思考、再丢缓存率，保住模型名与 TTFT。
        let narrow = format_usage(&result, 60);
        let narrow_lines: Vec<&str> = narrow.lines().collect();
        let narrow_table = &narrow_lines[narrow_lines.len() - 6..];
        for line in narrow_table {
            assert_eq!(str_width(line), str_width(narrow_table[0]), "{narrow}");
            assert!(str_width(line) <= 57, "{narrow}");
        }
        assert!(
            narrow_table[0].contains("TTFT")
                && !narrow_table[0].contains("思考")
                && !narrow_table[0].contains("缓存率"),
            "窄终端保留 TTFT，丢掉低优先级列：{narrow}"
        );

        // 新会话没有明细：不输出会话统计，表头、分隔线、合计三行。
        let empty = format_usage(
            &SessionUsageResult {
                models: Vec::new(),
                total: ModelUsageSummary {
                    model: "total".into(),
                    source: moc_protocol::methods::UsageSource::Session,
                    calls: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                    active_ms: 0,
                    ttft_ms: 0,
                    ttft_calls: 0,
                    gen_ms: 0,
                },
                tool_ms: 0,
            },
            74,
        );
        assert_eq!(empty.lines().count(), 3, "{empty}");
    }

    /// `/usage` 与状态栏后台刷新走同一条 core 统计：回执带请求序号，
    /// `show` 决定是否把明细写进转写。
    #[tokio::test]
    async fn usage_refresh_reads_session_totals_from_core() {
        let (_core, connection, mut app, mut receiver, _root) = app_with_core().await;

        execute_command(&mut app, &connection, "/usage".into());
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("用量回执应在超时前返回")
            .expect("事件队列未关闭");
        let AppEvent::Usage {
            request,
            result,
            show,
        } = event
        else {
            panic!("应收到用量事件");
        };
        assert!(show, "/usage 要把明细写进转写");
        assert_eq!(request, 1, "回执带回发起时的请求序号");
        assert_eq!(result.total.calls, 0, "新会话还没有模型调用");
        app.apply_session_usage(request, result);
        assert_eq!(app.session_tokens(), (0, 0));

        // 提示收尾后的后台刷新同样取统计，但不写转写。
        let blocks = app.blocks.len();
        refresh_usage(&mut app, &connection, false);
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("后台用量回执应在超时前返回")
            .expect("事件队列未关闭");
        let AppEvent::Usage {
            request,
            result,
            show,
        } = event
        else {
            panic!("应收到用量事件");
        };
        assert!(!show, "后台刷新静默");
        assert!(request > 1);
        app.apply_session_usage(request, result);
        assert_eq!(app.blocks.len(), blocks, "静默刷新不写转写");
    }

    /// 点状态栏右侧统计段 = 输入 `/usage`：命中区正好盖住渲染的统计文本，
    /// 点击发出带 `show` 的刷新请求（明细写进转写）。
    #[tokio::test]
    async fn statusline_usage_click_runs_usage_command() {
        use moc_protocol::{ModelUsageSummary, SessionUsageResult};

        let (core, connection, mut app, mut receiver, _root) = app_with_core().await;
        let summary = || ModelUsageSummary {
            model: "p/m".into(),
            source: moc_protocol::methods::UsageSource::Session,
            calls: 1,
            input_tokens: 1_200,
            output_tokens: 340,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
            active_ms: 1_000,
            ttft_ms: 0,
            ttft_calls: 0,
            gen_ms: 0,
        };
        let request = app.begin_usage_request();
        app.apply_session_usage(
            request,
            SessionUsageResult {
                models: Vec::new(),
                total: summary(),
                tool_ms: 0,
            },
        );

        let terminal = draw_frame(&mut app, 100, 24);
        let area = app.usage_area;
        assert!(!area.is_empty(), "有用量时统计段应登记命中区");
        let text: String = (area.x..area.x.saturating_add(area.width))
            .filter_map(|x| terminal.backend().buffer().cell((x, area.y)))
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains('⇅'), "命中区应正好盖住统计段：{text:?}");

        handle_mouse_with_connection(&mut app, &connection, left_click(area.x, area.y));
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("点击应触发用量回执")
            .expect("事件队列未关闭");
        let AppEvent::Usage { show, .. } = event else {
            panic!("应收到用量事件");
        };
        assert!(show, "点击统计段等价 /usage：明细要写进转写");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 离底时输入框上方浮出「回到底部」胶囊，点击贴回最新消息。
    #[tokio::test]
    async fn jump_bottom_pill_scrolls_back_to_bottom() {
        use crate::transcript::Block;

        let (core, connection, mut app) = setup_test_app().await;
        for index in 0..50 {
            app.push_block(Block::User {
                text: format!("历史消息 {index}"),
            });
        }
        app.scroll_offset = 10;
        let terminal = draw_frame(&mut app, 100, 24);
        let area = app.jump_bottom_area;
        assert!(!area.is_empty(), "离底时应画出回到底部胶囊");
        // 胶囊落在输入框上方的缝行。
        assert_eq!(area.y, app.prompt_area.y - 1);
        let text: String = (area.x..area.x.saturating_add(area.width))
            .filter_map(|x| terminal.backend().buffer().cell((x, area.y)))
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains('◖') && text.contains('↓') && text.contains('◗'),
            "命中区应正好盖住圆角胶囊：{text:?}"
        );

        handle_mouse_with_connection(&mut app, &connection, left_click(area.x, area.y));
        assert_eq!(app.scroll_offset, 0, "点击胶囊应贴回底部");

        // 贴底后胶囊退场。
        draw_frame(&mut app, 100, 24);
        assert!(app.jump_bottom_area.is_empty(), "贴底后不再登记命中区");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 搭一个连到真 core 的 app，并把事件接收端一并交出（用例要观察回执）。
    async fn app_with_core() -> (
        std::sync::Arc<moc_core::Core>,
        EmbeddedConnection,
        App,
        tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        tempfile::TempDir,
    ) {
        use moc_core::{Core, CoreConfig};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\ncontext_window=200000\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .unwrap();
        let (events, receiver) = unbounded_channel();
        let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        app.session = Some(session);
        (core, connection, app, receiver, root)
    }

    /// 插件命令参与补全与分派：`/plugins` 读回清单，插件命令的结果写进转写。
    #[tokio::test]
    async fn plugin_commands_complete_and_run_through_core() {
        let (core, connection, mut app, mut receiver, root) = app_with_core().await;
        let dir = root.path().join(".moc/plugins/demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        std::fs::write(
            dir.join("index.js"),
            r#"
            export default {
              commands: { demo: { description: "示例命令", run: args => `cmd:${args}` } }
            };
            "#,
        )
        .unwrap();

        execute_command(&mut app, &connection, "/plugins".into());
        let mut plugins = None;
        let mut listing = None;
        while plugins.is_none() || listing.is_none() {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
                .await
                .expect("插件清单应在超时前返回")
                .expect("事件队列未关闭");
            match event {
                AppEvent::Plugins(list) => plugins = Some(list),
                AppEvent::PluginOutput { text, error, .. } => {
                    assert!(!error, "{text}");
                    listing = Some(text);
                }
                _ => {}
            }
        }
        let plugins = plugins.expect("收到清单");
        // 主循环收到清单时会同步命令注册表；这里手动同步一次，不重复跑整个事件循环。
        app.commands.set_plugins(&plugins);
        app.plugins = plugins.clone();
        assert_eq!(plugins[0].name, "demo");
        assert_eq!(plugins[0].commands[0].name, "demo");
        assert!(
            listing.expect("列出插件").contains("命令 /demo"),
            "清单里要列出插件命令"
        );
        // 清单顺带刷新补全表；插件命令与内置命令一起参与前缀匹配。
        app.prompt.insert_str_raw("/demo");
        let completion =
            crate::widgets::completion::CompletionState::derive(&app).expect("插件命令可补全");
        assert_eq!(completion.candidates[0].value, "/demo");

        app.blocks.clear();
        execute_command(&mut app, &connection, "/demo 参数".into());
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
                .await
                .expect("命令结果应在超时前返回")
                .expect("事件队列未关闭");
            if let AppEvent::PluginOutput { name, text, error } = event {
                assert_eq!(name, "demo");
                assert!(!error, "{text}");
                assert_eq!(text, "cmd:参数");
                break;
            }
        }

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 插件停用通知保留多行原因（JS 调用栈），并附上恢复方式。
    #[test]
    fn plugin_notice_note_keeps_cause_and_recovery_hint() {
        assert_eq!(
            plugin_notice_note("demo", "E_PLUGIN_DISABLED", "TypeError: x\n  at y"),
            "插件 `demo` 已停用（E_PLUGIN_DISABLED）：TypeError: x\n  at y\n修复后 /plugins reload 可重载"
        );
    }

    /// `/context` 打开正文窗，并把 core 的真实分块装进窗里。
    #[tokio::test]
    async fn context_command_opens_content_window_with_core_sections() {
        use moc_protocol::methods::ContextContentResult;

        let (core, connection, mut app, mut receiver, _root) = app_with_core().await;
        execute_command(&mut app, &connection, "/context".into());
        assert!(
            matches!(app.modal, Some(Modal::ContextContent { .. })),
            "/context 打开上下文正文窗"
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("正文分块应在超时前返回")
            .expect("事件队列未关闭");
        let AppEvent::ContextContent {
            request,
            session_id,
            result,
        } = event
        else {
            panic!("应收到上下文正文事件");
        };
        assert_eq!(app.session.as_ref().unwrap().id, session_id);
        let content = result.expect("读取上下文正文");
        assert!(content.total_tokens > 0);
        assert!(
            content
                .sections
                .iter()
                .any(|section| section.role.as_deref() == Some("system")),
            "实际 system 消息必须在目录里"
        );
        assert!(
            content.sections.iter().all(|section| !matches!(
                section.name.as_str(),
                "user" | "assistant" | "tool result"
            )),
            "空会话不该有消息分块"
        );

        apply_context_content(&mut app, request, Ok(content));
        let Some(Modal::ContextContent { state }) = app.modal.as_ref() else {
            panic!("正文窗应保持打开");
        };
        assert!(state.content.is_some());
        assert!(!state.expanded.is_empty() && state.expanded.iter().all(|flag| !flag));

        // 窗已关掉时回执不得把窗重新打开。
        app.modal = None;
        apply_context_content(
            &mut app,
            request,
            Ok(ContextContentResult {
                context_window: 1,
                total_tokens: 1,
                sections: Vec::new(),
                ..Default::default()
            }),
        );
        assert!(app.modal.is_none());
        core.shutdown().await;
    }

    /// 正文窗的鼠标：滚轮滚动、点分块切换展开、拖滑块换算滚动位置。
    #[test]
    fn context_window_mouse_toggles_and_drags() {
        use crate::modal::{ContextLayout, ContextScrollbar};
        use moc_protocol::methods::{ContextContentResult, ContextContentSection};
        use ratatui::layout::Rect;

        let mut app = app();
        let mut state = ContextContentState::default();
        state.set_content(ContextContentResult {
            context_window: 100_000,
            total_tokens: 1_000,
            sections: (0..3)
                .map(|index| ContextContentSection {
                    id: index.to_string(),
                    name: format!("block{index}"),
                    tokens: 10,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        state.viewport = 10;
        state.layout = Some(ContextLayout {
            width: 40,
            spans: vec![0..5, 5..12, 12..30],
            lines: vec![ratatui::text::Line::default(); 30],
        });
        state.scrollbar = ContextScrollbar {
            track: Rect::new(50, 4, 1, 10),
            thumb: 3,
            max_offset: 20,
        };
        app.modal = Some(Modal::ContextContent { state });
        let mouse = |kind, column, row| crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let scroll = |app: &App| match app.modal.as_ref() {
            Some(Modal::ContextContent { state }) => state.scroll,
            _ => panic!("正文窗应保持打开"),
        };

        // 滚轮：一次三行，夹在 [0, max_offset]。
        handle_context_content_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 10, 6));
        assert_eq!(scroll(&app), 3);
        handle_context_content_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 10, 6));
        assert_eq!(scroll(&app), 0);

        // 抓滑块顶端拖到轨道末端：偏移走满。
        handle_context_content_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 50, 4),
        );
        handle_context_content_mouse(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), 50, 11),
        );
        assert_eq!(scroll(&app), 20);
        handle_context_content_mouse(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), 50, 11),
        );
        let Some(Modal::ContextContent { state }) = app.modal.as_ref() else {
            panic!("正文窗应保持打开");
        };
        assert!(state.drag.is_none(), "抬起后结束拖拽");

        // 点第 3 块正文：命中该块并展开。
        handle_context_content_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 10, 6),
        );
        let Some(Modal::ContextContent { state }) = app.modal.as_ref() else {
            panic!("正文窗应保持打开");
        };
        assert_eq!(state.focus, Some(2), "命中的是滚动位置对应的块");
        assert_eq!(state.expanded, vec![false, false, true]);
        assert!(state.layout.is_none(), "展开后必须重排");
        // 轨道之外的点不改变焦点。
        let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
            panic!("正文窗应保持打开");
        };
        state.layout = Some(ContextLayout {
            width: 40,
            spans: vec![0..5, 5..12, 12..30],
            lines: vec![ratatui::text::Line::default(); 30],
        });
        handle_context_content_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 10, 30),
        );
        let Some(Modal::ContextContent { state }) = app.modal.as_ref() else {
            panic!("正文窗应保持打开");
        };
        assert_eq!(state.focus, Some(2), "正文区之外的点不命中分块");
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
            ptc_worker: None,
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
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .unwrap();
        let id = session.id.clone();
        let mut app = App::new(Theme::global(), events);
        app.session = Some(session);
        app.tool_detail = DetailLevel::Full;
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
                    if let crate::protocol::UiUpdate::Session(update) = update {
                        apply_update(&mut app, update.update);
                    }
                }
                if let event @ (AppEvent::PromptReturned { .. } | AppEvent::SteerReturned { .. }) =
                    event
                {
                    crate::interaction::handle_event(&mut app, &connection, event)
                }
            }
            while let Ok(update) = connection.updates.try_recv() {
                if let crate::protocol::UiUpdate::Session(update) = update {
                    apply_update(&mut app, update.update);
                }
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
                omit_auxiliary: false,
                session_id: id,
                max_entries: None,
                read_only: false,
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

    /// 压缩提示从 0s 开始计时，完成后原位替换，不追加第二条。
    #[test]
    fn compaction_note_ticks_then_replaces_in_place() {
        use crate::transcript::Block;
        use moc_protocol::{SessionEngine, SessionInfo};

        let mut app = app();
        app.session = Some(SessionInfo {
            id: "ses_compact".into(),
            title: "压缩".into(),
            model: "test/model".into(),
            permission: moc_protocol::SessionPermission::Full,
            engine: SessionEngine::Standard,
            autonomy: moc_protocol::SessionAutonomy::Default,
            plan_active: false,
            fast_mode: false,
            engine_locked: false,
            parent_session_id: None,
            forked_from: None,
            agent_preset: None,
            delegation_depth: 0,
            cwd: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        });
        let note = |app: &App| match app.blocks.last().map(|entry| &entry.block) {
            Some(Block::Note { text, error }) => (text.clone(), *error),
            other => panic!("压缩提示应为 Note：{other:?}"),
        };

        apply_update(&mut app, SessionUpdate::ContextCompactionStarted);
        assert_eq!(note(&app), ("上下文压缩中…(0s)".into(), false));

        // 3 秒后同一条提示只改秒数。
        app.compaction_progress
            .as_mut()
            .expect("压缩在飞")
            .started_at -= std::time::Duration::from_secs(3);
        assert!(app.tick_compaction());
        assert_eq!(note(&app), ("上下文压缩中…(3s)".into(), false));
        assert_eq!(app.blocks.len(), 1, "计时不应追加新块");

        apply_update(
            &mut app,
            SessionUpdate::ContextCompacted {
                method: "soft".into(),
                tokens_before: 426_070,
                tokens_after: 247_960,
                full: true,
            },
        );
        assert_eq!(
            note(&app),
            ("上下文已压缩：426.07k → 247.96k".into(), false)
        );
        assert_eq!(app.blocks.len(), 1, "完成时应原位替换");
        assert!(app.compaction_progress.is_none());
    }

    /// 仅裁剪历史时不能说成「已压缩」：措辞与全量摘要区分开。
    #[test]
    fn pruning_compaction_is_labelled_as_trim() {
        use crate::transcript::Block;
        use moc_protocol::{SessionEngine, SessionInfo};

        let mut app = app();
        app.session = Some(SessionInfo {
            id: "ses_prune".into(),
            title: "裁剪".into(),
            model: "test/model".into(),
            permission: moc_protocol::SessionPermission::Full,
            engine: SessionEngine::Standard,
            autonomy: moc_protocol::SessionAutonomy::Default,
            plan_active: false,
            fast_mode: false,
            engine_locked: false,
            parent_session_id: None,
            forked_from: None,
            agent_preset: None,
            delegation_depth: 0,
            cwd: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        });

        apply_update(&mut app, SessionUpdate::ContextCompactionStarted);
        apply_update(
            &mut app,
            SessionUpdate::ContextCompacted {
                method: "soft".into(),
                tokens_before: 426_070,
                tokens_after: 247_960,
                full: false,
            },
        );
        let Some(Block::Note { text, error }) = app.blocks.last().map(|entry| &entry.block) else {
            panic!("裁剪提示应为 Note");
        };
        assert_eq!(text, "上下文已裁剪：426.07k → 247.96k（未生成摘要）");
        assert!(!error);
        assert_eq!(app.blocks.len(), 1, "裁剪只留一条终态提示");
        assert!(app.compaction_progress.is_none());

        // 本地裁剪没有进行中提示：完成事件直接落一条终态，不留下“压缩中”。
        app.blocks.clear();
        apply_update(
            &mut app,
            SessionUpdate::ContextCompacted {
                method: "shake".into(),
                tokens_before: 426_070,
                tokens_after: 247_960,
                full: false,
            },
        );
        let Some(Block::Note { text, .. }) = app.blocks.last().map(|entry| &entry.block) else {
            panic!("裁剪完成应为 Note");
        };
        assert_eq!(text, "上下文已裁剪：426.07k → 247.96k（未生成摘要）");
        assert_eq!(app.blocks.len(), 1);
    }

    /// 压缩失败同样收掉进行中提示，不留一条永远计时的挂起块。
    #[test]
    fn failed_compaction_ends_the_running_note() {
        use crate::transcript::Block;
        use moc_protocol::{SessionEngine, SessionInfo};

        let mut app = app();
        app.session = Some(SessionInfo {
            id: "ses_failed".into(),
            title: "失败".into(),
            model: "test/model".into(),
            permission: moc_protocol::SessionPermission::Full,
            engine: SessionEngine::Standard,
            autonomy: moc_protocol::SessionAutonomy::Default,
            plan_active: false,
            fast_mode: false,
            engine_locked: false,
            parent_session_id: None,
            forked_from: None,
            agent_preset: None,
            delegation_depth: 0,
            cwd: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        });

        apply_update(&mut app, SessionUpdate::ContextCompactionStarted);
        apply_update(
            &mut app,
            SessionUpdate::ContextCompactionFailed {
                message: "上游不可用".into(),
            },
        );
        let Some(Block::Note { text, error }) = app.blocks.last().map(|entry| &entry.block) else {
            panic!("失败提示应为 Note");
        };
        assert_eq!(text, "上下文压缩失败：上游不可用");
        assert!(error);
        assert!(app.compaction_progress.is_none());
    }

    #[tokio::test]
    async fn closing_modal_resumes_queue_after_turn_finishes() {
        use moc_core::{Core, CoreConfig};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let upstream = MockServer::builder().start().await;
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
            ptc_worker: None,
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
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .unwrap();
        let id = session.id.clone();
        app.session = Some(session);
        app.prompt_running = true;
        app.inflight_prompts.insert(id.clone());
        app.modal = Some(Modal::Help { scroll: 0 });
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
                omit_auxiliary: false,
                session_id: id,
                max_entries: None,
                read_only: false,
            })
            .await
            .unwrap();
        // 运行时提醒也占 transcript 槽位，比对问答文本时先跳过。
        let texts: Vec<String> = loaded
            .entries
            .iter()
            .filter(|entry| entry.reminder.is_none())
            .map(|entry| entry.message.text())
            .collect();
        assert_eq!(texts, vec!["弹窗关闭后继续执行", "队列已恢复"]);
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

        let upstream = MockServer::builder().start().await;
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
            "[models.roles]\ndefault='test/model:off'\n[providers.test]\napi='openai_chat'\nbase_url='{}'\n[providers.test.models.model]\ncontext_window=100000\n[providers.test.models.tiny]\ncontext_window=32000\nmax_output_tokens=4096\n[compaction]\nmethod='moc'\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
            upstream.uri(),
        )).unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
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
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
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
        // 历史超过目标窗口，但系统前缀和摘要请求仍能放下。
        connection
            .client
            .call::<SessionPrompt>(PromptParams {
                session_id: id,
                input: vec![InputPart::Text {
                    text: "上下文填充".repeat(8000),
                }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await
            .unwrap();

        app.open_model_picker();
        let Some(Modal::Model {
            entries, picker, ..
        }) = app.modal.as_mut()
        else {
            panic!("模型选择器应打开");
        };
        // 候选首位是 Fusion 特殊项：按 selector 找 tiny 的条目下标。
        picker.selected = entries
            .iter()
            .position(|entry| entry.selector == "test/tiny")
            .expect("候选里有 tiny");
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
        assert_eq!(overflow.context_window, 32000);
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
                if let crate::protocol::UiUpdate::Session(update) = update {
                    apply_update(&mut app, update.update);
                }
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

            let upstream = MockServer::builder().start().await;
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
                "[models.roles]\ndefault='test/model:off'\n[providers.test]\napi='openai_chat'\nbase_url='{}'\n[providers.test.models.model]\ncontext_window=100000\n[providers.test.models.tiny]\ncontext_window=1000\nmax_output_tokens=4096\n[compaction]\nauto=false\nmethod='moc'\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
                upstream.uri(),
            )).unwrap();
            let core = Core::new(CoreConfig {
                home: root.path().to_owned(),
                cwd: root.path().to_owned(),
                overlays: Vec::new(),
                session_dir: Some(root.path().join("sessions")),
                client: Default::default(),
                ptc_worker: None,
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
                    engine: SessionEngine::Standard,
                    plan_active: false,
                    fast_mode: false,
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
                    agent_message: None,
                    goal_round: None,
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
                    omit_auxiliary: false,
                    session_id: self.session.clone(),
                    max_entries: None,
                    read_only: false,
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
        let Some(Modal::Model {
            entries, picker, ..
        }) = h.app.modal.as_mut()
        else {
            panic!("模型选择器应打开");
        };
        // 候选首位是 Fusion 特殊项：按 selector 找 tiny 的条目下标。
        picker.selected = entries
            .iter()
            .position(|entry| entry.selector == "test/tiny")
            .expect("候选里有 tiny");
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
        execute_command(&mut h.app, &h.connection, "/compact 保留当前任务".into());
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
            ptc_worker: None,
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
            recommended: Vec::new(),
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
            custom: vec![crate::modal::CustomAnswer::default(); 2],
            preview_scroll: 0,
            question_scroll: 0,
            parked: false,
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
        // /r 的候选里 /retry 需要会话；补一个会话保留双候选的回绕语义。
        app.session = Some(moc_protocol::SessionInfo {
            id: "ses_completion".into(),
            title: "补全".into(),
            model: "test/model".into(),
            permission: moc_protocol::SessionPermission::Full,
            engine: SessionEngine::Standard,
            autonomy: moc_protocol::SessionAutonomy::Default,
            plan_active: false,
            fast_mode: false,
            engine_locked: false,
            parent_session_id: None,
            forked_from: None,
            agent_preset: None,
            delegation_depth: 0,
            cwd: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        });
        app.prompt.insert_str_raw("/r");
        app.completion = crate::widgets::completion::CompletionState::derive(&app);
        app
    }

    /// 接收侧 IRC 卡片与等待进度都必须落到主流。
    #[test]
    fn agent_message_update_reaches_current_transcript() {
        use crate::transcript::Block;
        use moc_protocol::methods::{
            AgentMessageMeta, AgentMessageOutcome, AgentStatus, ToolProgressRow,
        };

        let mut app = app();
        apply_update(
            &mut app,
            SessionUpdate::AgentMessage {
                meta: AgentMessageMeta {
                    from_id: "ses_child".into(),
                    from_title: "探查者".into(),
                    delivery: AgentMessageOutcome::Injected,
                },
                body: "子代理汇报".into(),
            },
        );
        assert!(
            matches!(
                app.blocks.last().map(|entry| &entry.block),
                Some(Block::AgentMessage { body, .. }) if body == "子代理汇报"
            ),
            "代理消息必须成为转写卡片"
        );

        app.start_tool_block(ToolCallStart {
            id: "call_wait".into(),
            name: "wait_agent".into(),
            parent_id: None,
            arguments: "{}".into(),
        });
        apply_update(
            &mut app,
            SessionUpdate::ToolProgress {
                prompt_id: "prompt_1".into(),
                tool_call_id: "call_wait".into(),
                rows: vec![ToolProgressRow {
                    agent_id: "ses_child".into(),
                    title: "探查者".into(),
                    status: AgentStatus::Running,
                    elapsed_ms: 500,
                    activity: None,
                }],
            },
        );
        let tool = app
            .tool_entry_mut("call_wait")
            .map(|entry| match &entry.block {
                Block::Tool(tool) => tool,
                _ => panic!("等待块应是工具块"),
            })
            .expect("等待块存在");
        assert_eq!(
            tool.progress.as_ref().map(Vec::len),
            Some(1),
            "进度行必须写进工具块"
        );
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
        assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(1));

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
        assert_eq!(app.prompt.text, "/resume ");
        assert!(app.completion.is_none());

        // Tab 与 Enter 共用确认分支：切换选中项后同样接受当前项。
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
            Some("/retry")
        );
        assert!(handle_completion_key(
            &mut app,
            KeyModifiers::NONE,
            KeyCode::Tab
        ));
        assert_eq!(app.prompt.text, "/retry ");
        assert!(app.completion.is_none());
        assert_eq!(app.focus, Focus::Prompt);
    }

    async fn setup_test_app() -> (std::sync::Arc<moc_core::Core>, EmbeddedConnection, App) {
        let (core, connection, app, _root) = setup_test_app_keeping_root().await;
        (core, connection, app)
    }

    /// 同 [`setup_test_app`]，但把临时目录一并交出，供需要再建会话的用例使用。
    ///
    /// 会话的 cwd 是那个临时目录：函数返回即删除，之后再 `session/new` 会因
    /// cwd 不存在而失败，所以这些用例必须自己握住目录。
    async fn setup_test_app_keeping_root() -> (
        std::sync::Arc<moc_core::Core>,
        EmbeddedConnection,
        App,
        tempfile::TempDir,
    ) {
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
            ptc_worker: None,
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
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .unwrap();
        app.session = Some(session);
        (core, connection, app, root)
    }

    #[tokio::test]
    async fn terminal_event_arrows_preserve_completion_selection() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_str_raw("/co");
        app.completion = crate::widgets::completion::CompletionState::derive(&app);
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

    /// HUD 鼠标：单击切换展开/折叠、按住卡片拖动；HUD 上的按下不启动转写点击。
    #[test]
    fn todo_hud_mouse_toggles_and_drags() {
        let mut app = app();
        app.scrollback_area = ratatui::layout::Rect::new(0, 2, 80, 20);
        let mouse = |kind: MouseEventKind, column: u16, row: u16| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // 展开态只有卡片命中区；悬停算命中但不改状态。
        app.todo_hud.card_area = ratatui::layout::Rect::new(40, 2, 35, 10);
        app.hover_chart = Some((0, 0));
        assert!(handle_todo_hud_mouse(
            &mut app,
            &mouse(MouseEventKind::Moved, 42, 3)
        ));
        assert!(app.todo_hud.is_open());
        assert!(app.hover_chart.is_none(), "悬停 HUD 时清掉图表高亮");

        // 单击卡片折叠成胶囊；不进入转写点击/拖选状态。
        assert!(handle_todo_hud_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), 42, 3)
        ));
        assert!(app.pending_click.is_none());
        assert!(handle_todo_hud_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), 42, 3)
        ));
        assert!(!app.todo_hud.is_open());

        // 折叠态单击胶囊回到展开。
        app.todo_hud.chip_area = ratatui::layout::Rect::new(60, 2, 15, 1);
        assert!(handle_todo_hud_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), 62, 2)
        ));
        assert!(handle_todo_hud_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), 62, 2)
        ));
        assert!(app.todo_hud.is_open());

        // 按住卡片拖过阈值：更新记忆位置，保持展开。
        app.todo_hud.card_area = ratatui::layout::Rect::new(40, 2, 35, 10);
        assert!(handle_todo_hud_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), 42, 3)
        ));
        assert!(handle_todo_hud_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), 45, 6)
        ));
        assert_eq!(app.todo_hud.drag_origin, Some((43, 5)));
        assert!(handle_todo_hud_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), 45, 6)
        ));
        assert!(app.todo_hud.is_open());
    }

    /// 画一帧整屏 UI，让鼠标命中几何与真实绘制同源。
    fn draw_frame(
        app: &mut App,
        width: u16,
        height: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut view = crate::render::ScrollbackView::new();
        terminal
            .draw(|frame| crate::render::draw(frame, app, &mut view))
            .unwrap();
        terminal
    }

    include!("event_model_picker_tests.rs");

    /// 列表类模态（PickerState）：滚轮只滚 viewport，悬停只铺底色，选中项不动；
    /// 键盘导航重新显露选中项。语义与模型选择器一致。
    #[tokio::test]
    async fn sessions_wheel_and_hover_preserve_selection() {
        use crossterm::event::MouseEventKind::{Moved, ScrollDown};
        let (core, connection, mut app) = setup_test_app().await;
        let session = |index: usize| moc_protocol::methods::SessionInfo {
            id: format!("ses_{index:04}"),
            title: format!("会话 {index}"),
            model: "model".into(),
            permission: Default::default(),
            engine: Default::default(),
            autonomy: Default::default(),
            plan_active: false,
            fast_mode: false,
            engine_locked: false,
            parent_session_id: None,
            forked_from: None,
            agent_preset: None,
            delegation_depth: 0,
            cwd: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        };
        app.modal = Some(Modal::Sessions {
            sessions: (0..30).map(session).collect(),
            cwd: "/tmp".into(),
            scope: crate::modal::SessionScope::All,
            picker: Default::default(),
        });
        let _ = draw_frame(&mut app, 100, 24);
        let (_, row) = *app.modal_item_hits.first().expect("列表有命中行");
        for step in 0..4 {
            handle_mouse_with_connection(
                &mut app,
                &connection,
                mouse_event(ScrollDown, row.x, row.y),
            );
            handle_mouse_with_connection(
                &mut app,
                &connection,
                mouse_event(Moved, row.x, row.y + step % 2),
            );
            let _ = draw_frame(&mut app, 100, 24);
            let Some(Modal::Sessions { picker, .. }) = &app.modal else {
                panic!("选择器应保持打开")
            };
            assert_eq!(picker.selected, 0, "滚轮与悬停不能改选中项");
            assert_eq!(picker.offset, (step as usize + 1) * 3);
            assert_eq!(app.modal_item_hits.first().unwrap().0, picker.offset);
        }
        // 普通重绘不把视口拉回选中项。
        let before = app.modal_item_hits.clone();
        let _ = draw_frame(&mut app, 100, 24);
        assert_eq!(app.modal_item_hits, before, "重绘不能拉回选中项");
        // 悬停只铺底色。
        let (_, hover_row) = *app.modal_item_hits.last().unwrap();
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(Moved, hover_row.x, hover_row.y),
        );
        let terminal = draw_frame(&mut app, 100, 24);
        assert_eq!(
            terminal.backend().buffer()[(hover_row.x, hover_row.y)].bg,
            app.theme.bg_hover
        );
        // 键盘导航重新显露选中项。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        let _ = draw_frame(&mut app, 100, 24);
        let Some(Modal::Sessions { picker, .. }) = &app.modal else {
            panic!("选择器应保持打开")
        };
        assert_eq!(picker.selected, 1);
        assert!(
            app.modal_item_hits.iter().any(|(index, _)| *index == 1),
            "键盘导航后选中项在视口内"
        );
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// /jobs：滚轮只滚 viewport；悬停不换行也不再误撤待确认标记。
    #[tokio::test]
    async fn jobs_wheel_and_hover_preserve_selection_and_pending_kill() {
        use crossterm::event::MouseEventKind::{Moved, ScrollDown};
        let (core, connection, mut app) = setup_test_app().await;
        let job = |index: usize| moc_protocol::methods::JobInfo {
            job_id: format!("job-{index}"),
            command: format!("sleep {index}"),
            description: None,
            tool_call_id: format!("call-{index}"),
            status: moc_protocol::methods::JobStatus::Running,
            exit_code: None,
            lifecycle: moc_protocol::methods::JobLifecycle::Monitored,
            origin: moc_protocol::methods::JobOrigin::Explicit,
            stalled: false,
            output_elided: false,
            started_at_ms: 0,
            duration_ms: None,
            log_uri: format!("artifact://jobs/{index}"),
        };
        app.modal = Some(Modal::Jobs {
            jobs: (0..30).map(job).collect(),
            selected: 0,
            pending_kill: None,
            offset: 0,
            scroll: Default::default(),
            pointer: None,
        });
        let _ = draw_frame(&mut app, 100, 24);
        // 待确认标记：按一次 x。
        handle_modal_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('x'),
        );
        let (_, row) = *app.modal_item_hits.last().expect("列表有命中行");
        handle_mouse_with_connection(&mut app, &connection, mouse_event(Moved, row.x, row.y));
        let Some(Modal::Jobs {
            selected,
            pending_kill,
            ..
        }) = &app.modal
        else {
            panic!("任务列表应保持打开")
        };
        assert_eq!(*selected, 0, "悬停不能改选中项");
        assert_eq!(*pending_kill, Some(0), "悬停不能撤掉待确认标记");
        for _ in 0..3 {
            handle_mouse_with_connection(
                &mut app,
                &connection,
                mouse_event(ScrollDown, row.x, row.y),
            );
        }
        let _ = draw_frame(&mut app, 100, 24);
        let Some(Modal::Jobs {
            selected, offset, ..
        }) = &app.modal
        else {
            panic!("任务列表应保持打开")
        };
        assert_eq!(*selected, 0, "滚轮不能改选中项");
        assert_eq!(*offset, 9, "滚轮推进列表视口");
        assert_eq!(app.modal_item_hits.first().unwrap().0, 9);
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 回退时间线：滚轮只滚列表 viewport，不改选中项；键盘导航重新显露。
    #[tokio::test]
    async fn rewind_wheel_scrolls_timeline_not_selection() {
        use crossterm::event::MouseEventKind::ScrollUp;
        let (core, connection, mut app) = setup_test_app().await;
        let entry = |index: usize| moc_protocol::methods::TranscriptEntry {
            id: format!("e{index}"),
            at_ms: index as u64,
            message: moc_protocol::Message::user_text(format!("提问 {index}")),
            tool: None,
            bash: None,
            agent: None,
            goal_round: None,
            reminder: None,
        };
        let mut state = crate::modal::RewindState::default();
        state.set_entries((0..30).map(entry).collect(), vec![]);
        let selected = state.selected;
        app.modal = Some(Modal::Rewind { state });
        let _ = draw_frame(&mut app, 120, 32);
        let timeline = app.rewind_timeline_area;
        assert!(!timeline.is_empty(), "时间线区域已登记");
        for _ in 0..3 {
            handle_mouse_with_connection(
                &mut app,
                &connection,
                mouse_event(ScrollUp, timeline.x + 1, timeline.y + 2),
            );
        }
        let _ = draw_frame(&mut app, 120, 32);
        let scrolled = match &app.modal {
            Some(Modal::Rewind { state }) => {
                assert_eq!(state.selected, selected, "滚轮不能改选中项");
                assert!(state.list_scroll > 0, "时间线视口已滚动");
                state.list_scroll
            }
            _ => panic!("回退视图应保持打开"),
        };
        // 普通重绘不把视口拉回选中项。
        let _ = draw_frame(&mut app, 120, 32);
        let Some(Modal::Rewind { state }) = &app.modal else {
            panic!("回退视图应保持打开")
        };
        assert_eq!(state.list_scroll, scrolled, "重绘不能拉回选中项");
        // 键盘导航重新显露选中项：视口向选中项回移。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Up);
        let _ = draw_frame(&mut app, 120, 32);
        let Some(Modal::Rewind { state }) = &app.modal else {
            panic!("回退视图应保持打开")
        };
        assert_eq!(state.selected, selected - 1);
        assert!(
            state.list_scroll > scrolled,
            "键盘导航把选中项收回窗内"
        );
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// workflow run 面板：滚轮滚正文 viewport，不改 focus；键盘移动焦点重新显露。
    #[tokio::test]
    async fn workflow_run_wheel_scrolls_body_not_focus() {
        use crossterm::event::MouseEventKind::ScrollDown;
        let (core, connection, mut app) = setup_test_app().await;
        let role = |index: usize| moc_protocol::methods::WorkflowRoleState {
            role_id: format!("role-{index}"),
            name: Some(format!("角色 {index}")),
            fan_out: false,
            status: "running".into(),
            model: None,
            asks_in_flight: 0,
            asks_done: 0,
            spent_tokens: 0,
        };
        let state = crate::workflow::WorkflowRunState {
            run_id: "run-1".into(),
            title: "run".into(),
            status: moc_protocol::methods::WorkflowRunStatus::Running,
            plan: serde_json::json!({"roles": []}),
            phases: vec![],
            roles: (0..8).map(role).collect(),
            commands: vec![],
            reports: vec![],
            artifacts: vec![],
            holes: vec![],
            usage: serde_json::json!({}),
            log: (0..80).map(|i| format!("log {i}")).collect(),
            artifact_versions: Default::default(),
            artifact_view: None,
            focus: 0,
            body_scroll: 0,
            list_scroll: 0,
            scroll: Default::default(),
            pointer: None,
        };
        app.modal = Some(Modal::WorkflowRun {
            state: Box::new(state),
        });
        let _ = draw_frame(&mut app, 100, 24);
        let (_, row) = *app.modal_item_hits.first().expect("role 行有命中");
        for _ in 0..3 {
            handle_mouse_with_connection(
                &mut app,
                &connection,
                mouse_event(ScrollDown, row.x, row.y),
            );
        }
        let _ = draw_frame(&mut app, 100, 24);
        let Some(Modal::WorkflowRun { state }) = &app.modal else {
            panic!("run 面板应保持打开")
        };
        assert_eq!(state.focus, 0, "滚轮不能改焦点");
        assert!(state.body_scroll > 0, "正文视口已滚动");
        // 键盘移动焦点重新显露焦点行。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        let _ = draw_frame(&mut app, 100, 24);
        let Some(Modal::WorkflowRun { state }) = &app.modal else {
            panic!("run 面板应保持打开")
        };
        assert_eq!(state.focus, 1);
        assert!(
            app.modal_item_hits.iter().any(|(index, _)| *index == 1),
            "键盘导航后焦点行在视口内"
        );
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 模型选择器：点详情卡思考等级行展开下拉；会话目标下点等级候选
    /// 立即发起切换（选择器保持打开、进入 loading）。
    #[tokio::test]
    async fn model_picker_mouse_selects_row_then_level_applies_inline() {
        let (core, connection, mut app) = setup_test_app().await;
        let entry = |selector: &str| crate::modal::ModelEntry {
            selector: selector.into(),
            label: selector.into(),
            display_name: String::new(),
            levels: vec!["off".into(), "low".into(), "high".into()],
            context_window: None,
            max_output_tokens: None,
            inputs: Vec::new(),
            pricing: None,
            fast_mode: false,
        };
        app.modal = Some(Modal::Model {
            entries: vec![entry("test/a"), entry("test/b")],
            current: "test/a".into(),
            picker: crate::modal::PickerState::default(),
            level: crate::modal::LevelState::default(),
            detail: Default::default(),
            target: crate::modal::ModelTarget::Session,
        });
        let picker = |app: &App| match app.modal.as_ref() {
            Some(Modal::Model { picker, level, .. }) => {
                (picker.selected, picker.loading, level.manual.clone())
            }
            _ => panic!("模型选择器应保持打开"),
        };

        let _ = draw_frame(&mut app, 140, 30);
        let (_, row_b) = app.modal_item_hits[1];
        // 点未选中的行：只选中，未调整的模型使用默认等级。
        handle_mouse_with_connection(&mut app, &connection, left_click(row_b.x + 3, row_b.y));
        assert_eq!(picker(&app), (1, false, None));

        // 点详情卡 Level 行：展开等级下拉。
        let _ = draw_frame(&mut app, 140, 30);
        let (_, level_row) = *app
            .modal_detail_hits
            .iter()
            .find(|(row, _)| *row == crate::modal::DetailRow::Level)
            .expect("普通模型详情卡有思考等级行");
        handle_mouse_with_connection(
            &mut app,
            &connection,
            left_click(level_row.x + 1, level_row.y),
        );
        let _ = draw_frame(&mut app, 140, 30);
        let (_, low) = app.modal_menu_hits[1];

        // 点下拉候选 "low"：记下等级并立即发起切换（会话目标内联生效）。
        handle_mouse_with_connection(&mut app, &connection, left_click(low.x, low.y));
        assert_eq!(picker(&app), (1, true, Some("low".into())));
        let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert_eq!(detail.pending.as_deref(), Some("test/b:low"));

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// popover 语义：点三块浮层之外一次即关并把点击交给被点位置；
    /// 点触发标签只收起；下拉打开时点列表行先收起下拉再执行行点击。
    #[tokio::test]
    async fn model_picker_outside_click_closes_once_and_falls_through() {
        let (core, connection, mut app) = setup_test_app().await;
        let entry = |selector: &str| crate::modal::ModelEntry {
            selector: selector.into(),
            label: selector.into(),
            display_name: String::new(),
            levels: vec!["off".into(), "low".into(), "high".into()],
            context_window: None,
            max_output_tokens: None,
            inputs: Vec::new(),
            pricing: None,
            fast_mode: false,
        };
        let open_modal = || Modal::Model {
            entries: vec![entry("test/a"), entry("test/b")],
            current: "test/a".into(),
            picker: crate::modal::PickerState::default(),
            level: crate::modal::LevelState::default(),
            detail: crate::modal::DetailState {
                focus: Some(crate::modal::DetailRow::Level),
                menu: Some(crate::modal::MenuState {
                    row: crate::modal::DetailRow::Level,
                    selected: 0,
                    offset: 0,
                    scroll: Default::default(),
                    query: String::new(),
                }),
                ..Default::default()
            },
            target: crate::modal::ModelTarget::Session,
        };
        // a) 详情焦点+下拉打开时，点浮层之外一次：选择器直接关闭。
        app.modal = Some(open_modal());
        let _ = draw_frame(&mut app, 140, 30);
        handle_mouse_with_connection(&mut app, &connection, left_click(5, 5));
        assert!(app.modal.is_none(), "点选择器外一次即关");

        // b) 点触发标签：收起且不会立刻重新打开。
        app.modal = Some(open_modal());
        let _ = draw_frame(&mut app, 140, 30);
        let tag = app.model_tag_area;
        assert!(!tag.is_empty(), "测试假定有触发标签");
        handle_mouse_with_connection(&mut app, &connection, left_click(tag.x, tag.y));
        assert!(app.modal.is_none(), "点触发标签只收起不 reopen");

        // c) 下拉打开时点另一个列表行：下拉收起、选中变成该行。
        app.modal = Some(open_modal());
        let _ = draw_frame(&mut app, 140, 30);
        let (_, row_b) = app.modal_item_hits[1];
        handle_mouse_with_connection(&mut app, &connection, left_click(row_b.x + 3, row_b.y));
        let Some(Modal::Model { picker, detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert!(detail.menu.is_none(), "点击后下拉已收起");
        assert_eq!(picker.selected, 1, "点击落到列表行");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 窄屏下详情卡盖在列表卡上：卡内非控制行的移动与点击不穿透到候选列表，
    /// 列表高亮与详情焦点都不变。
    #[tokio::test]
    async fn model_picker_overlay_detail_blocks_mouse_passthrough() {
        let (core, connection, mut app) = setup_test_app().await;
        let entry = |selector: &str| crate::modal::ModelEntry {
            selector: selector.into(),
            label: selector.into(),
            display_name: String::new(),
            levels: vec!["off".into(), "low".into(), "high".into()],
            context_window: None,
            max_output_tokens: None,
            inputs: Vec::new(),
            pricing: None,
            fast_mode: false,
        };
        app.modal = Some(Modal::Model {
            entries: vec![entry("test/a"), entry("test/b")],
            current: "test/a".into(),
            picker: crate::modal::PickerState::default(),
            level: crate::modal::LevelState::default(),
            detail: crate::modal::DetailState {
                focus: Some(crate::modal::DetailRow::Level),
                ..Default::default()
            },
            target: crate::modal::ModelTarget::Session,
        });
        // 60 列放不下两块浮层：详情卡覆盖在列表卡上（因 focus 而绘制）。
        let _ = draw_frame(&mut app, 60, 24);
        let detail_area = app.modal_detail_area;
        assert!(
            !detail_area.is_empty() && !detail_area.intersection(app.modal_card_area).is_empty(),
            "窄屏下详情卡应覆盖列表卡"
        );
        // 详情卡内容首行是模型名（非控制行）：移上去什么都不发生。
        let point = (detail_area.x + 2, detail_area.y + 1);
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(crossterm::event::MouseEventKind::Moved, point.0, point.1),
        );
        let Some(Modal::Model { picker, detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert_eq!(picker.selected, 0, "详情卡上的移动不改列表高亮");
        assert_eq!(
            detail.focus,
            Some(crate::modal::DetailRow::Level),
            "详情卡上的移动不丢焦点"
        );
        handle_mouse_with_connection(&mut app, &connection, left_click(point.0, point.1));
        let Some(Modal::Model { picker, detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert_eq!(picker.selected, 0, "详情卡上的点击不选中列表行");
        assert_eq!(
            detail.focus,
            Some(crate::modal::DetailRow::Level),
            "详情卡上的点击不丢焦点"
        );

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 表格入口：点中切换图表形态，再点还原；列在按钮外不触发。
    #[test]
    fn clicking_the_table_entry_toggles_the_chart() {
        use crate::app::ChartHit;
        use crate::transcript::Block;

        let mut app = app();
        let text =
            "| 项 | 耗时 | 内存 |\n|---|---|---|\n| a | 10 ms | 2 MiB |\n| b | 20 ms | 4 MiB |";
        app.push_block(Block::Assistant { text: text.into() });
        app.scrollback_area = ratatui::layout::Rect::new(3, 0, 80, 24);
        // 命中表由绘制填充；这里直接给出按钮所在行与列区间
        app.chart_hits = vec![ChartHit {
            entry: 0,
            table: 0,
            row: 12,
            x: 60,
            w: 12,
        }];

        let click = |app: &mut App, column: u16| {
            handle_mouse(
                app,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: 12,
                    modifiers: KeyModifiers::NONE,
                },
            );
            handle_mouse(
                app,
                MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column,
                    row: 12,
                    modifiers: KeyModifiers::NONE,
                },
            );
        };

        // 列落在按钮之外：既不切换，也不动档位
        let generation = app.blocks[0].generation;
        click(&mut app, 59);
        assert!(
            app.blocks[0].table_chart(0).is_none(),
            "按钮外点击不该展开图表"
        );
        assert_eq!(app.blocks[0].generation, generation);

        // 点中按钮：展开图表（渲染缓存失效）
        click(&mut app, 66);
        let state = app.blocks[0].table_chart(0).expect("应当展开图表");
        assert_eq!(state.image_id, 1, "首张图分配 1 号图像 ID");
        assert!(state.payload.is_none(), "载荷在贴图时才生成");
        assert!(app.blocks[0].generation > generation, "展开必须让缓存失效");

        // 再点一次：还原成表格
        click(&mut app, 66);
        assert!(app.blocks[0].table_chart(0).is_none(), "二次点击应还原");
    }

    #[tokio::test]
    async fn alt_m_cycles_the_session_engine() {
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
            ptc_worker: None,
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
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
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
                        omit_auxiliary: false,
                        session_id: session_id.clone(),
                        max_entries: None,
                        read_only: false,
                    })
                    .await
                    .unwrap()
                    .session;
                if session.engine == SessionEngine::Ptc {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("Alt+M 应切换到 PTC 引擎");

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
            ptc_worker: None,
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
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
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
        assert_eq!(queued[0].mode, Some(moc_protocol::methods::PromptMode::Ask));
        assert_eq!(queued[0].draft.text, "有没有遗留的临时文件");
        assert_eq!(
            app.session.as_ref().map(|session| session.engine),
            Some(SessionEngine::Standard),
            "临时问答不得改写会话引擎"
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

    /// 点图例弹出就地菜单：悬停高亮、点条目切换引擎、再点图例关闭，键位同样可用。
    #[tokio::test]
    async fn engine_menu_switches_the_session_engine() {
        use moc_core::{Core, CoreConfig};

        async fn wait_for_engine(
            connection: &EmbeddedConnection,
            session_id: &str,
            expected: SessionEngine,
        ) {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let session = connection
                        .client
                        .call::<SessionLoad>(SessionLoadParams {
                            omit_auxiliary: false,
                            session_id: session_id.to_owned(),
                            max_entries: None,
                            read_only: false,
                        })
                        .await
                        .unwrap()
                        .session;
                    if session.engine == expected {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("引擎应切换为期望值");
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
            ptc_worker: None,
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
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .unwrap();
        let session_id = session.id.clone();
        app.session = Some(session);

        // 图例命中区与菜单区域由绘制回填；这里摆出与渲染一致的坐标。
        app.engine_area = ratatui::layout::Rect::new(60, 20, 4, 1);
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
        assert!(app.engine_menu.is_some(), "点模式图例应弹出菜单");
        assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(0));

        // 悬停在条目上即高亮该条，移出菜单不改选择。
        app.engine_menu.as_mut().unwrap().area = menu_area;
        handle_mouse_with_connection(&mut app, &connection, mouse(MouseEventKind::Moved, 45, 17));
        assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));
        handle_mouse_with_connection(&mut app, &connection, mouse(MouseEventKind::Moved, 10, 5));
        assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));

        // 菜单内滚轮与 ↑↓ 同语义循环换高亮；菜单外照旧穿透、菜单不关。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::ScrollDown, 45, 16),
        );
        assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(0));
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::ScrollUp, 45, 16),
        );
        assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::ScrollDown, 10, 5),
        );
        assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));
        assert!(app.engine_menu.is_some(), "菜单外滚轮不关菜单");

        // 点第二条（ptc）直达该引擎。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 45, 16),
        );
        assert!(app.engine_menu.is_none(), "选中条目后菜单关闭");
        wait_for_engine(&connection, &session_id, SessionEngine::Ptc).await;
        // core 通过 `EngineChanged` 推送新引擎，事件循环据此更新本地会话；此处手动对齐。
        app.session.as_mut().unwrap().engine = SessionEngine::Ptc;

        // 再点图例开菜单，再点一次图例应关闭而不是重开。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
        );
        assert!(app.engine_menu.is_some());
        assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(1));
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
        );
        assert!(app.engine_menu.is_none(), "再次点图例关闭菜单");

        // 键盘路径：开菜单后 ↓ 选到 clm，Enter 应用。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
        );
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        assert!(app.engine_menu.is_none());
        wait_for_engine(&connection, &session_id, SessionEngine::ClmExperimental).await;

        // 普通输入先关掉菜单再照常处理：菜单不会吞键。
        app.session.as_mut().unwrap().engine = SessionEngine::Ptc;
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
        );
        assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(1));
        let revision = app.prompt_revision;
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('h'),
        );
        assert!(app.engine_menu.is_none(), "输入字符只关菜单");
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

    /// 右缘滚动条：按在滑块上保持抓取点拖动，按在轨道空白处把滑块中线挪过来；
    /// 命中滚动条的按下与拖动不进入文本选区。
    #[test]
    fn scrollbar_click_and_drag_move_the_viewport() {
        use crate::app::ScrollbackScrollbar;
        use ratatui::layout::Rect;

        fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
            MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }
        }

        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        // 终端 80 列、轨道占滚动区 4..14 行：右缘 79 列，10 行里滑块 3 行。
        app.scrollbar = ScrollbackScrollbar {
            track: Rect::new(79, 4, 1, 10),
            thumb: 3,
            max_offset: 20,
        };
        app.scroll_max_offset = 20;

        // 跟随尾部时滑块贴底（第 11 行起）：按在它上面不跳动，只进入拖拽。
        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 79, 11),
        );
        assert_eq!(app.scroll_offset, 0, "按在滑块上不改变偏移");
        assert!(app.scrollbar_drag.is_some());
        assert!(app.pending_click.is_none(), "滚动条按下不进入文本选区");

        // 拖到轨道顶端：走满历史。
        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), 79, 4),
        );
        assert_eq!(app.scroll_offset, 20);
        assert!(app.selection.is_none(), "拖动滚动条不产生选区");
        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), 79, 4),
        );
        assert!(app.scrollbar_drag.is_none(), "抬起后结束拖拽");

        // 按在轨道空白处：滑块中线（3/2 行）挪到光标所在行。
        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 79, 8),
        );
        assert_eq!(app.scroll_offset, 12);
        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), 79, 8),
        );

        // 轨道之外的按下仍走普通点击路径。
        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 40, 8),
        );
        assert!(app.pending_click.is_some());

        // 内容不溢出（没有画过滚动条）时，右缘不构成命中区。
        app.scrollbar = ScrollbackScrollbar::default();
        app.pending_click = None;
        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 79, 8),
        );
        assert!(app.scrollbar_drag.is_none());
        assert!(app.pending_click.is_some(), "没有滚动条时右缘仍是普通点击");
    }

    #[tokio::test]
    async fn ctrl_c_clears_input_when_nonempty_within_three_seconds() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_text("hello moc");
        assert_eq!(app.prompt_text(), "hello moc");

        // 第一次 Ctrl+C：提示 "再次按 Ctrl+C 清空输入"，不应清空
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.prompt_text(), "hello moc");
        assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 清空输入"));
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
        assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 退出"));
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
        assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 清空输入"));

        // 模拟超过 3 秒
        if let Some((_, at)) = app.ctrl_c_pending.as_mut() {
            *at -= std::time::Duration::from_secs(4);
        }

        // 再次按 Ctrl+C：超时，不应清空，应重新触发第一次提示
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.prompt_text(), "some text");
        assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 清空输入"));
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
        assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 退出"));
        assert!(!app.should_quit);

        // 模拟超过 3 秒
        if let Some((_, at)) = app.ctrl_c_pending.as_mut() {
            *at -= std::time::Duration::from_secs(4);
        }

        // 再次按 Ctrl+C：超时，不应退出
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert!(!app.should_quit);
        assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 退出"));

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
        assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 清空输入"));

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// C5：Ctrl+C 清空输入后补全菜单立即失效，Enter/Tab 不得按陈旧区间替换空串。
    #[tokio::test]
    async fn ctrl_c_clear_invalidates_completion_menu() {
        let (core, connection, mut app) = setup_test_app().await;
        for ch in "/mo".chars() {
            handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
        }
        assert!(app.completion.is_some(), "输入 /mo 应打开补全菜单");

        // 连按两次 Ctrl+C：第一次提示，第二次清空。
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.prompt_text(), "/mo", "首次 Ctrl+C 只提示");
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.prompt_text(), "");
        assert!(app.completion.is_none(), "清空后补全菜单必须失效");

        // 空串上按 Enter/Tab：既不能越界替换，也不能吞掉按键语义。
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        assert_eq!(app.focus, Focus::Prompt, "空输入 Enter 无操作");
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
        assert_eq!(app.focus, Focus::Scrollback, "Tab 仍按焦点切换处理");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// C5：双 Esc 清空输入后同样不得留下补全菜单。
    #[tokio::test]
    async fn esc_clear_invalidates_completion_menu() {
        let (core, connection, mut app) = setup_test_app().await;
        for ch in "/re".chars() {
            handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
        }
        assert!(app.completion.is_some());

        // 首次 Esc 只关菜单，第二次进入清空确认，第三次清空。
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        assert!(app.completion.is_none());
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        assert_eq!(app.prompt_text(), "/re");
        assert_eq!(app.notice.as_deref(), Some("再次按 Esc 清空输入"));
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        assert_eq!(app.prompt_text(), "");
        assert!(app.completion.is_none(), "清空后补全菜单必须失效");

        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        assert_eq!(app.prompt_text(), "", "空输入 Enter 无操作");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// M16：没有选区时小写 `y` 照常输入；有选区时才走复制快捷键。
    #[tokio::test]
    async fn lowercase_y_inserts_without_selection() {
        let (core, connection, mut app) = setup_test_app().await;
        for ch in "yes".chars() {
            handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
        }
        assert_eq!(app.prompt_text(), "yes", "无选区时 y 必须进入输入框");

        // 拖选后 y 仍是复制键：内容不进输入框（选区正文由 paint_lines 提供）。
        app.prompt.clear();
        app.prompt_changed();
        app.selection = Some(((4, 2), (9, 2)));
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('y'),
        );
        assert_eq!(app.prompt_text(), "", "有选区时 y 不进入输入框");
        assert!(app.selection.is_some(), "空选区内容不清空选区");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 拖选复制按显示列切片：端点格子包含在内，跨行时首尾行裁边、中间整行。
    #[test]
    fn selection_text_slices_columns_inclusively() {
        let mut app = app();
        app.scrollback_area = ratatui::layout::Rect::new(2, 1, 20, 4);
        app.paint_lines = vec![
            (1, "abcdef".into()),
            (2, "123456".into()),
            (3, "ABCDEF".into()),
        ];

        app.selection = Some(((4, 1), (6, 3)));
        assert_eq!(selection_text(&app), "cdef\n123456\nABCDE");
        app.selection = Some(((6, 3), (4, 1)));
        assert_eq!(
            selection_text(&app),
            "cdef\n123456\nABCDE",
            "反向拖选结果一致"
        );

        // 宽字符按显示列整字切，不劈开半个字。
        app.paint_lines = vec![(1, "中文字".into())];
        app.selection = Some(((2, 1), (5, 1)));
        assert_eq!(selection_text(&app), "中文");
        app.selection = Some(((2, 1), (7, 1)));
        assert_eq!(selection_text(&app), "中文字");
    }

    /// 原生语义：有选区时 Ctrl+C 复制选区，而不是清空输入或取消。
    #[tokio::test]
    async fn ctrl_c_copies_selection_when_present() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_text("pending text");
        app.scrollback_area = ratatui::layout::Rect::new(0, 1, 20, 4);
        app.paint_lines = vec![(1, "copy me".into())];
        app.selection = Some(((0, 1), (6, 1)));

        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.prompt_text(), "pending text", "复制优先于清空输入");
        assert!(app.selection.is_none());
        assert_eq!(app.notice.as_deref(), Some("已复制选中内容"));
        assert!(!app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

        // 没有选区时恢复 Ctrl+C 原语义：第一次只提示清空输入。
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 清空输入"));

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 右键复制：有选区时与 Windows Terminal 一样直接落剪贴板并清掉选区。
    #[tokio::test]
    async fn right_click_copies_selection() {
        let mut app = app();
        app.scrollback_area = ratatui::layout::Rect::new(0, 1, 20, 4);
        app.paint_lines = vec![(1, "copy me".into())];
        app.selection = Some(((0, 1), (6, 1)));
        let mouse = |kind| MouseEvent {
            kind,
            column: 3,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };

        handle_mouse(&mut app, mouse(MouseEventKind::Up(MouseButton::Right)));
        assert!(app.selection.is_none());
        assert_eq!(app.notice.as_deref(), Some("已复制选中内容"));
    }

    /// M17+M18：运行中切会话后，旧会话的更新不得写进新会话，收尾仍回收在飞记账。
    ///
    /// 不按会话/代理路由时，旧会话的 delta 直接落进新会话转写、PromptStarted 复位
    /// 新会话状态、工具块与耗时回填全都串台。
    #[tokio::test]
    async fn foreign_session_updates_are_dropped_and_recycled() {
        use moc_protocol::methods::{AssistantDelta, SessionUpdate};
        use moc_protocol::{ContentPart, FinishReason, Message, Role, Usage};

        let (core, connection, mut app) = setup_test_app().await;
        let old_session = app.session.as_ref().expect("测试会话").id.clone();
        // 旧会话的提示仍在飞，前端运行中切到另一个会话。
        app.inflight_prompts.insert(old_session.clone());
        app.interrupt_pending.insert(old_session.clone());
        let mut other = app.session.clone().expect("测试会话");
        other.id = "ses_other".into();
        app.switch_session(other, Vec::new());
        assert!(!app.prompt_running);

        // 旧会话开始的推送：不得把新会话置为运行中
        apply_update_and_refresh(
            &mut app,
            &connection,
            old_session.clone(),
            SessionUpdate::PromptStarted {
                prompt_id: "prompt_old".into(),
                model: "test/model".into(),
                at_ms: 0,
            },
        );
        assert!(!app.prompt_running, "旧会话的 PromptStarted 不得复位新会话");

        // 旧会话的正文增量：不得进入当前转写
        apply_update_and_refresh(
            &mut app,
            &connection,
            old_session.clone(),
            SessionUpdate::AssistantDelta {
                prompt_id: "prompt_old".into(),
                delta: AssistantDelta::Text {
                    delta: "旧会话内容".into(),
                },
            },
        );
        assert!(app.blocks.is_empty(), "旧会话增量不得写入当前转写");

        // 旧会话收尾：只回收本前端的在飞记账，仍不改当前会话状态
        apply_update_and_refresh(
            &mut app,
            &connection,
            old_session.clone(),
            SessionUpdate::PromptFinished {
                prompt_id: "prompt_old".into(),
                message: Message {
                    role: Role::Assistant,
                    content: moc_protocol::message_content![ContentPart::Text("旧回复".into())],
                },
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            },
        );
        assert!(
            !app.inflight_prompts.contains(&old_session),
            "旧会话收尾应回收在飞记账"
        );
        assert!(!app.interrupt_pending.contains(&old_session));
        assert!(app.blocks.is_empty(), "旧会话内容不得落到新会话转写");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 思考耗时按「一段思考」定稿：工具调用与下一轮生成都必须先收掉上一块，
    /// 否则工具循环里除最后一段之外全是没测到的 0。
    #[test]
    fn thinking_duration_is_stamped_per_response() {
        use crate::transcript::Block;
        use moc_protocol::methods::{AssistantDelta, SessionUpdate};
        use std::time::{Duration, Instant};

        let mut app = app();
        let duration_of = |app: &App, index: usize| match &app.blocks[index].block {
            Block::Thinking { duration_ms, .. } => *duration_ms,
            _ => panic!("下标 {index} 不是思考块"),
        };

        // 第一响应：思考后直接调工具。
        app.start_thinking();
        app.thinking_started_at = Some(Instant::now() - Duration::from_millis(1_200));
        apply_update(
            &mut app,
            SessionUpdate::ToolStarted {
                prompt_id: "prompt_1".into(),
                tool_call_id: "call_1".into(),
                name: "read".into(),
                parent_tool_call_id: None,
                arguments: "{}".into(),
            },
        );
        assert!(
            duration_of(&app, 0).is_some_and(|ms| ms >= 1_000),
            "工具开始即定稿第一段思考"
        );

        // 第二响应：生成开始后另起一块，正文到达时定稿。
        apply_update(
            &mut app,
            SessionUpdate::GenerationStarted {
                prompt_id: "prompt_1".into(),
            },
        );
        apply_update(
            &mut app,
            SessionUpdate::AssistantDelta {
                prompt_id: "prompt_1".into(),
                delta: AssistantDelta::Reasoning {
                    delta: "再想".into(),
                },
            },
        );
        assert_eq!(app.blocks.len(), 3, "新响应另起思考块");
        app.thinking_started_at = Some(Instant::now() - Duration::from_millis(900));
        apply_update(
            &mut app,
            SessionUpdate::AssistantDelta {
                prompt_id: "prompt_1".into(),
                delta: AssistantDelta::Text {
                    delta: "答案".into(),
                },
            },
        );
        assert!(
            duration_of(&app, 2).is_some_and(|ms| ms >= 800),
            "正文开始即定稿第二段思考"
        );
    }

    #[tokio::test]
    async fn interrupt_submit_still_cancels_immediately() {
        let (core, connection, mut app) = setup_test_app().await;
        let session = app.session.as_ref().expect("测试会话").id.clone();
        app.preferences.active_submit = crate::config::ActiveSubmit::Interrupt;
        app.prompt_running = true;
        app.prompt.insert_text("中断消息");
        crate::queue::submit(&mut app, &connection, false);
        assert_eq!(app.queues[&session].len(), 1);
        assert!(app.interrupt_pending.contains(&session));
        assert!(app.cancel_requested);
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 普通提示（非 `/` 命令）发送后必须能按 ↑ 召回，不能只有斜杠命令进历史。
    #[tokio::test]
    async fn submitted_prompt_enters_input_history() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt_running = true;
        app.prompt.insert_str_raw("继续");
        crate::queue::submit(&mut app, &connection, false);
        assert!(app.prompt.is_empty(), "提交后输入框清空");
        assert!(app.prompt.history_up(), "刚发送的提示应在历史里");
        assert_eq!(app.prompt.text, "继续");
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 提交历史会落盘；重新挂载（等价重启）后仍能召回。
    #[tokio::test]
    async fn submitted_history_survives_restart() {
        let (core, connection, mut app) = setup_test_app().await;
        let home = core.config_home();
        app.load_input_history(&home);
        app.prompt.insert_str_raw("继续");
        crate::queue::submit(&mut app, &connection, false);
        assert!(app.prompt.is_empty());

        let mut restarted = App::new(Theme::global(), app.events.clone());
        restarted.load_input_history(&home);
        assert!(restarted.prompt.history_up(), "重启后仍应召回上次的提示");
        assert_eq!(restarted.prompt.text, "继续");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 「按项目」范围只召回当前会话目录的提示。
    #[tokio::test]
    async fn project_history_scope_filters_by_session_cwd() {
        let (core, connection, mut app) = setup_test_app().await;
        let home = core.config_home();
        let mut history = crate::history::InputHistory::load(&home);
        history.record("A 项目的提示", "/project/a");
        history.record("B 项目的提示", "/project/b");
        app.history = history;
        app.preferences.history_scope = crate::history::HistoryScope::Project;
        app.session.as_mut().expect("测试会话").cwd = "/project/a".into();
        app.refresh_input_history();

        assert!(app.prompt.history_up());
        assert_eq!(app.prompt.text, "A 项目的提示");
        assert!(!app.prompt.history_up(), "只允许当前项目的条目");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 没有会话时提交不能把草稿吃掉。
    #[tokio::test]
    async fn submitting_without_session_keeps_the_draft() {
        let (core, connection, mut app) = setup_test_app().await;
        app.session = None;
        app.prompt.insert_str_raw("别丢了我");
        crate::queue::submit(&mut app, &connection, false);
        assert_eq!(app.prompt.text, "别丢了我", "被拒绝的草稿要回到输入框");
        assert_eq!(
            app.notice.as_deref(),
            Some("请先用 /model 选择模型；没有候选时到 /config 添加供应商")
        );
        connection.shutdown().await;
        core.shutdown().await;
    }

    #[tokio::test]
    async fn slash_paths_submit_as_normal_messages() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt_running = true;
        let session = app.session.as_ref().unwrap().id.clone();
        for text in [
            "/tmp/a.txt",
            "/tmp",
            "/goal/file.txt",
            "/tmp/my file.txt 请检查",
            "/unknown-command",
        ] {
            for paste in [false, true] {
                if paste {
                    app.prompt.insert_text(text);
                } else {
                    for ch in text.chars() {
                        app.prompt.insert_char(ch);
                    }
                }
                commit_prompt(&mut app, &connection);
                assert_eq!(app.steers[&session].back().unwrap().draft.text, text);
                assert!(app.prompt.is_empty(), "提交后清空输入框");
                assert!(
                    app.notice
                        .as_deref()
                        .is_none_or(|text| !text.starts_with("未知命令"))
                );
            }
        }
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 斜杠命令不吃附件：带图片时拒绝执行并原样保留输入，删掉占位后同一条命令可执行。
    #[tokio::test]
    async fn slash_command_with_image_keeps_input() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_str_raw("/help ");
        app.prompt.insert_image("image/png".into(), "AAAA".into());
        commit_prompt(&mut app, &connection);
        assert!(app.modal.is_none(), "带图片时不得执行命令");
        assert_eq!(
            app.notice.as_deref(),
            Some("斜杠命令不支持图片，请移除图片后执行")
        );
        assert_eq!(app.prompt.text, "/help [image#1]", "输入原样保留");

        app.prompt.backspace();
        commit_prompt(&mut app, &connection);
        assert!(matches!(app.modal, Some(Modal::Help { .. })));
        assert!(app.prompt.is_empty());
        connection.shutdown().await;
        core.shutdown().await;
    }

    #[tokio::test]
    async fn steer_returned_keeps_message_pending_until_applied() {
        use crate::transcript::Block;

        let (core, connection, mut app) = setup_test_app().await;
        let session = app.session.as_ref().expect("测试会话").id.clone();
        app.steers
            .entry(session.clone())
            .or_default()
            .push_back(crate::queue::PendingSteer {
                id: 1,
                draft: crate::widgets::prompt::PromptState::from_text("先保留原文件".into()),
                display_text: "先保留原文件".into(),
                echo_text: "先保留原文件".into(),
                core_id: None,
                withdrawal: None,
            });
        crate::interaction::handle_event(
            &mut app,
            &connection,
            AppEvent::SteerReturned {
                session: session.clone(),
                prompt_id: 1,
                result: Ok(moc_protocol::SteerResult {
                    accepted: true,
                    id: Some(5),
                }),
            },
        );
        assert!(
            app.blocks.is_empty(),
            "accepted 不等于已写入，不能提前写转写"
        );
        assert_eq!(app.steers[&session][0].core_id, Some(5));
        apply_update_and_refresh(
            &mut app,
            &connection,
            session.clone(),
            SessionUpdate::SteeringApplied {
                id: 5,
                text: "先保留原文件".into(),
            },
        );
        assert!(app.steers[&session].is_empty());
        assert!(matches!(
            &app.blocks[0].block,
            Block::User { text } if text == "先保留原文件"
        ));
        connection.shutdown().await;
        core.shutdown().await;
    }

    #[tokio::test]
    async fn withdrawn_steer_edit_restores_draft() {
        let (core, connection, mut app) = setup_test_app().await;
        let session = app.session.as_ref().expect("测试会话").id.clone();
        app.steers
            .entry(session.clone())
            .or_default()
            .push_back(crate::queue::PendingSteer {
                id: 1,
                draft: crate::widgets::prompt::PromptState::from_text("撤回的草稿".into()),
                display_text: "撤回的草稿".into(),
                echo_text: "撤回的草稿".into(),
                core_id: Some(5),
                withdrawal: Some(crate::queue::WithdrawAction::Edit),
            });
        crate::interaction::handle_event(
            &mut app,
            &connection,
            AppEvent::UnsteerReturned {
                session: session.clone(),
                prompt_id: 1,
                action: crate::queue::WithdrawAction::Edit,
                result: Ok(true),
            },
        );
        assert!(app.steers[&session].is_empty());
        assert_eq!(app.prompt_text(), "撤回的草稿");
        connection.shutdown().await;
        core.shutdown().await;
    }

    #[tokio::test]
    async fn pending_steer_blocks_dispatch_until_steering_applied() {
        use crate::transcript::Block;

        let (core, connection, mut app) = setup_test_app().await;
        let session = app.session.as_ref().expect("测试会话").id.clone();
        app.steers
            .entry(session.clone())
            .or_default()
            .push_back(crate::queue::PendingSteer {
                id: 1,
                draft: crate::widgets::prompt::PromptState::from_text("补充内容".into()),
                display_text: "补充内容".into(),
                echo_text: "补充内容".into(),
                core_id: Some(5),
                withdrawal: None,
            });
        app.queues
            .entry(session.clone())
            .or_default()
            .push_back(crate::queue::QueuedPrompt {
                id: 2,
                draft: crate::widgets::prompt::PromptState::from_text("排队内容".into()),
                mode: None,
            });
        crate::queue::dispatch(&mut app, &connection);
        assert_eq!(
            app.queues[&session].len(),
            1,
            "pending steering 必须挡住下一条提示"
        );
        apply_update_and_refresh(
            &mut app,
            &connection,
            session.clone(),
            SessionUpdate::SteeringApplied {
                id: 5,
                text: "补充内容".into(),
            },
        );
        let users: Vec<&str> = app
            .blocks
            .iter()
            .filter_map(|entry| match &entry.block {
                Block::User { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            users,
            vec!["补充内容", "排队内容"],
            "用户块必须按注入顺序落地"
        );
        assert!(app.queues[&session].is_empty());
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 小9：多问时自定义答案各归各问，第 1 问的输入不会挂到最后一问。
    #[tokio::test]
    async fn ask_custom_answers_stay_with_their_question() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(
            vec![ask_question("q1"), ask_question("q2")],
            Some(reply_tx),
        ));

        // 第 1 问：z 进自定义、输入「甲」、Enter 推进到第 2 问
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('z'),
        );
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('甲'),
        );
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        assert_eq!(ask_custom(&app), (String::new(), 0), "第 2 问的缓冲是空的");
        match app.modal.as_ref() {
            Some(Modal::Ask {
                custom, question, ..
            }) => {
                assert_eq!(*question, 1, "已推进到第 2 问");
                assert_eq!(custom[0].text, "甲", "第 1 问的答案留在第 1 问");
                assert!(custom[1].text.is_empty());
            }
            _ => unreachable!("ask 模态"),
        }

        // 第 2 问不做自定义，末问 enter 进入确认页，确认页 enter 提交：答案必须各归各问
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char(' '),
        );
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        match app.modal.as_ref() {
            Some(Modal::Ask { question, .. }) => {
                assert_eq!(*question, 2, "越过最后一问应落在确认页")
            }
            _ => unreachable!("确认页上模态应保留"),
        }
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let result = reply_rx
            .try_recv()
            .expect("确认页提交后应回报")
            .expect("提问结果");
        assert_eq!(result.answers.len(), 2);
        assert_eq!(result.answers[0].custom.as_deref(), Some("甲"));
        assert_eq!(
            result.answers[1].custom, None,
            "第 2 问不得带上第 1 问的答案"
        );
        assert_eq!(result.answers[1].selected, vec!["选项一".to_owned()]);
        assert!(app.modal.is_none(), "提交后模态关闭");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 补全表里的每条命令都必须有实际处理分支：输入后不得回落到「未知命令」。
    ///
    /// 补全表、帮助与实际处理三者漂移过一次（`/context`、`/quit` 曾在表里但没有分支），
    /// 这条用例把「表 ⊆ 处理」钉住。
    #[tokio::test]
    async fn every_slash_command_has_an_execution_branch() {
        let (core, connection, mut app) = setup_test_app().await;
        for spec in crate::commands::COMMANDS {
            // 逐条重置：模态、设置页、提示、退出标记与忙标记都不该影响下一条
            app.modal = None;
            app.settings = None;
            app.notice = None;
            app.should_quit = false;
            app.inflight_session_ops.clear();

            execute_command(&mut app, &connection, format!("/{}", spec.name));
            let notice = app.notice.clone().unwrap_or_default();
            assert!(
                !notice.starts_with("未知命令"),
                "/{} 没有处理分支（提示：{notice}）",
                spec.name
            );
            if spec.name == "quit" {
                assert!(app.should_quit, "/quit 应走既有退出路径");
            }
        }

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// `/retry` 只重发上下文：不往转写里塞用户块，但占用提示通道。
    #[tokio::test]
    async fn retry_starts_a_prompt_without_adding_a_user_block() {
        let (core, connection, mut app) = setup_test_app().await;
        let session_id = app.session.as_ref().unwrap().id.clone();
        let user_blocks = app
            .blocks
            .iter()
            .filter(|entry| matches!(entry.block, crate::transcript::Block::User { .. }))
            .count();

        execute_command(&mut app, &connection, "/retry".to_owned());

        assert_eq!(
            app.blocks
                .iter()
                .filter(|entry| matches!(entry.block, crate::transcript::Block::User { .. }))
                .count(),
            user_blocks,
            "重试不得追加用户消息"
        );
        assert!(app.prompt_running, "重试应占用提示通道");
        assert!(app.inflight_prompts.contains(&session_id));

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// core 剪掉失败回合后会回报 `prompt_rewound`：界面那段预览跟着消失。
    #[test]
    fn prompt_rewound_drops_the_trimmed_preview() {
        use crate::transcript::{Block, ToolCallFinish, ToolCallStart};
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.push_block(Block::Assistant {
            text: "半截正文".into(),
        });
        app.start_tool_block(ToolCallStart {
            id: "call_1".into(),
            name: "read".into(),
            parent_id: None,
            arguments: "{}".into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_1".into(),
            name: "read".into(),
            parent_id: None,
            arguments: "{}".into(),
            status: ToolStatus::Cancelled,
            result: ToolResult {
                tool_call_id: "call_1".into(),
                content: String::new(),
                is_error: true,
                images: Vec::new(),
            },
            duration_ms: None,
            diff: None,
        });

        apply_update(
            &mut app,
            SessionUpdate::PromptRewound {
                replayed_tools: false,
            },
        );

        assert!(app.blocks.is_empty(), "失败回合的预览与占位工具块都要撤掉");
    }

    /// 生成中不能重试：原地提示，不占通道。
    #[tokio::test]
    async fn retry_is_rejected_while_a_prompt_is_running() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt_running = true;

        execute_command(&mut app, &connection, "/retry".to_owned());

        assert_eq!(
            app.notice.as_deref(),
            Some("正在运行中，暂时不能执行 /retry")
        );
        assert!(app.inflight_prompts.is_empty(), "被拒的重试不得占通道");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// `.` 独占整条输入时是 `/retry` 的快捷写法：走同一分派，不追加用户块。
    #[tokio::test]
    async fn lone_dot_dispatches_retry() {
        let (core, connection, mut app) = setup_test_app().await;
        let session_id = app.session.as_ref().unwrap().id.clone();
        app.prompt.insert_str_raw(".");

        commit_prompt(&mut app, &connection);

        assert!(app.prompt.is_empty(), "`. ` 提交后输入框应清空");
        assert!(app.prompt_running, "`. ` 应占用提示通道");
        assert!(app.inflight_prompts.contains(&session_id));
        assert!(
            !app.blocks
                .iter()
                .any(|entry| matches!(entry.block, crate::transcript::Block::User { .. })),
            "`. ` 不得追加用户消息"
        );

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// `.` 只在独占整条输入时触发：后面还有内容按普通消息发送。
    #[tokio::test]
    async fn dot_with_more_text_sends_a_normal_prompt() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt.insert_str_raw(".tail");

        commit_prompt(&mut app, &connection);

        assert!(
            app.blocks
                .iter()
                .any(|entry| matches!(entry.block, crate::transcript::Block::User { .. })),
            "`.tail` 应作为普通消息发出"
        );

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// `.` 在生成中同样被 `/retry` 的运行中门控拦下。
    #[tokio::test]
    async fn lone_dot_is_rejected_while_running() {
        let (core, connection, mut app) = setup_test_app().await;
        app.prompt_running = true;
        app.prompt.insert_str_raw(".");

        commit_prompt(&mut app, &connection);

        assert_eq!(
            app.notice.as_deref(),
            Some("正在运行中，暂时不能执行 /retry")
        );
        assert!(app.inflight_prompts.is_empty(), "被拒的 `. ` 不得占通道");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 多选按空格切换多个选项，取消后保留顺序并提交所选标签。
    #[tokio::test]
    async fn ask_multi_select_toggles_multiple_options() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(vec![multi_ask_question()], Some(reply_tx)));

        for key in [
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Char(' '),
        ] {
            handle_key(&mut app, &connection, KeyModifiers::NONE, key);
        }

        match app.modal.as_ref() {
            Some(Modal::Ask { selected, .. }) => assert_eq!(
                selected[0],
                vec!["缓存".to_owned(), "日志".to_owned()],
                "取消最后一项后保留之前的选择顺序"
            ),
            _ => unreachable!("ask 模态应保持打开"),
        }

        // 末问 enter 进入确认页，确认页 enter 才提交。
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        assert!(reply_rx.try_recv().is_err(), "进确认页本身不提交");
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let result = reply_rx
            .try_recv()
            .expect("确认后应收到 ask 回执")
            .expect("提问结果");
        assert_eq!(result.answers[0].selected, ["缓存", "日志"]);
        assert!(app.modal.is_none(), "提交后模态关闭");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 多选 Enter = 空格：非末问只切换聚焦项不跳问；末问 Enter 是通往
    /// 确认页的手势（不切勾），空集合按未回答提交。
    #[tokio::test]
    async fn ask_multi_enter_toggles_and_confirms_only_on_last() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        let mut second = multi_ask_question();
        second.id = "features2".into();
        app.modal = Some(ask_modal_with(
            vec![multi_ask_question(), second],
            Some(reply_tx),
        ));

        // 第 1 问非末问：Enter 切勾聚焦项「缓存」，不跳问。
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        match app.modal.as_ref() {
            Some(Modal::Ask {
                selected, question, ..
            }) => {
                assert_eq!(selected[0], ["缓存"], "Enter 与空格同为切换手势");
                assert_eq!(*question, 0, "非末问 Enter 不跳问");
            }
            _ => unreachable!("ask 模态应保持打开"),
        }

        // tab 换到第 2 问；末问 Enter 进确认页，不切勾第 2 问的聚焦项。
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        assert!(
            matches!(app.modal.as_ref(), Some(Modal::Ask { question, .. }) if *question == 2),
            "末问 Enter 应落在确认页"
        );
        match app.modal.as_ref() {
            Some(Modal::Ask { selected, .. }) => {
                assert!(selected[1].is_empty(), "末问 Enter 不做切换")
            }
            _ => unreachable!("确认页上模态应保留"),
        }

        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let result = reply_rx
            .try_recv()
            .expect("确认页 Enter 应提交")
            .expect("提问结果");
        assert_eq!(result.answers[0].selected, ["缓存"]);
        assert!(
            result.answers[1].selected.is_empty() && result.answers[1].custom.is_none(),
            "末问未勾选按未回答提交"
        );

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// tab 只在问题间循环：末问 tab 回第一问，永不落确认页；
    /// shift+tab 反向循环：第一问回末问。
    #[tokio::test]
    async fn ask_tab_cycles_questions_and_never_reaches_confirm() {
        let (core, connection, mut app) = setup_test_app().await;
        app.modal = Some(ask_modal_with(
            vec![ask_question("q1"), ask_question("q2")],
            None,
        ));
        let question_of = |app: &App| match app.modal.as_ref() {
            Some(Modal::Ask { question, .. }) => *question,
            _ => unreachable!("ask 模态"),
        };

        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
        assert_eq!(question_of(&app), 1, "tab 到下一问");
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
        assert_eq!(question_of(&app), 0, "末问 tab 绕回第一问，不进确认页");
        handle_key(&mut app, &connection, KeyModifiers::SHIFT, KeyCode::BackTab);
        assert_eq!(question_of(&app), 1, "第一问 shift+tab 反向绕回末问");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 数字直达：单选选中即推进下一问；多选只切换勾选，不推进。
    #[tokio::test]
    async fn ask_digit_shortcuts_pick_and_toggle() {
        let (core, connection, mut app) = setup_test_app().await;
        let single = AskQuestion {
            id: "single".into(),
            question: "单选哪个？".into(),
            header: None,
            multi: false,
            recommended: Vec::new(),
            options: vec![
                AskOption {
                    label: "甲".into(),
                    description: None,
                    preview: None,
                },
                AskOption {
                    label: "乙".into(),
                    description: None,
                    preview: None,
                },
            ],
        };
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(
            vec![single, multi_ask_question()],
            Some(reply_tx),
        ));

        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('2'),
        );
        match app.modal.as_ref() {
            Some(Modal::Ask {
                question, selected, ..
            }) => {
                assert_eq!(*question, 1, "单选数字直达后推进到下一问");
                assert_eq!(selected[0], vec!["乙".to_owned()]);
            }
            _ => unreachable!("还有未答的问题，模态应保持打开"),
        }

        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('1'),
        );
        match app.modal.as_ref() {
            Some(Modal::Ask {
                question, selected, ..
            }) => {
                assert_eq!(*question, 1, "多选数字只切换勾选，不推进");
                assert_eq!(selected[1], vec!["缓存".to_owned()]);
            }
            _ => unreachable!("多选切换后模态应保持打开"),
        }
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('1'),
        );
        if let Some(Modal::Ask { selected, .. }) = app.modal.as_ref() {
            assert!(selected[1].is_empty(), "重复数字应取消勾选");
        }

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// Shift+X 跳过整单提问：按未回答提交空答案，模态关闭。
    #[tokio::test]
    async fn ask_shift_x_dismisses_with_empty_answers() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));

        handle_key(
            &mut app,
            &connection,
            KeyModifiers::SHIFT,
            KeyCode::Char('X'),
        );
        let result = reply_rx
            .try_recv()
            .expect("跳过后应回报")
            .expect("跳过按未回答成功收场");
        assert!(result.answers.is_empty(), "跳过提交空答案");
        assert!(app.modal.is_none(), "跳过后模态关闭");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// Esc 分层：第一次折叠（模态保留、键盘交给转写区），Tab 回卡片；
    /// 折叠态再 Esc 才取消整单。
    #[tokio::test]
    async fn ask_esc_parks_before_cancelling() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));

        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        match app.modal.as_ref() {
            Some(Modal::Ask { parked, .. }) => {
                assert!(*parked, "第一次 Esc 应折叠而不是取消")
            }
            _ => unreachable!("折叠时卡片应保留"),
        }
        assert!(reply_rx.try_recv().is_err(), "折叠不提交也不取消");

        app.scroll_max_offset = 100;
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Up);
        assert_eq!(app.scroll_offset, 3, "折叠态 ↑ 应滚动转写");

        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
        if let Some(Modal::Ask { parked, .. }) = app.modal.as_ref() {
            assert!(!parked, "Tab 应回到提问卡片");
        }

        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        assert!(
            reply_rx.try_recv().expect("折叠后 Esc 应取消").is_err(),
            "取消按错误回报"
        );
        assert!(app.modal.is_none(), "取消后模态关闭");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 粘贴在自定义答案编辑态写入那一行，而不是落到背后的主输入框。
    #[tokio::test]
    async fn ask_paste_lands_in_editing_custom_answer() {
        let (core, connection, mut app) = setup_test_app().await;
        app.modal = Some(ask_modal());

        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('z'),
        );
        handle_terminal_event(
            &mut app,
            &connection,
            Event::Paste("/etc/config.toml".into()),
        );
        assert_eq!(ask_custom(&app).0, "/etc/config.toml");
        assert!(app.prompt.text.is_empty(), "粘贴不得落到主输入框");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// Ctrl+C 在提问卡片任何状态下直接取消整单（与权限弹窗同语义）。
    #[tokio::test]
    async fn ask_ctrl_c_cancels_from_any_state() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));

        // 编辑态下 Ctrl+C 也取消，而不是被输入行吞掉。
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('z'),
        );
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('c'),
        );
        assert!(
            reply_rx.try_recv().expect("Ctrl+C 应取消提问").is_err(),
            "取消按错误回报"
        );
        assert!(app.modal.is_none(), "取消后模态关闭");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// ←→ 只翻看问题指针：不写入也不提交，已选答案原样保留。
    #[tokio::test]
    async fn ask_arrow_keys_browse_questions_without_submitting() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(
            vec![ask_question("q1"), multi_ask_question()],
            Some(reply_tx),
        ));

        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char(' '),
        );
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
        match app.modal.as_ref() {
            Some(Modal::Ask { question, .. }) => assert_eq!(*question, 1, "→ 应翻到下一问"),
            _ => unreachable!("翻看问题不应提交"),
        }
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
        if let Some(Modal::Ask { question, .. }) = app.modal.as_ref() {
            assert_eq!(*question, 2, "最后一问上 → 进入确认页");
        }
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
        if let Some(Modal::Ask { question, .. }) = app.modal.as_ref() {
            assert_eq!(*question, 2, "确认页上 → 不再前进");
        }
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
        if let Some(Modal::Ask { question, .. }) = app.modal.as_ref() {
            assert_eq!(*question, 1, "确认页 ← 回最后一问");
        }
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
        match app.modal.as_ref() {
            Some(Modal::Ask {
                question, selected, ..
            }) => {
                assert_eq!(*question, 0, "← 应翻回上一问");
                assert_eq!(selected[0], vec!["选项一".to_owned()], "翻看不丢已选答案");
            }
            _ => unreachable!("模态应保持打开"),
        }
        assert!(reply_rx.try_recv().is_err(), "翻看问题不得提交");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 折叠态 ←→ 同样翻看问题，且不解除折叠。
    #[tokio::test]
    async fn ask_parked_arrow_keys_browse_questions() {
        let (core, connection, mut app) = setup_test_app().await;
        app.modal = Some(ask_modal_with(
            vec![ask_question("q1"), ask_question("q2")],
            None,
        ));

        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
        match app.modal.as_ref() {
            Some(Modal::Ask {
                question, parked, ..
            }) => {
                assert!(*parked, "翻看问题不解除折叠");
                assert_eq!(*question, 1);
            }
            _ => unreachable!("卡片应保留"),
        }
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
        if let Some(Modal::Ask { question, .. }) = app.modal.as_ref() {
            assert_eq!(*question, 0, "折叠态 ← 应翻回上一问");
        }

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 鼠标点选：单选点击即选中并提交整单。
    #[tokio::test]
    async fn ask_mouse_click_selects_and_submits_single_choice() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));
        app.ask_option_hits = vec![
            (0, ratatui::layout::Rect::new(10, 5, 40, 1)),
            (1, ratatui::layout::Rect::new(10, 6, 40, 1)),
        ];

        handle_mouse_with_connection(&mut app, &connection, left_click(10, 5));
        match app.modal.as_ref() {
            Some(Modal::Ask { question, .. }) => {
                assert_eq!(*question, 1, "单选点击选中并推进到确认页")
            }
            _ => unreachable!("确认页上模态应保留"),
        }
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let result = reply_rx
            .try_recv()
            .expect("确认页 Enter 应提交")
            .expect("提问结果");
        assert_eq!(result.answers[0].selected, ["选项一"]);
        assert!(app.modal.is_none(), "提交后模态关闭");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 鼠标点选：多选点击切换勾选、不推进；点击自定义行进入编辑。
    #[tokio::test]
    async fn ask_mouse_click_toggles_multi_and_opens_custom_editor() {
        let (core, connection, mut app) = setup_test_app().await;
        app.modal = Some(ask_modal_with(vec![multi_ask_question()], None));
        app.ask_option_hits = vec![
            (0, ratatui::layout::Rect::new(10, 5, 40, 1)),
            (1, ratatui::layout::Rect::new(10, 6, 40, 1)),
            (2, ratatui::layout::Rect::new(10, 7, 40, 1)),
            (3, ratatui::layout::Rect::new(10, 8, 40, 1)),
        ];

        handle_mouse_with_connection(&mut app, &connection, left_click(10, 6));
        match app.modal.as_ref() {
            Some(Modal::Ask {
                selected,
                highlight,
                ..
            }) => {
                assert_eq!(selected[0], vec!["日志".to_owned()], "多选点击切换勾选");
                assert_eq!(highlight[0], 1, "点击行获得聚焦");
            }
            _ => unreachable!("多选点击不推进"),
        }
        handle_mouse_with_connection(&mut app, &connection, left_click(10, 6));
        if let Some(Modal::Ask { selected, .. }) = app.modal.as_ref() {
            assert!(selected[0].is_empty(), "重复点击取消勾选");
        }

        handle_mouse_with_connection(&mut app, &connection, left_click(10, 8));
        match app.modal.as_ref() {
            Some(Modal::Ask {
                custom, highlight, ..
            }) => {
                assert!(custom[0].editing, "点击自定义行进入编辑");
                assert_eq!(highlight[0], 3);
            }
            _ => unreachable!("编辑态模态应保持打开"),
        }

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 折叠态点击细条：只展开回作答（不选择）；展开后点选项才提交。
    #[tokio::test]
    async fn ask_mouse_click_strip_unparks_without_selecting() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));
        app.ask_option_hits = vec![(0, ratatui::layout::Rect::new(10, 5, 40, 1))];
        app.ask_strip_area = ratatui::layout::Rect::new(3, 20, 114, 3);
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);

        // 折叠态选项命中区已清空：点在旧选项位置不动作，点细条才展开。
        handle_mouse_with_connection(&mut app, &connection, left_click(10, 5));
        match app.modal.as_ref() {
            Some(Modal::Ask { parked, .. }) => assert!(*parked, "折叠态点卡片外不动作"),
            _ => unreachable!("折叠时模态应保留"),
        }
        handle_mouse_with_connection(&mut app, &connection, left_click(30, 21));
        match app.modal.as_ref() {
            Some(Modal::Ask { parked, .. }) => assert!(!*parked, "点细条应展开回作答"),
            _ => unreachable!("模态应保留"),
        }
        assert!(reply_rx.try_recv().is_err(), "展开本身不提交");

        app.ask_option_hits = vec![(0, ratatui::layout::Rect::new(10, 5, 40, 1))];
        handle_mouse_with_connection(&mut app, &connection, left_click(10, 5));
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let result = reply_rx
            .try_recv()
            .expect("展开后点选项选中，确认页 Enter 提交")
            .expect("提问结果");
        assert_eq!(result.answers[0].selected, ["选项一"]);
        assert!(app.modal.is_none(), "提交后模态关闭");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 作答态滚轮悬停分流：落在可滚文本区（溢出的问题窗/详情区）滚该区偏移，
    /// 其余位置照旧移动选项光标；命中区域只在溢出时登记。
    #[tokio::test]
    async fn ask_mouse_wheel_routes_by_hover_region() {
        let (core, connection, mut app) = setup_test_app().await;
        app.modal = Some(ask_modal_with(vec![multi_ask_question()], None));
        app.ask_question_area = ratatui::layout::Rect::new(10, 3, 60, 5);
        app.ask_detail_area = ratatui::layout::Rect::new(10, 20, 60, 5);

        // 悬停问题正文窗口：滚动问题偏移，光标不动。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(MouseEventKind::ScrollDown, 20, 4),
        );
        match app.modal.as_ref() {
            Some(Modal::Ask {
                highlight,
                question_scroll,
                preview_scroll,
                ..
            }) => {
                assert_eq!(*question_scroll, ASK_PREVIEW_SCROLL_STEP);
                assert_eq!(*preview_scroll, 0);
                assert_eq!(highlight[0], 0, "悬停文本区不动光标");
            }
            _ => unreachable!("模态应保留"),
        }

        // 悬停详情区：滚动详情偏移。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(MouseEventKind::ScrollDown, 20, 21),
        );
        match app.modal.as_ref() {
            Some(Modal::Ask { preview_scroll, .. }) => {
                assert_eq!(*preview_scroll, ASK_PREVIEW_SCROLL_STEP)
            }
            _ => unreachable!("模态应保留"),
        }

        // 两区之外（选项/空白）：恢复移动光标语义。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(MouseEventKind::ScrollDown, 20, 12),
        );
        match app.modal.as_ref() {
            Some(Modal::Ask { highlight, .. }) => assert_eq!(highlight[0], 1),
            _ => unreachable!("模态应保留"),
        }

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 滚动条轨道在转写区外：作答态按下 = 折叠并开始拖拽；折叠态拖动/抬起走通用路径。
    #[tokio::test]
    async fn ask_scrollbar_drag_works_while_parked() {
        let (core, connection, mut app) = setup_test_app().await;
        app.modal = Some(ask_modal());
        app.ask_card_area = ratatui::layout::Rect::new(10, 5, 80, 10);
        app.ask_strip_area = ratatui::layout::Rect::new(3, 20, 114, 2);
        app.scrollback_area = ratatui::layout::Rect::new(3, 2, 114, 18);
        app.scrollbar = crate::app::ScrollbackScrollbar {
            track: ratatui::layout::Rect::new(119, 2, 1, 18),
            thumb: 6,
            max_offset: 36,
        };
        app.scroll_max_offset = 36;

        // 作答态按下轨道空白：折叠 + 拖拽开始（滑块中线跟到光标行）。
        handle_mouse_with_connection(&mut app, &connection, left_click(119, 10));
        match app.modal.as_ref() {
            Some(Modal::Ask { parked, .. }) => assert!(*parked, "按滚动条应折叠"),
            _ => unreachable!("模态应保留"),
        }
        assert!(app.scrollbar_drag.is_some(), "按下即进入拖拽");
        assert!(app.scroll_offset > 0, "按轨道空白滑块应跟到光标");

        // 折叠态拖到轨道顶：走满历史（通用拖拽路径）。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 119, 2),
        );
        assert_eq!(app.scroll_offset, 36, "拖到轨道顶应滚到历史顶端");

        // 抬起结束拖拽，卡片保持折叠。
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(MouseEventKind::Up(MouseButton::Left), 119, 2),
        );
        assert!(app.scrollbar_drag.is_none(), "抬起后结束拖拽");
        assert!(
            matches!(app.modal, Some(Modal::Ask { parked: true, .. })),
            "拖拽全程卡片保持折叠"
        );

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 点击导航条跳问（只动指针不提交）；点击卡片外折叠。
    #[tokio::test]
    async fn ask_mouse_nav_click_jumps_and_outside_click_parks() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(
            vec![ask_question("q1"), ask_question("q2")],
            Some(reply_tx),
        ));
        app.ask_card_area = ratatui::layout::Rect::new(10, 5, 80, 10);
        app.scrollback_area = ratatui::layout::Rect::new(3, 18, 114, 5);
        app.ask_nav_hits = vec![
            (0, ratatui::layout::Rect::new(12, 6, 10, 1)),
            (1, ratatui::layout::Rect::new(24, 6, 10, 1)),
        ];

        handle_mouse_with_connection(&mut app, &connection, left_click(26, 6));
        match app.modal.as_ref() {
            Some(Modal::Ask {
                question, parked, ..
            }) => {
                assert_eq!(*question, 1, "点导航条应跳到第二问");
                assert!(!*parked, "跳问不折叠");
            }
            _ => unreachable!("跳问不应提交"),
        }
        assert!(reply_rx.try_recv().is_err(), "跳问不提交");

        // 卡片外但非转写区（输入框/状态栏死区）：不折叠。
        handle_mouse_with_connection(&mut app, &connection, left_click(5, 26));
        match app.modal.as_ref() {
            Some(Modal::Ask { parked, .. }) => assert!(!*parked, "死区点击不折叠"),
            _ => unreachable!("模态应保留"),
        }

        handle_mouse_with_connection(&mut app, &connection, left_click(5, 20));
        match app.modal.as_ref() {
            Some(Modal::Ask { parked, .. }) => assert!(*parked, "点转写区应折叠"),
            _ => unreachable!("模态应保留"),
        }

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 导航条「确认」项是纯鼠标的提交链路：作答页点它落到确认页，
    /// 确认页上再点即提交（多选末问也能由此离开作答页）。
    #[tokio::test]
    async fn ask_mouse_confirm_nav_jumps_and_submits() {
        let (core, connection, mut app) = setup_test_app().await;
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.modal = Some(ask_modal_with(
            vec![multi_ask_question(), ask_question("q2")],
            Some(reply_tx),
        ));
        app.ask_card_area = ratatui::layout::Rect::new(10, 5, 80, 10);
        app.ask_nav_hits = vec![
            (0, ratatui::layout::Rect::new(12, 6, 10, 1)),
            (1, ratatui::layout::Rect::new(24, 6, 10, 1)),
            (2, ratatui::layout::Rect::new(36, 6, 8, 1)),
        ];

        // 作答页点「确认」（下标 = questions.len()）：多选不切勾、不提交，落确认页。
        handle_mouse_with_connection(&mut app, &connection, left_click(38, 6));
        match app.modal.as_ref() {
            Some(Modal::Ask {
                question,
                selected,
                preview_scroll,
                question_scroll,
                ..
            }) => {
                assert_eq!(*question, 2, "点「确认」应落到确认页");
                assert!(selected.iter().all(|set| set.is_empty()), "跳页不代选");
                assert_eq!(*preview_scroll + *question_scroll, 0, "跳页复位滚动");
            }
            _ => unreachable!("落确认页不应提交"),
        }
        assert!(reply_rx.try_recv().is_err(), "未点第二次不应提交");

        // 确认页上再点「确认」：提交并关闭模态。
        handle_mouse_with_connection(&mut app, &connection, left_click(38, 6));
        assert!(app.modal.is_none(), "确认页点「确认」应提交并关闭");
        let answers = reply_rx
            .await
            .expect("应收到提交结果")
            .expect("提交应成功")
            .answers;
        assert_eq!(answers.len(), 2, "每问一条答案记录");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 左键单击事件。
    fn left_click(column: u16, row: u16) -> crossterm::event::MouseEvent {
        mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column,
            row,
        )
    }

    /// 指定类型与坐标的鼠标事件。
    fn mouse_event(
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
    ) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// 默认高亮落在推荐项上；没有推荐才停在第一项。
    #[test]
    fn ask_highlight_defaults_to_recommendation() {
        assert_eq!(recommended_highlight(&multi_ask_question()), 0);
        let mut question = ask_question("q1");
        question.options.push(AskOption {
            label: "选项二".into(),
            description: None,
            preview: None,
        });
        question.recommended = vec![1];
        assert_eq!(recommended_highlight(&question), 1);
    }

    /// 单问 ask 模态：选项一个 + 自定义行（`z` 进入编辑态）。
    fn ask_modal() -> Modal {
        ask_modal_with(vec![ask_question("q1")], None)
    }

    fn ask_question(id: &str) -> AskQuestion {
        AskQuestion {
            id: id.into(),
            question: "选哪个？".into(),
            header: None,
            multi: false,
            recommended: Vec::new(),
            options: vec![AskOption {
                label: "选项一".into(),
                description: None,
                preview: None,
            }],
        }
    }

    fn multi_ask_question() -> AskQuestion {
        AskQuestion {
            id: "features".into(),
            question: "开启哪些特性？".into(),
            header: None,
            multi: true,
            recommended: vec![0, 2],
            options: vec![
                AskOption {
                    label: "缓存".into(),
                    description: None,
                    preview: None,
                },
                AskOption {
                    label: "日志".into(),
                    description: None,
                    preview: None,
                },
                AskOption {
                    label: "监控".into(),
                    description: None,
                    preview: None,
                },
            ],
        }
    }

    fn ask_modal_with(
        questions: Vec<AskQuestion>,
        reply: Option<tokio::sync::oneshot::Sender<Result<AskResult, moc_protocol::RpcError>>>,
    ) -> Modal {
        let count = questions.len();
        Modal::Ask {
            params: AskParams {
                session_id: "ses_test".into(),
                prompt_id: "prompt_test".into(),
                request_id: "ask_test".into(),
                questions,
            },
            question: 0,
            highlight: vec![0; count],
            selected: vec![Vec::new(); count],
            custom: vec![crate::modal::CustomAnswer::default(); count],
            preview_scroll: 0,
            question_scroll: 0,
            parked: false,
            reply,
        }
    }

    /// 当前问自定义输入的文本与光标。
    fn ask_custom(app: &App) -> (String, usize) {
        match app.modal.as_ref() {
            Some(Modal::Ask {
                custom, question, ..
            }) => custom
                .get(*question)
                .map(|answer| (answer.text.clone(), answer.cursor))
                .unwrap_or_default(),
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
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('z'),
        );
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('中'),
        );
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('文'),
        );
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
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::NONE,
            KeyCode::Char('中'),
        );
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

    /// 造一个子代理视图：`session/new` 出来的会话 ID 就是代理 ID。
    fn agent_view(session: &moc_protocol::methods::SessionInfo) -> crate::subagent::SubagentView {
        crate::subagent::SubagentView::new(moc_protocol::methods::AgentInfo {
            id: session.id.clone(),
            parent_id: session.parent_session_id.clone().unwrap_or_default(),
            title: session.title.clone(),
            model: session.model.clone(),
            cwd: session.cwd.clone(),
            preset: None,
            status: moc_protocol::methods::AgentStatus::Running,
            created_at_ms: session.created_at_ms,
            updated_at_ms: session.updated_at_ms,
            last_message: None,
        })
    }

    async fn load_page(
        connection: &EmbeddedConnection,
        session_id: &str,
    ) -> anyhow::Result<moc_protocol::methods::SessionLoadPageResult> {
        connection
            .client
            .history_page(SessionLoadPageParams {
                full_messages: true,
                plan_page: Some(Default::default()),
                session_id: session_id.to_owned(),
                before_entry_id: None,
                max_entries: 128,
                max_message_bytes: Some(64 * 1024),
                redo_page: Some(Default::default()),
            })
            .await
            .map_err(anyhow::Error::from)
    }

    /// 子代理会话在测试里的替身：`session/new` 起的空会话即可承载聚焦语义。
    async fn new_child(
        connection: &EmbeddedConnection,
        title: &str,
    ) -> moc_protocol::methods::SessionInfo {
        connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: Some(title.to_owned()),
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .expect("创建子会话")
    }

    /// `Ctrl+N`/`/agents` 唤出右侧子代理侧栏而非旧居中模态；名册为空时只提示。
    #[tokio::test]
    async fn agents_action_summons_sidebar_not_modal() {
        let (core, connection, mut app) = setup_test_app().await;

        // 名册为空：不强开面板，只给提示。
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('n'),
        );
        assert!(app.modal.is_none(), "Ctrl+N 不得打开模态");
        assert_eq!(app.subagent_sidebar, Some(true), "记录唤出意图");
        assert!(!app.subagent_sidebar_visible(), "空名册仍不可见");
        assert_eq!(app.notice.as_deref(), Some("当前没有子代理"));

        // 名册有代理：Ctrl+N 后立即可见，先前手动关掉的也能再唤出。
        app.notice = None;
        app.subagents.insert(
            "agent_1".into(),
            crate::subagent::SubagentView::new(moc_protocol::methods::AgentInfo {
                id: "agent_1".into(),
                parent_id: "ses_parent".into(),
                title: "任务".into(),
                model: "test/model".into(),
                cwd: "/tmp".into(),
                preset: None,
                status: moc_protocol::methods::AgentStatus::Running,
                created_at_ms: 0,
                updated_at_ms: 0,
                last_message: None,
            }),
        );
        app.subagent_sidebar = Some(false);
        handle_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('n'),
        );
        assert!(app.modal.is_none());
        assert!(app.subagent_sidebar_visible(), "Ctrl+N 唤出侧栏");

        // /agents 与 Ctrl+N 同路。
        app.subagent_sidebar = Some(false);
        execute_command(&mut app, &connection, "/agents".to_owned());
        assert!(app.modal.is_none());
        assert!(app.subagent_sidebar_visible(), "/agents 唤出侧栏");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 聚焦：点中侧栏卡片（`focus_agent`）把当前会话换成子代理，
    /// 空输入的 `Esc` 返回并还原草稿。
    #[tokio::test]
    async fn focusing_a_subagent_switches_session_and_escape_returns() {
        let (core, connection, mut app, _root) = setup_test_app_keeping_root().await;
        let home = app.session.clone().expect("主会话");
        let child = new_child(&connection, "探查者").await;
        let view = agent_view(&child);
        app.subagents.insert(child.id.clone(), view.clone());
        app.prompt.insert_str_raw("未发送的草稿");

        focus_agent(&mut app, &connection, child.id.clone());
        assert!(app.focused.is_some(), "聚焦状态必须置位");
        assert_eq!(
            app.subagent_selected.as_deref(),
            Some(child.id.as_str()),
            "聚焦的子代理应记为选中项"
        );
        assert_eq!(
            app.agents_root_id().as_deref(),
            Some(home.id.as_str()),
            "代理面在被聚焦期间仍挂靠主会话"
        );

        let page = load_page(&connection, &child.id)
            .await
            .expect("加载子代理会话");
        apply_focus_loaded(&mut app, &connection, false, Ok(page));
        assert_eq!(
            app.session.as_ref().map(|session| session.id.as_str()),
            Some(child.id.as_str()),
            "当前会话应换成子代理"
        );

        // 空输入的 Esc 只返回，不打断子代理。
        app.prompt.clear();
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        assert!(app.focused.is_none(), "Esc 返回主会话");
        let page = load_page(&connection, &home.id).await.expect("加载主会话");
        apply_focus_loaded(&mut app, &connection, true, Ok(page));
        assert_eq!(
            app.session.as_ref().map(|session| session.id.as_str()),
            Some(home.id.as_str())
        );
        assert_eq!(app.prompt_text(), "未发送的草稿", "返回时还原主会话草稿");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 双击 `←` 返回主会话；单次按键仍是移动光标。
    #[tokio::test]
    async fn double_left_returns_from_focus() {
        let (core, connection, mut app, _root) = setup_test_app_keeping_root().await;
        let child = new_child(&connection, "探查者").await;
        let view = agent_view(&child);
        app.subagents.insert(child.id.clone(), view.clone());
        let home = app.session.clone().expect("主会话");
        app.focused = Some(crate::app::FocusedAgent {
            home,
            home_draft: String::new(),
            home_scroll: 0,
        });

        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
        assert!(app.focused.is_some(), "单次左移不返回");
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
        assert!(app.focused.is_none(), "双击左移返回主会话");

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 聚焦期间主会话发来的代理状态事件仍要更新名册，否则侧栏在聚焦时僵住。
    #[tokio::test]
    async fn agent_status_updates_reach_the_roster_while_focused() {
        let (core, connection, mut app, _root) = setup_test_app_keeping_root().await;
        let home = app.session.clone().expect("主会话");
        let child = new_child(&connection, "探查者").await;
        app.subagents.insert(child.id.clone(), agent_view(&child));
        app.session = Some(child.clone());
        app.focused = Some(crate::app::FocusedAgent {
            home: home.clone(),
            home_draft: String::new(),
            home_scroll: 0,
        });

        apply_update_and_refresh(
            &mut app,
            &connection,
            home.id.clone(),
            SessionUpdate::AgentStatusChanged {
                agent_id: child.id.clone(),
                status: moc_protocol::methods::AgentStatus::Completed,
                last_message: Some("done".into()),
            },
        );

        let view = app.subagents.get(&child.id).expect("名册里有该代理");
        assert_eq!(view.status_label(), crate::render::labels::status::DONE);
        assert_eq!(view.info.last_message.as_deref(), Some("done"));
        assert!(!view.running);

        connection.shutdown().await;
        core.shutdown().await;
    }
}

/// `/config` 的角色字段与 `Ctrl+P` 共用选择器：自成模块，避免与主测试模块的长夹具混在一起。
#[cfg(test)]
mod config_picker_tests {
    use super::*;

    #[tokio::test]
    async fn startup_selects_then_remembers_model_and_resume_ignores_missing_preference() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "data: {\"id\":\"reply\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"已记录\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                "text/event-stream",
            ))
            .expect(1)
            .mount(&upstream)
            .await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            format!("[providers.p]\napi='openai_chat'\nbase_url='{}/v1'\n[providers.p.models.m]\n[workspace]\nwatch=false\n[plugins]\nenabled=false\n", upstream.uri()),
        )
        .unwrap();
        let core = Core::new(moc_core::CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            session_dir: Some(root.path().join("sessions")),
            ..Default::default()
        })
        .unwrap();
        let (events, mut receiver) = unbounded_channel();
        let connection = connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events.clone());
        app.startup_plan = true;
        bootstrap(
            &connection.client,
            &mut app,
            &TuiOptions {
                plan: true,
                ..Default::default()
            },
        )
        .await;
        assert!(app.session.is_none());
        let Some(Modal::Model {
            entries, picker, ..
        }) = app.modal.as_mut()
        else {
            panic!("没有记忆时应打开模型选择器");
        };
        picker.selected = entries
            .iter()
            .position(|entry| entry.selector.starts_with("p/m"))
            .unwrap();
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let info = loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(3), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::Loaded(info, ..) = event {
                break info;
            }
        };
        assert_eq!(info.model, "p/m:off");
        assert!(info.plan_active);
        // 空会话刻意不落盘；先完成一轮真实 RPC 提示，再验证卸载后的冷恢复。
        connection
            .client
            .call::<moc_protocol::SessionPrompt>(moc_protocol::PromptParams {
                session_id: info.id.clone(),
                input: vec![moc_protocol::InputPart::Text {
                    text: "恢复前的历史".into(),
                }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await
            .unwrap();
        let mut next = App::new(Theme::global(), events.clone());
        bootstrap(&connection.client, &mut next, &TuiOptions::default()).await;
        assert_eq!(next.session.as_ref().unwrap().model, info.model);
        moc_core::config::selection::save(root.path(), "p/deleted:off").unwrap();
        connection
            .client
            .call::<moc_protocol::SessionUnload>(moc_protocol::SessionUnloadParams {
                session_id: info.id.clone(),
            })
            .await
            .unwrap();
        let mut restored = App::new(Theme::global(), events);
        bootstrap(
            &connection.client,
            &mut restored,
            &TuiOptions {
                resume: Some(Some(info.id.clone())),
                ..Default::default()
            },
        )
        .await;
        let session = restored
            .session
            .as_ref()
            .unwrap_or_else(|| panic!("恢复会话失败：{:?}", restored.notice));
        assert_eq!(session.id, info.id);
        assert_eq!(session.model, "p/m:off");
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// `/config` 的模型角色字段与 `Ctrl+P` 共用选择器：候选取自 core（带思考等级），
    /// 选中后写回该字段的配置值，不切换会话模型。

    #[tokio::test]
    async fn config_model_role_picker_shares_the_searchable_picker() {
        use crate::settings::{History, RowKind, SettingsPanel, View};
        use moc_core::{Core, CoreConfig};
        use moc_protocol::ConfigScope;
        use moc_protocol::methods::SettingsDocument;
        use serde_json::{Value, json};
        use std::collections::BTreeSet;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .unwrap();
        let (events, _receiver) = unbounded_channel();
        let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        app.models = connection
            .client
            .call::<ConfigGet>(ConfigGetParams {})
            .await
            .unwrap()
            .models;

        // 面板只放探索模型字段：行由文档重建，选中该行后按 Enter。
        let role = vec![
            "models".to_owned(),
            "roles".to_owned(),
            "explore".to_owned(),
        ];
        app.settings = Some(SettingsPanel {
            id: 1,
            cwd: "/tmp".into(),
            scope: ConfigScope::Global,
            data: Some(SettingsDocument {
                path: "config.toml".into(),
                revision: "0".into(),
                document: json!({}),
                effective: json!({}),
                inherited: json!({
                    "models": {"roles": {"explore": "test/big:low"}},
                    "providers": {"test": {"models": {"big": {}, "small": {}}}},
                }),
                defaults: json!({}),
                fields: Vec::new(),
                provider_template: json!({}),
                model_template: json!({}),
            }),
            tui: Value::Null,
            effective: Value::Null,
            fields: Vec::new(),
            controls: Vec::new(),
            rows: Vec::new(),
            collapsed: BTreeSet::new(),
            expanded: BTreeSet::new(),
            tab: 0,
            selected: 0,
            offset: 0,
            scroll: Default::default(),
            pointer: None,
            advanced: true,
            filter: String::new(),
            edit: None,
            error: None,
            message: None,
            busy: false,
            hits: Vec::new(),
            pending: None,
            view: View::Fields,
            providers: Default::default(),
            history: History::default(),
            outcome: None,
            restore: None,
        });
        {
            let panel = app.settings.as_mut().expect("设置面板");
            panel.rebuild();
            let index = panel
                .rows
                .iter()
                .position(|row| {
                    matches!(&row.kind, RowKind::Field(index) if panel.fields[*index].path == role)
                })
                .expect("探索模型行");
            panel.selected = index;
        }

        crate::settings::handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        assert!(
            matches!(
                &app.modal,
                Some(Modal::Model {
                    target: crate::modal::ModelTarget::Config { path, .. },
                    ..
                }) if *path == role
            ),
            "探索模型字段按 Enter 打开共用选择器"
        );
        let choice = |app: &App| {
            app.modal
                .as_ref()
                .and_then(Modal::model_choice)
                .expect("选择器有高亮条目")
                .1
        };
        assert_eq!(choice(&app), "test/big:low", "沿用配置里写明的等级");

        // 选择器就是搜索框：j／h／l 这类字符必须进查询串，而不是被当成方向键。
        let query = |app: &App| match app.modal.as_ref() {
            Some(Modal::Model { picker, .. }) => picker.query.clone(),
            _ => panic!("选择器应当打开"),
        };
        for key in ['g', 'j', 'h', 'l'] {
            handle_modal_key(
                &mut app,
                &connection,
                KeyModifiers::NONE,
                KeyCode::Char(key),
            );
        }
        assert_eq!(query(&app), "gjhl", "字符键全部进搜索框");
        handle_modal_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('u'),
        );
        assert_eq!(query(&app), "", "Ctrl+U 清空查询");

        // 按模型名筛选，不依赖继承/禁用选项在列表中的数量。
        for key in "small".chars() {
            handle_modal_key(
                &mut app,
                &connection,
                KeyModifiers::NONE,
                KeyCode::Char(key),
            );
        }
        assert_eq!(choice(&app), "test/small:medium");

        // 切回 big 恢复原 low；Tab 展开等级下拉，选中 max 后确认。
        handle_modal_key(
            &mut app,
            &connection,
            KeyModifiers::CONTROL,
            KeyCode::Char('u'),
        );
        for key in "big".chars() {
            handle_modal_key(
                &mut app,
                &connection,
                KeyModifiers::NONE,
                KeyCode::Char(key),
            );
        }
        assert_eq!(choice(&app), "test/big:low");
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        assert_eq!(choice(&app), "test/big:max", "下拉里选 max 生效");
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);

        assert!(app.modal.is_none(), "确认后关闭选择器");
        let panel = app.settings.as_ref().expect("设置面板");
        let document = &panel.data.as_ref().expect("设置文档").document;
        assert_eq!(
            document
                .pointer("/models/roles/explore")
                .and_then(Value::as_str),
            Some("test/big:max"),
            "选中的 selector 写回配置"
        );
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 会话模型选择器的 Fusion 组合复用两侧槽位选择器。
    #[tokio::test]
    async fn session_fusion_pair_flows_through_slot_pickers() {
        use moc_core::{Core, CoreConfig};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .unwrap();
        let (events, _receiver) = unbounded_channel();
        let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        app.models = connection
            .client
            .call::<ConfigGet>(ConfigGetParams {})
            .await
            .unwrap()
            .models;

        app.open_model_picker_for(
            "test/big:low".into(),
            &[],
            crate::modal::ModelTarget::Session,
        );

        // Fusion 特殊项排在候选首位；→ 进入详情卡，焦点落在 Lead 行。
        let Some(Modal::Model {
            entries, picker, ..
        }) = app.modal.as_mut()
        else {
            panic!("模型选择器应打开");
        };
        assert_eq!(entries[0].selector, crate::modal::FUSION_ENTRY_SELECTOR);
        picker.selected = 0;
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
        let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert_eq!(detail.focus, Some(crate::modal::DetailRow::Lead));
        assert_eq!(
            detail.fusion.lead, "test/big:low",
            "无默认组合时沿用当前执行模型"
        );
        assert!(detail.fusion.sidekick.is_empty());

        // 移到 Sidekick 行，Enter 展开模型下拉；输入 "small" 后 Enter 回填。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        for ch in "small".chars() {
            handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
        }
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert_eq!(detail.fusion.sidekick, "test/small:medium");

        // 回到 LeadLevel 行，下拉里把等级从 low 改成 max。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Up);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert_eq!(detail.fusion.lead, "test/big:max");
        assert_eq!(
            crate::modal::fusion_selector(&detail.fusion.lead, &detail.fusion.sidekick),
            "fusion:test/big:max+test/small:medium"
        );

        // Esc 回列表焦点，再 Esc 关闭。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert_eq!(detail.focus, None);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
        assert!(app.modal.is_none());
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 会话模型同样可应用 Fusion 组合：应用回执落地后配置卡关闭，
    /// 会话模型经 SessionUpdate 推送变成规范组合串。
    #[tokio::test]
    async fn session_fusion_pair_applies_and_closes() {
        use moc_core::{Core, CoreConfig};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .unwrap();
        let (events, mut receiver) = unbounded_channel();
        let mut connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        app.models = connection
            .client
            .call::<ConfigGet>(ConfigGetParams {})
            .await
            .unwrap()
            .models;
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .unwrap();
        app.session = Some(session);

        // 选择器首位是 Fusion 特殊项 → → 进入详情卡（Lead 沿用当前模型）。
        app.open_model_picker();
        let Some(Modal::Model { picker, .. }) = app.modal.as_mut() else {
            panic!("模型选择器应打开");
        };
        picker.selected = 0;
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
        let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert_eq!(detail.fusion.lead, "test/big:low");

        // Sidekick 行：下拉里选第二个模型。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        for ch in "small".chars() {
            handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
        }
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
            panic!("模型选择器应保持打开");
        };
        assert_eq!(detail.fusion.sidekick, "test/small:medium");

        // 移到 Apply 行应用：发起 session/set_model，收尾回执落地后选择器关闭。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
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
        .expect("应收到切换收尾回执");
        crate::interaction::handle_event(&mut app, &connection, event);
        assert!(app.modal.is_none(), "收尾后模型选择器应关闭");

        // 会话模型经 SessionUpdate 推送更新为规范组合串。
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let update = connection.updates.recv().await.unwrap();
                let model_changed = matches!(
                    &update,
                    crate::protocol::UiUpdate::Session(params)
                        if matches!(&params.update, SessionUpdate::ModelChanged { .. })
                );
                apply_ui_update(&mut app, &connection, update);
                if model_changed {
                    break;
                }
            }
        })
        .await
        .expect("应收到模型变更推送");
        assert_eq!(
            app.session.as_ref().map(|session| session.model.as_str()),
            Some("fusion:test/big:low+test/small:medium")
        );
        connection.shutdown().await;
        core.shutdown().await;
    }

    /// 会话目标下在等级下拉里选档：立即发起切换，回执后选择器保持打开、
    /// `current` 写回新 selector；换到别的模型同样生效。
    #[tokio::test]
    async fn session_level_pick_applies_inline_and_stays_open() {
        use moc_core::{Core, CoreConfig};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .unwrap();
        let (events, mut receiver) = unbounded_channel();
        let mut connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        app.models = connection
            .client
            .call::<ConfigGet>(ConfigGetParams {})
            .await
            .unwrap()
            .models;
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .unwrap();
        app.session = Some(session);

        // 收掉一次切换的收尾回执与 ModelChanged 推送，返回会话模型。
        async fn settle(
            app: &mut App,
            connection: &mut crate::protocol::EmbeddedConnection,
            receiver: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        ) -> Option<String> {
            let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if let Some(event @ AppEvent::SessionControlSettled { .. }) =
                        receiver.recv().await
                    {
                        break event;
                    }
                }
            })
            .await
            .expect("应收到切换收尾回执");
            crate::interaction::handle_event(app, connection, event);
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    let update = connection.updates.recv().await.unwrap();
                    let changed = matches!(
                        &update,
                        crate::protocol::UiUpdate::Session(params)
                            if matches!(&params.update, SessionUpdate::ModelChanged { .. })
                    );
                    apply_ui_update(app, connection, update);
                    if changed {
                        break;
                    }
                }
            })
            .await
            .expect("应收到模型变更推送");
            app.session.as_ref().map(|session| session.model.clone())
        }

        // a) 高亮 big（选择器打开时默认落第一行真实模型），Tab 开等级下拉，
        //    移到 max 提交：立即切换，选择器不关。
        app.open_model_picker();
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let model = settle(&mut app, &mut connection, &mut receiver).await;
        assert_eq!(model.as_deref(), Some("test/big:max"));
        let Some(Modal::Model {
            current, picker, ..
        }) = app.modal.as_ref()
        else {
            panic!("内联切换后选择器应保持打开");
        };
        assert_eq!(current, "test/big:max");
        assert!(!picker.loading);

        // b) 高亮移到 small，Tab 后唯一档位 medium 直接提交：同样立即生效。
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        let model = settle(&mut app, &mut connection, &mut receiver).await;
        assert_eq!(model.as_deref(), Some("test/small:medium"));
        let Some(Modal::Model {
            current, picker, ..
        }) = app.modal.as_ref()
        else {
            panic!("第二次内联切换后选择器仍应保持打开");
        };
        assert_eq!(current, "test/small:medium");
        assert!(!picker.loading);

        connection.shutdown().await;
        core.shutdown().await;
    }

    /// Fast Mode 行只出现在支持该能力的模型详情卡里，且只限会话目标；
    /// Enter 发起 `session/set_fast_mode`，推送落地后写回会话状态。
    #[tokio::test]
    async fn fast_mode_row_only_for_fast_models_and_session_target() {
        use moc_core::{Core, CoreConfig};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/big:low'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.big]\nreasoning=true\nthinking_levels=['low','max']\nfast_mode=true\n[providers.test.models.small]\nreasoning=true\nthinking_levels=['medium']\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .unwrap();
        let (events, _receiver) = unbounded_channel();
        let mut connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        app.models = connection
            .client
            .call::<ConfigGet>(ConfigGetParams {})
            .await
            .unwrap()
            .models;
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .unwrap();
        app.session = Some(session);

        // Session 目标：fast 模型的详情行含 Fast；不支持 fast 的模型不含。
        app.open_model_picker();
        let Some(Modal::Model {
            entries,
            picker,
            detail,
            target,
            ..
        }) = app.modal.as_ref()
        else {
            panic!("模型选择器应打开");
        };
        let rows_of = |index: usize| {
            crate::modal::detail_rows(entries.get(index), entries, target, &detail.fusion)
        };
        assert!(
            rows_of(picker.selected).contains(&crate::modal::DetailRow::Fast),
            "fast 模型的会话目标详情行含 Fast"
        );
        let small = entries
            .iter()
            .position(|entry| entry.selector == "test/small")
            .expect("small 条目");
        assert!(
            !rows_of(small).contains(&crate::modal::DetailRow::Fast),
            "非 fast 模型不显示 Fast 行"
        );

        // Config 目标下同一模型也不显示 Fast 行。
        app.open_config_model_picker(
            vec!["models".into(), "roles".into(), "explore".into()],
            "探索模型".into(),
            "test/big:low".into(),
            &[],
        );
        let Some(Modal::Model {
            entries,
            picker,
            detail,
            target,
            ..
        }) = app.modal.as_ref()
        else {
            panic!("配置选择器应打开");
        };
        assert!(
            !crate::modal::detail_rows(
                entries.get(picker.selected),
                entries,
                target,
                &detail.fusion,
            )
            .contains(&crate::modal::DetailRow::Fast),
            "配置目标不显示 Fast 行"
        );

        // 回到会话选择器：聚焦 Fast 行按 Enter 发起切换。
        app.open_model_picker();
        let Some(Modal::Model { detail, .. }) = app.modal.as_mut() else {
            panic!("模型选择器应打开");
        };
        detail.focus = Some(crate::modal::DetailRow::Fast);
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let update = connection.updates.recv().await.unwrap();
                let fast_changed = matches!(
                    &update,
                    crate::protocol::UiUpdate::Session(params)
                        if matches!(&params.update, SessionUpdate::FastModeChanged { enabled: true })
                );
                apply_ui_update(&mut app, &connection, update);
                if fast_changed {
                    break;
                }
            }
        })
        .await
        .expect("应收到 Fast Mode 推送");
        assert!(
            app.session
                .as_ref()
                .is_some_and(|session| session.fast_mode),
            "推送落地后会话 fast_mode 变 true"
        );
        connection.shutdown().await;
        core.shutdown().await;
    }
    /// 后代会话的用量行在 /usage 表上标出来源。
    #[test]
    fn usage_table_marks_descendant_sources() {
        use moc_protocol::methods::UsageSource;
        use moc_protocol::{ModelUsageSummary, SessionUsageResult};

        let summary = |model: &str, source| ModelUsageSummary {
            model: model.into(),
            source,
            calls: 1,
            input_tokens: 100,
            output_tokens: 10,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
            active_ms: 10,
            ttft_ms: 0,
            ttft_calls: 0,
            gen_ms: 0,
        };
        let lines = usage_table(
            &SessionUsageResult {
                models: vec![
                    summary("p/lead:off", UsageSource::Session),
                    summary("p/cheap:off", UsageSource::Sidekick),
                    summary("p/lead:off", UsageSource::Subagent),
                ],
                total: summary("total", UsageSource::Session),
                tool_ms: 0,
            },
            100,
        );
        let text = lines.join("\n");
        assert!(text.contains("p/lead (子代理)"), "{text}");
        assert!(text.contains("p/cheap (sidekick)"), "{text}");
    }
}

#[cfg(test)]
#[path = "event_plan_tests.rs"]
mod plan_tests;

/// 胶囊点击取下一个待开审批：最小化（parked）票优先，其次队首排队请求。
fn take_next_approval(app: &mut App) -> Option<Box<crate::workflow::WorkflowApprovalState>> {
    if let Some(state) = app.workflow.parked_approval.take() {
        return Some(state);
    }
    app.workflow
        .queued_approvals
        .pop_front()
        .map(|request| Box::new(crate::workflow::WorkflowApprovalState::new(request)))
}

/// 打开 workflow run 列表：`session/workflow_runs` → `AppEvent::OpenWorkflowRuns`。
fn open_workflow_runs(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.set_notice("当前没有活动会话");
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<moc_protocol::methods::SessionWorkflowRuns>(
                moc_protocol::methods::SessionWorkflowRunsParams { session_id },
            )
            .await
        {
            Ok(result) => {
                let _ = events.send(AppEvent::OpenWorkflowRuns(result.runs));
            }
            Err(error) => {
                let _ = events.send(AppEvent::Notice(format!("获取 run 列表失败：{error}")));
            }
        }
    });
}

#[cfg(test)]
mod workflow_approval_tests {
    use super::*;
    use moc_protocol::methods::{WorkflowApproval, WorkflowApprovalKind, WorkflowEvent};

    fn approval(id: &str) -> Box<WorkflowApproval> {
        Box::new(WorkflowApproval {
            request_id: id.into(),
            run_id: "r1".into(),
            script: String::new(),
            plan: serde_json::json!({}),
            defaults: moc_protocol::methods::WorkflowApprovalDefaults {
                role_models: Default::default(),
                model_ref_map: Default::default(),
            },
            args: serde_json::Value::Null,
            max_concurrency: 0,
            kind: WorkflowApprovalKind::Launch,
            new_commands: None,
        })
    }

    #[test]
    fn approval_queues_while_modal_busy() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), tx);
        // modal 占用 → 完整载荷进队列。
        app.modal = Some(Modal::Help { scroll: 0 });
        apply_workflow_update(
            &mut app,
            "r1".into(),
            WorkflowEvent::ApprovalRequested {
                request: approval("a1"),
            },
        );
        assert_eq!(app.workflow.queued_approvals.len(), 1);
        assert_eq!(app.workflow.queued_approvals[0].request_id, "a1");
        // ApprovalResolved 按 request_id 移除。
        apply_workflow_update(
            &mut app,
            "r1".into(),
            WorkflowEvent::ApprovalResolved {
                request_id: "a1".into(),
                approved: true,
                feedback: None,
            },
        );
        assert!(app.workflow.queued_approvals.is_empty());
    }

    #[test]
    fn approval_opens_modal_and_take_next_pops_queue() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), tx);
        // 空档 → 直接弹窗。
        apply_workflow_update(
            &mut app,
            "r1".into(),
            WorkflowEvent::ApprovalRequested {
                request: approval("a1"),
            },
        );
        assert!(matches!(app.modal, Some(Modal::WorkflowApproval { .. })));
        // 打开中的审批被服务端 resolve → modal 关闭。
        apply_workflow_update(
            &mut app,
            "r1".into(),
            WorkflowEvent::ApprovalResolved {
                request_id: "a1".into(),
                approved: true,
                feedback: None,
            },
        );
        assert!(app.modal.is_none());
        // 排队请求可被胶囊点击打开。
        app.modal = Some(Modal::Help { scroll: 0 });
        apply_workflow_update(
            &mut app,
            "r1".into(),
            WorkflowEvent::ApprovalRequested {
                request: approval("a2"),
            },
        );
        app.modal = None;
        let state = take_next_approval(&mut app).expect("队首应可弹出");
        assert_eq!(state.request.request_id, "a2");
        assert!(app.workflow.queued_approvals.is_empty());
    }
}

#[cfg(test)]
mod goal_command_tests {
    use super::*;
    use moc_protocol::methods::GoalAction;

    #[test]
    fn parse_goal_action_covers_all_subcommands() {
        assert_eq!(parse_goal_action("").unwrap(), GoalAction::Show);
        assert_eq!(parse_goal_action("  ").unwrap(), GoalAction::Show);
        assert_eq!(parse_goal_action("pause").unwrap(), GoalAction::Pause);
        assert_eq!(parse_goal_action("resume").unwrap(), GoalAction::Resume);
        assert_eq!(parse_goal_action("clear").unwrap(), GoalAction::Unset);
        assert_eq!(
            parse_goal_action("edit 换个说法").unwrap(),
            GoalAction::Edit {
                objective: "换个说法".into()
            }
        );
        // `edit` 必须跟空白分隔；`editing` 整段当 objective 建目标。
        assert_eq!(
            parse_goal_action("editing plans").unwrap(),
            GoalAction::Create {
                objective: "editing plans".into()
            }
        );
        assert_eq!(
            parse_goal_action("把这件事做完").unwrap(),
            GoalAction::Create {
                objective: "把这件事做完".into()
            }
        );
        assert!(parse_goal_action("edit").is_err());
    }
}

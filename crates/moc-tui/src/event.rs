use std::sync::Arc;

use anyhow::Context as _;
use moc_core::Core;
use moc_protocol::SessionEngine;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::app::{App, Focus};
use crate::input::{InputEvent, InputThread};
use crate::modal::{Modal, SessionScope};
use crate::protocol::{AppEvent, ClipboardPayload, EmbeddedConnection, connect_embedded};
use crate::render;
use crate::terminal::TerminalGuard;
use crate::theme::Theme;
use crate::transcript::DetailLevel;

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
    /// 启动覆盖显示档位；工具的 Full 按简略处理，`None` 用偏好设置。
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
        app.tool_detail = level.tool_preset();
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

mod bootstrap;
use bootstrap::bootstrap;
use bootstrap::init_logging;
use bootstrap::load_display_session;
use bootstrap::load_mutation_page;
use bootstrap::refresh_git_branch;
mod context;
use context::apply_context_content;
use context::refresh_context;
mod plugins;
use plugins::refresh_plugins;
mod agents;
use agents::apply_focus_loaded;
use agents::load_subagents_for_parent;
mod rewind;
use rewind::apply_reasked;
use rewind::apply_rewind_loaded;
use rewind::apply_rewound;
mod completion;
use completion::current_token_range;
mod mouse;
mod scrollback;
use scrollback::apply_history_page;
use scrollback::request_history_page;
use scrollback::scroll;
mod keyboard;
mod legend;
pub(crate) use keyboard::cancel_prompt;
use keyboard::handle_terminal_event;
mod ask;
use ask::advance_ask;
use ask::dismiss_ask;
use ask::pick_ask_option;
use ask::recommended_highlight;
use ask::submit_ask;
mod model;
use model::apply_fusion_pick;
mod plan;
pub(crate) use plan::apply_plan_approval;
pub(crate) use plan::apply_plan_diff;
pub(crate) use plan::apply_plan_execution_loaded;
use plan::apply_plan_review;
pub(crate) use plan::apply_plan_revised;
mod updates;
use updates::apply_ui_update;
mod usage;
mod workflow;
use usage::format_usage;
use usage::refresh_usage;
mod clipboard;
mod commands;
pub(crate) use clipboard::write_osc52;
mod modal_key;

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
        // 通知、告警与选区复制反馈到期时重绘；选区操作提示本身不计时。
        let notice_deadline = app.notice_deadline().map(tokio::time::Instant::from_std);
        let warning_deadline = app.warning_deadline().map(tokio::time::Instant::from_std);
        let selection_copy_deadline = app
            .selection_copy_deadline()
            .map(tokio::time::Instant::from_std);
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
            _ = tokio::time::sleep_until(selection_copy_deadline.unwrap_or_else(tokio::time::Instant::now)), if selection_copy_deadline.is_some() => {
                app.selection_copied_at = None;
                dirty = true;
            }
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
                        if matches!(&event, AppEvent::ConfigurationLoaded(_) | AppEvent::SettingsResponse { .. })
                            && let Some(Modal::ContextContent { state }) = app.modal.as_mut() {
                            state.stale = true;
                        }
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
                        app.context_usage = Some(usage);
                    }
                    Some(AppEvent::ContextContent { request, session_id, result }) => {
                        if !matches!(&app.modal, Some(Modal::ContextContent{state}) if state.request==request) {
                            if let Ok(content)=result {
                                let client=connection.client.clone();
                                tokio::spawn(async move {let _=client.call::<moc_protocol::methods::SessionContextClose>(moc_protocol::methods::SessionContextCloseParams{session_id,view_id:content.view_id}).await;});
                            }
                        } else {apply_context_content(app, request, result);}
                    }
                    Some(AppEvent::ContextCopied { request, result }) => {
                        if matches!(&app.modal, Some(Modal::ContextContent { state }) if state.request == request) {
                            match result {
                                Ok(text) => {
                                    write_osc52(&text);
                                    tokio::spawn(async move { let _ = crate::clipboard::copy_text_to_clipboard(&text).await; });
                                    app.set_notice("已复制完整原文");
                                }
                                Err(error) => app.set_notice(format!("复制失败：{error}")),
                            }
                        }
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
                    Some(AppEvent::SkillsProgress(update)) => app.update_skills_compaction(&update),
                    Some(AppEvent::SkillsCompacted(result)) => {
                        let succeeded = result.is_ok();
                        app.finish_skills_compaction(result);
                        if succeeded { refresh_context(app, connection, false); }
                    }
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

/// 提问预览每次翻页滚动的行数；渲染层会夹到实际内容高度。
const ASK_PREVIEW_SCROLL_STEP: usize = 8;
/// 帮助页 PgUp/PgDn 每次滚动的行数；渲染层会夹到实际内容高度。
const HELP_PAGE: usize = 8;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod config_picker_tests;

#[cfg(test)]
#[path = "event_plan_tests.rs"]
mod plan_tests;

#[cfg(test)]
mod workflow_approval_tests;

#[cfg(test)]
mod goal_command_tests;

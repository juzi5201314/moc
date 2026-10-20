//! Enter 补充当前提示（待发送区，安全边界注入），Ctrl+Enter 排队。
//!
//! 补充消息只有 core 发来 `SteeringApplied` 后才移入转写；队列消息在真正发出
//! `session/prompt` / `session/bash` 时才移出待发送区。`interrupt` 仍是用户
//! 显式选择的“立即停止再提问”例外。

use crate::{
    app::{App, Focus},
    config::ActiveSubmit,
    protocol::{AppEvent, EmbeddedConnection},
    widgets::prompt::PromptState,
};
use moc_protocol::{
    BashCommand, BashParams, ClientHandle, InputPart, PromptParams, SessionBash, SessionMode,
    SessionPrompt, SessionSteer, SessionUnsteer, SteerParams, UnsteerParams,
};
use tokio::sync::mpsc::UnboundedSender;

pub struct QueuedPrompt {
    pub id: u64,
    pub draft: PromptState,
    /// 仅本条消息生效的模式覆盖（`/ask` 注入）；`None` 沿用会话模式。
    pub mode: Option<SessionMode>,
}

/// 待发送补充消息的撤回动作：恢复到输入框编辑，或直接丢弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawAction {
    Edit,
    Discard,
}

/// 已提交给 core、但还没在安全边界写入会话的补充消息。
pub struct PendingSteer {
    pub id: u64,
    pub draft: PromptState,
    /// 待发送区展示文本（图片会显示成 `[图片]`）。
    pub display_text: String,
    /// `session/steer` 接受后由 core 分配的 ID；发送途中撤回时仍为 `None`。
    pub core_id: Option<u64>,
    /// `Some` 表示用户已请求撤回，等待 core 回执；该项暂时不渲染。
    pub withdrawal: Option<WithdrawAction>,
}

pub fn submit(app: &mut App, connection: &EmbeddedConnection, queued: bool) {
    if app.prompt.is_empty() {
        return;
    }
    if app.prompt.is_bash_command() && app.prompt.has_attached_images() {
        app.set_notice("Bash 命令不支持图片，请移除图片后执行");
        return;
    }
    // 先确认会话存在，再动输入框：被拒绝的草稿要原样留在用户手里。
    let Some(session) = app.require_session() else {
        return;
    };
    let (draft, history) = app.prompt.take_submitted_draft();
    if let Some(text) = history {
        app.record_input_history(&text);
    }
    enqueue(app, connection, session, draft, queued, None);
    if let Some(stash) = app.prompt_stash.take() {
        app.prompt.restore_draft(stash);
    }
}

/// 直接投递一条现成草稿并指定本次提示模式（`/ask <提示词>` 用），不经输入框。
///
/// 一律按「忙则排队」处理：本次提示的临时模式无法通过 steering 注入正在飞行的提示，
/// 因此它必须成为独立的一次提示。
pub fn submit_with_mode(
    app: &mut App,
    connection: &EmbeddedConnection,
    draft: PromptState,
    mode: SessionMode,
) {
    let Some(session) = app.require_session() else {
        return;
    };
    enqueue(app, connection, session, draft, true, Some(mode));
}

/// 把草稿投递到 `session` 的待发送区（补充、排队或直接派发）。
fn enqueue(
    app: &mut App,
    connection: &EmbeddedConnection,
    session: String,
    draft: PromptState,
    queued: bool,
    mode: Option<SessionMode>,
) {
    app.next_queue_id += 1;
    let prompt = QueuedPrompt {
        id: app.next_queue_id,
        draft,
        mode,
    };
    // 手动命令始终独立执行，绝不经 steering 注入正在运行的 agent。
    let manual = mode.is_none() && prompt.draft.is_bash_command();
    let busy = app.prompt_running || app.inflight_prompts.contains(&session);
    if queued || !busy || manual {
        app.queues.entry(session).or_default().push_back(prompt);
        dispatch(app, connection);
    } else if app.preferences.active_submit == ActiveSubmit::Interrupt {
        app.interrupt_pending.insert(session.clone());
        app.queues.entry(session).or_default().push_front(prompt);
        app.cancel_requested = true;
        crate::event::cancel_prompt(&connection.client, app);
    } else {
        debug_assert!(prompt.mode.is_none(), "临时模式必须走独立队列");
        let parts = prompt.draft.preview_parts();
        let display_text = crate::event::display_text_of(&parts);
        let prompt_id = prompt.id;
        app.steers
            .entry(session.clone())
            .or_default()
            .push_back(PendingSteer {
                id: prompt_id,
                draft: prompt.draft,
                display_text,
                core_id: None,
                withdrawal: None,
            });
        spawn_steer(
            &connection.client,
            session,
            prompt_id,
            parts,
            app.events.clone(),
        );
    }
}

fn spawn_steer(
    client: &ClientHandle,
    session: String,
    prompt_id: u64,
    input: Vec<InputPart>,
    events: UnboundedSender<AppEvent>,
) {
    let client = client.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionSteer>(SteerParams {
                session_id: session.clone(),
                input,
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SteerReturned {
            session,
            prompt_id,
            result,
        });
    });
}

/// 请求 core 撤回一条尚未写入会话的补充消息。
pub fn request_unsteer(
    app: &App,
    connection: &EmbeddedConnection,
    session: String,
    prompt_id: u64,
    core_id: u64,
    action: WithdrawAction,
) {
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionUnsteer>(UnsteerParams {
                session_id: session.clone(),
                id: core_id,
            })
            .await
            .map(|result| result.removed)
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::UnsteerReturned {
            session,
            prompt_id,
            action,
            result,
        });
    });
}

pub fn dispatch(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let steers_pending = app
        .steers
        .get(&session)
        .is_some_and(|entries| !entries.is_empty());
    if app.prompt_running
        || app.inflight_prompts.contains(&session)
        || app.inflight_session_ops.contains(&session)
        || steers_pending
        || app.modal.is_some()
        || app.settings.is_some()
    {
        return;
    }
    let Some(mut prompt) = app
        .queues
        .get_mut(&session)
        .and_then(|queue| queue.pop_front())
    else {
        return;
    };
    let parts = prompt.draft.build_input_parts();
    let command = if prompt.mode.is_none()
        && let [InputPart::Text { text }] = parts.as_slice()
    {
        BashCommand::parse(text)
    } else {
        None
    };
    if command.is_none() {
        app.push_user(crate::event::display_text_of(&parts));
    }
    app.inflight_prompts.insert(session.clone());
    app.prompt_running = true;
    // 本次提示实际生效的模式：覆盖值只在提示内展示，不改写会话状态。
    app.transient_mode = prompt.mode;
    if let Some(command) = command {
        send_bash(&connection.client, session, command, app.events.clone());
    } else {
        send_prompt(
            &connection.client,
            session,
            parts,
            prompt.mode,
            app.events.clone(),
        );
    }
}

pub fn send_prompt(
    client: &ClientHandle,
    session_id: String,
    parts: Vec<InputPart>,
    mode: Option<SessionMode>,
    events: UnboundedSender<AppEvent>,
) {
    let client = client.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionPrompt>(PromptParams {
                session_id: session_id.clone(),
                input: parts,
                mode,
                agent_message: None,
            })
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::PromptReturned {
            session: session_id,
            result,
        });
    });
}

fn send_bash(
    client: &ClientHandle,
    session_id: String,
    command: BashCommand,
    events: UnboundedSender<AppEvent>,
) {
    let client = client.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionBash>(BashParams {
                session_id: session_id.clone(),
                command,
            })
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::PromptReturned {
            session: session_id,
            result,
        });
    });
}

/// 按本地 ID 查找一条待发送补充消息。
pub fn steer_mut<'a>(app: &'a mut App, session: &str, id: u64) -> Option<&'a mut PendingSteer> {
    app.steers
        .get_mut(session)?
        .iter_mut()
        .find(|entry| entry.id == id)
}

/// 按本地 ID 取出一条待发送补充消息。
pub fn take_steer(app: &mut App, session: &str, id: u64) -> Option<PendingSteer> {
    let entries = app.steers.get_mut(session)?;
    let index = entries.iter().position(|entry| entry.id == id)?;
    entries.remove(index)
}

fn take_steer_by_core(app: &mut App, session: &str, core_id: u64) -> Option<PendingSteer> {
    let entries = app.steers.get_mut(session)?;
    let index = entries
        .iter()
        .position(|entry| entry.core_id == Some(core_id))?;
    entries.remove(index)
}

/// 事件先于 RPC 响应时按正文认领最早的一条；只匹配还没挂 core ID 的项。
fn take_unresolved_steer_by_text(app: &mut App, session: &str, text: &str) -> Option<PendingSteer> {
    if text.is_empty() {
        return None;
    }
    let entries = app.steers.get_mut(session)?;
    let index = entries
        .iter()
        .position(|entry| entry.core_id.is_none() && entry.display_text == text)?;
    entries.remove(index)
}

/// core 已把补充消息写入会话：移出待发送区，并在当前会话补一条用户消息。
pub fn steering_applied(app: &mut App, session: &str, id: u64, text: &str, current: bool) {
    if !app
        .resolved_steers
        .entry(session.to_owned())
        .or_default()
        .insert(id)
    {
        return;
    }
    let mut pending = take_steer_by_core(app, session, id);
    if pending.is_none() {
        pending = take_unresolved_steer_by_text(app, session, text);
    }
    match pending {
        Some(entry) => {
            if entry.withdrawal.is_some() {
                app.set_notice("消息已进入本轮上下文，无法撤回");
            }
            if current {
                let body = if entry.display_text.is_empty() {
                    text.to_owned()
                } else {
                    entry.display_text
                };
                app.push_user(if body.is_empty() {
                    "[图片]".to_owned()
                } else {
                    body
                });
            }
        }
        None if current => {
            let body = if text.is_empty() {
                "[图片]".to_owned()
            } else {
                text.to_owned()
            };
            app.push_user(body);
        }
        None => {}
    }
}

/// 把撤回的草稿恢复到输入框；输入框已有待编辑草稿时排回队列，避免覆盖。
pub fn restore_or_requeue(app: &mut App, session: &str, prompt: QueuedPrompt) {
    if app.prompt_stash.is_some() {
        app.set_notice("输入框正在编辑其他消息，撤回的草稿已排回待发送");
        app.queues
            .entry(session.to_owned())
            .or_default()
            .push_front(prompt);
        return;
    }
    if !app.prompt.is_empty() {
        app.prompt_stash = Some(app.prompt.take_draft());
    }
    app.prompt.restore_draft(prompt.draft);
    app.focus = Focus::Prompt;
}

pub fn edit(app: &mut App, connection: &EmbeddedConnection, id: u64) {
    let Some(session) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    if app.prompt_stash.is_some() {
        app.set_notice("请先发送或排队当前正在编辑的消息");
        return;
    }
    if let Some(pending) = app.steers.get_mut(&session).and_then(|entries| {
        entries
            .iter_mut()
            .find(|entry| entry.id == id && entry.withdrawal.is_none())
    }) {
        pending.withdrawal = Some(WithdrawAction::Edit);
        let (prompt_id, core_id) = (pending.id, pending.core_id);
        if let Some(core_id) = core_id {
            request_unsteer(
                app,
                connection,
                session,
                prompt_id,
                core_id,
                WithdrawAction::Edit,
            );
        }
        return;
    }
    let Some(queue) = app.queues.get_mut(&session) else {
        return;
    };
    let Some(index) = queue.iter().position(|entry| entry.id == id) else {
        return;
    };
    let prompt = queue.remove(index).expect("已找到队列项");
    restore_or_requeue(app, &session, prompt);
}

pub fn remove(app: &mut App, connection: &EmbeddedConnection, id: u64) {
    let Some(session) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    if let Some(pending) = app.steers.get_mut(&session).and_then(|entries| {
        entries
            .iter_mut()
            .find(|entry| entry.id == id && entry.withdrawal.is_none())
    }) {
        pending.withdrawal = Some(WithdrawAction::Discard);
        let (prompt_id, core_id) = (pending.id, pending.core_id);
        if let Some(core_id) = core_id {
            request_unsteer(
                app,
                connection,
                session,
                prompt_id,
                core_id,
                WithdrawAction::Discard,
            );
        }
        return;
    }
    if let Some(queue) = app.queues.get_mut(&session) {
        queue.retain(|entry| entry.id != id);
    }
}

pub fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    use ratatui::{
        layout::Rect,
        style::Style,
        text::{Line, Span},
        widgets::{Block, Clear, Paragraph},
    };
    app.queue_hits.clear();
    app.queue_area = ratatui::layout::Rect::default();
    let Some(session) = &app.session else {
        return;
    };
    let steers: Vec<&PendingSteer> = app
        .steers
        .get(&session.id)
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry.withdrawal.is_none())
                .collect()
        })
        .unwrap_or_default();
    let queue_len = app.queues.get(&session.id).map_or(0, |queue| queue.len());
    let total = steers.len() + queue_len;
    if total == 0 {
        return;
    }
    let available = app.prompt_area.y.saturating_sub(2);
    if available < 3 {
        return;
    }
    let visible = total.min(usize::from(available.saturating_sub(2))).min(6);
    debug_assert!(visible <= 6, "队列预览上界为六行");
    let height = u16::try_from(visible).expect("队列视图最多六行") + 2;
    let width = app.prompt_area.width.min(66);
    let area = Rect::new(
        app.prompt_area.right().saturating_sub(width),
        app.prompt_area.y.saturating_sub(height),
        width,
        height,
    );
    app.queue_area = area;
    app.queue_offset = app.queue_offset.min(total.saturating_sub(visible));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::bordered()
            .title(format!(
                " 待发送 {total} · 插队 {} / 队列 {queue_len} · 点击/× 撤回 ",
                steers.len()
            ))
            .style(
                Style::new()
                    .bg(app.theme.bg_base)
                    .fg(app.theme.text_primary),
            ),
        area,
    );

    let mut rows: Vec<(u64, &'static str, usize, String)> = Vec::with_capacity(total);
    for (index, pending) in steers.iter().enumerate() {
        let text = pending.display_text.lines().next().unwrap_or("[图片]");
        rows.push((pending.id, "插队", index + 1, text.to_owned()));
    }
    if let Some(queue) = app.queues.get(&session.id) {
        for (index, prompt) in queue.iter().enumerate() {
            let text = prompt.draft.text.lines().next().unwrap_or("[图片]");
            rows.push((prompt.id, "队列", index + 1, text.to_owned()));
        }
    }
    for (row_index, (id, kind, number, text)) in rows
        .into_iter()
        .skip(app.queue_offset)
        .take(visible)
        .enumerate()
    {
        let row = Rect::new(
            area.x + 1,
            area.y + 1 + u16::try_from(row_index).expect("队列视图最多六行"),
            area.width.saturating_sub(4),
            1,
        );
        let label = if kind == "插队" {
            Style::new().fg(app.theme.accent_running)
        } else {
            Style::new().fg(app.theme.text_primary)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{kind} "), label),
                Span::raw(format!("{number}. {text}")),
            ])),
            row,
        );
        let cancel = Rect::new(area.right().saturating_sub(3), row.y, 2, 1);
        frame.render_widget(
            Paragraph::new("×").style(Style::new().fg(app.theme.gray)),
            cancel,
        );
        app.queue_hits.push((id, row, cancel));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Block;
    use moc_protocol::{SessionInfo, SessionMode, SessionPermission};
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    fn app_with_session() -> App {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.session = Some(SessionInfo {
            id: "ses_test".into(),
            title: "测试".into(),
            model: "test/model".into(),
            permission: SessionPermission::Full,
            mode: SessionMode::Normal,
            parent_session_id: None,
            forked_from: None,
            cwd: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        });
        app
    }

    fn pending(id: u64, text: &str, core_id: Option<u64>) -> PendingSteer {
        PendingSteer {
            id,
            draft: PromptState::from_text(text.into()),
            display_text: text.into(),
            core_id,
            withdrawal: None,
        }
    }

    #[test]
    fn steering_applied_moves_pending_once() {
        let mut app = app_with_session();
        app.steers
            .entry("ses_test".into())
            .or_default()
            .push_back(pending(1, "先保留原文件", Some(7)));
        steering_applied(&mut app, "ses_test", 7, "先保留原文件", true);
        assert!(app.steers["ses_test"].is_empty());
        assert_eq!(app.blocks.len(), 1);
        assert!(matches!(
            &app.blocks[0].block,
            Block::User { text } if text == "先保留原文件"
        ));
        // 同一事件重复到达不得重复写转写。
        steering_applied(&mut app, "ses_test", 7, "先保留原文件", true);
        assert_eq!(app.blocks.len(), 1);
    }

    #[test]
    fn steering_event_before_response_claims_pending_by_text() {
        let mut app = app_with_session();
        app.steers
            .entry("ses_test".into())
            .or_default()
            .push_back(pending(3, "图片消息", None));
        steering_applied(&mut app, "ses_test", 9, "图片消息", true);
        assert!(app.resolved_steers["ses_test"].contains(&9));
        assert_eq!(app.blocks.len(), 1, "事件先到也要先写用户块");
        assert!(app.steers["ses_test"].is_empty(), "按正文认领后立即移除");
    }

    #[test]
    fn steering_event_without_matching_pending_still_records_resolution() {
        let mut app = app_with_session();
        steering_applied(&mut app, "ses_test", 10, "事件文本", true);
        assert!(app.resolved_steers["ses_test"].contains(&10));
        assert_eq!(app.blocks.len(), 1);
    }

    #[test]
    fn non_current_session_does_not_write_transcript() {
        let mut app = app_with_session();
        app.steers
            .entry("ses_other".into())
            .or_default()
            .push_back(pending(4, "别的会话", Some(12)));
        steering_applied(&mut app, "ses_other", 12, "别的会话", false);
        assert!(app.blocks.is_empty());
        assert!(app.steers["ses_other"].is_empty());
        assert!(app.resolved_steers["ses_other"].contains(&12));
    }

    #[test]
    fn pending_panel_shows_steering_and_queue_groups() {
        let mut app = app_with_session();
        app.prompt_area = Rect::new(0, 10, 80, 3);
        app.steers
            .entry("ses_test".into())
            .or_default()
            .push_back(pending(1, "插队内容", Some(7)));
        app.queues
            .entry("ses_test".into())
            .or_default()
            .push_back(QueuedPrompt {
                id: 2,
                draft: PromptState::from_text("排队内容".into()),
                mode: None,
            });
        let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let surface = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = surface.replace(' ', "");
        assert!(compact.contains("待发送2"), "surface:\n{surface}");
        assert!(compact.contains("插队内容"), "surface:\n{surface}");
        assert!(compact.contains("排队内容"), "surface:\n{surface}");
        assert!(compact.contains("插队1."), "surface:\n{surface}");
        assert!(compact.contains("队列1."), "surface:\n{surface}");
    }
}

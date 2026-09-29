//! Enter 补充当前轮次，Ctrl+Enter 排队；只有请求真正收尾后才启动下一条。

use crate::{
    app::{App, Focus},
    config::ActiveSubmit,
    protocol::{AppEvent, EmbeddedConnection},
    widgets::prompt::PromptState,
};
use moc_protocol::{
    ClientHandle, InputPart, PromptParams, SessionPrompt, SessionSteer, SteerParams,
};
use tokio::sync::mpsc::UnboundedSender;

pub struct QueuedPrompt {
    pub id: u64,
    pub draft: PromptState,
}

pub fn submit(app: &mut App, connection: &EmbeddedConnection, queued: bool) {
    if app.prompt.is_empty() {
        return;
    }
    let Some(session) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.set_notice("请先在 /config 配置一个主模型");
        return;
    };
    app.next_queue_id += 1;
    let prompt = QueuedPrompt {
        id: app.next_queue_id,
        draft: app.prompt.take_draft(),
    };
    if let Some(stash) = app.prompt_stash.take() {
        app.prompt.restore_draft(stash);
    }
    let busy = app.turn_running || app.inflight_turns.contains(&session);
    if queued || !busy {
        app.queues.entry(session).or_default().push_back(prompt);
        dispatch(app, connection);
    } else if app.preferences.active_submit == ActiveSubmit::Interrupt {
        app.interrupt_pending.insert(session.clone());
        app.queues.entry(session).or_default().push_front(prompt);
        crate::event::cancel_turn(&connection.client, app);
    } else {
        let client = connection.client.clone();
        let events = app.events.clone();
        let parts = prompt.draft.preview_parts();
        tokio::spawn(async move {
            let result = client
                .call::<SessionSteer>(SteerParams {
                    session_id: session.clone(),
                    input: parts,
                })
                .await
                .map(|result| result.accepted)
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::SteerReturned {
                session,
                prompt,
                result,
            });
        });
    }
}

pub fn dispatch(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    if app.turn_running
        || app.inflight_turns.contains(&session)
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
    app.push_user(crate::event::display_text_of(&parts));
    app.inflight_turns.insert(session.clone());
    app.turn_running = true;
    send_turn(&connection.client, session, parts, app.events.clone());
}

pub fn send_turn(
    client: &ClientHandle,
    session_id: String,
    parts: Vec<InputPart>,
    events: UnboundedSender<AppEvent>,
) {
    let client = client.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionPrompt>(PromptParams {
                session_id: session_id.clone(),
                input: parts,
            })
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::TurnReturned {
            session: session_id,
            result,
        });
    });
}

pub fn edit(app: &mut App, id: u64) {
    let Some(session) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    if app.prompt_stash.is_some() {
        app.set_notice("请先发送或排队当前正在编辑的消息");
        return;
    }
    let Some(queue) = app.queues.get_mut(&session) else {
        return;
    };
    let Some(index) = queue.iter().position(|entry| entry.id == id) else {
        return;
    };
    let prompt = queue.remove(index).expect("已找到队列项");
    if !app.prompt.is_empty() {
        app.prompt_stash = Some(app.prompt.take_draft());
    }
    app.prompt.restore_draft(prompt.draft);
    app.focus = Focus::Prompt;
}

pub fn remove(app: &mut App, id: u64) {
    let Some(session) = app.session.as_ref() else {
        return;
    };
    if let Some(queue) = app.queues.get_mut(&session.id) {
        queue.retain(|entry| entry.id != id);
    }
}

pub fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    use ratatui::{
        layout::Rect,
        style::Style,
        widgets::{Block, Clear, Paragraph},
    };
    app.queue_hits.clear();
    app.queue_area = ratatui::layout::Rect::default();
    let Some(session) = &app.session else {
        return;
    };
    let Some(queue) = app
        .queues
        .get(&session.id)
        .filter(|queue| !queue.is_empty())
    else {
        return;
    };
    let available = app.prompt_area.y.saturating_sub(2);
    if available < 3 {
        return;
    }
    let visible = queue
        .len()
        .min(usize::from(available.saturating_sub(2)))
        .min(6);
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
    app.queue_offset = app.queue_offset.min(queue.len().saturating_sub(visible));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::bordered()
            .title(format!(
                " 后续队列 {} · 滚轮浏览 · 点击编辑 / × 取消 ",
                queue.len()
            ))
            .style(
                Style::new()
                    .bg(app.theme.bg_base)
                    .fg(app.theme.text_primary),
            ),
        area,
    );
    for (index, prompt) in queue
        .iter()
        .skip(app.queue_offset)
        .take(visible)
        .enumerate()
    {
        let row = Rect::new(
            area.x + 1,
            area.y + 1 + u16::try_from(index).expect("队列视图最多六行"),
            area.width.saturating_sub(4),
            1,
        );
        let text = format!(
            "{}. {}",
            index + app.queue_offset + 1,
            prompt.draft.text.lines().next().unwrap_or("[图片]")
        );
        frame.render_widget(Paragraph::new(text), row);
        let cancel = Rect::new(area.right().saturating_sub(3), row.y, 2, 1);
        frame.render_widget(
            Paragraph::new("×").style(Style::new().fg(app.theme.gray)),
            cancel,
        );
        app.queue_hits.push((prompt.id, row, cancel));
    }
}

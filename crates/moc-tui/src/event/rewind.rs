//! 会话回退与重新作答。

use moc_i18n::t;

use super::agents::load_subagents_for_parent;
use super::context::refresh_context;
use super::modal_key::handle_modal_key;
use super::usage::refresh_usage;
use crate::app::App;
use crate::app::Focus;
use crate::history_page::HistoryClient;
use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use anyhow::Context;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use moc_protocol::ClientHandle;
use moc_protocol::ContentPart;
use moc_protocol::ImageSource;
use moc_protocol::Role;
use moc_protocol::SessionLoadPageParams;

pub(super) fn apply_rewind_loaded(
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
            state.error =
                Some(t!("tui.rewind.load_failed_fmt", err = error.to_string()).into_owned());
        }
    }
}

pub(super) fn apply_rewound(
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
                state.error = Some(
                    t!(
                        "tui.rewind.op_failed_fmt",
                        op = if is_redo {
                            t!("tui.rewind.redo")
                        } else {
                            t!("tui.rewind.undo")
                        },
                        err = error.to_string()
                    )
                    .into_owned(),
                );
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
    app.set_modal(None);
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
    app.set_notice(
        if is_redo {
            t!("tui.rewind.redone")
        } else {
            t!("tui.rewind.undone")
        }
        .into_owned(),
    );
    refresh_context(app, connection);
    // 回填的用户消息可能带引用 token；会话也在此处切换，顺带清场旧缓存。
    crate::refs::request_inspection(app, connection);
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
pub(super) fn apply_reasked(
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
            app.set_notice(t!("tui.rewind.retry_failed_fmt", err = error.to_string()).into_owned());
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
    app.set_notice(t!("tui.rewind.retried").into_owned());
    refresh_context(app, connection);
    refresh_usage(app, connection, false);
    load_subagents_for_parent(
        app,
        &connection.client,
        app.session.as_ref().map(|session| session.id.clone()),
    );
    crate::queue::send_retry(&connection.client, session_id, app.events.clone());
}

pub(super) fn restore_user_message(
    prompt: &mut crate::widgets::prompt::PromptState,
    message: &moc_protocol::Message,
) {
    for part in &message.content {
        match part {
            // 附件注入块不是用户输入：恢复输入框时跳过，`@path` 引用已在正文里。
            ContentPart::Text(text) if moc_core::resources::attach::is_attachment(text) => {}
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

pub(super) async fn load_rewind_pages(
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
                    .context(t!("tui.rewind.redo_cursor_missing"))?
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
pub(super) fn open_rewind(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.set_notice(t!("tui.common.no_session").into_owned());
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

/// 消息回退视图的鼠标处理：滚动时间线列表或详情预览；点击选择时间线节点，
/// 再点已选中的节点即确认（与 Enter 同义）。
pub(super) fn handle_rewind_mouse(
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

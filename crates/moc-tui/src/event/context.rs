//! 上下文阅读与工具目录交互。

use crate::app::App;
use crate::modal::ContextContentState;
use crate::modal::ContextDrag;
use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use moc_protocol::SessionContext;
use moc_protocol::SessionContextContent;
use moc_protocol::SessionContextContentParams;

pub(super) fn refresh_context(app: &App, connection: &EmbeddedConnection, notify_failure: bool) {
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

pub(super) fn open_tools(app: &mut App, connection: &EmbeddedConnection) {
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

/// `/context` 与顶部占用条统一打开冻结总览。
pub(super) fn open_context(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.set_notice("当前没有活动会话");
        return;
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let request = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    app.modal = Some(Modal::ContextContent {
        state: Box::new(ContextContentState {
            request,
            session_id: session_id.clone(),
            ..Default::default()
        }),
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
pub(super) fn apply_context_content(
    app: &mut App,
    request: u64,
    result: anyhow::Result<moc_protocol::methods::ContextContentResult>,
) {
    if let Some(Modal::ContextContent { state }) = app.modal.as_mut() {
        if state.request != request {
            return;
        }
        match result {
            Ok(content) => state.set_content(content),
            Err(error) => state.set_error(error.to_string()),
        }
    }
}

pub(super) fn flush_context_loads(app: &mut App, connection: &EmbeddedConnection) {
    let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
        return;
    };
    if state.refresh {
        open_context(app, connection);
        return;
    }
    let Some(content) = &state.content else {
        return;
    };
    let view_id = content.view_id.clone();
    let request = state.request;
    let session_id = state.session_id.clone();
    if std::mem::take(&mut state.copy)
        && let Some(node) = state
            .current
            .as_ref()
            .filter(|node| node.hidden.is_none() && node.kind != "category")
    {
        let node_id = node.id.clone();
        let client = connection.client.clone();
        let events = app.events.clone();
        let view_id = view_id.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let result = async {
                let mut cursor = None;
                let mut text = String::new();
                loop {
                    let page = client
                        .call::<moc_protocol::methods::SessionContextRead>(
                            moc_protocol::methods::SessionContextReadParams {
                                session_id: session_id.clone(),
                                view_id: view_id.clone(),
                                node_id: node_id.clone(),
                                cursor,
                                limit: None,
                            },
                        )
                        .await?;
                    if let Some(body) = page.text {
                        text.push_str(&body);
                    }
                    cursor = page.next_cursor;
                    if cursor.is_none() {
                        break;
                    }
                }
                Ok::<_, anyhow::Error>(text)
            }
            .await;
            let _ = events.send(AppEvent::ContextCopied { request, result });
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

/// 统一上下文面板的鼠标：滚轮滚动、点击进入下一级、拖拽滚动条。
///
/// 几何全部来自上一帧绘制（轨道、滑块、行区间），因此这里只做换算，不重排正文。
pub(super) fn handle_context_content_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) {
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
                state.enter(index);
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

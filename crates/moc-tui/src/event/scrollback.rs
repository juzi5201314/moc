//! 转写区滚动、选中与历史分页。

use super::clipboard::copy_selection;
use crate::app::App;
use crate::app::ChartHit;
use crate::app::Focus;
use crate::app::ScrollbackDrag;
use crate::history_page::HistoryClient;
use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use crate::transcript::DetailLevel;
use moc_protocol::SessionLoadPageParams;

/// 转写区右缘滚动条：按在滑块上保持抓取点拖动，按在轨道空白处把滑块中线挪过来。
///
/// 几何来自上一帧绘制（[`App::scrollbar`]），这里只做换算；命中滚动条的按下与
/// 拖动不进入文本选区。
pub(super) fn handle_scrollbar_mouse(app: &mut App, mouse: &crossterm::event::MouseEvent) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    let bar = app.scrollbar;
    let on_track = bar.max_offset > 0 && bar.track.contains((mouse.column, mouse.row).into());
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
pub(super) fn drag_scroll(app: &mut App, row: u16, grab: i32) {
    app.scroll_offset = app.scrollbar.offset_at(row, grab);
    request_history_if_at_edge(app);
}

/// 鼠标：滚轮滚动、点击折叠、拖选复制。
pub(super) fn handle_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) {
    use crossterm::event::{MouseButton, MouseEventKind};
    app.scrollbar_pointer = Some((mouse.column, mouse.row));
    // 新的一次按下总是结束上一轮滚动条拖拽：拖拽途中弹出模态时抬起事件会被吞掉。
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        app.scrollbar_drag = None;
        app.selection = None;
        app.pending_click = None;
        app.selection_copied_at = None;
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
            // 「回到底部」浮动胶囊：按下即贴尾，不进入文本选区。
            if app.jump_bottom_area.contains(position.into()) {
                app.scroll_offset = 0;
                return;
            }
            let area = app.prompt_area;
            if position.0 >= area.x
                && position.0 < area.x.saturating_add(area.width)
                && position.1 >= area.y
                && position.1 < area.y.saturating_add(area.height)
            {
                app.focus = Focus::Prompt;
            }
            app.pending_click = Some(position);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(start) = app.pending_click {
                app.selection = Some((start, position));
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let clicked = app.pending_click.take() == Some(position);
            if clicked {
                if app.prompt_area.contains(position.into()) {
                    // 单击才移动光标；按下即移动会让长输入的视口在拖选前跳动。
                    if let Some(cursor) = crate::render::prompt_cursor_from_point(
                        app,
                        app.prompt_area,
                        position.0,
                        position.1,
                    ) {
                        app.prompt.cursor = cursor;
                        crate::refs::sync_prompt_atomic(app);
                    }
                    app.selection = None;
                    return;
                }
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
            } else if let Some((_, end)) = app.selection.as_mut() {
                *end = position;
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
pub(super) fn chart_hit_at(app: &App, position: (u16, u16)) -> Option<ChartHit> {
    app.chart_hits.iter().copied().find(|hit| {
        hit.row == position.1 && position.0 >= hit.x && position.0 < hit.x.saturating_add(hit.w)
    })
}

/// 坐标命中的参考链接折叠入口（列区间限定在按钮文字上）。
pub(super) fn link_hit_at(app: &App, position: (u16, u16)) -> Option<crate::app::LinkHit> {
    app.link_hits.iter().copied().find(|hit| {
        hit.row == position.1 && position.0 >= hit.x && position.0 < hit.x.saturating_add(hit.w)
    })
}

/// 滚动视口：`delta > 0` 向历史方向推进（更早的行），`delta < 0` 回到尾部。
///
/// [`App::scroll_offset`] 的语义是「距尾部行数」，因此正向 delta 直接增大偏移。
pub(super) fn request_history_page(app: &mut App, connection: &EmbeddedConnection) {
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

pub(super) fn apply_history_page(
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

pub(super) fn scroll(app: &mut App, delta: i32) {
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
pub(super) fn request_history_if_at_edge(app: &mut App) {
    if app.scroll_offset >= app.scroll_max_offset && app.history_has_more {
        app.history_load_requested = true;
    }
}

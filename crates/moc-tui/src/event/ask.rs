//! 提问卡片的作答与鼠标交互。

use super::ASK_PREVIEW_SCROLL_STEP;
use super::scrollback::handle_mouse;
use super::scrollback::handle_scrollbar_mouse;
use super::scrollback::scroll;
use crate::app::App;
use crate::modal::Modal;
use crate::protocol::EmbeddedConnection;

/// 取出问题默认高亮的选项下标：推荐项优先，没有推荐停在第一项——
/// 用户直接回车选中的就是模型建议的答案。
pub(super) fn recommended_highlight(question: &moc_protocol::AskQuestion) -> usize {
    question
        .recommended
        .first()
        .map_or(0, |index| *index as usize)
}

/// 提问卡片的鼠标处理。
/// 作答态：点选项即选择（单选直接推进、多选切换勾选），点自定义行进入编辑，
/// 点导航条跳问（「确认」项落确认页、确认页上再点即提交），点转写区或按
/// 滚动条折叠；滚轮按悬停区分流——落在可滚文本区（溢出的问题正文窗/详情区）
/// 滚该区，其余位置与 ↑↓ 同语义移动选项光标。
/// 折叠态：点细条展开回作答，其余鼠标行为与转写区一致（滚动/拖选/滚动条）。
/// 命中矩形由渲染层每帧写入 `ask_option_hits`/`ask_nav_hits`/`ask_card_area`/
/// `ask_question_area`/`ask_detail_area`/`ask_strip_area`；未命中区域不响应。
pub(super) fn handle_ask_mouse(
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
                    app.show_next_pending_modal();
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

/// 写入一次选项选择：多选切换勾选，单选整体覆盖（返回 true 表示已就绪，调用方据此推进）。
/// 空格、数字直达与鼠标点选共用这一份语义。
pub(super) fn pick_ask_option(
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
pub(super) fn dismiss_ask(
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
pub(super) fn advance_ask(
    question: &mut usize,
    preview_scroll: &mut usize,
    question_scroll: &mut usize,
) {
    *question += 1;
    *preview_scroll = 0;
    *question_scroll = 0;
}

/// 确认页提交：按各问勾选与自定义文本组装 AskResult 回给 core；返回是否应关闭模态。
pub(super) fn submit_ask(
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

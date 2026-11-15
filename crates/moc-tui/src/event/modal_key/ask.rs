//! 提问作答的键盘交互。

use rust_i18n::t;

use super::Actions;
use crate::app::App;
use crate::event::ASK_PREVIEW_SCROLL_STEP;
use crate::event::advance_ask;
use crate::event::dismiss_ask;
use crate::event::pick_ask_option;
use crate::event::scroll;
use crate::event::submit_ask;
use crate::event::write_osc52;
use crate::modal::Modal;
use crate::widgets::prompt::clamp_cursor;
use crate::widgets::prompt::cursor_next;
use crate::widgets::prompt::cursor_prev;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;

pub(super) fn handle(
    app: &mut App,
    modifiers: KeyModifiers,
    code: KeyCode,
    actions: &mut Actions,
) -> bool {
    let Some(modal) = app.modal.as_mut() else {
        return false;
    };
    match modal {
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
                actions.close = true;
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
                        actions.close |= dismiss_ask(reply);
                    }
                    (KeyCode::Esc, _) => {
                        // 折叠后再退一次才取消整单（core 侧按「用户没有回答」收场）。
                        if let Some(reply) = reply.take() {
                            let _ = reply.send(Err(moc_protocol::RpcError::of(
                                moc_protocol::RpcErrorCode::RequestCancelled,
                            )));
                        }
                        actions.close = true;
                    }
                    _ => {}
                }
            } else if *question >= params.questions.len() {
                // 确认页：越过最后一问强制经过，提交前最后一次通览全部回答。
                // enter/tab 确认提交；←/shift+tab 回上一问；↑↓ 滚动答案列表（复用
                // preview_scroll 当滚动偏移）；esc 折叠；shift+X 整单跳过。
                match (code, modifiers) {
                    (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Tab, KeyModifiers::NONE) => {
                        actions.close |= submit_ask(params, selected, custom, reply);
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
                        actions.close |= dismiss_ask(reply);
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
                                app.set_notice(t!("tui.event.option_copied").into_owned());
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
                            actions.close |= dismiss_ask(reply);
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
        _ => unreachable!("模态路由必须与处理器一致"),
    }
    true
}

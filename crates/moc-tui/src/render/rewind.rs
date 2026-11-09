//! 消息回退视图：受 omp (oh-my-pi) 启发的全景时间线回退选择器与预览面板。
//!
//! 支持：
//! - 线性时间线逐条导航（↑↓ / jk）与用户提问快速跳转（←→ / hl）
//! - 搜索过滤（f / / 触发，支持多关键词实时匹配）
//! - 清晰的保留 vs 丢弃分界线与醒目的回退横幅（Rewind Banner）
//! - 选中文档与提问正文预览（支持滚动）
//! - omp 风格的底部 Dock 操作栏

use chrono::TimeZone;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::modal::{Modal, RewindState};
use crate::render::labels::KeyHit;
use crate::render::wrap::{str_width, wrap_text};
use crate::theme::Theme;

/// 计算回退模态区域：宽屏下占据约 92% 宽、88% 高，留出舒适的边缘。
fn modal_area(area: Rect) -> Rect {
    let width = (area.width * 92 / 100)
        .clamp(44, 150)
        .min(area.width.saturating_sub(2));
    let height = (area.height * 88 / 100)
        .clamp(16, 48)
        .min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// 格式化时间戳为本地 HH:MM:SS
fn format_clock(at_ms: u64) -> String {
    if at_ms == 0 {
        return "--:--:--".to_string();
    }
    let Some(millis) = i64::try_from(at_ms).ok() else {
        return "--:--:--".to_string();
    };
    chrono::Local
        .timestamp_millis_opt(millis)
        .single()
        .map(|at| at.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_string())
}

/// 格式化时间戳为简短的 HH:MM
fn format_short_clock(at_ms: u64) -> String {
    if at_ms == 0 {
        return String::new();
    }
    let Some(millis) = i64::try_from(at_ms).ok() else {
        return String::new();
    };
    chrono::Local
        .timestamp_millis_opt(millis)
        .single()
        .map(|at| at.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// 绘制回退视图
pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let win = modal_area(area);
    app.modal_card_area = win;

    // 降低外部背景对比度
    frame
        .buffer_mut()
        .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
    super::overlay::paint_opaque(frame.buffer_mut(), win, Style::new().bg(theme.bg_base));

    // 外层边框
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_set(ratatui::symbols::border::ROUNDED)
        .border_style(Style::new().fg(theme.accent_user))
        .style(Style::new().bg(theme.bg_base))
        .title(Span::styled(
            " ⏪ 会话消息回退 (Rewind) ",
            Style::new()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(outer_block, win);

    let inner = Rect {
        x: win.x + 2,
        y: win.y + 1,
        width: win.width.saturating_sub(4),
        height: win.height.saturating_sub(2),
    };

    if inner.height < 6 || inner.width < 20 {
        return;
    }

    let session_title = app
        .session
        .as_ref()
        .map(|s| s.title.clone())
        .unwrap_or_else(|| "未命名会话".to_string());

    // 垂直切分：顶部说明(2)、主内容区、底部 Dock(2)
    let header_height = if inner.height >= 16 { 2 } else { 1 };
    let dock_height = 2;
    let body_height = inner.height.saturating_sub(header_height + dock_height);

    let header_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: header_height,
    };
    let body_area = Rect {
        x: inner.x,
        y: inner.y + header_height,
        width: inner.width,
        height: body_height,
    };
    let dock_area = Rect {
        x: inner.x,
        y: inner.y + header_height + body_height,
        width: inner.width,
        height: dock_height,
    };

    if body_area.width >= 72 {
        let left_width = (body_area.width * 42 / 100).clamp(32, 58);
        app.rewind_timeline_area = Rect {
            x: body_area.x,
            y: body_area.y,
            width: left_width,
            height: body_area.height,
        };
        app.rewind_detail_area = Rect {
            x: body_area.x + left_width + 1,
            y: body_area.y,
            width: body_area.width.saturating_sub(left_width + 1),
            height: body_area.height,
        };
    } else {
        app.rewind_timeline_area = body_area;
        app.rewind_detail_area = Rect::default();
    }

    let Some(Modal::Rewind { state }) = app.modal.as_mut() else {
        return;
    };

    // 1. 顶部向导文字
    draw_header(frame, &session_title, state, header_area, theme);

    // 2. 主体区（时间线 + 详情）
    draw_body(frame, state, body_area, theme);

    // 3. 底部 Dock 操作栏（omp 风格）；键位提示可点。
    app.key_hits = draw_dock(frame, state, dock_area, theme);
}

fn draw_header(
    frame: &mut Frame,
    session_title: &str,
    state: &RewindState,
    area: Rect,
    theme: &Theme,
) {
    let total_nodes = state.items.len();

    let mut lines = Vec::new();
    let mut row1 = vec![
        Span::styled(
            "会话回退向导",
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::new().fg(theme.gray_dim)),
        Span::styled(
            format!("当前会话：「{session_title}」"),
            Style::new().fg(theme.text_secondary),
        ),
    ];
    if total_nodes > 0 {
        row1.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
        row1.push(Span::styled(
            format!("共 {total_nodes} 个历史节点"),
            Style::new()
                .fg(theme.accent_model)
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(row1));

    if area.height >= 2 {
        lines.push(Line::from(Span::styled(
            crate::render::wrap::truncate(
                "确认后该条用户消息回到输入框重新编辑，之后的对话将被丢弃。",
                usize::from(area.width),
            )
            .into_owned(),
            Style::new().fg(theme.gray_dim),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_body(frame: &mut Frame, state: &mut RewindState, area: Rect, theme: &Theme) {
    if state.loading && state.items.is_empty() {
        let text = vec![
            Line::from(""),
            Line::from(Span::styled(
                "正在加载会话消息历史，请稍候…",
                Style::new().fg(theme.accent_running),
            )),
        ];
        frame.render_widget(Paragraph::new(text), area);
        return;
    }

    if let Some(error) = &state.error {
        let text = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("❌ 加载失败：{error}"),
                Style::new()
                    .fg(theme.accent_error)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "按 Enter 重试，或按 Esc 关闭窗口",
                Style::new().fg(theme.gray_dim),
            )),
        ];
        frame.render_widget(Paragraph::new(text), area);
        return;
    }

    if state.items.is_empty() {
        let text = vec![
            Line::from(""),
            Line::from(Span::styled(
                "当前会话暂无可以回退的消息记录",
                Style::new().fg(theme.gray),
            )),
        ];
        frame.render_widget(Paragraph::new(text), area);
        return;
    }

    // 双列布局：左侧时间线列表，右侧回退影响与详情预览
    if area.width >= 72 {
        let left_width = (area.width * 42 / 100).clamp(32, 58);
        let left_area = Rect {
            x: area.x,
            y: area.y,
            width: left_width,
            height: area.height,
        };
        let right_area = Rect {
            x: area.x + left_width + 1,
            y: area.y,
            width: area.width.saturating_sub(left_width + 1),
            height: area.height,
        };

        draw_timeline_list(frame, state, left_area, theme);
        draw_detail_panel(frame, state, right_area, theme);
    } else {
        // 紧凑窄屏：单列列表
        draw_timeline_list(frame, state, area, theme);
    }
}

/// 绘制左侧节点时间线列表
fn draw_timeline_list(frame: &mut Frame, state: &mut RewindState, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(ratatui::symbols::border::ROUNDED)
        .border_style(Style::new().fg(theme.border))
        .title(Span::styled(
            " 节点时间线 (Timeline) ",
            Style::new()
                .fg(theme.text_secondary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let matches = state.filtered_indices();
    if matches.is_empty() {
        let text = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("未找到匹配「{}」的节点", state.query),
                Style::new().fg(theme.gray),
            )),
            Line::from(Span::styled(
                "按 Esc 清空过滤条件",
                Style::new().fg(theme.gray_dim),
            )),
        ];
        frame.render_widget(Paragraph::new(text), inner);
        return;
    }

    let item_height = if inner.height >= 14 { 2 } else { 1 };
    let visible_count = usize::from(inner.height) / item_height;
    let selected_pos = matches
        .iter()
        .position(|&idx| idx == state.selected)
        .unwrap_or(0);

    // 选中项移动时收进窗内（键盘/点击/搜索置 reveal）；滚轮只动 list_scroll，
    // 普通重绘不把视口拉回选中项。
    if state.scroll.reveal_selection {
        if selected_pos < state.list_scroll {
            state.list_scroll = selected_pos;
        }
        if selected_pos >= state.list_scroll + visible_count && visible_count > 0 {
            state.list_scroll = selected_pos.saturating_sub(visible_count - 1);
        }
    }
    state.scroll.max_offset = matches.len().saturating_sub(visible_count);
    state.list_scroll = state.list_scroll.min(state.scroll.max_offset);
    state.scroll.reveal_selection = false;

    let total_items = state.items.len();
    let mut lines = Vec::new();

    for &match_idx in matches.iter().skip(state.list_scroll).take(visible_count) {
        let item = &state.items[match_idx];
        let is_selected = match_idx == state.selected;
        let is_kept = match_idx < state.selected;
        let is_dropped = match_idx > state.selected;

        let (marker, status_tag, tag_style) = if is_selected {
            if item.future {
                (
                    "❯ ⏩ ",
                    "[前滚目标]",
                    Style::new()
                        .fg(theme.accent_model)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    "❯ ⏪ ",
                    "[回退目标]",
                    Style::new()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                )
            }
        } else if item.future {
            if is_kept {
                ("  ↪  ", "[将接回]", Style::new().fg(theme.accent_success))
            } else {
                ("  ⏩  ", "[已回退]", Style::new().fg(theme.gray_dim))
            }
        } else if is_kept {
            ("  ✓  ", "[保留]", Style::new().fg(theme.accent_success))
        } else {
            ("  ✗  ", "[丢弃]", Style::new().fg(theme.gray_dim))
        };

        let index_str = format!("[{}/{}] ", match_idx + 1, total_items);
        let role_style = match item.role {
            moc_protocol::Role::User => Style::new()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
            moc_protocol::Role::Assistant => Style::new()
                .fg(theme.accent_model)
                .add_modifier(Modifier::BOLD),
            moc_protocol::Role::Tool => Style::new().fg(theme.text_secondary),
            _ => Style::new().fg(theme.gray_dim),
        };

        let clock_str = format_short_clock(item.at_ms);

        let row1_base_style = if is_selected {
            Style::new().bg(theme.bg_visual)
        } else if is_dropped {
            Style::new().fg(theme.gray_dim)
        } else {
            Style::new()
        };

        let mut row1_spans = vec![
            Span::styled(
                marker,
                if is_selected {
                    Style::new()
                        .fg(theme.accent_user)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else if is_kept {
                    Style::new().fg(theme.accent_success)
                } else {
                    Style::new().fg(theme.gray_dim)
                },
            ),
            Span::styled(
                index_str,
                if is_selected {
                    Style::new()
                        .fg(theme.text_primary)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.gray_dim)
                },
            ),
            Span::styled(
                format!("{} ", item.role_label()),
                if is_selected {
                    role_style.bg(theme.bg_visual)
                } else if is_dropped {
                    Style::new().fg(theme.gray_dim)
                } else {
                    role_style
                },
            ),
            Span::styled(
                status_tag,
                if is_selected {
                    tag_style.bg(theme.bg_visual)
                } else {
                    tag_style
                },
            ),
        ];

        if !clock_str.is_empty() {
            let used_w: usize = row1_spans.iter().map(|s| str_width(&s.content)).sum();
            let pad = usize::from(inner.width).saturating_sub(used_w + str_width(&clock_str) + 1);
            if pad > 0 {
                row1_spans.push(Span::styled(" ".repeat(pad), row1_base_style));
            }
            row1_spans.push(Span::styled(
                clock_str,
                if is_selected {
                    Style::new().fg(theme.text_secondary).bg(theme.bg_visual)
                } else {
                    Style::new().fg(theme.gray_dim)
                },
            ));
        }

        lines.push(Line::from(row1_spans));

        if item_height >= 2 {
            let snippet = crate::render::wrap::truncate(
                item.preview.as_str(),
                usize::from(inner.width).saturating_sub(6),
            )
            .into_owned();
            let row2_style = if is_selected {
                Style::new().fg(theme.text_primary).bg(theme.bg_visual)
            } else if is_dropped {
                Style::new().fg(theme.gray_dim)
            } else {
                Style::new().fg(theme.text_secondary)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    "       ",
                    if is_selected {
                        Style::new().bg(theme.bg_visual)
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(snippet, row2_style),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);

    // 悬停只铺底色：pointer 坐标由事件层记录，选中项保持自己的高亮。
    if let Some(pos) = state.pointer.filter(|p| inner.contains(*p))
        && let item_pos = usize::from(pos.y - inner.y) / item_height
        && let Some(&match_idx) = matches.get(item_pos + state.list_scroll)
        && match_idx != state.selected
    {
        let y = inner.y + (item_pos * item_height) as u16;
        let height = (item_height as u16).min(inner.bottom().saturating_sub(y));
        if height > 0 {
            frame.buffer_mut().set_style(
                Rect::new(inner.x, y, inner.width, height),
                Style::new().bg(theme.bg_hover),
            );
        }
    }
}

/// 绘制右侧详情与回退影响面板
fn draw_detail_panel(frame: &mut Frame, state: &mut RewindState, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(ratatui::symbols::border::ROUNDED)
        .border_style(Style::new().fg(theme.border))
        .title(Span::styled(
            " 回退影响与内容详情 ",
            Style::new()
                .fg(theme.text_secondary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 6 || inner.width < 10 {
        return;
    }

    let Some(current_item) = state.items.get(state.selected) else {
        return;
    };

    let total = state.items.len();
    let dropped_count = total.saturating_sub(state.selected + 1);

    // 1. 顶部 Rewind Banner (omp 风格)
    let banner_h = 5.min(inner.height);
    let banner_rect = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: banner_h,
    };

    let banner_border_color = if current_item.is_user {
        theme.accent_user
    } else {
        theme.accent_model
    };

    let banner_block = Block::default()
        .borders(Borders::ALL)
        .border_set(ratatui::symbols::border::ROUNDED)
        .border_style(Style::new().fg(banner_border_color))
        .style(Style::new().bg(theme.bg_visual));

    let banner_inner = banner_block.inner(banner_rect);
    frame.render_widget(banner_block, banner_rect);

    let mut banner_lines = Vec::new();
    if current_item.future {
        // 前滚：把已回退的尾部原样接回，选中的节点及其之前的历史条目全部保留。
        let restored_count = total
            .saturating_sub(state.future_start)
            .saturating_sub(total.saturating_sub(state.selected + 1));
        banner_lines.push(Line::from(vec![
            Span::styled(
                "⏩ ",
                Style::new()
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "前滚到此处 (Redo to here)",
                Style::new()
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        banner_lines.push(Line::from(Span::styled(
            format!("• 将把已回退的历史向前接回，恢复该节点及之前共 {restored_count} 条"),
            Style::new().fg(theme.text_primary),
        )));
        if dropped_count > 0 {
            banner_lines.push(Line::from(Span::styled(
                format!("• 该节点之后仍有 {dropped_count} 条保持回退状态，可继续前滚"),
                Style::new().fg(theme.accent_warning),
            )));
        } else {
            banner_lines.push(Line::from(Span::styled(
                "• 已回退的内容将全部接回，恢复到回退前的最新状态",
                Style::new().fg(theme.accent_success),
            )));
        }
    } else if current_item.is_user {
        banner_lines.push(Line::from(vec![
            Span::styled(
                "⏪ ",
                Style::new()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "回退到此处 (Rewind to here)",
                Style::new()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        banner_lines.push(Line::from(Span::styled(
            "• 该用户提问将完整恢复到底部输入框，供您修改后重新发送",
            Style::new().fg(theme.text_primary),
        )));
        if dropped_count > 0 {
            banner_lines.push(Line::from(Span::styled(
                format!("• 下方共 {dropped_count} 条对话记录将被丢弃（不再计入会话历史）"),
                Style::new().fg(theme.accent_warning),
            )));
        } else {
            banner_lines.push(Line::from(Span::styled(
                "• 当前节点已经是最新消息，下方无多余对话丢弃",
                Style::new().fg(theme.accent_success),
            )));
        }
    } else {
        banner_lines.push(Line::from(vec![
            Span::styled(
                "⏪ ",
                Style::new()
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "从此处继续 (Continue from here)",
                Style::new()
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        banner_lines.push(Line::from(Span::styled(
            "• 会话历史将截断至该回复节点，您可以继续向模型提问",
            Style::new().fg(theme.text_primary),
        )));
        if dropped_count > 0 {
            banner_lines.push(Line::from(Span::styled(
                format!("• 下方共 {dropped_count} 条对话记录将被丢弃（不再计入会话历史）"),
                Style::new().fg(theme.accent_warning),
            )));
        } else {
            banner_lines.push(Line::from(Span::styled(
                "• 当前节点已经是最新消息，下方无多余对话丢弃",
                Style::new().fg(theme.accent_success),
            )));
        }
    }
    frame.render_widget(Paragraph::new(banner_lines), banner_inner);

    // 2. 元信息与消息正文预览
    let content_y = inner.y + banner_h + 1;
    let content_h = inner.height.saturating_sub(banner_h + 1);
    if content_h == 0 {
        return;
    }

    let content_area = Rect {
        x: inner.x,
        y: content_y,
        width: inner.width,
        height: content_h,
    };

    let mut detail_lines = Vec::new();
    let meta_spans = vec![
        Span::styled("节点信息：", Style::new().fg(theme.gray_dim)),
        Span::styled(
            format!("{} · ", current_item.role_label()),
            if current_item.is_user {
                Style::new()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD)
            },
        ),
        Span::styled("记录时间：", Style::new().fg(theme.gray_dim)),
        Span::styled(
            format!("{} · ", format_clock(current_item.at_ms)),
            Style::new().fg(theme.text_secondary),
        ),
        Span::styled("条目ID：", Style::new().fg(theme.gray_dim)),
        Span::styled(
            crate::render::wrap::truncate(current_item.entry_id.as_str(), 14).into_owned(),
            Style::new().fg(theme.gray_dim),
        ),
    ];
    detail_lines.push(Line::from(meta_spans));

    if !current_item.tool_calls.is_empty() {
        detail_lines.push(Line::from(vec![
            Span::styled("调用工具：", Style::new().fg(theme.gray_dim)),
            Span::styled(
                current_item.tool_calls.join("、"),
                Style::new().fg(theme.text_primary),
            ),
        ]));
    }

    detail_lines.push(Line::from(""));
    detail_lines.push(Line::from(Span::styled(
        "消息正文预览：",
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    )));

    let wrap_w = usize::from(content_area.width).saturating_sub(2).max(10);
    let wrapped_body = wrap_text(&current_item.full_text, wrap_w);
    let total_body_lines = wrapped_body.len();

    let text_lines_available =
        usize::from(content_area.height).saturating_sub(detail_lines.len() + 1);
    let scroll = state.detail_scroll.min(total_body_lines.saturating_sub(1));

    for line_str in wrapped_body
        .into_iter()
        .skip(scroll)
        .take(text_lines_available)
    {
        detail_lines.push(Line::from(Span::styled(
            format!("  {line_str}"),
            Style::new().fg(theme.text_primary),
        )));
    }

    if total_body_lines > scroll + text_lines_available && text_lines_available > 0 {
        let remaining = total_body_lines.saturating_sub(scroll + text_lines_available);
        detail_lines.push(Line::from(Span::styled(
            format!("  ▼ （还有 {remaining} 行内容 · 按 PgUp/PgDn 或滚轮滚动预览）"),
            Style::new().fg(theme.accent_model),
        )));
    }

    frame.render_widget(Paragraph::new(detail_lines), content_area);
}

/// 绘制底部 Dock 操作栏（omp 风格）；返回其中可点击键位（Esc / Enter / 搜索）的命中区。
fn draw_dock(frame: &mut Frame, state: &RewindState, area: Rect, theme: &Theme) -> Vec<KeyHit> {
    if area.height == 0 || area.width == 0 {
        return Vec::new();
    }

    let mut spans = Vec::new();
    let mut keys = Vec::new();
    let mut column = 0u16;
    // 逐段累计列宽：带键位的段整段可点，被右缘裁掉的段不登记。
    let mut push = |span: Span<'static>, key: Option<KeyCode>| {
        let width = u16::try_from(span.width()).unwrap_or(u16::MAX);
        if let Some(code) = key
            && column.saturating_add(width) <= area.width
        {
            keys.push(KeyHit {
                area: Rect::new(area.x + column, area.y, width, 1),
                modifiers: KeyModifiers::NONE,
                code,
            });
        }
        column = column.saturating_add(width);
        spans.push(span);
    };
    let sep = || Span::styled(" · ", Style::new().fg(theme.gray_dim));

    // 回退徽标
    let badge = Span::styled(
        " ⏪ 回退 ",
        Style::new()
            .bg(theme.accent_user)
            .fg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
    );
    // 徽标 + 其后一格空白。
    let prefix_width = badge.width() + 1;
    push(badge, None);
    push(Span::raw(" "), None);

    // 项列表：(span 段, 键位, 丢弃优先级)；u8::MAX = 必须保留，数字越小越先丢。
    // 段内不含分隔点，渲染时只在相邻的保留段之间插入，丢项后不会出现孤点或双点。
    let mut segments: Vec<(Vec<Span<'static>>, Option<KeyCode>, u8)> = Vec::new();
    if state.filtering {
        let matches = state.filtered_indices();
        segments.push((
            vec![
                Span::styled(
                    format!("过滤: \"{}\"", state.query),
                    Style::new()
                        .fg(theme.accent_warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" (匹配 {}/{})", matches.len(), state.items.len()),
                    Style::new().fg(theme.text_secondary),
                ),
            ],
            None,
            u8::MAX,
        ));
        segments.push((
            vec![Span::styled(
                "↑↓ 切换匹配",
                Style::new().fg(theme.text_primary),
            )],
            None,
            0,
        ));
        segments.push((
            vec![Span::styled(
                "Esc 清除过滤",
                Style::new().fg(theme.gray_dim),
            )],
            Some(KeyCode::Esc),
            u8::MAX,
        ));
        segments.push((
            vec![enter_hint_span(state, theme)],
            Some(KeyCode::Enter),
            u8::MAX,
        ));
    } else {
        if !state.items.is_empty() {
            segments.push((
                vec![Span::styled(
                    format!("{}/{}", state.selected + 1, state.items.len()),
                    Style::new()
                        .fg(theme.accent_model)
                        .add_modifier(Modifier::BOLD),
                )],
                None,
                u8::MAX,
            ));
        }
        segments.push((
            vec![Span::styled(
                "↑↓/jk 逐条选择",
                Style::new().fg(theme.text_primary),
            )],
            None,
            1,
        ));
        segments.push((
            vec![Span::styled(
                "←→/hl 仅跳用户提问",
                Style::new().fg(theme.accent_user),
            )],
            None,
            0,
        ));
        segments.push((
            vec![Span::styled(
                "f 或 / 搜索",
                Style::new().fg(theme.text_secondary),
            )],
            Some(KeyCode::Char('f')),
            2,
        ));
        // 选中含提问调用的历史条目时提示可重新作答。
        if state.selected_item_ref().is_some_and(|item| {
            !item.future
                && item
                    .entry
                    .message
                    .tool_calls()
                    .any(|call| call.name == "ask")
        }) {
            segments.push((
                vec![Span::styled(
                    "R 重新作答",
                    Style::new().fg(theme.accent_model),
                )],
                Some(KeyCode::Char('r')),
                3,
            ));
        }
        segments.push((
            vec![Span::styled("Esc 取消", Style::new().fg(theme.gray_dim))],
            Some(KeyCode::Esc),
            u8::MAX,
        ));
        segments.push((
            vec![enter_hint_span(state, theme)],
            Some(KeyCode::Enter),
            u8::MAX,
        ));
    }
    // 宽度不够时按优先级丢次要项，最重要的项恒在；总宽含徽标与段间分隔点。
    let sep_width = sep().width();
    let total = |segments: &[(Vec<Span<'static>>, Option<KeyCode>, u8)]| -> usize {
        prefix_width
            + segments
                .iter()
                .flat_map(|(spans, _, _)| spans.iter().map(Span::width))
                .sum::<usize>()
            + sep_width * segments.len().saturating_sub(1)
    };
    for rank in 0..=3u8 {
        if total(&segments) <= usize::from(area.width) {
            break;
        }
        if let Some(index) = segments.iter().position(|(_, _, r)| *r == rank) {
            segments.remove(index);
        }
    }
    for (index, (segs, key, _)) in segments.into_iter().enumerate() {
        if index > 0 {
            push(sep(), None);
        }
        let last = segs.len() - 1;
        for (position, span) in segs.into_iter().enumerate() {
            // 键位只登记在段尾的文字上。
            push(span, (position == last).then_some(key).flatten());
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    keys
}

/// 底部 Dock 的确认键提示：选中已回退条目时是前滚，否则是回退。
fn enter_hint_span<'a>(state: &RewindState, theme: &Theme) -> Span<'a> {
    let is_redo = state.selected_item_ref().is_some_and(|item| item.future);
    Span::styled(
        if is_redo {
            "Enter 前滚到此处"
        } else {
            "Enter 回退到此处"
        },
        Style::new()
            .fg(if is_redo {
                theme.accent_model
            } else {
                theme.accent_user
            })
            .add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::{Message, TranscriptEntry};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn rewind_modal_panels_render_with_rounded_borders() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let mut rewind_state = RewindState::default();
        rewind_state.set_entries(
            vec![
                TranscriptEntry {
                    id: "entry-1".into(),
                    at_ms: 1700000000000,
                    message: Message::user_text("提问一：详细提问内容"),
                    tool: None,
                    bash: None,
                    agent: None,
                    goal_round: None,
                    reminder: None,
                },
                TranscriptEntry {
                    id: "entry-2".into(),
                    at_ms: 1700000001000,
                    message: Message::assistant_text("回复一：详细回复内容"),
                    tool: None,
                    bash: None,
                    agent: None,
                    goal_round: None,
                    reminder: None,
                },
            ],
            vec![],
        );
        rewind_state.selected = 0;
        app.modal = Some(Modal::Rewind {
            state: rewind_state,
        });

        let (width, height) = (100u16, 30u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, &mut app, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let win = modal_area(Rect::new(0, 0, width, height));
        assert_eq!(buffer.cell((win.x, win.y)).unwrap().symbol(), "╭");
        assert_eq!(buffer.cell((win.right() - 1, win.y)).unwrap().symbol(), "╮");
        assert_eq!(
            buffer.cell((win.x, win.bottom() - 1)).unwrap().symbol(),
            "╰"
        );
        assert_eq!(
            buffer
                .cell((win.right() - 1, win.bottom() - 1))
                .unwrap()
                .symbol(),
            "╯"
        );

        let all_symbols: String = (0..height)
            .flat_map(|y| (0..width).map(move |x| buffer.cell((x, y)).unwrap().symbol()))
            .collect();
        assert!(all_symbols.contains('╭'), "必须包含圆角 ╭");
        assert!(all_symbols.contains('╮'), "必须包含圆角 ╮");
        assert!(all_symbols.contains('╰'), "必须包含圆角 ╰");
        assert!(all_symbols.contains('╯'), "必须包含圆角 ╯");
        assert!(!all_symbols.contains('┌'), "不应含有直角 ┌");
        assert!(!all_symbols.contains('┐'), "不应含有直角 ┐");
        assert!(!all_symbols.contains('└'), "不应含有直角 └");
        assert!(!all_symbols.contains('┘'), "不应含有直角 ┘");
    }
}

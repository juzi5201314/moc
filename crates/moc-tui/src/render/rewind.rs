//! 消息回退视图：受 omp (oh-my-pi) 启发的全景时间线回退选择器与预览面板。
//!
//! 支持：
//! - 线性时间线逐条导航（↑↓ / jk）与用户提问快速跳转（←→ / hl）
//! - 搜索过滤（f / / 触发，支持多关键词实时匹配）
//! - 清晰的保留 vs 丢弃分界线与醒目的回退横幅（Rewind Banner）
//! - 选中文档与提问正文预览（支持滚动）
//! - omp 风格的底部 Dock 操作栏

use chrono::TimeZone;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::modal::{Modal, RewindState};
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

/// 截断文本并补省略号
fn ellipsize(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut width = 0;
    let mut result = String::new();
    for ch in text.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if width + ch_w > max_width.saturating_sub(1) && max_width > 3 {
            result.push('…');
            break;
        } else if width + ch_w > max_width {
            break;
        }
        result.push(ch);
        width += ch_w;
    }
    result
}

/// 绘制回退视图
pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let win = modal_area(area);

    // 降低外部背景对比度
    frame
        .buffer_mut()
        .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
    frame.render_widget(Clear, win);

    // 外层边框
    let outer_block = Block::default()
        .borders(Borders::ALL)
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

    // 3. 底部 Dock 操作栏（omp 风格）
    draw_dock(frame, state, dock_area, theme);
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
            "选择要回退到的历史节点。确认后选中的用户消息将恢复到输入框重新编辑，该节点之后的所有对话记录将被丢弃。",
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

    // 滚动视口收敛
    if selected_pos < state.list_scroll {
        state.list_scroll = selected_pos;
    }
    if selected_pos >= state.list_scroll + visible_count && visible_count > 0 {
        state.list_scroll = selected_pos.saturating_sub(visible_count - 1);
    }

    let total_items = state.items.len();
    let mut lines = Vec::new();

    for &match_idx in matches.iter().skip(state.list_scroll).take(visible_count) {
        let item = &state.items[match_idx];
        let is_selected = match_idx == state.selected;
        let is_kept = match_idx < state.selected;
        let is_dropped = match_idx > state.selected;

        let (marker, status_tag, tag_style) = if is_selected {
            (
                "❯ ⏪ ",
                "[回退目标]",
                Style::new()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::BOLD),
            )
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
            let snippet = ellipsize(&item.preview, usize::from(inner.width).saturating_sub(6));
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
}

/// 绘制右侧详情与回退影响面板
fn draw_detail_panel(frame: &mut Frame, state: &mut RewindState, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
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
        .border_style(Style::new().fg(banner_border_color))
        .style(Style::new().bg(theme.bg_visual));

    let banner_inner = banner_block.inner(banner_rect);
    frame.render_widget(banner_block, banner_rect);

    let mut banner_lines = Vec::new();
    if current_item.is_user {
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
            ellipsize(&current_item.entry_id, 14),
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

/// 绘制底部 Dock 操作栏（omp 风格）
fn draw_dock(frame: &mut Frame, state: &RewindState, area: Rect, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let mut spans = Vec::new();

    // 回退徽标
    spans.push(Span::styled(
        " ⏪ 回退 ",
        Style::new()
            .bg(theme.accent_user)
            .fg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(" "));

    if state.filtering {
        spans.push(Span::styled(
            format!("过滤: \"{}\"", state.query),
            Style::new()
                .fg(theme.accent_warning)
                .add_modifier(Modifier::BOLD),
        ));
        let matches = state.filtered_indices();
        spans.push(Span::styled(
            format!(" (匹配 {}/{})", matches.len(), state.items.len()),
            Style::new().fg(theme.text_secondary),
        ));
        spans.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
        spans.push(Span::styled(
            "↑↓ 切换匹配",
            Style::new().fg(theme.text_primary),
        ));
        spans.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
        spans.push(Span::styled(
            "Esc 清除过滤",
            Style::new().fg(theme.gray_dim),
        ));
        spans.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
        spans.push(Span::styled(
            "Enter 回退到此处",
            Style::new()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        if !state.items.is_empty() {
            spans.push(Span::styled(
                format!("{}/{} ", state.selected + 1, state.items.len()),
                Style::new()
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("· ", Style::new().fg(theme.gray_dim)));
        }
        spans.push(Span::styled(
            "↑↓/jk 逐条选择",
            Style::new().fg(theme.text_primary),
        ));
        spans.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
        spans.push(Span::styled(
            "←→/hl 仅跳用户提问",
            Style::new().fg(theme.accent_user),
        ));
        spans.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
        spans.push(Span::styled(
            "f// 搜索过滤",
            Style::new().fg(theme.text_secondary),
        ));
        spans.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
        spans.push(Span::styled("Esc 取消", Style::new().fg(theme.gray_dim)));
        spans.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
        spans.push(Span::styled(
            "Enter 回退到此处",
            Style::new()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

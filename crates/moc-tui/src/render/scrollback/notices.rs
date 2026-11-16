//! 省略通知按工具聚合；父调用已展示的同一条通知不在子调用重复。

use std::collections::HashSet;

use rust_i18n::t;

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use super::super::wrap::wrap_text;
use crate::{
    app::App,
    transcript::{DetailLevel, ToolCallBlock, tool_notices},
};

fn long_line(notice: &str) -> bool {
    notice.trim_start().starts_with("[Line ") && notice.contains("content omitted;")
}

pub(super) fn rows(
    tool: &ToolCallBlock,
    app: &App,
    width: usize,
    level: DetailLevel,
) -> Vec<Line<'static>> {
    let parent: HashSet<_> = tool
        .parent_id
        .as_deref()
        .and_then(|id| app.tool_block(id))
        .and_then(|parent| parent.result.as_deref())
        .into_iter()
        .flat_map(tool_notices)
        .collect();
    let mut seen = HashSet::new();
    let notices: Vec<_> = tool
        .result
        .as_deref()
        .into_iter()
        .flat_map(tool_notices)
        .filter(|notice| !parent.contains(notice) && seen.insert(*notice))
        .collect();
    let count = notices.iter().filter(|notice| long_line(notice)).count();
    let mut rows = Vec::new();
    let mut push =
        |text: &str| {
            rows.extend(wrap_text(text, width).into_iter().map(|row| {
                Line::from(Span::styled(row, Style::new().fg(app.theme.accent_warning)))
            }));
        };
    if count > 0 {
        let hint = if level == DetailLevel::Full {
            String::new()
        } else {
            t!("tui.scroll.expand_hint").into_owned()
        };
        push(
            &t!(
                "tui.scroll.incomplete_fmt",
                count = count,
                hint = hint.as_str()
            )
            .into_owned(),
        );
        if level == DetailLevel::Full {
            for notice in &notices {
                if long_line(notice) {
                    push(
                        notice
                            .split(';')
                            .next()
                            .unwrap_or(notice)
                            .trim_start_matches('['),
                    );
                }
            }
            push(t!("tui.scroll.read_hint").as_ref());
        }
    }
    for notice in notices {
        if long_line(notice)
            || notice.starts_with("Lines shown as a size notice")
            || notice.starts_with("[Long lines omitted;")
        {
            continue;
        }
        push(notice);
    }
    rows
}

use super::{ask_layout, modal_body, modal_lines};
use crate::{
    app::App,
    modal::{Modal, PickerState, SessionScope},
    theme::Theme,
};
use moc_protocol::methods::{
    AskOption, AskParams, AskQuestion, ContextUsageResult, ContextUsageSection, SessionEngine,
    SessionInfo, SessionPermission,
};
use ratatui::style::Color;
use ratatui::text::Line;
use tokio::sync::mpsc::unbounded_channel;

mod ask;
mod behavior;
mod context;
mod menus;
mod model;
mod plan;

/// 单问、单选的提问模态；`highlight` 指向聚焦选项。
fn ask_modal(highlight: usize, preview_scroll: usize) -> Modal {
    Modal::Ask {
        params: AskParams {
            session_id: "ses_1".into(),
            prompt_id: "prompt_1".into(),
            request_id: "ask_1".into(),
            questions: vec![AskQuestion {
                id: "layout".into(),
                question: "选哪个布局？".into(),
                header: None,
                multi: false,
                recommended: Vec::new(),
                options: vec![
                    AskOption {
                        label: "上中下".into(),
                        description: None,
                        preview: Some("```\n+--+\n|A |\n+--+\n```".into()),
                    },
                    AskOption {
                        label: "左右".into(),
                        description: None,
                        preview: None,
                    },
                ],
            }],
        },
        question: 0,
        highlight: vec![highlight],
        selected: vec![Vec::new()],
        custom: vec![crate::modal::CustomAnswer::default()],
        preview_scroll,
        question_scroll: 0,
        parked: false,
        reply: None,
    }
}

/// 一行渲染后的文本。
fn line_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// 一行渲染后的显示宽度（格）。
fn line_width(text: &str) -> usize {
    crate::render::wrap::str_width(text)
}

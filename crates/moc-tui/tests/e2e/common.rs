//! Common test harness, fixtures, and buffer assertion utilities for E2E tests.

use moc_protocol::methods::{
    AgentInfo, AgentStatus, AskOption, AskParams, AskQuestion, ContextUsageResult,
    ContextUsageSection, SessionAutonomy, SessionEngine, SessionInfo, SessionPermission,
};
use moc_tui::app::App;
use moc_tui::modal::ModelEntry;
use moc_tui::protocol::AppEvent;
use moc_tui::theme::{ColorLevel, Theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Creates a test app instance with a headless test terminal of specified width and height.
pub fn create_test_terminal(
    width: u16,
    height: u16,
) -> (App, UnboundedReceiver<AppEvent>, Terminal<TestBackend>) {
    let (events_tx, events_rx) = unbounded_channel();
    let app = App::new(Theme::global(), events_tx);
    let terminal = Terminal::new(TestBackend::new(width, height)).expect("create test backend");
    (app, events_rx, terminal)
}

/// Creates a test app instance with a specific color-level resolved theme.
pub fn create_test_terminal_with_level(
    width: u16,
    height: u16,
    level: ColorLevel,
) -> (App, UnboundedReceiver<AppEvent>, Terminal<TestBackend>) {
    let (events_tx, events_rx) = unbounded_channel();
    static TRUECOLOR_THEME: std::sync::LazyLock<Theme> =
        std::sync::LazyLock::new(|| Theme::resolved(ColorLevel::TrueColor));
    static ANSI256_THEME: std::sync::LazyLock<Theme> =
        std::sync::LazyLock::new(|| Theme::resolved(ColorLevel::Ansi256));
    static BASIC_THEME: std::sync::LazyLock<Theme> =
        std::sync::LazyLock::new(|| Theme::resolved(ColorLevel::Basic));
    static NONE_THEME: std::sync::LazyLock<Theme> =
        std::sync::LazyLock::new(|| Theme::resolved(ColorLevel::None));

    let theme = match level {
        ColorLevel::TrueColor => &*TRUECOLOR_THEME,
        ColorLevel::Ansi256 => &*ANSI256_THEME,
        ColorLevel::Basic => &*BASIC_THEME,
        ColorLevel::None => &*NONE_THEME,
    };
    let app = App::new(theme, events_tx);
    let terminal = Terminal::new(TestBackend::new(width, height)).expect("create test backend");
    (app, events_rx, terminal)
}

/// Extracts all text rows from a terminal backend buffer.
pub fn buffer_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    use ratatui::buffer::CellWidth;
    let buffer = terminal.backend().buffer();
    let width = buffer.area().width;
    let height = buffer.area().height;
    (0..height)
        .map(|row| {
            let mut line = String::new();
            let mut col = 0;
            while col < width {
                if let Some(cell) = buffer.cell((col, row)) {
                    let w = cell.cell_width();
                    line.push_str(cell.symbol());
                    if w > 1 {
                        col += u16::from(w);
                        continue;
                    }
                }
                col += 1;
            }
            line
        })
        .collect()
}

/// Extracts compact text rows (trailing whitespace trimmed).
pub fn buffer_compact_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    buffer_rows(terminal)
        .into_iter()
        .map(|r| r.trim_end().to_string())
        .collect()
}

/// Extracts rows with all whitespace stripped, enabling robust text matching for CJK glyphs.
pub fn buffer_squashed_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    buffer_rows(terminal)
        .into_iter()
        .map(|r| r.chars().filter(|c| !c.is_whitespace()).collect())
        .collect()
}

/// Checks whether any cell inside an area has a specific background color.
pub fn area_has_bg(buffer: &Buffer, area: Rect, expected_bg: Color) -> bool {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buffer.cell((x, y)) {
                if cell.style().bg == Some(expected_bg) {
                    return true;
                }
            }
        }
    }
    false
}

/// Pre-fills the entire terminal buffer with a noise character and background color
/// to detect transparency leaks.
pub fn prefill_noise(terminal: &mut Terminal<TestBackend>, noise_char: &str, noise_bg: Color) {
    terminal
        .draw(|frame| {
            let buffer = frame.buffer_mut();
            let area = buffer.area().clone();
            for y in 0..area.height {
                for x in 0..area.width {
                    if let Some(cell) = buffer.cell_mut((x, y)) {
                        cell.set_symbol(noise_char);
                        cell.set_bg(noise_bg);
                    }
                }
            }
        })
        .expect("prefill noise");
}

/// Checks if any noise characters leak through inside a given Rect.
pub fn count_leaked_noise(buffer: &Buffer, area: Rect, noise_char: &str) -> usize {
    let mut leaks = 0;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buffer.cell((x, y)) {
                if cell.symbol() == noise_char {
                    leaks += 1;
                }
            }
        }
    }
    leaks
}

/// Checks if four corners of a rect match the rounded border characters ╭ ╮ ╰ ╯.
pub fn is_rounded_box(buffer: &Buffer, rect: Rect) -> bool {
    if rect.width < 2 || rect.height < 2 {
        return false;
    }
    let top_left = buffer.cell((rect.x, rect.y)).map(|c| c.symbol());
    let top_right = buffer
        .cell((rect.x + rect.width - 1, rect.y))
        .map(|c| c.symbol());
    let bottom_left = buffer
        .cell((rect.x, rect.y + rect.height - 1))
        .map(|c| c.symbol());
    let bottom_right = buffer
        .cell((rect.x + rect.width - 1, rect.y + rect.height - 1))
        .map(|c| c.symbol());

    top_left == Some("╭")
        && top_right == Some("╮")
        && bottom_left == Some("╰")
        && bottom_right == Some("╯")
}

/// Checks if four corners of a rect match the sharp box-drawing corners ┌ ┐ └ ┘.
pub fn is_sharp_box(buffer: &Buffer, rect: Rect) -> bool {
    if rect.width < 2 || rect.height < 2 {
        return false;
    }
    let top_left = buffer.cell((rect.x, rect.y)).map(|c| c.symbol());
    let top_right = buffer
        .cell((rect.x + rect.width - 1, rect.y))
        .map(|c| c.symbol());
    let bottom_left = buffer
        .cell((rect.x, rect.y + rect.height - 1))
        .map(|c| c.symbol());
    let bottom_right = buffer
        .cell((rect.x + rect.width - 1, rect.y + rect.height - 1))
        .map(|c| c.symbol());

    top_left == Some("┌")
        && top_right == Some("┐")
        && bottom_left == Some("└")
        && bottom_right == Some("┘")
}

/// Helper fixture for sample ModelEntry.
pub fn sample_model_entry(selector: &str, label: &str) -> ModelEntry {
    ModelEntry {
        selector: selector.into(),
        label: label.into(),
        display_name: label.into(),
        levels: vec!["off".into(), "low".into(), "high".into()],
        context_window: Some(200_000),
        max_output_tokens: Some(32_000),
        inputs: vec![moc_protocol::methods::ModelInput::Text],
        remote_compaction: false,
        pricing: None,
    }
}

/// Helper fixture for sample SessionInfo.
pub fn sample_session_info(id: &str, title: &str, cwd: &str) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        title: title.into(),
        model: "provider/model".into(),
        permission: SessionPermission::Full,
        engine: SessionEngine::Standard,
        autonomy: SessionAutonomy::Default,
        plan_active: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: cwd.into(),
        created_at_ms: 1000,
        updated_at_ms: 2000,
        lock: None,
    }
}

/// Helper fixture for sample AgentInfo.
pub fn sample_agent_info(id: &str, title: &str, status: AgentStatus) -> AgentInfo {
    AgentInfo {
        id: id.into(),
        parent_id: "agent_parent".into(),
        title: title.into(),
        model: "provider/model".into(),
        cwd: "/home/project".into(),
        preset: None,
        status,
        created_at_ms: 1000,
        updated_at_ms: 2000,
        last_message: None,
    }
}

/// Helper fixture for sample AskParams.
pub fn sample_ask_params(question: &str, options: &[&str]) -> AskParams {
    AskParams {
        session_id: "ses_001".into(),
        prompt_id: "prm_001".into(),
        request_id: "req_001".into(),
        questions: vec![AskQuestion {
            id: "q1".into(),
            question: question.into(),
            header: Some("确认提问".into()),
            options: options
                .iter()
                .map(|opt| AskOption {
                    label: (*opt).into(),
                    description: None,
                    preview: None,
                })
                .collect(),
            multi: false,
            recommended: Vec::new(),
        }],
    }
}

/// Helper fixture for sample PermissionRequestParams.
pub fn sample_permission_params(
    title: &str,
    options: &[&str],
) -> moc_protocol::methods::PermissionRequestParams {
    moc_protocol::methods::PermissionRequestParams {
        session_id: "ses_001".into(),
        prompt_id: "prm_001".into(),
        request_id: "req_001".into(),
        tool_call_id: "call_001".into(),
        tool_name: "bash".into(),
        title: title.into(),
        body: Some("cargo test".into()),
        diff: None,
        options: options
            .iter()
            .enumerate()
            .map(|(i, opt)| moc_protocol::methods::PermissionOption {
                id: format!("opt_{i}"),
                label: (*opt).into(),
                kind: moc_protocol::methods::PermissionOptionKind::AllowOnce,
            })
            .collect(),
    }
}

/// Helper fixture for sample Btw Turn.
pub fn sample_btw_turn(question: &str, reasoning: &str, answer: &str) -> moc_tui::btw::Turn {
    moc_tui::btw::Turn {
        request_id: 1,
        question: question.into(),
        answer: answer.into(),
        reasoning: reasoning.into(),
        status: moc_tui::btw::Status::Complete,
        model: "provider/model".into(),
        usage: None,
        history_turns_used: 0,
    }
}

/// Helper fixture for sample SubagentView.
pub fn sample_subagent_view(
    id: &str,
    title: &str,
    status: AgentStatus,
) -> moc_tui::subagent::SubagentView {
    moc_tui::subagent::SubagentView::new(sample_agent_info(id, title, status))
}

/// Helper fixture for sample QueuedPrompt.
pub fn sample_queued_prompt(id: u64, text: &str) -> moc_tui::queue::QueuedPrompt {
    let mut draft = moc_tui::widgets::prompt::PromptState::default();
    draft.insert_text(text);
    moc_tui::queue::QueuedPrompt {
        id,
        draft,
        mode: None,
    }
}

/// Helper fixture for sample CompletionState.
pub fn sample_completion_state(query: &str) -> moc_tui::widgets::completion::CompletionState {
    moc_tui::widgets::completion::CompletionState::derive(query, &[])
        .unwrap_or_else(|| moc_tui::widgets::completion::CompletionState::derive("/", &[]).unwrap())
}

/// Helper fixture for sample CompletionCandidate.
pub fn sample_slash_candidate(
    label: &str,
    description: &str,
) -> moc_tui::widgets::completion::CompletionCandidate {
    moc_tui::widgets::completion::CompletionCandidate {
        value: label.into(),
        label: label.into(),
        description: description.into(),
        append_space: true,
        source: moc_tui::widgets::completion::CompletionSource::Slash,
    }
}

/// Helper fixture for custom CompletionState.
pub fn sample_custom_completion(
    candidates: Vec<moc_tui::widgets::completion::CompletionCandidate>,
) -> moc_tui::widgets::completion::CompletionState {
    moc_tui::widgets::completion::CompletionState {
        candidates,
        selected: 0,
        replace_range: 0..1,
        area: Rect::default(),
        offset: 0,
        visible_count: 0,
    }
}

/// Helper fixture for sample TodoPhase.
pub fn sample_todo_phase(name: &str, task: &str) -> moc_protocol::methods::TodoPhase {
    moc_protocol::methods::TodoPhase {
        phase: name.into(),
        items: vec![moc_protocol::methods::TodoItem {
            status: moc_protocol::methods::TodoStatus::InProgress,
            task: task.into(),
            reason: None,
        }],
    }
}

/// Helper fixture for sample ContextUsageResult.
pub fn sample_context_usage() -> ContextUsageResult {
    ContextUsageResult {
        context_window: 200_000,
        total_tokens: 80_000,
        sections: vec![
            ContextUsageSection {
                name: "system".into(),
                tokens: 30_000,
            },
            ContextUsageSection {
                name: "tools".into(),
                tokens: 20_000,
            },
            ContextUsageSection {
                name: "messages".into(),
                tokens: 30_000,
            },
        ],
    }
}

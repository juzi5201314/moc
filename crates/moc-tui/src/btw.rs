//! 旁路问答：会话内存线程、单 flight、定向取消与非模态交互。

use std::collections::{HashMap, VecDeque};

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use moc_protocol::{
    BtwCancelParams, BtwExchange, BtwParams, BtwResult, BtwUpdate, BtwUpdateParams, FinishReason,
    MAX_BTW_HISTORY_TURNS, SessionBtw, SessionBtwCancel, Usage,
};
use ratatui::layout::Rect;

use crate::{
    app::{App, Focus},
    protocol::{AppEvent, EmbeddedConnection},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Starting,
    Waiting,
    Generating,
    Complete,
    Cancelled,
    Failed(String),
}

impl Status {
    pub fn label(&self) -> &str {
        match self {
            Self::Starting => "连接中",
            Self::Waiting => "等待模型",
            Self::Generating => "生成中",
            Self::Complete => "完成",
            Self::Cancelled => "已取消",
            Self::Failed(_) => "未完成",
        }
    }
}

pub struct Turn {
    pub request_id: u64,
    pub question: String,
    pub answer: String,
    pub reasoning: String,
    pub status: Status,
    pub model: String,
    pub usage: Option<Usage>,
    pub history_turns_used: usize,
}

#[derive(Default)]
pub struct Thread {
    /// 含失败/取消的浏览记录最多 20 轮；只有 Complete 会作为历史回放。
    pub turns: VecDeque<Turn>,
    pub scroll: usize,
}

impl Thread {
    fn history(&self) -> Vec<BtwExchange> {
        debug_assert!(self.turns.len() <= MAX_BTW_HISTORY_TURNS);
        self.turns
            .iter()
            .filter(|turn| turn.status == Status::Complete)
            .map(|turn| BtwExchange {
                question: turn.question.clone(),
                answer: turn.answer.clone(),
            })
            .collect()
    }
}

/// 不依附可见面板：清空、收起、切会话后仍须等待 RPC 收尾。
pub struct Flight {
    pub session_id: String,
    pub request_id: u64,
    started: bool,
    cancel_pending: bool,
    cancel_sent: bool,
}

#[derive(Default)]
pub struct BtwState {
    pub threads: HashMap<String, Thread>,
    pub flight: Option<Flight>,
    /// App 生命周期单调递增；清空或切会话都不得复位。
    next_request_id: u64,
    pub expanded: bool,
    pub area: Rect,
    pub scroll_max: usize,
}

impl BtwState {
    pub fn thread(&self, session: &str) -> Option<&Thread> {
        self.threads.get(session)
    }

    fn begin(&mut self, session_id: String, question: String) -> BtwParams {
        debug_assert!(self.flight.is_none());
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .expect("旁路请求序号耗尽");
        let request_id = self.next_request_id;
        let thread = self.threads.entry(session_id.clone()).or_default();
        // 先为本次问题腾出位置，再取历史：回放的内容必须都还在可见线程里。
        while thread.turns.len() >= MAX_BTW_HISTORY_TURNS {
            thread.turns.pop_front();
        }
        let history = thread.history();
        thread.turns.push_back(Turn {
            request_id,
            question: question.clone(),
            answer: String::new(),
            reasoning: String::new(),
            status: Status::Starting,
            model: String::new(),
            usage: None,
            history_turns_used: history.len(),
        });
        thread.scroll = 0;
        self.flight = Some(Flight {
            session_id: session_id.clone(),
            request_id,
            started: false,
            cancel_pending: false,
            cancel_sent: false,
        });
        self.expanded = true;
        BtwParams {
            session_id,
            request_id,
            question,
            history,
        }
    }

    fn turn_mut(&mut self, session: &str, request: u64) -> Option<&mut Turn> {
        self.threads
            .get_mut(session)?
            .turns
            .iter_mut()
            .find(|turn| turn.request_id == request)
    }

    pub fn cancel(&mut self) {
        if let Some(flight) = &mut self.flight {
            flight.cancel_pending = true;
        }
    }

    pub fn leave_session(&mut self, session: &str) {
        if self
            .flight
            .as_ref()
            .is_some_and(|flight| flight.session_id == session)
        {
            self.cancel();
        }
        self.expanded = false;
        self.area = Rect::default();
    }

    pub fn clear(&mut self, session: &str) {
        if self
            .flight
            .as_ref()
            .is_some_and(|flight| flight.session_id == session)
        {
            self.cancel();
        }
        self.threads.remove(session);
    }

    fn take_cancel(&mut self) -> Option<BtwCancelParams> {
        let flight = self.flight.as_mut()?;
        if !flight.started || !flight.cancel_pending || flight.cancel_sent {
            return None;
        }
        flight.cancel_sent = true;
        Some(BtwCancelParams {
            session_id: flight.session_id.clone(),
            request_id: flight.request_id,
        })
    }

    pub fn apply_update(&mut self, params: BtwUpdateParams) {
        let Some(flight) = self.flight.as_mut().filter(|flight| {
            flight.session_id == params.session_id && flight.request_id == params.request_id
        }) else {
            return;
        };
        if matches!(params.update, BtwUpdate::Started) {
            flight.started = true;
        }
        let Some(turn) = self.turn_mut(&params.session_id, params.request_id) else {
            return;
        };
        match params.update {
            BtwUpdate::Started | BtwUpdate::Waiting => turn.status = Status::Waiting,
            BtwUpdate::Generating => turn.status = Status::Generating,
            BtwUpdate::TextDelta { text } => turn.answer.push_str(&text),
            BtwUpdate::ReasoningDelta { text } => turn.reasoning.push_str(&text),
        }
    }

    pub fn finish(&mut self, session: &str, request: u64, result: anyhow::Result<BtwResult>) {
        if !self
            .flight
            .as_ref()
            .is_some_and(|flight| flight.session_id == session && flight.request_id == request)
        {
            return;
        }
        let flight = self.flight.take().expect("已匹配当前请求");
        let Some(turn) = self.turn_mut(session, request) else {
            return;
        };
        match result {
            Ok(result) => {
                // RPC 全文是权威终态；之后到达的缓冲 delta 不再匹配 flight。
                turn.answer = result.text;
                turn.model = result.model;
                turn.usage = Some(result.usage);
                turn.history_turns_used = result.history_turns_used;
                turn.status = if flight.cancel_pending
                    || matches!(
                        result.finish_reason,
                        FinishReason::Cancelled | FinishReason::Interrupted
                    ) {
                    Status::Cancelled
                } else if matches!(
                    result.finish_reason,
                    FinishReason::Stop | FinishReason::StopSequence
                ) {
                    Status::Complete
                } else {
                    Status::Failed(format!("结束原因：{:?}", result.finish_reason))
                };
            }
            Err(error) => {
                turn.status = if flight.cancel_pending {
                    Status::Cancelled
                } else {
                    Status::Failed(error.to_string())
                };
            }
        }
    }
}

pub fn question(text: &str) -> Option<&str> {
    let text = text.trim();
    let rest = text.strip_prefix("/btw")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

enum Submission {
    Unrecognized,
    Handled,
    Request(BtwParams),
}

/// 所有主输入提交入口共用；返回 true 表示已识别（含拒绝且保留的草稿）。
pub fn submit(app: &mut App, connection: &EmbeddedConnection) -> bool {
    match prepare_submission(app) {
        Submission::Unrecognized => false,
        Submission::Handled => true,
        Submission::Request(params) => {
            let session_id = params.session_id.clone();
            let request_id = params.request_id;
            let client = connection.client.clone();
            let events = app.events.clone();
            // 不持有可被 UI drop 的任务句柄；取消必须走定向 RPC，原请求一直等到收尾。
            tokio::spawn(async move {
                let result = client
                    .call::<SessionBtw>(params)
                    .await
                    .map_err(anyhow::Error::from);
                let _ = events.send(AppEvent::BtwReturned {
                    session_id,
                    request_id,
                    result,
                });
            });
            true
        }
    }
}

fn prepare_submission(app: &mut App) -> Submission {
    let text = app.prompt.expanded_text();
    let Some(question) = question(&text) else {
        return Submission::Unrecognized;
    };
    if app.prompt.has_attached_images() {
        app.set_notice("旁路问答仅支持文本，请移除图片后发送");
        return Submission::Handled;
    }
    let Some(session_id) = app.require_session() else {
        return Submission::Handled;
    };
    if !question.is_empty() && app.btw.flight.is_some() {
        app.set_notice("旁路问答仍在运行，草稿已保留；先等待或在面板按 Ctrl+C");
        return Submission::Handled;
    }
    // 只读会话同样可以旁路：core 只读快照，不申请写租约。
    app.prompt.take_draft();
    app.prompt_changed();
    app.completion = None;
    app.selection = None;
    reopen(app);
    let submission = if question.is_empty() {
        Submission::Handled
    } else {
        Submission::Request(app.btw.begin(session_id, question.to_owned()))
    };
    if let Some(stash) = app.prompt_stash.take() {
        app.prompt.restore_draft(stash);
    }
    submission
}

pub fn reopen(app: &mut App) {
    if let Some(session) = &app.session {
        app.btw.threads.entry(session.id.clone()).or_default();
        app.btw.expanded = true;
        app.focus = Focus::Btw;
    }
}

/// 每次处理完 UI 事件后调用，也用于 Started 抵达后立即补发取消。
pub fn flush_cancel(app: &mut App, connection: &EmbeddedConnection) {
    let Some(params) = app.btw.take_cancel() else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        if let Err(error) = client.call::<SessionBtwCancel>(params).await {
            let _ = events.send(AppEvent::Notice(format!("取消旁路问答失败：{error}")));
        }
    });
}

pub fn handle_key(app: &mut App, modifiers: KeyModifiers, code: KeyCode) -> bool {
    if modifiers == KeyModifiers::ALT && code == KeyCode::Char('b') {
        let Some(session_id) = app.require_session() else {
            return true;
        };
        app.btw.threads.entry(session_id).or_default();
        if app.focus == Focus::Btw && app.btw.expanded {
            app.btw.expanded = false;
            app.focus = Focus::Prompt;
        } else {
            app.btw.expanded = true;
            app.focus = Focus::Btw;
            app.selection = None;
            app.completion = None;
        }
        return true;
    }
    if app.focus != Focus::Btw {
        return false;
    }
    match (code, modifiers) {
        (KeyCode::Esc, _) => {
            app.btw.expanded = false;
            app.focus = Focus::Prompt;
        }
        (KeyCode::Tab, KeyModifiers::NONE) => app.focus = Focus::Prompt,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            app.ctrl_c_pending = None;
            app.btw.cancel();
        }
        (KeyCode::Char('x'), KeyModifiers::NONE) => {
            if let Some(session) = &app.session {
                app.btw.clear(&session.id);
                app.btw.threads.entry(session.id.clone()).or_default();
            }
        }
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            let answer = app
                .session
                .as_ref()
                .and_then(|session| app.btw.thread(&session.id))
                .and_then(|thread| thread.turns.back())
                .map(|turn| turn.answer.clone());
            if let Some(answer) = answer.filter(|text| !text.is_empty()) {
                crate::event::write_osc52(&answer);
                app.set_notice("已复制旁路回答");
                tokio::spawn(async move {
                    if let Err(error) = crate::clipboard::copy_text_to_clipboard(&answer).await {
                        tracing::debug!(%error, "旁路本地剪贴板复制失败，已发送 OSC52");
                    }
                });
            }
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => scroll(app, 3),
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => scroll(app, -3),
        (KeyCode::PageUp, _) => scroll(app, 10),
        (KeyCode::PageDown, _) => scroll(app, -10),
        (KeyCode::Home, _) => scroll(app, isize::MAX),
        (KeyCode::End, _) => scroll(app, isize::MIN),
        // 面板不编辑主草稿；先 Tab/点击回主输入，再 /btw 继续线程。
        _ => {}
    }
    true
}

fn scroll(app: &mut App, amount: isize) {
    if let Some(thread) = app
        .session
        .as_ref()
        .and_then(|session| app.btw.threads.get_mut(&session.id))
    {
        thread.scroll = thread
            .scroll
            .saturating_add_signed(amount)
            .min(app.btw.scroll_max);
    }
}

pub fn handle_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    if !app.btw.area.contains((mouse.column, mouse.row).into()) {
        return false;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.btw.expanded = true;
            app.focus = Focus::Btw;
            app.selection = None;
            app.completion = None;
        }
        MouseEventKind::ScrollUp => scroll(app, 3),
        MouseEventKind::ScrollDown => scroll(app, -3),
        _ => {}
    }
    true
}

#[cfg(test)]
#[path = "btw_tests.rs"]
mod tests;

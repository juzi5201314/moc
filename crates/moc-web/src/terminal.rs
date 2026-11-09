//! `/pty/*`：WebUI 终端分页的进程内 PTY 表。
//!
//! 一个终端 = portable-pty 起的一个登录 shell（Unix `$SHELL`，Windows 按
//! `moc_core::shell::detect()` 的交互参数）。PTY 读写都是阻塞调用：每终端
//! 三个线程——读线程把输出写进有界滚动缓冲并 broadcast 给所有附着者，写
//! 线程串行消费输入通道，wait 线程在退出时置位并广播。WebSocket 断开不杀
//! 进程；进程退出后条目保留到 DELETE；进程整体退出时靠 Drop 兜底杀子进程。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

use crate::{AppState, origin_allowed};

/// 全进程终端数上限。
const MAX_TERMINALS: usize = 32;
/// 每终端滚动缓冲上限；回放只发这么多。
const SCROLLBACK_BYTES: usize = 512 * 1024;
/// broadcast 通道积压上限；落后的附着者被断开，重连后靠回放恢复一致画面。
const EVENT_CAPACITY: usize = 512;

/// 进程内终端表：`id` 单调递增，BTreeMap 序即创建时间序。
#[derive(Clone, Default)]
pub struct Terminals {
    inner: Arc<Mutex<TableInner>>,
}

#[derive(Default)]
struct TableInner {
    next_id: u64,
    items: BTreeMap<u64, Arc<Terminal>>,
}

/// 一条终端的对外视图（`/pty` 与 `/pty/list` 的 JSON 形状）。
#[derive(Debug, Serialize)]
pub struct TerminalInfo {
    pub id: u64,
    pub cwd: String,
    pub shell: String,
    pub created_at_ms: u64,
    pub exit_code: Option<i32>,
    pub exited: bool,
}

/// 附着者之间分发的事件：输出是原始字节，退出带状态码。
#[derive(Clone)]
enum TermEvent {
    Output(Bytes),
    Exit(Option<i32>),
}

struct TermState {
    scroll: Scrollback,
    /// `Some(code)` 表示进程已退出。
    exit: Option<Option<i32>>,
}

/// 一个 PTY 终端。
struct Terminal {
    id: u64,
    cwd: PathBuf,
    shell: String,
    created_at_ms: u64,
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// `clone_killer` 拿来的杀进程句柄：child 本体归 wait 线程做阻塞 wait。
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// 用户输入 → 写线程 → PTY；Sender 全部 drop 后写线程退出。
    input: Mutex<std::sync::mpsc::Sender<Vec<u8>>>,
    state: Mutex<TermState>,
    events: tokio::sync::broadcast::Sender<TermEvent>,
}

impl Terminal {
    fn info(&self) -> TerminalInfo {
        let exit = self.state.lock().expect("终端状态锁").exit;
        TerminalInfo {
            id: self.id,
            cwd: self.cwd.to_string_lossy().into_owned(),
            shell: self.shell.clone(),
            created_at_ms: self.created_at_ms,
            exit_code: exit.flatten(),
            exited: exit.is_some(),
        }
    }

    /// 附着：同一把锁内快照缓冲并 subscribe，回放与实时流之间不丢不重。
    /// 第三个返回值是「附着时进程已退出」的退出码。
    fn attach(
        &self,
    ) -> (
        Vec<u8>,
        tokio::sync::broadcast::Receiver<TermEvent>,
        Option<Option<i32>>,
    ) {
        let state = self.state.lock().expect("终端状态锁");
        let receiver = self.events.subscribe();
        (state.scroll.snapshot(), receiver, state.exit)
    }

    fn write(&self, bytes: &[u8]) {
        let _ = self.input.lock().expect("输入通道锁").send(bytes.to_vec());
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.lock().expect("master 锁").resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// 杀掉终端进程。读/写/wait 线程各持一份 `Arc<Terminal>`，靠 drop 计数
    /// 杀不掉进程（线程退出前永远有引用），必须显式调用；moc web 进程退出时
    /// master fd 关闭，shell 收 SIGHUP 结束。
    fn kill(&self) {
        if let Ok(mut killer) = self.killer.lock() {
            let _ = killer.kill();
        }
    }
}

/// 512 KiB 有界滚动缓冲：超出时从头部淘汰，并对齐到下一个 `\n`，
/// 回放不从半个转义序列/半个 UTF-8 字符开始。
struct Scrollback {
    buffer: Vec<u8>,
}

impl Scrollback {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
        let excess = self.buffer.len().saturating_sub(SCROLLBACK_BYTES);
        if excess == 0 {
            return;
        }
        let cut = self.buffer[excess..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| excess + index + 1)
            .unwrap_or(excess);
        self.buffer.drain(..cut);
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buffer.clone()
    }
}

/// 起一个新终端；`cwd` 必须是已存在的绝对目录。
fn spawn(cwd: PathBuf, id: u64, cols: u16, rows: u16) -> std::io::Result<Arc<Terminal>> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| std::io::Error::other(error.to_string()))?;

    #[cfg(unix)]
    let (mut builder, shell_label) = {
        let builder = CommandBuilder::new_default_prog();
        let label = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        (builder, label)
    };
    // 用户终端加载用户配置：不能用给 agent 的空闲参数（--noprofile/-NoProfile）。
    #[cfg(windows)]
    let (mut builder, shell_label) = {
        use moc_core::shell::ShellKind;
        let choice = moc_core::shell::detect()
            .ok_or_else(|| std::io::Error::other("no supported shell found on this platform"))?;
        let args: &[&str] = match choice.kind {
            ShellKind::Bash | ShellKind::GitBash => &["--login", "-i"],
            ShellKind::Zsh => &["-l", "-i"],
            ShellKind::Sh => &["-i"],
            ShellKind::PowerShell => &["-NoLogo"],
            ShellKind::Cmd => &[],
        };
        let mut builder = CommandBuilder::new(choice.program.clone());
        builder.args(args);
        (builder, choice.label().to_owned())
    };
    builder.cwd(&cwd);
    builder.env("TERM", "xterm-256color");
    builder.env("COLORTERM", "truecolor");

    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let killer = child.clone_killer();
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| std::io::Error::other(error.to_string()))?;

    let (events, _) = tokio::sync::broadcast::channel(EVENT_CAPACITY);
    let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let terminal = Arc::new(Terminal {
        id,
        cwd,
        shell: shell_label,
        created_at_ms: now_ms(),
        master: Mutex::new(pair.master),
        killer: Mutex::new(killer),
        input: Mutex::new(input_tx),
        state: Mutex::new(TermState {
            scroll: Scrollback::new(),
            exit: None,
        }),
        events,
    });

    // 读线程：输出 → 滚动缓冲 + broadcast（同一把锁，保证 attach 语义）。
    let reading = Arc::clone(&terminal);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let chunk = Bytes::copy_from_slice(&buffer[..count]);
                    let mut state = reading.state.lock().expect("终端状态锁");
                    state.scroll.append(&chunk);
                    let _ = reading.events.send(TermEvent::Output(chunk));
                }
            }
        }
    });

    // 写线程：串行消费输入，写入失败（进程已死）即退出。
    std::thread::spawn(move || {
        while let Ok(bytes) = input_rx.recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    // wait 线程：退出后置位并广播；不 join 读线程——孙进程可能占着 pty 让
    // 读线程迟迟不 EOF，退出事件不该被它拖住。
    let waiting = Arc::clone(&terminal);
    std::thread::spawn(move || {
        let code = child.wait().ok().map(|status| {
            if status.signal().is_some() {
                None
            } else {
                Some(status.exit_code().min(i32::MAX as u32) as i32)
            }
        });
        let code = code.flatten();
        let mut state = waiting.state.lock().expect("终端状态锁");
        state.exit = Some(code);
        let _ = waiting.events.send(TermEvent::Exit(code));
    });

    Ok(terminal)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    cwd: String,
}

/// `GET /pty/list`：该 cwd 子树下的终端（按创建时间排序）——「在终端中打开
/// 子目录」建的终端也要算进所属工作区。
pub async fn list(State(state): State<AppState>, Query(query): Query<ListQuery>) -> Response {
    let cwd = match canonical_dir(&query.cwd) {
        Some(cwd) => cwd,
        None => return (StatusCode::BAD_REQUEST, "cwd 不是已存在的绝对目录").into_response(),
    };
    let inner = state.terminals.inner.lock().expect("终端表锁");
    Json(
        inner
            .items
            .values()
            .filter(|terminal| terminal.cwd.starts_with(&cwd))
            .map(|terminal| terminal.info())
            .collect::<Vec<_>>(),
    )
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    cwd: String,
    cols: u16,
    rows: u16,
}

/// `POST /pty`：创建终端；cwd 必须是存在的绝对目录，全进程上限 32 个。
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Response {
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "跨站请求被拒绝").into_response();
    }
    let Some(cwd) = canonical_dir(&body.cwd) else {
        return (StatusCode::BAD_REQUEST, "cwd 不是已存在的绝对目录").into_response();
    };
    let id = {
        let mut inner = state.terminals.inner.lock().expect("终端表锁");
        if inner.items.len() >= MAX_TERMINALS {
            return (StatusCode::TOO_MANY_REQUESTS, "终端数已达上限").into_response();
        }
        inner.next_id += 1;
        inner.next_id
    };
    let terminals = state.terminals.clone();
    match tokio::task::spawn_blocking(move || spawn(cwd, id, body.cols, body.rows)).await {
        Ok(Ok(terminal)) => {
            let info = terminal.info();
            terminals
                .inner
                .lock()
                .expect("终端表锁")
                .items
                .insert(id, terminal);
            Json(info).into_response()
        }
        Ok(Err(error)) => {
            (StatusCode::BAD_REQUEST, format!("无法启动终端：{error}")).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("无法启动终端：{error}"),
        )
            .into_response(),
    }
}

/// `DELETE /pty/{id}`：杀进程并移除条目。
pub async fn remove(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<u64>,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "跨站请求被拒绝").into_response();
    }
    let removed = state
        .terminals
        .inner
        .lock()
        .expect("终端表锁")
        .items
        .remove(&id);
    match removed {
        Some(terminal) => {
            terminal.kill();
            StatusCode::NO_CONTENT.into_response()
        }
        None => (StatusCode::NOT_FOUND, "终端不存在").into_response(),
    }
}

/// `GET /pty/{id}/ws`：附着终端——先发回放，再推实时输出；进程退出发
/// `{"type":"exit","code":N|null}`。客户端 binary 帧 = 输入字节，
/// text 帧 = `{"type":"resize","cols":N,"rows":N}`。
pub async fn attach(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<u64>,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "跨站 WebSocket 被拒绝").into_response();
    }
    let terminal = state
        .terminals
        .inner
        .lock()
        .expect("终端表锁")
        .items
        .get(&id)
        .cloned();
    let Some(terminal) = terminal else {
        return (StatusCode::NOT_FOUND, "终端不存在").into_response();
    };
    ws.on_upgrade(move |socket| serve(socket, terminal))
}

async fn serve(socket: WebSocket, terminal: Arc<Terminal>) {
    let (mut sink, mut stream) = socket.split();
    let (snapshot, mut events, exited) = terminal.attach();
    if !snapshot.is_empty() && sink.send(Message::Binary(snapshot.into())).await.is_err() {
        return;
    }
    if let Some(code) = exited {
        let _ = sink.send(exit_frame(code)).await;
        return;
    }

    // 服务端 → 客户端：实时输出与退出帧。
    let mut outbound = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            let message = match event {
                TermEvent::Output(bytes) => Message::Binary(bytes),
                TermEvent::Exit(code) => {
                    let _ = sink.send(exit_frame(code)).await;
                    return;
                }
            };
            if sink.send(message).await.is_err() {
                return;
            }
        }
    });

    // 客户端 → 服务端：binary 帧进 PTY，text 帧做 resize；断开即返回，
    // 进程不死（重连后可继续回放）。
    let mut inbound = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Binary(bytes) => terminal.write(&bytes),
                Message::Text(text) => apply_control(&terminal, &text),
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    });

    tokio::select! {
        _ = &mut outbound => inbound.abort(),
        _ = &mut inbound => outbound.abort(),
    }
}

fn exit_frame(code: Option<i32>) -> Message {
    Message::Text(
        serde_json::json!({"type": "exit", "code": code})
            .to_string()
            .into(),
    )
}

/// text 帧只认 resize；其余控制类型与非法 JSON 直接忽略。
fn apply_control(terminal: &Terminal, text: &str) {
    #[derive(Deserialize)]
    struct Control<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
        #[serde(default)]
        cols: u16,
        #[serde(default)]
        rows: u16,
    }
    let Ok(control) = serde_json::from_str::<Control<'_>>(text) else {
        return;
    };
    if control.kind == "resize" {
        terminal.resize(control.cols, control.rows);
    }
}

fn canonical_dir(raw: &str) -> Option<PathBuf> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    canonical.is_dir().then_some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_caps_and_aligns_eviction_to_newline() {
        let mut scroll = Scrollback::new();
        // 第一段末尾带换行：淘汰应对齐到它之后，不切碎 ANSI/UTF-8。
        let mut first = vec![b'x'; SCROLLBACK_BYTES / 2];
        first.extend_from_slice(b"\n");
        scroll.append(&first);
        let mut second = vec![b'y'; SCROLLBACK_BYTES];
        second.extend_from_slice(b"tail");
        scroll.append(&second);
        let snapshot = scroll.snapshot();
        assert!(snapshot.len() <= SCROLLBACK_BYTES);
        assert_eq!(snapshot.last(), Some(&b'l'));

        // 缓冲内无换行时按裸过量字节淘汰，仍不越上限。
        let mut scroll = Scrollback::new();
        scroll.append(&vec![b'z'; SCROLLBACK_BYTES + 10]);
        assert_eq!(scroll.snapshot().len(), SCROLLBACK_BYTES);
    }
}

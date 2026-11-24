//! 输入事件源：独立线程阻塞 read crossterm，转投异步通道。
//!
//! 阻塞 read 放在 OS 线程是为了让主循环的 `tokio::select!` 永远不被
//! 终端读取挂住（与 moc-cli REPL 的 stdin 线程同一理由）。空闲时主循环
//! 完全 park，没有 metronome。
//!
//! 启用 win32-input-mode 时 crossterm 不认识按键记录，改由本模块直接读 fd 0
//! 并用 [`win32::Decoder`] 解码。

mod win32;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crossterm::event::{self, Event};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// 主循环关心的输入事件（crossterm 事件的直接投影）。
#[derive(Debug)]
pub enum InputEvent {
    Terminal(Event),
}

pub struct InputThread {
    handle: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    receiver: UnboundedReceiver<InputEvent>,
}

impl InputThread {
    /// 启动读取线程。进程生命周期内只创建一次；`win32_input` 表示终端已切到
    /// win32-input-mode。
    pub fn spawn(win32_input: bool) -> Self {
        let (sender, receiver): (UnboundedSender<InputEvent>, _) = unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("moc-tui-input".into())
            .spawn(move || {
                #[cfg(unix)]
                if win32_input {
                    read_win32(&sender, &thread_stop);
                    return;
                }
                #[cfg(not(unix))]
                let _ = win32_input;
                while !thread_stop.load(Ordering::Acquire) {
                    match event::poll(std::time::Duration::from_millis(100)) {
                        Ok(true) => match event::read() {
                            Ok(terminal) => {
                                if sender.send(InputEvent::Terminal(terminal)).is_err() {
                                    return;
                                }
                            }
                            Err(error) => {
                                tracing::warn!(%error, "crossterm 事件读取失败");
                                return;
                            }
                        },
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(%error, "crossterm 事件轮询失败");
                            return;
                        }
                    }
                }
            })
            .expect("启动输入线程");
        Self {
            handle: Some(handle),
            stop,
            receiver,
        }
    }

    pub async fn recv(&mut self) -> Option<InputEvent> {
        self.receiver.recv().await
    }
}

/// win32-input-mode 下的读取循环：直接读 fd 0，解码后按 crossterm 事件投递。
#[cfg(unix)]
fn read_win32(sender: &UnboundedSender<InputEvent>, stop: &AtomicBool) {
    use std::time::Duration;

    let send = |events: Vec<Event>| {
        events
            .into_iter()
            .all(|event| sender.send(InputEvent::Terminal(event)).is_ok())
    };
    // 键盘协商期间的键入还留在 crossterm 的队列里，先交出去
    while let Ok(true) = event::poll(Duration::ZERO) {
        let Ok(event) = event::read() else { return };
        if !send(vec![event]) {
            return;
        }
    }
    let stdin = std::io::stdin();
    let mut decoder = win32::Decoder::default();
    // SIGWINCH 由 crossterm 的事件源处理，这条路径不经过它：每轮比对尺寸
    let mut size = crossterm::terminal::size().ok();
    let mut chunk = [0u8; 4096];
    while !stop.load(Ordering::Acquire) {
        // 半截输入等 50ms（与 crossterm 区分单独 Esc 的等待同量级），否则按停止检查周期等
        let wait = if decoder.pending() {
            50_000_000
        } else {
            100_000_000
        };
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: wait,
        };
        let mut fds = [rustix::event::PollFd::new(
            &stdin,
            rustix::event::PollFlags::IN,
        )];
        let events = match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => decoder.flush(),
            Ok(_) => match rustix::io::read(&stdin, &mut chunk) {
                Ok(0) => return,
                Ok(read) => decoder.feed(&chunk[..read]),
                Err(rustix::io::Errno::INTR) => Vec::new(),
                Err(error) => {
                    tracing::warn!(%error, "终端输入读取失败");
                    return;
                }
            },
            Err(rustix::io::Errno::INTR) => Vec::new(),
            Err(error) => {
                tracing::warn!(%error, "终端输入轮询失败");
                return;
            }
        };
        if !send(events) {
            return;
        }
        let now = crossterm::terminal::size().ok();
        if now != size {
            size = now;
            if let Some((columns, rows)) = now
                && !send(vec![Event::Resize(columns, rows)])
            {
                return;
            }
        }
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

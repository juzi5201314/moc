//! 输入事件源：独立线程阻塞 read crossterm，转投异步通道。
//!
//! 阻塞 read 放在 OS 线程是为了让主循环的 `tokio::select!` 永远不被
//! 终端读取挂住（与 moc-cli REPL 的 stdin 线程同一理由）。空闲时主循环
//! 完全 park，没有 metronome。

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
    /// 启动读取线程。进程生命周期内只创建一次。
    pub fn spawn() -> Self {
        let (sender, receiver): (UnboundedSender<InputEvent>, _) = unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("moc-tui-input".into())
            .spawn(move || {
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

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

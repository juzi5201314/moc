//! Core 级长期 PTC Host：单一 actor 持有子进程和路由，避免旧读任务误伤重启后的进程。

use super::ToolError;
use crate::{PtcWorkerCommand, config::PtcConfig};
use moc_ptc_protocol::wire::{
    GuestMessage, GuestState, GuestStep, HostCompletion, HostMessage, MAX_FRAME_BYTES, StartRequest,
};
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdout},
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Instant,
};

const CANCEL_GRACE: Duration = Duration::from_secs(2);
type Steps = mpsc::UnboundedSender<Result<GuestStep, ToolError>>;

enum Request {
    Start {
        id: u64,
        request: StartRequest,
        steps: Steps,
        max_runs: usize,
        idle: Duration,
    },
    Complete {
        id: u64,
        completions: Vec<HostCompletion>,
    },
    Cancel {
        id: u64,
        code: String,
    },
    Shutdown,
}

struct ShutdownState {
    requested: AtomicBool,
    done: watch::Sender<bool>,
}

/// 一个 Core 共享一个管理器；只有首次 execution 到来时才启动子进程。
pub(crate) struct PtcHost {
    tx: mpsc::UnboundedSender<Request>,
    next_id: Mutex<u64>,
    shutdown: Arc<ShutdownState>,
}

impl PtcHost {
    pub(crate) fn new(command: PtcWorkerCommand) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (done, _) = watch::channel(false);
        let shutdown = Arc::new(ShutdownState {
            requested: AtomicBool::new(false),
            done,
        });
        tokio::spawn(manage(command, rx, Arc::clone(&shutdown)));
        Self {
            tx,
            next_id: Mutex::new(0),
            shutdown,
        }
    }

    pub(crate) fn start(
        &self,
        request: StartRequest,
        config: &PtcConfig,
    ) -> Result<Guest, ToolError> {
        let (steps, rx) = mpsc::unbounded_channel();
        // 同一把锁覆盖 ID 分配、Start 入队和 shutdown 标记，避免关闭请求与新 execution 乱序。
        let mut next_id = self.next_id.lock().unwrap();
        if self.shutdown.requested.load(Ordering::Acquire) {
            return Err(worker_error("PTC Host 已关闭"));
        }
        let id = *next_id;
        *next_id += 1;
        self.tx
            .send(Request::Start {
                id,
                request,
                steps,
                max_runs: config.max_concurrent_runs,
                idle: Duration::from_secs(config.host_idle_timeout_secs),
            })
            .map_err(|_| worker_error("PTC Host 已关闭"))?;
        Ok(Guest {
            tx: self.tx.clone(),
            id,
            rx,
            first: true,
            finished: false,
        })
    }

    pub(crate) async fn shutdown(&self) {
        let mut done = self.shutdown.done.subscribe();
        let send_shutdown = {
            let _next_id = self.next_id.lock().unwrap();
            if self
                .shutdown
                .requested
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.tx.send(Request::Shutdown).is_err()
            } else {
                false
            }
        };
        if send_shutdown {
            self.shutdown.done.send_replace(true);
        }
        if !*done.borrow() {
            let _ = done.changed().await;
        }
    }
}

pub(crate) struct Guest {
    tx: mpsc::UnboundedSender<Request>,
    id: u64,
    rx: mpsc::UnboundedReceiver<Result<GuestStep, ToolError>>,
    first: bool,
    finished: bool,
}

impl Guest {
    pub(crate) async fn step(
        &mut self,
        completions: Vec<HostCompletion>,
    ) -> Result<GuestStep, ToolError> {
        if self.first {
            self.first = false;
        } else {
            self.tx
                .send(Request::Complete {
                    id: self.id,
                    completions,
                })
                .map_err(|_| worker_error("PTC Host 已关闭"))?;
        }
        self.receive().await
    }

    pub(crate) async fn receive(&mut self) -> Result<GuestStep, ToolError> {
        let result = self
            .rx
            .recv()
            .await
            .unwrap_or_else(|| Err(worker_error("PTC execution 通道已关闭")));
        self.finished = !matches!(
            &result,
            Ok(GuestStep {
                state: GuestState::Pending,
                ..
            })
        );
        result
    }

    pub(crate) fn cancel(&self, code: &str) {
        if !self.finished {
            let _ = self.tx.send(Request::Cancel {
                id: self.id,
                code: code.into(),
            });
        }
    }

    pub(crate) async fn shutdown(&mut self) {
        self.cancel("E_CANCELLED");
        // actor 的中断宽限期是硬上限；不响应取消时会杀死整个 Host 并关闭路由。
        while !self.finished {
            let _ = self.receive().await;
        }
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.cancel("E_CANCELLED");
    }
}

struct Route {
    steps: Steps,
    limit: usize,
    expires: Instant,
    cancelling: bool,
}

struct Process {
    child: Child,
    input: mpsc::UnboundedSender<Vec<u8>>,
    writer: JoinHandle<std::io::Result<()>>,
    output: BufReader<ChildStdout>,
    buffer: Vec<u8>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_task: JoinHandle<()>,
}

impl Process {
    fn spawn(command: &PtcWorkerCommand) -> Result<Self, ToolError> {
        let mut child = tokio::process::Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| worker_error(format!("启动 PTC Host 失败：{error}")))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let (input, mut writes) = mpsc::unbounded_channel::<Vec<u8>>();
        let writer = tokio::spawn(async move {
            while let Some(bytes) = writes.recv().await {
                stdin.write_all(&bytes).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
            }
            Ok(())
        });
        let tail = Arc::new(Mutex::new(Vec::new()));
        let thread_tail = tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut buffer = [0u8; 1024];
            while let Ok(count @ 1..) = stderr.read(&mut buffer).await {
                let mut tail = thread_tail.lock().unwrap();
                tail.extend_from_slice(&buffer[..count]);
                let excess = tail.len().saturating_sub(4096);
                tail.drain(..excess);
            }
        });
        Ok(Self {
            child,
            input,
            writer,
            output: BufReader::new(stdout),
            buffer: Vec::new(),
            stderr: tail,
            stderr_task,
        })
    }

    fn send(&self, message: &HostMessage) -> Result<(), ToolError> {
        let bytes = serde_json::to_vec(message).map_err(|error| worker_error(error.to_string()))?;
        if bytes.len() + 1 > MAX_FRAME_BYTES {
            return Err(ToolError::new("E_OUTPUT_LIMIT", "PTC 协议帧超过传输上限"));
        }
        self.input
            .send(bytes)
            .map_err(|_| worker_error("PTC Host 写入通道已关闭"))
    }

    async fn stop(mut self) {
        self.writer.abort();
        self.stderr_task.abort();
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        let _ = self.writer.await;
        let _ = self.stderr_task.await;
    }
}

/// buffer 属于进程而非 future，select 取消读取时不会丢失半帧；分配前检查上限。
async fn read_frame(process: &mut Process) -> Result<GuestMessage, ToolError> {
    loop {
        let available = process
            .output
            .fill_buf()
            .await
            .map_err(|error| worker_error(error.to_string()))?;
        if available.is_empty() {
            return Err(worker_error("PTC Host 输出结束"));
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |n| n + 1);
        if process.buffer.len() + count > MAX_FRAME_BYTES {
            return Err(worker_error("PTC Host 输出帧超限"));
        }
        let complete = available[count - 1] == b'\n';
        process.buffer.extend_from_slice(&available[..count]);
        process.output.consume(count);
        if complete {
            let result = serde_json::from_slice(&process.buffer)
                .map_err(|error| worker_error(format!("PTC Host 协议错误：{error}")));
            process.buffer.clear();
            return result;
        }
    }
}

fn worker_error(message: impl Into<String>) -> ToolError {
    ToolError::new("E_PTC_WORKER", message)
}

async fn fail(process: &mut Option<Process>, routes: &mut HashMap<u64, Route>, error: ToolError) {
    let mut message = error.message;
    if let Some(process) = process.take() {
        let tail = String::from_utf8_lossy(&process.stderr.lock().unwrap())
            .trim()
            .to_owned();
        if !tail.is_empty() {
            message.push_str(&format!("\nPTC Host stderr:\n{tail}"));
        }
        // 完整回收旧进程之后才能处理下一条 Start，绝不出现两个存活的 Host。
        process.stop().await;
    }
    for (_, route) in routes.drain() {
        let _ = route
            .steps
            .send(Err(ToolError::new(error.code, message.clone())));
    }
}

async fn manage(
    command: PtcWorkerCommand,
    mut requests: mpsc::UnboundedReceiver<Request>,
    shutdown: Arc<ShutdownState>,
) {
    let mut process: Option<Process> = None;
    let mut routes = HashMap::<u64, Route>::new();
    let mut idle = Duration::from_secs(600);
    let mut idle_at = Instant::now() + idle;
    loop {
        let expires = routes
            .values()
            .map(|route| route.expires)
            .min()
            .unwrap_or(idle_at);
        tokio::select! {
            request = requests.recv() => match request {
                Some(Request::Start { id, request, steps, max_runs, idle: timeout }) => {
                    if steps.is_closed() { continue; }
                    if routes.len() >= max_runs {
                        let _ = steps.send(Err(ToolError::new("E_QUEUE_LIMIT", "PTC Host 活跃 execution 已达并发上限")));
                        continue;
                    }
                    if process.is_none() {
                        match Process::spawn(&command) {
                            Ok(child) => process = Some(child),
                            Err(error) => { let _ = steps.send(Err(error)); continue; }
                        }
                    }
                    idle = timeout;
                    idle_at = Instant::now() + idle;
                    let expires = Instant::now() + Duration::from_millis(request.timeout_ms) + CANCEL_GRACE;
                    routes.insert(id, Route { steps, expires, limit: request.max_message_bytes, cancelling: false });
                    if let Err(error) = process.as_ref().unwrap().send(&HostMessage::Start { execution_id: id, request }) {
                        fail(&mut process, &mut routes, error).await;
                    }
                }
                Some(Request::Complete { id, completions }) => {
                    if routes.get(&id).is_some_and(|route| !route.cancelling) {
                        if let Err(error) = process.as_ref().unwrap().send(&HostMessage::Complete { execution_id: id, completions }) {
                            fail(&mut process, &mut routes, error).await;
                        }
                    }
                }
                Some(Request::Cancel { id, code }) => {
                    if let Some(route) = routes.get_mut(&id) {
                        if !route.cancelling {
                            route.cancelling = true;
                            route.expires = route.expires.min(Instant::now() + CANCEL_GRACE);
                            if let Err(error) = process.as_ref().unwrap().send(&HostMessage::Cancel { execution_id: id, code }) {
                                fail(&mut process, &mut routes, error).await;
                            }
                        }
                    }
                }
                None | Some(Request::Shutdown) => {
                    requests.close();
                    fail(&mut process, &mut routes, worker_error("PTC Host 已关闭")).await;
                    shutdown.done.send_replace(true);
                    return;
                }
            },
            message = async {
                match process.as_mut() {
                    Some(process) => Some(read_frame(process).await),
                    None => None,
                }
            }, if process.is_some() => {
                let Some(message) = message else { continue; };
                match message {
                    Ok(GuestMessage::Step { execution_id, step }) => {
                        let Some(route) = routes.get(&execution_id) else {
                            fail(&mut process, &mut routes, worker_error("PTC Host 返回未知 execution ID")).await;
                            continue;
                        };
                        let terminal = !matches!(step.state, GuestState::Pending);
                        if serde_json::to_vec(&step).map_or(true, |bytes| bytes.len() > route.limit) {
                            let Some(route) = routes.get_mut(&execution_id) else {
                                continue;
                            };
                            let _ = route.steps.send(Err(ToolError::new(
                                "E_OUTPUT_LIMIT",
                                "PTC Host execution 输出超限",
                            )));
                            if terminal {
                                routes.remove(&execution_id);
                                if routes.is_empty() {
                                    idle_at = Instant::now() + idle;
                                }
                            } else {
                                route.cancelling = true;
                                route.expires = route.expires.min(Instant::now() + CANCEL_GRACE);
                                if let Err(error) = process.as_ref().unwrap().send(&HostMessage::Cancel {
                                    execution_id,
                                    code: "E_OUTPUT_LIMIT".into(),
                                }) {
                                    fail(&mut process, &mut routes, error).await;
                                }
                            }
                            continue;
                        }
                        let _ = route.steps.send(Ok(step));
                        if terminal { routes.remove(&execution_id); }
                        if routes.is_empty() { idle_at = Instant::now() + idle; }
                    }
                    Err(error) => fail(&mut process, &mut routes, error).await,
                }
            },
            _ = tokio::time::sleep_until(expires), if process.is_some() => {
                if routes.is_empty() {
                    if let Some(process) = process.take() { process.stop().await; }
                } else {
                    fail(&mut process, &mut routes, worker_error("PTC Host 在中断宽限期内未回收 execution，已强制重启")).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

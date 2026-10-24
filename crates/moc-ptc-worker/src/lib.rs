//! 长期 PTC Host：统一路由 execution，每次执行创建并销毁独立的 QuickJS Runtime。

use moc_ptc_protocol::wire::{
    GuestMessage, GuestState, GuestStep, HostCompletion, HostMessage, MAX_FRAME_BYTES, ScriptError,
    StartRequest,
};
use rquickjs::{Context, Function, Object, Persistent, Runtime};
use std::{
    collections::HashMap,
    io::{BufRead, Read, Write},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

enum Command {
    Complete(Vec<HostCompletion>),
    Cancel,
}

#[derive(Clone)]
struct Control {
    reason: Arc<Mutex<Option<String>>>,
    deadline: Instant,
}

impl Control {
    fn reason(&self) -> Option<String> {
        self.reason
            .lock()
            .unwrap()
            .clone()
            .or_else(|| (Instant::now() >= self.deadline).then(|| "E_TIMEOUT".to_owned()))
    }

    fn cancel(&self, code: String) {
        self.reason.lock().unwrap().get_or_insert(code);
    }
}

enum Event {
    Input(HostMessage),
    Closed,
    Error(String),
    Step(u64, GuestStep),
}

struct Execution {
    commands: mpsc::Sender<Command>,
    control: Control,
    join: thread::JoinHandle<()>,
}

/// 输入读取与 JS 执行互不阻塞；输出仅由 Host 主循环写入。
/// 输入线程遇到 Shutdown/EOF 就退出，协议错误由调用方结束整个 Host 进程。
pub fn run<R: BufRead + Send + 'static, W: Write>(mut input: R, mut output: W) -> Result<()> {
    let (events, incoming) = mpsc::sync_channel(64);
    let reader_events = events.clone();
    thread::Builder::new()
        .name("moc-ptc-input".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                loop {
                    let mut line = Vec::new();
                    // 在分配前限长，不使用先 read_line 再检查的无界缓冲。
                    let count = (&mut input)
                        .take((MAX_FRAME_BYTES + 1) as u64)
                        .read_until(b'\n', &mut line)?;
                    if count == 0 {
                        let _ = reader_events.send(Event::Closed);
                        return Ok(());
                    }
                    if count > MAX_FRAME_BYTES || !line.ends_with(b"\n") {
                        return Err("PTC 输入帧超限或不完整".into());
                    }
                    let message: HostMessage = serde_json::from_slice(&line)?;
                    let shutdown = matches!(message, HostMessage::Shutdown);
                    reader_events.send(Event::Input(message))?;
                    if shutdown {
                        return Ok(());
                    }
                }
            })();
            if let Err(error) = result {
                let _ = reader_events.send(Event::Error(error.to_string()));
            }
        })?;

    // 只保留活跃 execution；ID 随 Host 生命周期递增但活跃集合稀疏。
    let mut executions = HashMap::<u64, Execution>::new();
    let mut last_id = None;
    let mut stopping = false;
    let result = (|| -> Result<()> {
        loop {
            match incoming.recv()? {
                Event::Input(HostMessage::Start {
                    execution_id,
                    request,
                }) => {
                    if stopping {
                        continue;
                    }
                    if last_id.is_some_and(|last| execution_id <= last) {
                        return Err("PTC execution ID 必须递增且唯一".into());
                    }
                    last_id = Some(execution_id);
                    let (commands, receiver) = mpsc::channel();
                    let control = Control {
                        reason: Arc::new(Mutex::new(None)),
                        deadline: Instant::now() + Duration::from_millis(request.timeout_ms),
                    };
                    let thread_control = control.clone();
                    let thread_events = events.clone();
                    let join = thread::Builder::new()
                        .name(format!("moc-ptc-{execution_id}"))
                        .spawn(move || {
                            let limit = request.max_message_bytes;
                            let result = execute(&request, &receiver, &thread_control, |step| {
                                thread_events.send(Event::Step(execution_id, step))?;
                                Ok(())
                            });
                            // execute 返回时 Runtime 已经销毁；终态也是资源回收确认。
                            let mut step =
                                result.unwrap_or_else(|error| failed("E_JS", error.to_string()));
                            if let Some(reason) = thread_control.reason() {
                                step = failed(&reason, "PTC execution 已中断");
                            }
                            if serde_json::to_vec(&step).map_or(true, |bytes| bytes.len() > limit) {
                                step = failed("E_OUTPUT_LIMIT", "PTC execution 输出超限");
                            }
                            let _ = thread_events.send(Event::Step(execution_id, step));
                        })?;
                    executions.insert(
                        execution_id,
                        Execution {
                            commands,
                            control,
                            join,
                        },
                    );
                }
                Event::Input(HostMessage::Complete {
                    execution_id,
                    completions,
                }) => {
                    if let Some(execution) = executions.get(&execution_id) {
                        // 超时终态可能与 Core 的 completion 交错，已退出线程的消息可以丢弃。
                        let _ = execution.commands.send(Command::Complete(completions));
                    } else if last_id.is_none_or(|last| execution_id > last) {
                        return Err("未知的 PTC execution ID".into());
                    }
                }
                Event::Input(HostMessage::Cancel { execution_id, code }) => {
                    if let Some(execution) = executions.get(&execution_id) {
                        execution.control.cancel(code);
                        let _ = execution.commands.send(Command::Cancel);
                    } else if last_id.is_none_or(|last| execution_id > last) {
                        return Err("未知的 PTC execution ID".into());
                    }
                }
                Event::Input(HostMessage::Shutdown) | Event::Closed => {
                    // 只停止接收新 Start，并中断活跃 execution；它们的终态帧仍要写出。
                    stopping = true;
                    for execution in executions.values() {
                        execution.control.cancel("E_CANCELLED".into());
                        let _ = execution.commands.send(Command::Cancel);
                    }
                }
                Event::Error(error) => return Err(error.into()),
                Event::Step(execution_id, step) => {
                    if !matches!(step.state, GuestState::Pending)
                        && let Some(execution) = executions.remove(&execution_id)
                    {
                        execution
                            .join
                            .join()
                            .map_err(|_| "PTC execution 线程异常退出")?;
                    }
                    let line = serde_json::to_vec(&GuestMessage::Step { execution_id, step })?;
                    if line.len() + 1 > MAX_FRAME_BYTES {
                        return Err("PTC 输出帧超限".into());
                    }
                    output.write_all(&line)?;
                    output.write_all(b"\n")?;
                    output.flush()?;
                }
            }
            if stopping && executions.is_empty() {
                return Ok(());
            }
        }
    })();
    for execution in executions.values() {
        execution.control.cancel("E_CANCELLED".into());
        let _ = execution.commands.send(Command::Cancel);
    }
    // 先关闭接收端，避免执行线程在发送终态时阻塞 join。
    drop(incoming);
    for (_, execution) in executions {
        let _ = execution.join.join();
    }
    result
}

pub fn run_stdio() -> Result<()> {
    run(std::io::BufReader::new(std::io::stdin()), std::io::stdout())
}

fn execute(
    start: &StartRequest,
    commands: &mpsc::Receiver<Command>,
    control: &Control,
    mut emit: impl FnMut(GuestStep) -> Result<()>,
) -> Result<GuestStep> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(start.memory_limit_bytes);
    runtime.set_max_stack_size(256 * 1024);
    let interrupt = control.clone();
    // QuickJS 的同步循环也检查期限；等待宿主和微任务循环分别在下面检查。
    runtime.set_interrupt_handler(Some(Box::new(move || interrupt.reason().is_some())));
    let context = Context::full(&runtime)?;
    let object = context.with(|ctx| -> rquickjs::Result<Persistent<Object<'static>>> {
        let object: Object = ctx.eval(moc_ptc_protocol::PRELUDE)?;
        let start_fn: Function = object.get("start")?;
        start_fn.call::<_, ()>((
            start.code.as_str(),
            start.max_pending_calls,
            start.max_total_calls,
            start.max_output_bytes,
        ))?;
        Ok(Persistent::save(&ctx, object))
    })?;
    loop {
        if let Some(reason) = control.reason() {
            return Ok(failed(&reason, "PTC execution 已中断"));
        }
        while runtime.is_job_pending() {
            if let Some(reason) = control.reason() {
                return Ok(failed(&reason, "PTC execution 已中断"));
            }
            runtime
                .execute_pending_job()
                .map_err(|error| format!("PTC Promise job 失败：{error:?}"))?;
        }
        let step = context.with(|ctx| -> Result<GuestStep> {
            let object = object.clone().restore(&ctx)?;
            let take: Function = object.get("take")?;
            let status: Function = object.get("status")?;
            Ok(GuestStep {
                requests: serde_json::from_str(&take.call::<_, String>(())?)?,
                state: serde_json::from_str(&status.call::<_, String>(())?)?,
            })
        })?;
        if serde_json::to_vec(&step)?.len() > start.max_message_bytes {
            return Ok(failed("E_OUTPUT_LIMIT", "PTC execution 输出超限"));
        }
        if !matches!(step.state, GuestState::Pending) {
            return Ok(step);
        }
        emit(step)?;
        match commands.recv_timeout(control.deadline.saturating_duration_since(Instant::now())) {
            Ok(Command::Complete(completions)) => {
                let values = match completions
                    .into_iter()
                    .map(|completion| {
                        let (value, error) = match completion.result {
                            Ok(value) => (serde_json::to_string(&value)?, "null".into()),
                            Err(error) => ("null".into(), serde_json::to_string(&error)?),
                        };
                        Ok((completion.id, value, error))
                    })
                    .collect::<Result<Vec<_>>>()
                {
                    Ok(values) => values,
                    Err(error) => return Ok(failed("E_PTC_PROTOCOL", error.to_string())),
                };
                if let Err(error) = context.with(|ctx| -> rquickjs::Result<()> {
                    let object = object.clone().restore(&ctx)?;
                    let complete: Function = object.get("complete")?;
                    for (id, value, error) in values {
                        complete.call::<_, ()>((
                            id,
                            ctx.json_parse(value)?,
                            ctx.json_parse(error)?,
                        ))?;
                    }
                    Ok(())
                }) {
                    return Ok(failed("E_PTC_PROTOCOL", error.to_string()));
                }
            }
            Ok(Command::Cancel) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Ok(failed("E_TIMEOUT", "script timed out"));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(failed("E_CANCELLED", "宿主关闭了 execution"));
            }
        }
    }
}

fn failed(code: &str, message: impl ToString) -> GuestStep {
    GuestStep {
        requests: Vec::new(),
        state: GuestState::Failed {
            error: ScriptError {
                code: code.into(),
                message: message.to_string(),
                result: None,
            },
        },
    }
}

#[cfg(test)]
mod tests;

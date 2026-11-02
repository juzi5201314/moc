//! workflow 模式：一个 run 独占一个 worker 进程；无墙钟超时，只有取消与关闭。

use crate::Result;
use moc_ptc_protocol::{
    WORKFLOW_PRELUDE,
    wire::{
        GuestState, HostCompletion, MAX_FRAME_BYTES, ScriptError, WorkflowGuestMessage,
        WorkflowGuestStep, WorkflowHostMessage, WorkflowStallInfo, WorkflowStartRequest,
    },
};
use rquickjs::{Context, Function, Object, Persistent, Runtime};
use std::{
    io::{BufRead, Read, Write},
    sync::{Arc, Mutex, mpsc},
    thread,
};

enum Command {
    Complete(Vec<HostCompletion>),
    Cancel,
}

/// 取消原因；workflow 无墙钟超时，唯一中断源是宿主。
#[derive(Clone, Default)]
struct Control {
    reason: Arc<Mutex<Option<String>>>,
}

impl Control {
    fn reason(&self) -> Option<String> {
        self.reason.lock().unwrap().clone()
    }

    fn cancel(&self, code: String) {
        self.reason.lock().unwrap().get_or_insert(code);
    }
}

enum Event {
    Input(WorkflowHostMessage),
    Closed,
    Error(String),
    Step(WorkflowGuestStep),
}

/// 宿主一帧输入触发一步；输入读线程与 JS 线程分离，写只发生在主循环。
pub fn run_workflow<R: BufRead + Send + 'static, W: Write>(
    mut input: R,
    mut output: W,
) -> Result<()> {
    let (events, incoming) = mpsc::sync_channel(64);
    let reader_events = events.clone();
    thread::Builder::new()
        .name("moc-wf-input".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                loop {
                    let mut line = Vec::new();
                    let count = (&mut input)
                        .take((MAX_FRAME_BYTES + 1) as u64)
                        .read_until(b'\n', &mut line)?;
                    if count == 0 {
                        let _ = reader_events.send(Event::Closed);
                        return Ok(());
                    }
                    if count > MAX_FRAME_BYTES || !line.ends_with(b"\n") {
                        return Err("workflow 输入帧超限或不完整".into());
                    }
                    let message: WorkflowHostMessage = serde_json::from_slice(&line)?;
                    let shutdown = matches!(message, WorkflowHostMessage::Shutdown);
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

    let (commands, receiver) = mpsc::channel();
    let mut receiver = Some(receiver);
    let control = Control::default();
    let mut execution: Option<thread::JoinHandle<()>> = None;
    let mut stopping = false;
    let result = (|| -> Result<()> {
        loop {
            match incoming.recv()? {
                Event::Input(WorkflowHostMessage::Start { request }) => {
                    if stopping || execution.is_some() {
                        continue;
                    }
                    let thread_control = control.clone();
                    let thread_events = events.clone();
                    let receiver = receiver.take().unwrap();
                    execution = Some(
                        thread::Builder::new()
                            .name("moc-wf-run".into())
                            .spawn(move || {
                                let limit = request.max_message_bytes;
                                let result =
                                    execute(&request, &receiver, &thread_control, |step| {
                                        thread_events.send(Event::Step(step))?;
                                        Ok(())
                                    });
                                let mut step =
                                    result.unwrap_or_else(|error| failed("E_JS", error.to_string()));
                                if let Some(reason) = thread_control.reason() {
                                    step = failed(&reason, "workflow run 已中断");
                                }
                                if serde_json::to_vec(&step)
                                    .map_or(true, |bytes| bytes.len() > limit)
                                {
                                    step = failed("E_OUTPUT_LIMIT", "workflow run 输出超限");
                                }
                                let _ = thread_events.send(Event::Step(step));
                            })?,
                    );
                }
                Event::Input(WorkflowHostMessage::Complete { completions }) => {
                    if execution.is_some() {
                        let _ = commands.send(Command::Complete(completions));
                    }
                }
                Event::Input(WorkflowHostMessage::Cancel { code }) => {
                    control.cancel(code);
                    let _ = commands.send(Command::Cancel);
                }
                Event::Input(WorkflowHostMessage::Shutdown) | Event::Closed => {
                    stopping = true;
                    control.cancel("E_CANCELLED".into());
                    let _ = commands.send(Command::Cancel);
                    if execution.is_none() {
                        return Ok(());
                    }
                }
                Event::Error(error) => return Err(error.into()),
                Event::Step(step) => {
                    let terminal = !matches!(step.state, GuestState::Pending);
                    let line = serde_json::to_vec(&WorkflowGuestMessage::Step { step })?;
                    if line.len() + 1 > MAX_FRAME_BYTES {
                        return Err("workflow 输出帧超限".into());
                    }
                    output.write_all(&line)?;
                    output.write_all(b"\n")?;
                    output.flush()?;
                    if terminal {
                        if let Some(join) = execution.take() {
                            join.join().map_err(|_| "workflow run 线程异常退出")?;
                        }
                        if stopping {
                            return Ok(());
                        }
                    }
                }
            }
        }
    })();
    control.cancel("E_CANCELLED".into());
    let _ = commands.send(Command::Cancel);
    drop(incoming);
    if let Some(join) = execution {
        let _ = join.join();
    }
    result
}

pub fn run_workflow_stdio() -> Result<()> {
    run_workflow(std::io::BufReader::new(std::io::stdin()), std::io::stdout())
}

/// `take()`/`status()` 返回 JSON 文本；这里是 take 的批次部分。
struct Batch {
    events: Vec<moc_ptc_protocol::wire::WorkflowEvent>,
    requests: Vec<moc_ptc_protocol::wire::WorkflowHostRequest>,
}

impl Batch {
    fn parse(raw: &str) -> Result<Self> {
        let mut value: serde_json::Value = serde_json::from_str(raw)?;
        Ok(Self {
            events: serde_json::from_value(value["events"].take())?,
            requests: serde_json::from_value(value["requests"].take())?,
        })
    }
}

fn execute(
    start: &WorkflowStartRequest,
    commands: &mpsc::Receiver<Command>,
    control: &Control,
    mut emit: impl FnMut(WorkflowGuestStep) -> Result<()>,
) -> Result<WorkflowGuestStep> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(start.memory_limit_bytes);
    runtime.set_max_stack_size(256 * 1024);
    let interrupt = control.clone();
    // 无墙钟超时；取消经 interrupt handler 也能打断同步死循环。
    runtime.set_interrupt_handler(Some(Box::new(move || interrupt.reason().is_some())));
    let context = Context::full(&runtime)?;
    let object = context.with(|ctx| -> rquickjs::Result<Persistent<Object<'static>>> {
        let object: Object = ctx.eval(WORKFLOW_PRELUDE)?;
        let start_fn: Function = object.get("start")?;
        start_fn.call::<_, ()>((
            start.code.as_str(),
            serde_json::to_string(&start.args).unwrap_or_default(),
            serde_json::to_string(&start.run).unwrap_or_default(),
            start.max_pending_calls,
            start.max_total_calls,
            start.max_output_bytes,
        ))?;
        Ok(Persistent::save(&ctx, object))
    })?;
    loop {
        if let Some(reason) = control.reason() {
            return Ok(failed(&reason, "workflow run 已中断"));
        }
        while runtime.is_job_pending() {
            if let Some(reason) = control.reason() {
                return Ok(failed(&reason, "workflow run 已中断"));
            }
            runtime
                .execute_pending_job()
                .map_err(|error| format!("workflow Promise job 失败：{error:?}"))?;
        }
        let step = context.with(|ctx| -> Result<WorkflowGuestStep> {
            let object = object.clone().restore(&ctx)?;
            let take: Function = object.get("take")?;
            let status: Function = object.get("status")?;
            let batch = Batch::parse(&take.call::<_, String>(())?)?;
            let state: GuestState = serde_json::from_str(&status.call::<_, String>(())?)?;
            Ok(WorkflowGuestStep {
                events: batch.events,
                requests: batch.requests,
                state,
            })
        })?;
        if serde_json::to_vec(&step)?.len() > start.max_message_bytes {
            return Ok(failed("E_OUTPUT_LIMIT", "workflow run 输出超限"));
        }
        if !matches!(step.state, GuestState::Pending) {
            return Ok(step);
        }
        // 停滞检测：本步无新请求且脚本未完成 → 问 guest 是否还有在飞请求；没有即死局。
        if step.requests.is_empty() {
            emit(step)?;
            let info = context.with(|ctx| -> Result<Option<WorkflowStallInfo>> {
                let object = object.clone().restore(&ctx)?;
                let stalled: Function = object.get("stalled")?;
                let raw: String = stalled.call(())?;
                Ok(serde_json::from_str::<Option<WorkflowStallInfo>>(&raw)?)
            })?;
            if let Some(info) = info {
                if info.waiting.is_empty() {
                    return Ok(failed(
                        "ScriptStalled",
                        "the script is suspended with no request in flight and no channel receiver waiting, so nothing can ever resolve what it awaits",
                    ));
                }
                let detail = info
                    .waiting
                    .iter()
                    .map(|wait| format!("{} on \"{}\"", wait.receivers, wait.channel))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Ok(failed(
                    "ChannelDeadlock",
                    format!("deadlock: receiver(s) waiting ({detail}) with no request in flight"),
                ));
            }
        } else {
            emit(step)?;
        }
        match commands.recv() {
            Ok(Command::Complete(completions)) => {
                let values = completions
                    .into_iter()
                    .map(|completion| {
                        let (value, error) = match completion.result {
                            Ok(value) => (serde_json::to_string(&value)?, "null".into()),
                            Err(error) => ("null".into(), serde_json::to_string(&error)?),
                        };
                        Ok((completion.id, value, error))
                    })
                    .collect::<Result<Vec<_>>>()?;
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
            Err(mpsc::RecvError) => return Ok(failed("E_CANCELLED", "宿主关闭了 workflow run")),
        }
    }
}

fn failed(code: &str, message: impl ToString) -> WorkflowGuestStep {
    WorkflowGuestStep {
        events: Vec::new(),
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
mod tests {
    use super::*;
    use moc_ptc_protocol::wire::{WorkflowGuestMessage, WorkflowRunInfo};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::time::Duration;

    /// channel 输入：测试线程可在终态 step 之后再发 Shutdown（避免竞态取消）。
    struct ChanReader {
        rx: mpsc::Receiver<Vec<u8>>,
        buf: VecDeque<u8>,
    }
    impl std::io::Read for ChanReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let data = self.fill_buf()?;
            let n = data.len().min(out.len());
            out[..n].copy_from_slice(&data[..n]);
            self.consume(n);
            Ok(n)
        }
    }
    impl BufRead for ChanReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            while self.buf.is_empty() {
                match self.rx.recv() {
                    Ok(chunk) => self.buf.extend(chunk),
                    Err(_) => break,
                }
            }
            Ok(self.buf.make_contiguous())
        }
        fn consume(&mut self, amount: usize) {
            self.buf.drain(..amount.min(self.buf.len()));
        }
    }
    struct ChanWriter(mpsc::Sender<Vec<u8>>);
    impl Write for ChanWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .send(buf.to_vec())
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 驱动一个 run：发出 Start，等到终态 step 再发 Shutdown；返回全部 step。
    fn drive(code: &str) -> Vec<WorkflowGuestStep> {
        let start = WorkflowHostMessage::Start {
            request: WorkflowStartRequest {
                code: code.into(),
                args: json!({}),
                run: WorkflowRunInfo { id: "test-run".into(), resumed_from: None },
                memory_limit_bytes: 64 * 1024 * 1024,
                max_output_bytes: 1 << 20,
                max_message_bytes: 1 << 20,
                max_pending_calls: 64,
                max_total_calls: 256,
            },
        };
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        let worker = thread::spawn(move || {
            run_workflow(ChanReader { rx: in_rx, buf: VecDeque::new() }, ChanWriter(out_tx))
        });
        let mut frame = serde_json::to_vec(&start).unwrap();
        frame.push(b'\n');
        in_tx.send(frame).unwrap();
        let mut steps = Vec::new();
        let mut pending_line = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let Ok(chunk) = out_rx.recv_timeout(Duration::from_secs(10)) else {
                panic!("workflow 无输出");
            };
            pending_line.extend(chunk);
            while let Some(pos) = pending_line.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending_line.drain(..=pos).collect();
                let message: WorkflowGuestMessage =
                    serde_json::from_slice(&line[..line.len() - 1]).unwrap();
                let WorkflowGuestMessage::Step { step } = message;
                let terminal = !matches!(step.state, GuestState::Pending);
                steps.push(step);
                if terminal {
                    let mut shutdown = serde_json::to_vec(&WorkflowHostMessage::Shutdown).unwrap();
                    shutdown.push(b'\n');
                    in_tx.send(shutdown).unwrap();
                    drop(in_tx);
                    worker.join().unwrap().unwrap();
                    return steps;
                }
            }
        }
        panic!("workflow 未产生终态 step");
    }

    fn terminal(steps: &[WorkflowGuestStep]) -> &WorkflowGuestStep {
        steps
            .iter()
            .find(|s| !matches!(s.state, GuestState::Pending))
            .expect("run never produced a terminal step")
    }

    #[test]
    fn deterministic_prelude_bans_nondeterminism() {
        for code in ["Date.now()", "Math.random()", "performance.now()", "new Date()"] {
            let steps = drive(code);
            match &terminal(&steps).state {
                GuestState::Failed { error } => assert!(
                    error.message.contains("disabled")
                        || error.message.contains("not a function")
                        || error.message.contains("is not defined"),
                    "{code} should fail deterministically, got {error:?}"
                ),
                other => panic!("{code} should fail, got {other:?}"),
            }
        }
    }

    #[test]
    fn events_and_done() {
        let steps = drive(
            r#"__wf.log("log#1", "hello");
               __wf.agent("actor#1", "w");
               return 7;"#,
        );
        match &terminal(&steps).state {
            GuestState::Done { value } => assert_eq!(*value, json!(7)),
            other => panic!("expected done, got {other:?}"),
        }
        let events: Vec<_> = steps.iter().flat_map(|s| s.events.iter()).collect();
        assert!(events.len() >= 2, "expected log+actor events, got {events:?}");
    }

    #[test]
    fn channel_deadlock_detected() {
        let steps = drive(
            r#"const ch = __wf.channel("channel#1", "c");
               for await (const x of ch) {}"#,
        );
        match &terminal(&steps).state {
            GuestState::Failed { error } => assert_eq!(error.code, "ChannelDeadlock"),
            other => panic!("expected ChannelDeadlock, got {other:?}"),
        }
    }

    #[test]
    fn script_stalled_detected() {
        let steps = drive("await new Promise(() => {});");
        match &terminal(&steps).state {
            GuestState::Failed { error } => assert_eq!(error.code, "ScriptStalled"),
            other => panic!("expected ScriptStalled, got {other:?}"),
        }
    }
}

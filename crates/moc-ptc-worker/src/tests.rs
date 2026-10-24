//! 通过真实流式协议驱动长期 Host；不依赖外部二进制或网络。
use super::*;
use moc_ptc_protocol::wire::{Operation, ScriptToolResult};
use serde_json::{Value, json};
use std::io::{BufReader, Cursor};

struct Input {
    rx: mpsc::Receiver<Vec<u8>>,
    current: Cursor<Vec<u8>>,
}
impl Read for Input {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.current.position() == self.current.get_ref().len() as u64 {
            self.current = Cursor::new(self.rx.recv().unwrap_or_default());
        }
        self.current.read(buffer)
    }
}
struct Output(mpsc::Sender<GuestMessage>, Vec<u8>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.1.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .send(serde_json::from_slice(&self.1).unwrap())
            .unwrap();
        self.1.clear();
        Ok(())
    }
}
struct Host {
    input: mpsc::Sender<Vec<u8>>,
    output: mpsc::Receiver<GuestMessage>,
    join: Option<thread::JoinHandle<Result<()>>>,
}
impl Host {
    fn new() -> Self {
        let (input, rx) = mpsc::channel();
        let (tx, output) = mpsc::channel();
        let join = thread::spawn(move || {
            run(
                BufReader::new(Input {
                    rx,
                    current: Cursor::new(Vec::new()),
                }),
                Output(tx, Vec::new()),
            )
        });
        Self {
            input,
            output,
            join: Some(join),
        }
    }
    fn send(&self, message: HostMessage) {
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.input.send(bytes).unwrap();
    }
    fn receive(&self) -> (u64, GuestStep) {
        match self
            .output
            .recv_timeout(Duration::from_secs(3))
            .expect("Host 应及时响应")
        {
            GuestMessage::Step { execution_id, step } => (execution_id, step),
        }
    }
    fn start(&self, id: u64, request: StartRequest) {
        self.send(HostMessage::Start {
            execution_id: id,
            request,
        });
    }
    fn complete(&self, id: u64, call: u32, value: Value) {
        self.send(HostMessage::Complete {
            execution_id: id,
            completions: vec![HostCompletion {
                id: call,
                result: Ok(value),
            }],
        });
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.send(HostMessage::Shutdown);
        self.join.take().unwrap().join().unwrap().unwrap();
    }
}
fn start(code: &str) -> StartRequest {
    StartRequest {
        code: code.into(),
        memory_limit_bytes: 64 * 1024 * 1024,
        max_output_bytes: 4 * 1024 * 1024,
        max_message_bytes: 16 * 1024 * 1024,
        max_pending_calls: 256,
        max_total_calls: 1000,
        timeout_ms: 10_000,
    }
}
fn run_with(start: StartRequest, completions: &[Vec<HostCompletion>]) -> Vec<GuestStep> {
    let host = Host::new();
    host.start(1, start);
    let mut steps = vec![host.receive().1];
    for completions in completions {
        // 保持原有测试数据结构，不要求协议类型为测试派生 Clone。
        let completions =
            serde_json::from_value(serde_json::to_value(completions).unwrap()).unwrap();
        host.send(HostMessage::Complete {
            execution_id: 1,
            completions,
        });
        steps.push(host.receive().1);
    }
    steps
}
fn tool_result(content: &str, structured: Option<Value>) -> Value {
    serde_json::to_value(ScriptToolResult {
        content: content.into(),
        summary: None,
        structured_content: structured,
        is_error: false,
        status: "completed".into(),
        artifact: None,
    })
    .unwrap()
}
fn done(step: &GuestStep, expected: Value) {
    assert!(
        matches!(&step.state, GuestState::Done { value } if value == &expected),
        "{step:?}"
    );
}
fn failed_with(step: &GuestStep, code: &str) {
    assert!(
        matches!(&step.state, GuestState::Failed { error } if error.code == code),
        "{step:?}"
    );
}

#[test]
fn executes_arithmetic() {
    let steps = run_with(start("return [1,2,3].reduce((a,b)=>a+b,0);"), &[]);
    assert_eq!(steps.len(), 1);
    done(&steps[0], json!(6));
}
#[test]
fn round_trips_tool_result() {
    let completions = vec![HostCompletion {
        id: 0,
        result: Ok(tool_result("查询完成", Some(json!({"items":[{"id":7}]})))),
    }];
    let steps = run_with(
        start("const r = await tools.example({q:1}); return r.structuredContent.items[0].id;"),
        &[completions],
    );
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].requests.len(), 1);
    assert!(
        matches!(&steps[0].requests[0].operation, Operation::Tool { name, .. } if name == "example")
    );
    done(&steps[1], json!(7));
}
#[test]
fn keeps_parallel_tool_calls_in_one_batch() {
    let completions = vec![
        HostCompletion {
            id: 0,
            result: Ok(tool_result("a", None)),
        },
        HostCompletion {
            id: 1,
            result: Ok(tool_result("b", None)),
        },
    ];
    let steps = run_with(
        start(
            "const [a,b] = await Promise.all([tools.a(), tools.b()]); return [a.content,b.content];",
        ),
        &[completions],
    );
    assert_eq!(steps[0].requests.len(), 2);
    assert!(matches!(&steps[0].requests[0].operation, Operation::Tool { name, .. } if name == "a"));
    assert!(matches!(&steps[0].requests[1].operation, Operation::Tool { name, .. } if name == "b"));
    done(&steps[1], json!(["a", "b"]));
}
#[test]
fn reports_js_failure() {
    let steps = run_with(start("throw new Error('boom');"), &[]);
    assert!(
        matches!(&steps[0].state, GuestState::Failed { error } if error.message.contains("boom"))
    );
}
#[test]
fn state_operations_are_requests() {
    let store = vec![HostCompletion {
        id: 0,
        result: Ok(Value::Null),
    }];
    let load = vec![HostCompletion {
        id: 1,
        result: Ok(json!({"found": true, "value": {"n": 1}})),
    }];
    let steps = run_with(
        start("await state.store('k', {n: 1}); return await state.load('k');"),
        &[store, load],
    );
    assert_eq!(steps[0].requests.len(), 1);
    assert!(matches!(&steps[0].requests[0].operation, Operation::Store { key, .. } if key == "k"));
    assert!(matches!(&steps[1].requests[0].operation, Operation::Load { key } if key == "k"));
    done(&steps[2], json!({"n":1}));
}
#[test]
fn multiplexes_executions_and_resets_runtime() {
    let host = Host::new();
    host.start(
        1,
        start("globalThis.leak = 42; Object.prototype.leak = 43; return await tools.a();"),
    );
    assert_eq!(host.receive().0, 1);
    host.start(
        2,
        start("return [typeof globalThis.leak, typeof ({}).leak, await tools.b()];"),
    );
    let (id, step) = host.receive();
    assert_eq!(id, 2);
    assert_eq!(step.requests[0].id, 0);
    host.complete(2, 0, json!("second"));
    let (id, step) = host.receive();
    assert_eq!(id, 2);
    done(&step, json!(["undefined", "undefined", "second"]));
    host.complete(1, 0, json!("first"));
    done(&host.receive().1, json!("first"));
    host.start(3, start("return typeof globalThis.leak;"));
    done(&host.receive().1, json!("undefined"));
}
#[test]
fn total_call_budget_counts_tools_catalog_and_state() {
    let host = Host::new();
    let mut request =
        start("await tools.a(); await catalog.servers(); await state.keys(); await tools.b();");
    request.max_total_calls = 3;
    host.start(1, request);
    for call in 0..3 {
        let (_, step) = host.receive();
        assert_eq!(step.requests[0].id, call);
        host.complete(1, call, json!([]));
    }
    failed_with(&host.receive().1, "E_CALL_LIMIT");
}
#[test]
fn deadlines_interrupt_cpu_microtasks_and_tool_wait_without_killing_host() {
    let host = Host::new();
    for (index, code) in [
        "while (true) {}",
        "while (true) await Promise.resolve();",
        "await tools.slow();",
    ]
    .into_iter()
    .enumerate()
    {
        let id = index as u64 + 1;
        let mut request = start(code);
        request.timeout_ms = 100;
        host.start(id, request);
        loop {
            let (actual, step) = host.receive();
            assert_eq!(actual, id);
            if matches!(step.state, GuestState::Pending) {
                continue;
            }
            failed_with(&step, "E_TIMEOUT");
            break;
        }
    }
    host.start(4, start("return 7"));
    done(&host.receive().1, json!(7));
}
#[test]
fn cancellation_is_local_even_during_sync_loop() {
    let host = Host::new();
    host.start(1, start("while (true) {}"));
    host.start(2, start("return await tools.keep();"));
    let (id, _) = host.receive();
    assert_eq!(id, 2);
    host.send(HostMessage::Cancel {
        execution_id: 1,
        code: "E_CANCELLED".into(),
    });
    let (id, step) = host.receive();
    assert_eq!(id, 1);
    failed_with(&step, "E_CANCELLED");
    host.complete(2, 0, json!(8));
    done(&host.receive().1, json!(8));
}
#[test]
fn malformed_completion_is_a_protocol_error_and_does_not_poison_the_host() {
    let host = Host::new();
    host.start(1, start("await tools.a();"));
    let (id, step) = host.receive();
    assert_eq!(id, 1);
    assert_eq!(step.requests.len(), 1);
    host.send(HostMessage::Complete {
        execution_id: 1,
        completions: vec![HostCompletion {
            id: 99,
            result: Ok(json!(null)),
        }],
    });
    failed_with(&host.receive().1, "E_PTC_PROTOCOL");
    host.start(2, start("return 7"));
    done(&host.receive().1, json!(7));
}
#[test]
fn output_and_pending_limits_remain_enforced() {
    let mut request = start("return 'x'.repeat(2048);");
    request.max_output_bytes = 128;
    failed_with(&run_with(request, &[])[0], "E_OUTPUT_LIMIT");
    let mut request = start("await Promise.all([tools.a(), tools.b()]);");
    request.max_pending_calls = 1;
    failed_with(&run_with(request, &[])[0], "E_QUEUE_LIMIT");
}

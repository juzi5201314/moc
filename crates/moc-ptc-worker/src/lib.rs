//! PTC worker：一个进程只执行一段 JavaScript，通过一行一条 JSON 与宿主推进其中的 Promise。

use moc_ptc_protocol::wire::{GuestState, GuestStep, HostCompletion, ScriptError, StartRequest};
use rquickjs::{Context, Function, Object, Persistent, Runtime};
use std::io::{BufRead, Write};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

const MAX_START_LINE: usize = 16 * 1024 * 1024;

/// 在给定的 stdin/stdout 上运行一个 worker；生产入口是 [`run_stdio`]。
pub fn run<R: BufRead, W: Write>(mut input: R, mut output: W) -> Result<()> {
    let start = read_start(&mut input)?;
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(start.memory_limit_bytes);
    runtime.set_max_stack_size(256 * 1024);
    let context = Context::full(&runtime)?;
    let control = context.with(|ctx| -> rquickjs::Result<Persistent<Object<'static>>> {
        let control: Object = ctx.eval(moc_ptc_protocol::PRELUDE)?;
        let start_fn: Function = control.get("start")?;
        start_fn.call::<_, ()>((
            start.code.as_str(),
            start.max_pending_calls,
            start.max_output_bytes,
        ))?;
        Ok(Persistent::save(&ctx, control))
    })?;

    loop {
        let step = drive(&context, &control);
        write_step(&mut output, &step, start.max_message_bytes)?;
        if matches!(
            step.state,
            GuestState::Done { .. } | GuestState::Failed { .. }
        ) {
            return Ok(());
        }

        let mut line = String::new();
        read_limited(&mut input, &mut line, start.max_message_bytes)?;
        if line.trim().is_empty() {
            return Err("宿主关闭了 PTC worker 的输入".into());
        }
        let completions: Vec<HostCompletion> = serde_json::from_str(&line)?;
        let mut values = Vec::with_capacity(completions.len());
        for completion in &completions {
            let (value, error) = match &completion.result {
                Ok(value) => (serde_json::to_string(value)?, "null".to_owned()),
                Err(error) => ("null".to_owned(), serde_json::to_string(error)?),
            };
            values.push((completion.id, value, error));
        }
        if let Err(error) = context.with(|ctx| -> rquickjs::Result<()> {
            let control = control.clone().restore(&ctx)?;
            let complete: Function = control.get("complete")?;
            for (id, value, error) in &values {
                let value = ctx.json_parse(value.as_str())?;
                let error = ctx.json_parse(error.as_str())?;
                complete.call::<_, ()>((*id, value, error))?;
            }
            Ok(())
        }) {
            let step = failed("E_PTC_PROTOCOL", error.to_string());
            write_step(&mut output, &step, start.max_message_bytes)?;
            return Ok(());
        }
    }
}

/// 供 `moc-ptc-worker` 二进制使用的入口。
pub fn run_stdio() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run(std::io::BufReader::new(stdin.lock()), stdout.lock())
}

fn read_start<R: BufRead>(input: &mut R) -> Result<StartRequest> {
    let mut line = String::new();
    read_limited(input, &mut line, MAX_START_LINE)?;
    if line.trim().is_empty() {
        return Err("PTC worker 没有收到启动请求".into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn read_limited<R: BufRead>(input: &mut R, line: &mut String, limit: usize) -> Result<()> {
    line.clear();
    let read = input.read_line(line)?;
    if read == 0 {
        return Err("PTC worker 的输入已结束".into());
    }
    if line.len() > limit {
        return Err(format!("PTC worker 输入超过 {limit} 字节").into());
    }
    Ok(())
}

fn write_step<W: Write>(output: &mut W, step: &GuestStep, limit: usize) -> Result<()> {
    let line = serde_json::to_string(step)?;
    if line.len() > limit {
        return Err(format!("PTC worker 输出超过 {limit} 字节").into());
    }
    output.write_all(line.as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn drive(context: &Context, control: &Persistent<Object<'static>>) -> GuestStep {
    let result = context.with(|ctx| -> rquickjs::Result<GuestStep> {
        while ctx.execute_pending_job() {}
        let control = control.clone().restore(&ctx)?;
        let take: Function = control.get("take")?;
        let status: Function = control.get("status")?;
        let requests = take.call::<_, String>(())?;
        let status = status.call::<_, String>(())?;
        let requests = serde_json::from_str(&requests).map_err(|err| {
            rquickjs::Error::new_from_js_message("json", "requests", err.to_string())
        })?;
        let state = serde_json::from_str(&status).map_err(|err| {
            rquickjs::Error::new_from_js_message("json", "state", err.to_string())
        })?;
        Ok(GuestStep { requests, state })
    });
    result.unwrap_or_else(|err| failed("E_JS", err.to_string()))
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
mod tests {
    use super::*;
    use moc_ptc_protocol::wire::{Operation, ScriptToolResult};
    use serde_json::{Value, json};

    fn start(code: &str) -> StartRequest {
        StartRequest {
            code: code.into(),
            memory_limit_bytes: 64 * 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_message_bytes: 16 * 1024 * 1024,
            max_pending_calls: 256,
        }
    }

    fn line(value: &impl serde::Serialize) -> String {
        format!("{}\n", serde_json::to_string(value).unwrap())
    }

    fn run_with(start: StartRequest, completions: &[String]) -> Vec<GuestStep> {
        let mut input = line(&start);
        for completion in completions {
            input.push_str(completion);
        }
        let mut output = Vec::new();
        run(std::io::Cursor::new(input.into_bytes()), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn executes_arithmetic() {
        let steps = run_with(start("return [1,2,3].reduce((a,b)=>a+b,0);"), &[]);
        assert_eq!(steps.len(), 1);
        assert!(matches!(&steps[0].state, GuestState::Done { value } if value == &json!(6)));
    }

    #[test]
    fn round_trips_tool_result() {
        let completions = vec![HostCompletion {
            id: 0,
            result: Ok(serde_json::to_value(ScriptToolResult {
                content: "查询完成".into(),
                summary: None,
                structured_content: Some(json!({"items":[{"id":7}]})),
                is_error: false,
                status: "completed".into(),
                artifact: None,
            })
            .unwrap()),
        }];
        let steps = run_with(
            start("const r = await tools.example({q:1}); return r.structuredContent.items[0].id;"),
            &[line(&completions)],
        );
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].requests.len(), 1);
        assert!(matches!(
            &steps[0].requests[0].operation,
            Operation::Tool { name, .. } if name == "example"
        ));
        assert!(matches!(&steps[1].state, GuestState::Done { value } if value == &json!(7)));
    }

    #[test]
    fn keeps_parallel_tool_calls_in_one_batch() {
        let completions = vec![
            HostCompletion {
                id: 0,
                result: Ok(serde_json::to_value(ScriptToolResult {
                    content: "a".into(),
                    summary: None,
                    structured_content: None,
                    is_error: false,
                    status: "completed".into(),
                    artifact: None,
                })
                .unwrap()),
            },
            HostCompletion {
                id: 1,
                result: Ok(serde_json::to_value(ScriptToolResult {
                    content: "b".into(),
                    summary: None,
                    structured_content: None,
                    is_error: false,
                    status: "completed".into(),
                    artifact: None,
                })
                .unwrap()),
            },
        ];
        let steps = run_with(
            start(
                "const [a,b] = await Promise.all([tools.a(), tools.b()]); return [a.content,b.content];",
            ),
            &[line(&completions)],
        );
        assert_eq!(steps[0].requests.len(), 2);
        assert!(
            matches!(&steps[0].requests[0].operation, Operation::Tool { name, .. } if name == "a")
        );
        assert!(
            matches!(&steps[0].requests[1].operation, Operation::Tool { name, .. } if name == "b")
        );
        assert!(
            matches!(&steps[1].state, GuestState::Done { value } if value == &json!(["a","b"]))
        );
    }

    #[test]
    fn reports_js_failure() {
        let steps = run_with(start("throw new Error('boom');"), &[]);
        assert!(matches!(
            &steps[0].state,
            GuestState::Failed { error } if error.message.contains("boom")
        ));
    }

    #[test]
    fn state_operations_are_requests() {
        let store = line(&vec![HostCompletion {
            id: 0,
            result: Ok(Value::Null),
        }]);
        let load = line(&vec![HostCompletion {
            id: 1,
            result: Ok(json!({"found": true, "value": {"n": 1}})),
        }]);
        let steps = run_with(
            start("await state.store('k', {n: 1}); return await state.load('k');"),
            &[store, load],
        );
        assert_eq!(steps[0].requests.len(), 1);
        assert!(matches!(
            &steps[0].requests[0].operation,
            Operation::Store { key, .. } if key == "k"
        ));
        assert!(matches!(
            &steps[1].requests[0].operation,
            Operation::Load { key } if key == "k"
        ));
        assert!(matches!(&steps[2].state, GuestState::Done { value } if value == &json!({"n":1})));
    }
}

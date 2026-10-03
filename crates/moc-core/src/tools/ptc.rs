//! PTC 的 QuickJS 子进程与受控 JSON IPC。

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, ToolOutput, schema};
use crate::service::turn::tools::execute_nested_tool;
use futures_util::future::BoxFuture;
use moc_llm::{ToolCall, ToolDef};
use moc_protocol::methods::{SessionUpdate, ToolStatus, session::TranscriptChildCall};
use rquickjs::{Context, Function, Promise, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::process::{Child, Command as TokioCommand};

const WORKER_ARG: &str = "--ptc-worker";

#[derive(Debug, Serialize, Deserialize)]
struct WorkerRequest {
    code: String,
    timeout_ms: u64,
    memory_limit_bytes: usize,
    max_output_bytes: usize,
}

struct WorkerCompletion {
    content: String,
    images: Vec<moc_llm::ImageSource>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerMessage {
    Call {
        id: String,
        name: String,
        arguments: Value,
    },
    Result {
        id: String,
        content: String,
        is_error: bool,
    },
    Finished {
        content: String,
        is_error: bool,
    },
    Error {
        content: String,
    },
}

pub struct RunCodeTool;

impl Tool for RunCodeTool {
    fn name(&self) -> &str {
        "run_code"
    }
    fn description(&self) -> &str {
        "在隔离的 QuickJS 子进程中运行 JavaScript。使用 await tools.<name>(参数) 调用已注册工具，最后返回结果。"
    }
    fn tier(&self) -> Tier {
        Tier::Exec
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            self.name(),
            schema(
                serde_json::json!({
                    "code": {"type": "string", "description": "要执行的 JavaScript；工具调用写作 await tools.<name>(args)"},
                    "timeout_secs": {"type": "integer", "minimum": 1}
                }),
                &["code"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let code = args
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::new("E_ARGUMENT", "run_code 需要字符串参数 code"))?;
            let requested = args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(ctx.config.tools.ptc.default_timeout_secs);
            let timeout_secs = requested.min(ctx.config.tools.ptc.max_timeout_secs).max(1);
            let exe = std::env::current_exe().map_err(|error| {
                ToolError::new("E_WORKER", format!("无法定位 moc 可执行文件：{error}"))
            })?;
            let mut child = TokioCommand::new(exe)
                .arg(WORKER_ARG)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| {
                    ToolError::new("E_WORKER", format!("无法启动 QuickJS worker：{error}"))
                })?;
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| ToolError::new("E_WORKER", "worker stdin 不可用"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| ToolError::new("E_WORKER", "worker stdout 不可用"))?;
            let request = serde_json::to_vec(&WorkerRequest {
                code: code.to_owned(),
                timeout_ms: timeout_secs * 1000,
                memory_limit_bytes: ctx.config.tools.ptc.memory_limit_bytes,
                max_output_bytes: ctx.config.tools.ptc.max_output_bytes,
            })
            .map_err(|error| ToolError::new("E_IPC", error.to_string()))?;
            stdin
                .write_all(&request)
                .await
                .map_err(|error| ToolError::new("E_IPC", error.to_string()))?;
            stdin
                .write_all(b"\n")
                .await
                .map_err(|error| ToolError::new("E_IPC", error.to_string()))?;
            stdin
                .flush()
                .await
                .map_err(|error| ToolError::new("E_IPC", error.to_string()))?;
            let result = drive_worker(child, stdin, stdout, ctx, timeout_secs).await;
            match result {
                Ok(completion) => {
                    if completion.content.len() > ctx.config.tools.ptc.max_output_bytes {
                        return Ok(ToolOutcome::failure(
                            "E_OUTPUT_LIMIT",
                            "PTC 输出超过配置上限",
                        ));
                    }
                    let mut outcome = ToolOutcome::success(completion.content);
                    outcome.images = completion.images;
                    Ok(outcome)
                }
                Err(error) => Err(error),
            }
        })
    }
}

async fn drive_worker(
    mut child: Child,
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    ctx: &ToolCtx,
    timeout_secs: u64,
) -> Result<WorkerCompletion, ToolError> {
    let mut lines = AsyncBufReader::new(stdout).lines();
    let mut images = Vec::new();
    let worker = async {
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|error| ToolError::new("E_IPC", error.to_string()))?
        {
            if line.len() > ctx.config.tools.ptc.max_output_bytes + 4096 {
                return Err(ToolError::new("E_OUTPUT_LIMIT", "PTC IPC 消息超过配置上限"));
            }
            let message: WorkerMessage = serde_json::from_str(&line)
                .map_err(|error| ToolError::new("E_IPC", format!("worker 消息无效：{error}")))?;
            match message {
                WorkerMessage::Call {
                    id,
                    name,
                    arguments,
                } => {
                    let _permit =
                        ctx.ptc_slots.acquire().await.map_err(|_| {
                            ToolError::new("E_CONCURRENCY", "PTC 子工具并发控制已关闭")
                        })?;
                    let call = ToolCall {
                        id: format!("{}:{id}", ctx.tool_call_id),
                        name: name.clone(),
                        arguments: serde_json::to_string(&arguments)
                            .map_err(|error| ToolError::new("E_IPC", error.to_string()))?,
                    };
                    let parent_id = ctx.tool_call_id.clone();
                    let started = Instant::now();
                    ctx.request
                        .updates()
                        .emit(
                            ctx.session_id.clone(),
                            SessionUpdate::PtcChildStarted {
                                turn_id: ctx.turn_id.clone(),
                                parent_tool_call_id: parent_id.clone(),
                                tool_call_id: call.id.clone(),
                                name: name.clone(),
                            },
                        )
                        .await
                        .map_err(|error| ToolError::new("E_UPDATE", error.to_string()))?;
                    let outcome = execute_nested_tool(&ctx.core, ctx, &call)
                        .await
                        .map_err(|error| ToolError::new("E_TOOL", error.to_string()))?;
                    let status = outcome.status.unwrap_or(if outcome.is_error {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Completed
                    });
                    let content = output_text(&outcome.content);
                    images.extend(outcome.images.clone());
                    let duration_ms = Some(started.elapsed().as_millis() as u64);
                    let child_record = TranscriptChildCall {
                        id: call.id.clone(),
                        parent_tool_call_id: parent_id.clone(),
                        name: name.clone(),
                        arguments: call.arguments.clone(),
                        result: content.clone(),
                        status,
                        duration_ms,
                    };
                    ctx.sessions
                        .append(
                            &ctx.session_id,
                            crate::session::entries::EntryPayload::ChildTool { call: child_record },
                        )
                        .await
                        .map_err(|error| ToolError::new("E_SESSION", error.to_string()))?;
                    let tool_result = moc_llm::ToolResult {
                        tool_call_id: call.id.clone(),
                        content: content.clone(),
                        is_error: outcome.is_error,
                        images: outcome.images.clone(),
                    };
                    ctx.request
                        .updates()
                        .emit(
                            ctx.session_id.clone(),
                            SessionUpdate::PtcChildFinished {
                                turn_id: ctx.turn_id.clone(),
                                parent_tool_call_id: parent_id,
                                tool_call_id: call.id.clone(),
                                status,
                                result: tool_result,
                                duration_ms,
                                diff: outcome.diff,
                            },
                        )
                        .await
                        .map_err(|error| ToolError::new("E_UPDATE", error.to_string()))?;
                    send_result(&mut stdin, &id, &content, outcome.is_error).await?;
                }
                WorkerMessage::Finished { content, is_error } => {
                    if is_error {
                        return Err(ToolError::new("E_JS", content));
                    }
                    return Ok(WorkerCompletion { content, images });
                }
                WorkerMessage::Error { content } => return Err(ToolError::new("E_JS", content)),
                WorkerMessage::Result { .. } => {
                    return Err(ToolError::new("E_IPC", "收到未请求的 worker 结果"));
                }
            }
        }
        Err(ToolError::new("E_WORKER", "QuickJS worker 提前退出"))
    };
    tokio::pin!(worker);
    let result = tokio::select! {
        timed = tokio::time::timeout(Duration::from_secs(timeout_secs), &mut worker) => match timed {
            Ok(result) => result,
            Err(_) => Err(ToolError::new("E_TIMEOUT", format!("PTC 执行超过 {timeout_secs} 秒"))),
        },
        _ = ctx.cancel.cancelled() => {
            let _ = child.kill().await;
            return Err(ToolError::new("E_CANCELLED", "PTC 执行已取消"));
        }
    };
    if result.is_err() {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
    result
}

async fn send_result(
    stdin: &mut tokio::process::ChildStdin,
    id: &str,
    content: &str,
    is_error: bool,
) -> Result<(), ToolError> {
    let line = serde_json::to_string(&WorkerMessage::Result {
        id: id.to_owned(),
        content: content.to_owned(),
        is_error,
    })
    .map_err(|error| ToolError::new("E_IPC", error.to_string()))?;
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|error| ToolError::new("E_IPC", error.to_string()))?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(|error| ToolError::new("E_IPC", error.to_string()))?;
    stdin
        .flush()
        .await
        .map_err(|error| ToolError::new("E_IPC", error.to_string()))?;
    Ok(())
}

fn output_text(output: &ToolOutput) -> String {
    match output {
        ToolOutput::Text(text) => text.clone(),
        ToolOutput::File { path, .. } => format!("工具输出已写入 {}", path.display()),
        ToolOutput::Excerpt { text, .. } => text.clone(),
    }
}

/// moc 隐藏的 QuickJS worker 入口，仅由 `run_code` 子进程调用。
pub fn run_worker_stdio() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut line = String::new();
    input.read_line(&mut line)?;
    if line.len() > 16 * 1024 * 1024 {
        return Err("PTC worker 请求超过输入上限".into());
    }
    let request: WorkerRequest = serde_json::from_str(&line)?;
    let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
    let interrupted = Arc::new(AtomicBool::new(false));
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(request.memory_limit_bytes);
    let flag = interrupted.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        flag.load(Ordering::Relaxed) || Instant::now() >= deadline
    })));
    let context = Context::full(&runtime)?;
    let io = Arc::new(std::sync::Mutex::new(std::io::BufWriter::new(
        std::io::stdout(),
    )));
    let io_for_call = io.clone();
    let call_tool = move |name: String, arguments: String| -> rquickjs::Result<String> {
        let id = crate::ids::entry_id();
        let arguments: Value = serde_json::from_str(&arguments).unwrap_or(Value::Null);
        let request = serde_json::to_string(&WorkerMessage::Call {
            id: id.clone(),
            name,
            arguments,
        })
        .map_err(|error| rquickjs::Error::new_from_js_message("ipc", "json", error.to_string()))?;
        let mut guard = io_for_call
            .lock()
            .map_err(|_| rquickjs::Error::new_from_js("ipc", "stdout lock"))?;
        writeln!(guard, "{request}").map_err(|error| {
            rquickjs::Error::new_from_js_message("ipc", "stdout", error.to_string())
        })?;
        guard.flush().map_err(|error| {
            rquickjs::Error::new_from_js_message("ipc", "stdout", error.to_string())
        })?;
        drop(guard);
        let mut response = String::new();
        std::io::stdin().read_line(&mut response).map_err(|error| {
            rquickjs::Error::new_from_js_message("ipc", "stdin", error.to_string())
        })?;
        let message: WorkerMessage = serde_json::from_str(&response).map_err(|error| {
            rquickjs::Error::new_from_js_message("ipc", "json", error.to_string())
        })?;
        match message {
            WorkerMessage::Result {
                content, is_error, ..
            } if !is_error => Ok(content),
            WorkerMessage::Result { content, .. } => Err(rquickjs::Error::new_from_js_message(
                "tool", "result", content,
            )),
            WorkerMessage::Error { content } => Err(rquickjs::Error::new_from_js_message(
                "ipc", "error", content,
            )),
            _ => Err(rquickjs::Error::new_from_js("ipc", "worker 响应类型错误")),
        }
    };
    let result = context.with(|ctx| -> rquickjs::Result<String> {
        let global = ctx.globals();
        global.set("call_tool", Function::new(ctx.clone(), call_tool)?)?;
        ctx.eval::<(), _>("globalThis.tools = new Proxy({}, { get: (_, name) => (args = {}) => call_tool(String(name), JSON.stringify(args)) });")?;
        let promise: Promise = ctx.eval(format!("(async () => {{ return JSON.stringify(await (async () => {{\n{}\n}})()) || 'null'; }})()", request.code))?;
        promise.finish::<String>()
    });
    let message = match result {
        Ok(content) if content.len() > request.max_output_bytes => WorkerMessage::Finished {
            content: "[E_OUTPUT_LIMIT] PTC 输出超过配置上限".into(),
            is_error: true,
        },
        Ok(content) => WorkerMessage::Finished {
            content,
            is_error: false,
        },
        Err(error) => WorkerMessage::Finished {
            content: error.to_string(),
            is_error: true,
        },
    };
    let mut output = io.lock().map_err(|_| "stdout 锁失败")?;
    writeln!(output, "{}", serde_json::to_string(&message)?)?;
    output.flush()?;
    Ok(())
}

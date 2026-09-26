use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use moc_protocol::methods::{AssistantDelta, SessionUpdate, SessionUpdateParams};
use serde_json::Value;

#[derive(Clone)]
pub struct Renderer(Arc<Mutex<State>>);

struct State {
    print_mode: bool,
    hide_thinking: bool,
    calls: HashMap<String, ToolCallView>,
}

struct ToolCallView {
    name: String,
    arguments: String,
    announced: bool,
}

impl Renderer {
    pub fn new(print_mode: bool, hide_thinking: bool) -> Self {
        Self(Arc::new(Mutex::new(State {
            print_mode,
            hide_thinking,
            calls: HashMap::new(),
        })))
    }

    pub fn handle(&self, params: SessionUpdateParams) {
        let mut state = self.0.lock();
        if state.print_mode {
            return;
        }
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        match params.update {
            SessionUpdate::AssistantDelta { delta, .. } => match delta {
                AssistantDelta::Text { delta } => {
                    let _ = write!(out, "{delta}");
                    let _ = out.flush();
                }
                AssistantDelta::Reasoning { delta } if !state.hide_thinking => {
                    let _ = write!(out, "\x1b[90m{delta}\x1b[0m");
                    let _ = out.flush();
                }
                AssistantDelta::ToolCallStarted { tool_call_id, name } => {
                    state.calls.insert(
                        tool_call_id,
                        ToolCallView {
                            name,
                            arguments: String::new(),
                            announced: false,
                        },
                    );
                }
                AssistantDelta::ToolCallArgs {
                    tool_call_id,
                    delta,
                } => {
                    if let Some(call) = state.calls.get_mut(&tool_call_id) {
                        call.arguments.push_str(&delta);
                    }
                }
                AssistantDelta::Reasoning { .. } | AssistantDelta::Usage { .. } => {}
            },
            SessionUpdate::ToolStarted {
                tool_call_id, name, ..
            } => {
                let call = state
                    .calls
                    .entry(tool_call_id)
                    .or_insert_with(|| ToolCallView {
                        name,
                        arguments: String::new(),
                        announced: false,
                    });
                announce_call(&mut out, call);
            }
            SessionUpdate::ToolFinished {
                tool_call_id,
                status,
                result,
                duration_ms,
                ..
            } => {
                let mut call = state.calls.remove(&tool_call_id).unwrap_or(ToolCallView {
                    name: "tool".into(),
                    arguments: String::new(),
                    announced: false,
                });
                if !call.announced {
                    announce_call(&mut out, &mut call);
                }
                let summary = result
                    .content
                    .lines()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = writeln!(
                    out,
                    "  {}: {} ({status:?}, {} ms)",
                    call.name,
                    summary,
                    duration_ms.unwrap_or_default()
                );
            }
            SessionUpdate::TurnFinished { usage, .. } => {
                let _ = writeln!(
                    out,
                    "\n[用量：输入 {}，输出 {} token]",
                    usage.input_tokens, usage.output_tokens
                );
            }
            SessionUpdate::TodoChanged { phases } => {
                let total: usize = phases.iter().map(|phase| phase.items.len()).sum();
                let done: usize = phases
                    .iter()
                    .flat_map(|phase| &phase.items)
                    .filter(|item| item.status == moc_protocol::methods::TodoStatus::Completed)
                    .count();
                let _ = writeln!(out, "\n[todo：{done}/{total} 项完成]");
            }
            SessionUpdate::PlanSubmitted {
                plan_id,
                title,
                path,
                markdown,
            } => {
                let _ = writeln!(
                    out,
                    "\n[计划已提交：{title} ({plan_id})，{path}]\n{markdown}"
                );
            }
            SessionUpdate::ContextCompacted {
                method,
                tokens_before,
                tokens_after,
            } => {
                let _ = writeln!(
                    out,
                    "\n[上下文压缩：{method}，{tokens_before} → {tokens_after} token]"
                );
            }
            SessionUpdate::JobFinished {
                job_id,
                command,
                exit_code,
                log_path,
            } => {
                let _ = writeln!(
                    out,
                    "\n[后台任务 {job_id} 结束：{}，退出码 {:?}，日志 {log_path}]",
                    first_line(&command),
                    exit_code
                );
            }
            SessionUpdate::WorkspaceChanged { violation, changes } => {
                let label = if violation {
                    "只读模式违规"
                } else {
                    "工作区外部变更"
                };
                let paths = changes
                    .iter()
                    .map(|change| format!("{} {}", change.kind, change.path))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "\n[{label}：{paths}]");
            }
            SessionUpdate::Error { message, .. } => {
                let _ = writeln!(out, "\n[错误：{message}]");
            }
            SessionUpdate::ModeChanged { mode } => {
                let _ = writeln!(out, "\n[模式：{mode:?}]");
            }
            SessionUpdate::ModelChanged { model } => {
                let _ = writeln!(out, "\n[模型：{model}]");
            }
            SessionUpdate::TitleChanged { title } => {
                let _ = writeln!(out, "\n[会话标题：{title}]");
            }
            SessionUpdate::PermissionRequested { tool_call_id, .. } => {
                let _ = writeln!(out, "\n[等待工具授权：{tool_call_id}]");
            }
            SessionUpdate::PermissionResolved { .. }
            | SessionUpdate::TurnStarted { .. }
            | SessionUpdate::LockChanged { .. } => {}
        }
    }
}

fn announce_call(out: &mut impl Write, call: &mut ToolCallView) {
    if call.announced {
        return;
    }
    let detail = serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .and_then(|value| {
            value
                .get("command")
                .and_then(Value::as_str)
                .map(first_line)
                .or_else(|| {
                    (!call.arguments.is_empty()).then(|| call.arguments.chars().take(120).collect())
                })
        })
        .unwrap_or_default();
    if detail.is_empty() {
        let _ = writeln!(out, "\n→ {}", call.name);
    } else {
        let _ = writeln!(out, "\n→ {}: {detail}", call.name);
    }
    call.announced = true;
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_owned()
}

//! 原子更新会话任务列表，保持至多一个活动任务。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::{SessionUpdate, TodoItem, TodoPhase, TodoStatus};
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::session::entries::EntryPayload;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoArgs {
    op: String,
    list: Option<Vec<PhaseInput>>,
    task: Option<String>,
    phase: Option<String>,
    items: Option<Vec<String>>,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PhaseInput {
    phase: String,
    items: Vec<ItemInput>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ItemInput {
    Text(String),
    Object { task: String },
}

pub struct TodoTool;

impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }
    fn description(&self) -> &'static str {
        "Manage the current session's multi-step task list. Call init before starting a multi-step task, then call done immediately after each completed item. Each operation is atomic."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "todo",
            schema(
                serde_json::json!({
                    "op":{"type":"string","enum":["init","start","done","drop","block","unblock","append","view","rm"]},
                    "list":{"type":"array","items":{"type":"object","properties":{"phase":{"type":"string"},"items":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"object","properties":{"task":{"type":"string"}},"required":["task"],"additionalProperties":false}]}}},"required":["phase","items"],"additionalProperties":false}},
                    "task":{"type":"string"},
                    "phase":{"type":"string"},
                    "items":{"type":"array","items":{"type":"string"}},
                    "reason":{"type":"string"}
                }),
                &["op"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: TodoArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let current = ctx.state.lock().await.todos.clone();
            if args.op == "view" {
                return Ok(ToolOutcome::success(render(&current)));
            }
            let mut next = current;
            match args.op.as_str() {
                "init" => {
                    let phases = match (args.list, args.items) {
                        (Some(list), None) => list,
                        (None, Some(items)) => vec![PhaseInput {
                            phase: args.phase.unwrap_or_else(|| "tasks".into()),
                            items: items.into_iter().map(ItemInput::Text).collect(),
                        }],
                        _ => {
                            return Err(ToolError::new(
                                "E_BAD_PAYLOAD",
                                "init requires either list or items, not both",
                            ));
                        }
                    };
                    next = phases
                        .into_iter()
                        .map(|phase| TodoPhase {
                            phase: phase.phase,
                            items: phase
                                .items
                                .into_iter()
                                .map(|item| TodoItem {
                                    task: match item {
                                        ItemInput::Text(task) | ItemInput::Object { task } => task,
                                    },
                                    status: TodoStatus::Pending,
                                    reason: None,
                                })
                                .collect(),
                        })
                        .collect();
                }
                "append" => {
                    let phase = args
                        .phase
                        .ok_or_else(|| ToolError::new("E_BAD_PAYLOAD", "append requires phase"))?;
                    let items = args
                        .items
                        .ok_or_else(|| ToolError::new("E_BAD_PAYLOAD", "append requires items"))?;
                    if items.is_empty() {
                        return Err(ToolError::new(
                            "E_BAD_PAYLOAD",
                            "append requires at least one item",
                        ));
                    }
                    let index = next
                        .iter()
                        .position(|entry| entry.phase == phase)
                        .unwrap_or_else(|| {
                            next.push(TodoPhase {
                                phase: phase.clone(),
                                items: Vec::new(),
                            });
                            next.len() - 1
                        });
                    next[index]
                        .items
                        .extend(items.into_iter().map(|task| TodoItem {
                            task,
                            status: TodoStatus::Pending,
                            reason: None,
                        }));
                }
                "rm" => {
                    if let Some(phase) = args.phase {
                        let previous = next.len();
                        next.retain(|entry| entry.phase != phase);
                        if previous == next.len() {
                            return Err(ToolError::new(
                                "E_TODO_NOT_FOUND",
                                format!("unknown phase `{phase}`"),
                            ));
                        }
                    } else if let Some(task) = args.task {
                        let mut removed = false;
                        for phase in &mut next {
                            let previous = phase.items.len();
                            phase.items.retain(|item| item.task != task);
                            removed |= previous != phase.items.len();
                        }
                        next.retain(|phase| !phase.items.is_empty());
                        if !removed {
                            return Err(ToolError::new(
                                "E_TODO_NOT_FOUND",
                                format!("unknown task `{task}`"),
                            ));
                        }
                    } else {
                        next.clear();
                    }
                }
                op => {
                    let task = args.task.as_deref();
                    let phase_name = args.phase.as_deref();
                    if task.is_none() && phase_name.is_none() {
                        return Err(ToolError::new(
                            "E_BAD_PAYLOAD",
                            format!("{op} requires task or phase"),
                        ));
                    }
                    let mut found = false;
                    for phase in &mut next {
                        if phase_name == Some(phase.phase.as_str()) {
                            for item in &mut phase.items {
                                found = true;
                                apply_op(item, op, args.reason.as_deref())?;
                            }
                        } else if let Some(task) = task {
                            for item in &mut phase.items {
                                if item.task == task {
                                    found = true;
                                    apply_op(item, op, args.reason.as_deref())?;
                                }
                            }
                        }
                    }
                    if !found {
                        return Err(ToolError::new(
                            "E_TODO_NOT_FOUND",
                            "no matching todo phase or task",
                        ));
                    }
                }
            }
            let mut errors = Vec::new();
            let mut phases = std::collections::HashSet::new();
            let mut tasks = std::collections::HashSet::new();
            for phase in &next {
                if phase.phase.trim().is_empty() || !phases.insert(&phase.phase) {
                    errors.push(format!(
                        "phase name is empty or duplicated: {}",
                        phase.phase
                    ));
                }
                for item in &phase.items {
                    if item.task.trim().is_empty() || !tasks.insert(&item.task) {
                        errors.push(format!("task is empty or duplicated: {}", item.task));
                    }
                }
            }
            if !errors.is_empty() {
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    format!("Errors: {}", errors.join("; ")),
                ));
            }
            normalize(&mut next);
            ctx.sessions
                .append(
                    &ctx.session_id,
                    EntryPayload::Todos {
                        phases: next.clone(),
                    },
                )
                .await
                .map_err(|error| ToolError::new("E_TODO_WRITE", error.to_string()))?;
            ctx.request
                .updates()
                .emit(
                    ctx.session_id.clone(),
                    SessionUpdate::TodoChanged {
                        phases: next.clone(),
                    },
                )
                .await
                .map_err(|error| ToolError::new("E_UPDATE", error.to_string()))?;
            Ok(ToolOutcome::success(render(&next)))
        })
    }
}

fn apply_op(item: &mut TodoItem, op: &str, reason: Option<&str>) -> Result<(), ToolError> {
    match op {
        "start" => {
            item.status = TodoStatus::InProgress;
            item.reason = None;
        }
        "done" => {
            item.status = TodoStatus::Completed;
            item.reason = None;
        }
        "drop" => {
            item.status = TodoStatus::Abandoned;
            item.reason = reason.map(str::to_owned);
        }
        "block" => {
            item.status = TodoStatus::Blocked;
            item.reason = reason
                .filter(|reason| !reason.trim().is_empty())
                .map(str::to_owned);
        }
        "unblock" => {
            item.status = TodoStatus::Pending;
            item.reason = None;
        }
        _ => {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                format!("unknown todo op `{op}`"),
            ));
        }
    }
    Ok(())
}

fn normalize(phases: &mut [TodoPhase]) {
    let mut active = false;
    for item in phases.iter_mut().flat_map(|phase| &mut phase.items) {
        if item.status == TodoStatus::InProgress {
            if active {
                item.status = TodoStatus::Pending;
            } else {
                active = true;
            }
        }
    }
    if !active
        && let Some(item) = phases
            .iter_mut()
            .flat_map(|phase| &mut phase.items)
            .find(|item| item.status == TodoStatus::Pending)
    {
        item.status = TodoStatus::InProgress;
    }
}

fn render(phases: &[TodoPhase]) -> String {
    if phases.is_empty() {
        return "No todos.".into();
    }
    phases
        .iter()
        .map(|phase| {
            let mut output = format!("{}\n", phase.phase);
            for item in &phase.items {
                let marker = match item.status {
                    TodoStatus::Pending => "[ ]",
                    TodoStatus::InProgress => "[/]",
                    TodoStatus::Completed => "[x]",
                    TodoStatus::Blocked => "[!]",
                    TodoStatus::Abandoned => "[-]",
                };
                output.push_str(&format!("  {marker} {}", item.task));
                if let Some(reason) = &item.reason {
                    output.push_str(&format!(" ({reason})"));
                }
                output.push('\n');
            }
            output
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_normalization_keeps_one_active_and_promotes_first_pending() {
        let mut phases = vec![TodoPhase {
            phase: "build".into(),
            items: vec![
                TodoItem {
                    task: "one".into(),
                    status: TodoStatus::Pending,
                    reason: None,
                },
                TodoItem {
                    task: "two".into(),
                    status: TodoStatus::InProgress,
                    reason: None,
                },
                TodoItem {
                    task: "three".into(),
                    status: TodoStatus::InProgress,
                    reason: None,
                },
            ],
        }];
        normalize(&mut phases);
        assert_eq!(phases[0].items[1].status, TodoStatus::InProgress);
        assert_eq!(phases[0].items[2].status, TodoStatus::Pending);
        assert!(render(&phases).contains("[/] two"));
        let mut empty = vec![TodoPhase {
            phase: "x".into(),
            items: vec![TodoItem {
                task: "t".into(),
                status: TodoStatus::Pending,
                reason: None,
            }],
        }];
        empty[0].items[0].status = TodoStatus::Blocked;
        normalize(&mut empty);
        assert_eq!(empty[0].items[0].status, TodoStatus::Blocked);
    }
}

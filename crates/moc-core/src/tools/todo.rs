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
    op: Option<String>,
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

/// 扁平 `init` 合成的 phase 名。
const DEFAULT_INIT_PHASE: &str = "tasks";

/// 修复模型漏填 `op` 的无歧义形态：非空 `list` → init；非空 `items` 带 `phase` → append；
/// 非空 `items` 且当前没有 phase → init。其余形态交回模型重试，不做猜测。
fn infer_op(args: &TodoArgs, has_phases: bool) -> Option<&'static str> {
    if args.list.as_ref().is_some_and(|list| !list.is_empty()) {
        return Some("init");
    }
    if !args.items.as_ref().is_some_and(|items| !items.is_empty()) {
        return None;
    }
    if args.phase.as_deref().is_some_and(|phase| !phase.is_empty()) {
        return Some("append");
    }
    (!has_phases).then_some("init")
}

/// `init` 的载荷裁决：`list` 优先；没有 `list` 时用扁平 `items` 合成单 phase。模型常把单
/// phase 清单压成 `{op:"init", items:[...]}`，这与 `list` 等价，不该让它重试。
fn init_phases(args: TodoArgs) -> Result<Vec<TodoPhase>, ToolError> {
    let phases = match (args.list, args.items) {
        (Some(list), _) => list,
        (None, Some(items)) if !items.is_empty() => vec![PhaseInput {
            phase: args.phase.unwrap_or_else(|| DEFAULT_INIT_PHASE.to_owned()),
            items: items.into_iter().map(ItemInput::Text).collect(),
        }],
        _ => {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "init requires list or a non-empty items array",
            ));
        }
    };
    Ok(phases
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
        .collect())
}

pub struct TodoTool;

impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }
    fn description(&self) -> &'static str {
        "Track multi-step work in this session's task list. Init when work needs 3+ steps, spans multiple files, or the user listed several items; skip single-step requests. Emit todo calls in the same batch as your real tool calls, never alone. Mark items done as they finish; tasks are addressed by their verbatim text."
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
                    "op":{"type":"string","enum":["init","start","done","drop","block","unblock","append","view","rm"],"description":"init replaces the whole list; start/done/drop/block/unblock/rm target a task or phase; append adds tasks; view reads without writing"},
                    "list":{"type":"array","description":"phases for init","items":{"type":"object","properties":{"phase":{"type":"string"},"items":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"object","properties":{"task":{"type":"string"}},"required":["task"],"additionalProperties":false}]}}},"required":["phase","items"],"additionalProperties":false}},
                    "task":{"type":"string","description":"verbatim task content, not an id"},
                    "phase":{"type":"string","description":"phase name; append creates it lazily"},
                    "items":{"type":"array","items":{"type":"string"},"description":"tasks for a flat init or append"},
                    "reason":{"type":"string","description":"blocker note for block"}
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
            let op = match args.op.as_deref() {
                Some(op) => op.to_owned(),
                None => infer_op(&args, !current.is_empty())
                    .ok_or_else(|| ToolError::new("E_BAD_PAYLOAD", "missing op"))?
                    .to_owned(),
            };
            if op == "view" {
                return Ok(ToolOutcome::success(render(&current)));
            }
            let mut next = current;
            match op.as_str() {
                "init" => {
                    next = init_phases(args)?;
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
            ctx.state.lock().await.tool_calls_since_todo = 0;
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

    fn args(
        list: Option<Vec<PhaseInput>>,
        items: Option<Vec<String>>,
        phase: Option<&str>,
    ) -> TodoArgs {
        TodoArgs {
            op: None,
            list,
            task: None,
            phase: phase.map(str::to_owned),
            items,
            reason: None,
        }
    }

    fn one_phase(name: &str, task: &str) -> PhaseInput {
        PhaseInput {
            phase: name.into(),
            items: vec![ItemInput::Text(task.into())],
        }
    }

    #[test]
    fn init_prefers_list_and_accepts_flat_items() {
        // 两种编码同时出现时 list 优先，不消耗一次重试。
        let phases = init_phases(args(
            Some(vec![one_phase("a", "one")]),
            Some(vec!["two".into()]),
            None,
        ))
        .expect("list 优先");
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].phase, "a");
        assert_eq!(phases[0].items[0].task, "one");

        // 只有扁平 items 时合成单 phase，默认名 tasks。
        let phases =
            init_phases(args(None, Some(vec!["a".into(), "b".into()]), None)).expect("扁平 items");
        assert_eq!(phases[0].phase, DEFAULT_INIT_PHASE);
        assert_eq!(phases[0].items.len(), 2);

        // 两者都缺或 items 为空时仍要求模型重试。
        assert!(init_phases(args(None, None, None)).is_err());
        assert!(init_phases(args(None, Some(Vec::new()), None)).is_err());
    }

    #[test]
    fn op_is_inferred_only_for_unambiguous_payloads() {
        let items = Some(vec!["one".into()]);
        assert_eq!(
            infer_op(&args(Some(vec![one_phase("a", "one")]), None, None), true),
            Some("init")
        );
        assert_eq!(
            infer_op(&args(None, items.clone(), Some("a")), true),
            Some("append")
        );
        assert_eq!(
            infer_op(&args(None, items.clone(), None), false),
            Some("init")
        );
        assert_eq!(infer_op(&args(None, items, None), true), None);
        assert_eq!(
            infer_op(&args(None, Some(Vec::new()), Some("a")), false),
            None
        );
    }

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

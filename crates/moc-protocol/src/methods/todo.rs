//! Todo 状态投影。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 一个 todo 阶段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoPhase {
    pub phase: String,
    pub items: Vec<TodoItem>,
}

/// 一个 todo 任务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub status: TodoStatus,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Todo 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
    Abandoned,
}

/// Todo 清单的结果文本：`phase\n  [x] task (reason)\n`；空清单是 `No todos.`。
///
/// core 的 todo 工具逐字节输出这份文本——TUI 与 webui 都靠它还原当时的快照，
/// 口径改动必须同步 [`parse_todos`] 与展示端。
pub fn render_todos(phases: &[TodoPhase]) -> String {
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

/// [`render_todos`] 的逆操作：结果文本 → 阶段列表。
///
/// 条目行形如 `  [m] 任务文本`；行尾 ` (…)` 且括号内不含括号时剥出 reason，
/// 其余一律当作任务文本的一部分。认不出的非空行（且不是 `No todos.`）开启新阶段。
pub fn parse_todos(text: &str) -> Vec<TodoPhase> {
    let mut phases: Vec<TodoPhase> = Vec::new();
    for line in text.split('\n') {
        let item = line.strip_prefix("  ").and_then(|rest| {
            // 条目行形如 `[m] task`：先切出 `[m] ` 前缀，再用状态 match 认标记；
            // 认不出的标记（如 `  [#] x`）按非条目行走下面的阶段逻辑，不静默丢弃。
            let bytes = rest.as_bytes();
            let item_shape =
                bytes.len() >= 4 && bytes[0] == b'[' && bytes[2] == b']' && bytes[3] == b' ';
            let status = match item_shape.then_some(&rest[..3]) {
                Some("[ ]") => TodoStatus::Pending,
                Some("[/]") => TodoStatus::InProgress,
                Some("[x]") => TodoStatus::Completed,
                Some("[!]") => TodoStatus::Blocked,
                Some("[-]") => TodoStatus::Abandoned,
                _ => return None,
            };
            Some((status, &rest[4..]))
        });
        if let Some((status, task)) = item {
            if let Some(phase) = phases.last_mut() {
                let (task, reason) = split_reason(task);
                phase.items.push(TodoItem {
                    status,
                    task: task.to_owned(),
                    reason: reason.map(str::to_owned),
                });
            }
            continue;
        }
        if !line.trim().is_empty() && line != "No todos." {
            phases.push(TodoPhase {
                phase: line.to_owned(),
                items: Vec::new(),
            });
        }
    }
    phases
}

/// 剥出行尾 reason：`task (reason)` → `(task, reason)`；括号内有嵌套括号时不算。
fn split_reason(text: &str) -> (&str, Option<&str>) {
    let Some(body) = text.strip_suffix(')') else {
        return (text, None);
    };
    let Some(open) = body.rfind(" (") else {
        return (text, None);
    };
    let reason = &body[open + 2..];
    if reason.contains('(') || reason.contains(')') {
        return (text, None);
    }
    (&body[..open], Some(reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_states_use_snake_case_and_omit_missing_reason() {
        let phases = vec![TodoPhase {
            phase: "工作".into(),
            items: vec![TodoItem {
                status: TodoStatus::InProgress,
                task: "实现".into(),
                reason: None,
            }],
        }];
        assert_eq!(
            serde_json::to_value(phases).expect("todo 可序列化"),
            serde_json::json!([{
                "phase": "工作",
                "items": [{ "status": "in_progress", "task": "实现" }]
            }])
        );
    }

    /// 渲染与解析互逆：五种状态、带/不带 reason、多阶段都逐字节还原。
    #[test]
    fn render_and_parse_roundtrip() {
        let phases = vec![
            TodoPhase {
                phase: "协议".into(),
                items: vec![
                    TodoItem {
                        status: TodoStatus::Pending,
                        task: "待定".into(),
                        reason: None,
                    },
                    TodoItem {
                        status: TodoStatus::InProgress,
                        task: "进行中".into(),
                        reason: None,
                    },
                    TodoItem {
                        status: TodoStatus::Completed,
                        task: "已完成".into(),
                        reason: None,
                    },
                ],
            },
            TodoPhase {
                phase: "界面".into(),
                items: vec![
                    TodoItem {
                        status: TodoStatus::Blocked,
                        task: "等服务".into(),
                        reason: Some("上游未就绪".into()),
                    },
                    TodoItem {
                        status: TodoStatus::Abandoned,
                        task: "放弃（方案 A）".into(),
                        reason: Some("成本过高".into()),
                    },
                ],
            },
        ];
        let text = render_todos(&phases);
        assert_eq!(
            parse_todos(&text),
            phases,
            "parse(render(x)) 必须还原：{text}"
        );
        assert_eq!(
            render_todos(&parse_todos(&text)),
            text,
            "render(parse(text)) 必须逐字节还原"
        );
        assert_eq!(parse_todos("No todos."), Vec::new());
        assert_eq!(render_todos(&[]), "No todos.");
    }

    /// reason 只在「行尾 ` (…)` 且括号内不含括号」时剥出，其余留给任务文本。
    #[test]
    fn parse_reason_only_at_line_end_without_nested_parens() {
        let phases = parse_todos("阶段\n  [x] 任务 (原因)\n  [ ] a (x (y))\n  [-] b (尾)\n");
        let items = &phases[0].items;
        assert_eq!(items[0].task, "任务");
        assert_eq!(items[0].reason.as_deref(), Some("原因"));
        assert_eq!(items[1].task, "a (x (y))");
        assert_eq!(items[1].reason, None, "嵌套括号不剥 reason");
        assert_eq!(items[2].task, "b");
        assert_eq!(items[2].reason.as_deref(), Some("尾"));
        // 阶段行之前出现的条目行被丢弃（没有可挂的阶段）。
        assert_eq!(parse_todos("  [x] 游离\n阶段\n").len(), 1);
        // 认不出的标记行按非条目行走阶段逻辑，不能静默丢弃。
        let odd = parse_todos("阶段\n  [#] x\n  [x] 正常\n");
        assert_eq!(odd.len(), 2);
        assert_eq!(odd[1].phase, "  [#] x");
        assert_eq!(odd[1].items[0].status, TodoStatus::Completed);
    }
}

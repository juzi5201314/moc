//! Todo 状态投影。

use serde::{Deserialize, Serialize};

/// 一个 todo 阶段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoPhase {
    pub phase: String,
    pub items: Vec<TodoItem>,
}

/// 一个 todo 任务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub status: TodoStatus,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Todo 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
    Abandoned,
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
}

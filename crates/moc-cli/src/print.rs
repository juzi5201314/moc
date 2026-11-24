//! `--print` 的结构化输出：事件收集、JSON 汇总与退出码。
//!
//! 退出码约定：`0` 成功、`1` 模型/网络等一般错误、`2` 有工具调用被权限拒绝、
//! `3` 超出 `--max-turns` 轮数上限。`text` 之外的两个格式保证 stdout 始终可解析：
//! `json` 结束时输出一个 [`PrintResult`]，`stream-json` 把每条 `session/update`
//! 参数按行写为 NDJSON、末行输出同一个汇总对象。

use std::collections::HashSet;
use std::io::Write;

use moc_protocol::methods::{
    PermissionOptionKind, PermissionRequestParams, PermissionResult, SessionUpdate,
    SessionUpdateParams, ToolStatus,
};
use moc_protocol::{PromptResult, Usage};
use moc_i18n::t;
use serde::Serialize;

use crate::args::OutputFormat;

/// 有工具调用被权限拒绝。
pub const EXIT_DENIED: i32 = 2;
/// 超出 `--max-turns` 轮数上限。
pub const EXIT_MAX_TURNS: i32 = 3;

/// 一次工具调用的汇总。
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallSummary {
    pub tool_call_id: String,
    pub name: String,
    pub status: ToolStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "std::ops::Not::not", rename = "error")]
    pub is_error: bool,
}

/// `--print` 结束时输出的汇总对象；`stream-json` 的末行与 `json` 的唯一一行都是它。
#[derive(Debug, Clone, Serialize)]
pub struct PrintResult {
    #[serde(rename = "type")]
    kind: &'static str,
    pub session_id: String,
    pub prompt_id: String,
    /// 最终回答正文；提示没有产出时为 `null`。
    pub result: Option<String>,
    pub finish_reason: Option<String>,
    /// 本轮实际发生的模型请求数（`generation_started` 计数）。
    pub num_turns: usize,
    /// `edit` / `write` 成功落盘的文件（去重、按首次出现排序）。
    pub changed_files: Vec<String>,
    pub tool_calls: Vec<ToolCallSummary>,
    /// 被权限拒绝的工具调用数。
    pub denied_calls: usize,
    pub usage: Option<Usage>,
    pub duration_ms: u64,
    /// 整体错误（提示失败、超出轮次、权限拒绝提示等）；成功时为 `null`。
    pub error: Option<String>,
}

impl PrintResult {
    fn empty() -> Self {
        Self {
            kind: "result",
            session_id: String::new(),
            prompt_id: String::new(),
            result: None,
            finish_reason: None,
            num_turns: 0,
            changed_files: Vec::new(),
            tool_calls: Vec::new(),
            denied_calls: 0,
            usage: None,
            duration_ms: 0,
            error: None,
        }
    }

    /// 提示尚未开始就已失败（会话创建、加载、模型切换等）时给脚本一个可解析的错误对象。
    pub fn early_error(error: &anyhow::Error) -> Self {
        Self {
            error: Some(format!("{error:#}")),
            ..Self::empty()
        }
    }

    /// 按当前输出格式把汇总对象写到 stdout。
    pub fn emit(&self, format: OutputFormat) {
        if format == OutputFormat::Text {
            return;
        }
        match serde_json::to_string(self) {
            Ok(line) => println!("{line}"),
            Err(error) => eprintln!("{error:#}"),
        }
    }
}

/// 提示过程中的事件收集器；`stream-json` 同时把每条更新即时落到 stdout。
pub struct PrintCollector {
    format: OutputFormat,
    turns: usize,
    calls: Vec<ToolCallSummary>,
    denied: usize,
    changed_files: Vec<String>,
    changed_set: HashSet<String>,
    /// 会话工作目录：把 write/edit 参数里的相对路径解析成与 diff 一致的绝对路径。
    cwd: Option<std::path::PathBuf>,
}

impl PrintCollector {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            turns: 0,
            calls: Vec::new(),
            denied: 0,
            changed_files: Vec::new(),
            changed_set: HashSet::new(),
            cwd: None,
        }
    }

    /// 会话建立后注入工作目录（相对路径归一按会话 cwd）。
    pub fn bind(&mut self, cwd: std::path::PathBuf) {
        self.cwd = Some(cwd);
    }

    /// 记录一条变更路径（去重、按首次出现排序）。
    fn record_changed(&mut self, path: String) {
        if self.changed_set.insert(path.clone()) {
            self.changed_files.push(path);
        }
    }

    /// `write` 成功必落盘，但不产 diff 时（建空文件、仅行尾变化、覆盖非 UTF-8）
    /// 也要计入：从参数取 `path` 兜底；相对路径按会话 cwd 解析，与
    /// `ToolFileDiff.path`（经 `resolve_tool_path` 规范化）对齐。
    /// `edit` 无 diff 即无实际修改（比如把一行换成相同内容），不能靠参数判断，跳过。
    fn record_write_targets(&mut self, name: &str, arguments: &str) {
        if name != "write" {
            return;
        }
        let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) else {
            return;
        };
        let Some(path) = args.get("path").and_then(serde_json::Value::as_str) else {
            return;
        };
        let joined = match (&self.cwd, std::path::Path::new(path).is_absolute()) {
            (Some(cwd), false) => cwd.join(path),
            _ => std::path::PathBuf::from(path),
        };
        // 文件刚落盘必然存在：canonicalize 解决 `..` 与符号链接，与 diff 路径同形可去重；
        // 失败时退回词法归一，保证输出仍是可用的绝对路径。
        let resolved =
            std::fs::canonicalize(&joined).unwrap_or_else(|_| normalize_lexically(&joined));
        self.record_changed(resolved.display().to_string());
    }

    /// 处理一条 `session/update`：stream-json 先落行，再按需累计。
    pub fn handle(&mut self, params: &SessionUpdateParams) {
        if self.format == OutputFormat::StreamJson
            && let Ok(line) = serde_json::to_string(params)
        {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
        match &params.update {
            SessionUpdate::GenerationStarted { .. } => {
                self.turns += 1;
            }
            SessionUpdate::ToolFinished {
                tool_call_id,
                name,
                status,
                result,
                duration_ms,
                diff,
                arguments,
                ..
            } => {
                self.calls.push(ToolCallSummary {
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                    status: *status,
                    duration_ms: *duration_ms,
                    is_error: result.is_error,
                });
                if *status == ToolStatus::Denied {
                    self.denied += 1;
                }
                if let Some(diff) = diff {
                    for file in &diff.files {
                        self.record_changed(file.path.clone());
                    }
                }
                if *status == ToolStatus::Completed {
                    self.record_write_targets(name, arguments);
                }
            }
            _ => {}
        }
    }

    /// 收集结束后的快照，供构建汇总对象与退出码判定。
    pub fn report(&self) -> PrintReport {
        PrintReport {
            num_turns: self.turns,
            tool_calls: self.calls.clone(),
            denied_calls: self.denied,
            changed_files: self.changed_files.clone(),
        }
    }
}

/// 收集器的只读快照。
pub struct PrintReport {
    pub num_turns: usize,
    pub tool_calls: Vec<ToolCallSummary>,
    pub denied_calls: usize,
    pub changed_files: Vec<String>,
}

/// 由提示结果与收集快照构建汇总对象。
pub fn build_result(
    session_id: &str,
    report: &PrintReport,
    duration_ms: u64,
    result: &anyhow::Result<PromptResult>,
) -> PrintResult {
    let mut out = PrintResult::empty();
    out.session_id = session_id.to_owned();
    out.duration_ms = duration_ms;
    out.num_turns = report.num_turns;
    out.changed_files = report.changed_files.clone();
    out.tool_calls = report.tool_calls.clone();
    out.denied_calls = report.denied_calls;
    match result {
        Ok(result) => {
            out.prompt_id = result.prompt_id.clone();
            out.result = Some(result.message.text());
            out.finish_reason = Some(result.finish_reason.to_string());
            out.usage = Some(result.usage);
        }
        Err(error) => {
            out.error = Some(format!("{error:#}"));
        }
    }
    out
}

/// 判定进程退出码与面向用户的错误描述；`None` 表示成功。
///
/// 优先级：轮次上限 > 提示失败（含保护终止）> 权限拒绝。
pub fn classify(
    max_turns: Option<u32>,
    report: &PrintReport,
    result: &anyhow::Result<PromptResult>,
) -> (i32, Option<String>) {
    match result {
        Err(error) => (1, Some(format!("{error:#}"))),
        Ok(result) if result.finish_reason == moc_llm::FinishReason::MaxTurns => (
            EXIT_MAX_TURNS,
            Some(t!("cli.repl.max_turns", n = max_turns.unwrap_or_default()).into_owned()),
        ),
        Ok(result) if result.finish_reason.is_guard_stop() => (
            1,
            Some(format!(
                "{} ({})",
                t!("cli.repl.prompt_aborted"),
                result.finish_reason
            )),
        ),
        Ok(_) if report.denied_calls > 0 => (
            EXIT_DENIED,
            Some(t!("cli.repl.denied_tools", n = report.denied_calls).into_owned()),
        ),
        Ok(_) => (0, None),
    }
}

/// 词法归一化路径：折叠 `.` 与 `..`，不触碰文件系统（canonicalize 失败时的兜底）。
fn normalize_lexically(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 只弹普通组件：`..` 越过根或前缀时保持原样，不产生歧义路径。
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `stream-json` 的一条更新行就是 [`SessionUpdateParams`] 自身的 JSON 形状：
/// `{"session_id": "...", "update": {"type": "...", ...}}`，schema 与协议一致。
/// 提示尚未开始就已失败时按同一 stdout 约定补一行错误汇总。
pub fn emit_early_error(format: OutputFormat, error: &anyhow::Error) {
    if format == OutputFormat::Text {
        return;
    }
    PrintResult::early_error(error).emit(format);
}

/// `--print` 下的非交互权限应答：命中 `--allow-tools` 的申请选会话级放行
/// （没有会话级选项时退回单次放行），其余按拒绝处理——与「未注册处理器」语义一致。
pub fn auto_permission(params: &PermissionRequestParams, allow: &[String]) -> PermissionResult {
    let allowed = allow
        .iter()
        .any(|name| name == "*" || name.eq_ignore_ascii_case("all") || name == &params.tool_name);
    let preferred = if allowed {
        [
            PermissionOptionKind::AllowSession,
            PermissionOptionKind::AllowOnce,
        ]
    } else {
        [
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectOnce,
        ]
    };
    let option_id = preferred
        .iter()
        .find_map(|kind| params.options.iter().find(|option| option.kind == *kind))
        .or_else(|| {
            params
                .options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::RejectOnce)
        })
        .map_or_else(|| "reject_once".to_owned(), |option| option.id.clone());
    PermissionResult { option_id }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::methods::PermissionOption;

    fn options() -> Vec<PermissionOption> {
        vec![
            PermissionOption {
                id: "allow_once".into(),
                label: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionOption {
                id: "allow_session".into(),
                label: "Allow session".into(),
                kind: PermissionOptionKind::AllowSession,
            },
            PermissionOption {
                id: "reject_once".into(),
                label: "Deny".into(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ]
    }

    fn params(tool_name: &str) -> PermissionRequestParams {
        PermissionRequestParams {
            session_id: "ses_1".into(),
            prompt_id: "prompt_1".into(),
            request_id: "perm_1".into(),
            tool_call_id: "call_1".into(),
            tool_name: tool_name.into(),
            title: String::new(),
            body: None,
            diff: None,
            options: options(),
        }
    }

    #[test]
    fn auto_permission_selects_session_grant_or_reject() {
        let allow = vec!["bash".to_owned()];
        assert_eq!(
            auto_permission(&params("bash"), &allow).option_id,
            "allow_session"
        );
        assert_eq!(
            auto_permission(&params("edit"), &allow).option_id,
            "reject_once"
        );
        let all = vec!["all".to_owned()];
        assert_eq!(
            auto_permission(&params("edit"), &all).option_id,
            "allow_session"
        );
        let star = vec!["*".to_owned()];
        assert_eq!(
            auto_permission(&params("read"), &star).option_id,
            "allow_session"
        );
    }

    #[test]
    fn auto_permission_falls_back_to_allow_once() {
        let mut params = params("bash");
        params.options.retain(|option| option.id != "allow_session");
        assert_eq!(
            auto_permission(&params, &["bash".to_owned()]).option_id,
            "allow_once"
        );
    }

    fn prompt_result(finish_reason: moc_llm::FinishReason) -> PromptResult {
        PromptResult {
            session_id: "ses_1".into(),
            prompt_id: "p1".into(),
            message: moc_protocol::Message {
                role: moc_protocol::Role::Assistant,
                content: moc_protocol::MessageContent::default(),
            },
            finish_reason,
            usage: Usage::default(),
        }
    }

    #[test]
    fn classify_maps_max_turns_to_exit_three() {
        let report = PrintReport {
            num_turns: 2,
            tool_calls: Vec::new(),
            denied_calls: 0,
            changed_files: Vec::new(),
        };
        let result: anyhow::Result<PromptResult> =
            Ok(prompt_result(moc_llm::FinishReason::MaxTurns));
        let (code, error) = classify(Some(2), &report, &result);
        assert_eq!(code, EXIT_MAX_TURNS);
        assert!(error.is_some());
    }

    #[test]
    fn collector_counts_turns_and_denials() {
        let mut collector = PrintCollector::new(OutputFormat::Json);
        let update = |update: SessionUpdate| SessionUpdateParams {
            session_id: "ses_1".into(),
            update,
        };
        collector.handle(&update(SessionUpdate::GenerationStarted {
            prompt_id: "p1".into(),
        }));
        collector.handle(&update(SessionUpdate::ToolFinished {
            prompt_id: "p1".into(),
            tool_call_id: "call_1".into(),
            name: "bash".into(),
            parent_tool_call_id: None,
            arguments: "{}".into(),
            status: ToolStatus::Denied,
            result: moc_protocol::ToolResult {
                tool_call_id: "call_1".into(),
                content: "denied".into(),
                is_error: true,
                images: Vec::new(),
            },
            duration_ms: None,
            diff: None,
        }));
        let report = collector.report();
        assert_eq!(report.num_turns, 1);
        assert_eq!(report.denied_calls, 1);
        collector.handle(&update(SessionUpdate::GenerationStarted {
            prompt_id: "p1".into(),
        }));
        assert_eq!(collector.report().num_turns, 2);
    }
}

//! 用户侧 Markdown 导出：不调用模型，不改变会话，不导出思考和系统提示。

use super::{CoreInner, to_rpc};
use crate::{CoreError, session::state::SessionState};
use moc_llm::{ContentPart, Role};
use moc_protocol::{RpcError, SessionExportParams, SessionExportResult};
use rust_i18n::t;
use std::{collections::HashMap, fmt::Write as _, io::Write as _, path::Path};

const MAX_DOWNLOAD_BYTES: usize = 8 * 1024 * 1024;

pub(super) async fn run(
    inner: &CoreInner,
    params: SessionExportParams,
) -> Result<SessionExportResult, RpcError> {
    run_with_download_limit(inner, params, MAX_DOWNLOAD_BYTES).await
}

async fn run_with_download_limit(
    inner: &CoreInner,
    params: SessionExportParams,
    max_download_bytes: usize,
) -> Result<SessionExportResult, RpcError> {
    if params
        .path
        .as_deref()
        .is_some_and(|path| path.trim().is_empty())
    {
        return Err(super::errors::invalid_params(t!("core.export.path_empty")));
    }
    let entries = inner
        .sessions
        .read_entries(&params.session_id)
        .await
        .map_err(to_rpc)?;
    tokio::task::spawn_blocking(move || {
        let state = SessionState::from_entries(entries)?;
        let content = render(&state);
        let cwd = Path::new(&state.session.cwd);
        let path = if let Some(path) = params.path {
            let path = std::path::absolute(cwd.join(path))?;
            write_new(&path, &content)?;
            path
        } else {
            let short: String = state
                .session
                .id
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
                .take(16)
                .collect();
            let mut number = 1u64;
            loop {
                let suffix = if number == 1 {
                    String::new()
                } else {
                    format!("-{number}")
                };
                let path = std::path::absolute(cwd.join(format!("moc-export-{short}{suffix}.md")))?;
                match write_new(&path, &content) {
                    Ok(()) => break path,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
                    Err(error) => return Err(CoreError::Io(error)),
                }
            }
        };
        Ok(SessionExportResult {
            path: path.to_string_lossy().into_owned(),
            content: download_content_with_limit(content, max_download_bytes),
        })
    })
    .await
    .map_err(|error| to_rpc(CoreError::Session(error.to_string())))?
    .map_err(|error| {
        to_rpc(CoreError::Session(
            t!("core.export.failed", error = error.to_string()).into_owned(),
        ))
    })
}

fn download_content_with_limit(content: String, limit: usize) -> Option<String> {
    let mut encoded_bytes = 2;
    for byte in content.bytes() {
        encoded_bytes += match byte {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
            0..=31 => 6,
            _ => 1,
        };
        if encoded_bytes > limit {
            return None;
        }
    }
    Some(content)
}

fn write_new(path: &Path, content: &str) -> std::io::Result<()> {
    let mut file = tempfile::Builder::new()
        .prefix(".moc-export-")
        .tempfile_in(path.parent().unwrap_or(Path::new(".")))?;
    file.write_all(content.as_bytes())?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}

fn render(state: &SessionState) -> String {
    let info = &state.session;
    let mut out = format!("# {}\n\n", info.title.replace(['\n', '\r'], " "));
    let _ = writeln!(
        out,
        "{}: {}\n\n{}: {}\n\n{}: {}\n",
        t!("core.export.model"),
        info.model,
        t!("core.export.session"),
        info.id,
        t!("core.export.created"),
        info.created_at_ms
    );
    let results: HashMap<_, _> = state
        .transcript
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .map(|result| (result.tool_call_id.as_str(), result.content.as_str()))
        .collect();
    let mut children: HashMap<&str, Vec<_>> = HashMap::new();
    for child in state.child_calls.iter() {
        children
            .entry(&child.parent_tool_call_id)
            .or_default()
            .push(child);
    }
    if state.transcript.is_empty() {
        let _ = writeln!(out, "{}\n", t!("core.export.empty"));
    }
    for entry in state.transcript.iter() {
        let message = &entry.message;
        if let Some(command) = &entry.bash {
            let prefix = if command.exclude_from_context {
                "!!"
            } else {
                "!"
            };
            tool_start(
                &mut out,
                "bash",
                &format!("{prefix}{}", command.command),
                &message.text(),
            );
            out.push_str("</details>\n\n");
            continue;
        }
        if !matches!(message.role, Role::User | Role::Assistant) {
            continue;
        }
        let body = message.text();
        let has_tools = message.tool_calls().next().is_some();
        if body.is_empty()
            && !has_tools
            && !message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::Image { .. }))
        {
            continue;
        }
        let label = if message.role == Role::User {
            t!("core.export.user")
        } else {
            t!("core.export.assistant")
        };
        let _ = writeln!(out, "## {label}\n\n{body}\n");
        for part in message.content.iter() {
            if matches!(part, ContentPart::Image { .. }) {
                let _ = writeln!(out, "{}\n", t!("core.export.image"));
            }
            if let ContentPart::ToolCall(call) = part {
                let output = results.get(call.id.as_str()).copied().unwrap_or_default();
                tool_start(&mut out, &call.name, &call.arguments, output);
                if let Some(children) = children.get(call.id.as_str()) {
                    for child in children {
                        tool_start(&mut out, &child.name, &child.arguments, &child.result);
                        out.push_str("</details>\n\n");
                    }
                }
                out.push_str("</details>\n\n");
            }
        }
    }
    redact(&out)
}

fn tool_start(out: &mut String, name: &str, arguments: &str, output: &str) {
    let arguments = redact_arguments(arguments);
    let summary = truncate(
        &arguments.split_whitespace().collect::<Vec<_>>().join(" "),
        120,
    );
    let _ = writeln!(
        out,
        "<details>\n<summary>{}: {}</summary>\n",
        html(&redact(name)),
        html(&summary)
    );
    let _ = writeln!(out, "{}\n", t!("core.export.arguments"));
    fence(out, &truncate(&arguments, 4096));
    let _ = writeln!(out, "{}\n", t!("core.export.output"));
    fence(out, &truncate(&redact(output), 16000));
}

fn redact_arguments(text: &str) -> String {
    fn visit(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = redact(text),
            serde_json::Value::Array(values) => values.iter_mut().for_each(visit),
            serde_json::Value::Object(values) => values.values_mut().for_each(visit),
            _ => {}
        }
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(mut value) => {
            visit(&mut value);
            redact(&value.to_string())
        }
        Err(_) => redact(text),
    }
}

fn html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn fence(out: &mut String, text: &str) {
    let longest = text.split(|ch| ch != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let _ = writeln!(out, "{fence}\n{text}\n{fence}\n");
}

fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}\n{}", &text[..end], t!("core.export.truncated")),
        None => text.to_owned(),
    }
}

fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut offset = 0;
    while offset < text.len() {
        let rest = &text[offset..];
        let boundary = offset == 0
            || !text.as_bytes()[offset - 1].is_ascii_alphanumeric()
                && text.as_bytes()[offset - 1] != b'_';
        let prefix = [
            "sk-",
            "rv_",
            "ghp_",
            "github_pat_",
            "gho_",
            "ghu_",
            "ghs_",
            "ghr_",
        ]
        .iter()
        .find(|prefix| boundary && rest.starts_with(**prefix));
        let bearer = boundary
            && rest
                .get(..6)
                .is_some_and(|word| word.eq_ignore_ascii_case("bearer"));
        let start = if let Some(prefix) = prefix {
            prefix.len()
        } else if bearer {
            6 + rest[6..]
                .bytes()
                .take_while(u8::is_ascii_whitespace)
                .count()
        } else {
            0
        };
        if start > 0 && (!bearer || start > 6) {
            let length = rest[start..]
                .bytes()
                .take_while(|ch| ch.is_ascii_alphanumeric() || b"_-./+=~".contains(ch))
                .count();
            if length > 0 {
                if bearer {
                    out.push_str(&rest[..start]);
                }
                out.push_str("[REDACTED]");
                offset += start + length;
                continue;
            }
        }
        let ch = rest.chars().next().expect("剩余正文非空");
        out.push(ch);
        offset += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_budget_counts_json_escaping_and_preserves_empty_content() {
        assert_eq!(MAX_DOWNLOAD_BYTES, 8 * 1024 * 1024);
        assert!(MAX_DOWNLOAD_BYTES < moc_protocol::DEFAULT_MAX_FRAME_BYTES);
        let limit = 1024;
        assert_eq!(
            download_content_with_limit(String::new(), limit),
            Some(String::new())
        );
        assert!(download_content_with_limit("x".repeat(limit - 2), limit).is_some());
        assert!(download_content_with_limit("x".repeat(limit - 1), limit).is_none());
        for text in [
            "\"", "\\", "\n", "\r", "\t", "\u{8}", "\u{c}", "\u{1}", "中",
        ] {
            let text = text.repeat(limit / 4);
            let fits = serde_json::to_vec(&text).unwrap().len() <= limit;
            assert_eq!(download_content_with_limit(text, limit).is_some(), fits);
        }
    }

    #[tokio::test]
    async fn oversized_export_writes_complete_file_without_returning_oversized_frame() {
        rust_i18n::set_locale("zh");
        use crate::{CoreConfig, service::Core};
        use moc_llm::Message;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config.toml"), "language='zh'\n[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[context_files]\nenabled=false\n[skills]\nenabled=false\n").unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            user_home: Some(root.path().join("userhome")),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .unwrap();
        let info = core
            .inner
            .sessions
            .create(
                root.path(),
                "test/model",
                Default::default(),
                Default::default(),
                None,
                Default::default(),
                false,
            )
            .await
            .unwrap();
        for _ in 0..6 {
            core.inner
                .sessions
                .append_message(
                    &info.id,
                    Message::user_text("\u{1}".repeat(1024)),
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let test_limit = 4096;
        let result = run_with_download_limit(
            &core.inner,
            SessionExportParams {
                session_id: info.id,
                path: Some("large.md".into()),
            },
            test_limit,
        )
        .await
        .unwrap();
        assert!(result.content.is_none());
        let body = std::fs::read_to_string(&result.path).unwrap();
        assert_eq!(body.bytes().filter(|byte| *byte == 1).count(), 6 * 1024);
        assert!(serde_json::to_vec(&body).unwrap().len() > test_limit);
        let response = serde_json::to_vec(&result).unwrap();
        assert!(response.len() < 1024);
        assert!(
            !serde_json::from_slice::<serde_json::Value>(&response)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("content")
        );
        core.shutdown().await;
    }

    fn empty_state() -> SessionState {
        SessionState::new(crate::session::entries::Entry::header(
            "ses_export".into(),
            std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "test/model".into(),
            Default::default(),
            Default::default(),
            Default::default(),
            "导出测试".into(),
            0,
        ))
        .unwrap()
    }

    #[test]
    fn manual_bash_entries_export_commands_and_folded_truncated_results() {
        rust_i18n::set_locale("zh");
        use crate::session::entries::{Entry, EntryPayload};
        use moc_protocol::{BashCommand, ToolExecution, ToolStatus};
        let mut state = empty_state();
        for (index, (command, output, exclude_from_context, status)) in [
            (
                "printf sk-command123",
                format!("rv_output123\n{}", "中".repeat(17000)),
                false,
                ToolStatus::Completed,
            ),
            (
                "false",
                "error ghp_error123".into(),
                true,
                ToolStatus::Error,
            ),
            ("", String::new(), false, ToolStatus::Denied),
        ]
        .into_iter()
        .enumerate()
        {
            let entry = Entry::from_payload(
                EntryPayload::Bash {
                    command: BashCommand {
                        command: command.into(),
                        exclude_from_context,
                    },
                    output,
                    tool: ToolExecution {
                        tool_call_id: format!("manual-{index}"),
                        status,
                        duration_ms: None,
                        diff: None,
                    },
                },
                state.entries.last().unwrap().id().to_owned(),
                0,
            );
            state.apply_entry(entry);
        }
        let output = render(&state);
        assert!(output.contains("<summary>bash: !printf [REDACTED]</summary>"));
        assert!(output.contains("\n!printf [REDACTED]\n"));
        assert!(output.contains("<summary>bash: !!false</summary>"));
        assert!(output.contains("error [REDACTED]"));
        assert!(output.contains("<summary>bash: !</summary>"));
        assert_eq!(output.matches("<details>").count(), 3);
        assert_eq!(output.matches("</details>").count(), 3);
        assert!(!output.contains(&format!("## {}", t!("core.export.user"))));
        assert!(!output.contains(&format!("## {}", t!("core.export.assistant"))));
        assert!(output.contains(&*t!("core.export.truncated")));
        assert!(!output.contains(&"中".repeat(16001)));
        for secret in ["sk-command123", "rv_output123", "ghp_error123"] {
            assert!(!output.contains(secret));
        }
    }

    #[test]
    fn json_arguments_redact_secrets_after_escaped_newlines() {
        let arguments = serde_json::json!({
            "command": "printf hello\nsk-afternewline123\r\nghp_afternewline123",
            "nested": [{ "value": "\trv_aftertab123\nBearer token123" }],
            "plain": "task-based risk-free",
        })
        .to_string();
        let output = redact_arguments(&arguments);
        assert_eq!(output.matches("[REDACTED]").count(), 4);
        for secret in [
            "sk-afternewline123",
            "ghp_afternewline123",
            "rv_aftertab123",
            "token123",
        ] {
            assert!(!output.contains(secret), "{secret}");
        }
        let decoded: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(decoded["command"], "printf hello\n[REDACTED]\r\n[REDACTED]");
        assert_eq!(decoded["plain"], "task-based risk-free");
        assert_eq!(redact_arguments("{}"), "{}");
        assert_eq!(redact_arguments(""), "");
        assert_eq!(redact_arguments("sk-malformed123 {"), "[REDACTED] {");
    }

    #[test]
    fn execute_exports_only_its_children_inside_parent_details() {
        rust_i18n::set_locale("zh");
        use crate::session::entries::{Entry, EntryPayload};
        use moc_llm::{Message, ToolCall};
        use moc_protocol::{ToolStatus, methods::session::TranscriptChildCall};
        let mut state = empty_state();
        let mut append = |payload| {
            let entry =
                Entry::from_payload(payload, state.entries.last().unwrap().id().to_owned(), 0);
            state.apply_entry(entry);
        };
        let mut message = Message::assistant_text("前言");
        for id in ["parent", "empty-parent"] {
            message
                .content
                .push(ContentPart::ToolCall(ToolCall::new(id, "execute", "{}")));
        }
        append(EntryPayload::Message {
            message,
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        });
        for (id, parent, name, arguments, result, status) in [
            (
                "child-read",
                "parent",
                "read",
                serde_json::json!({"path":"file\nghp_child123"}).to_string(),
                format!("rv_child123\n{}", "中".repeat(17000)),
                ToolStatus::Completed,
            ),
            (
                "child-bash",
                "parent",
                "bash",
                String::new(),
                "failed sk-child123".into(),
                ToolStatus::Error,
            ),
            (
                "orphan",
                "missing",
                "orphan",
                "{}".into(),
                "not attached".into(),
                ToolStatus::Completed,
            ),
        ] {
            append(EntryPayload::ChildTool {
                call: TranscriptChildCall {
                    id: id.into(),
                    parent_tool_call_id: parent.into(),
                    name: name.into(),
                    arguments,
                    result,
                    status,
                    duration_ms: None,
                    diff: None,
                },
            });
        }
        let output = render(&state);
        assert_eq!(output.matches("<details>").count(), 4);
        assert_eq!(output.matches("</details>").count(), 4);
        for summary in ["<summary>read:", "<summary>bash:"] {
            let before = &output[..output.find(summary).unwrap()];
            assert_eq!(
                before.matches("<details>").count() - before.matches("</details>").count(),
                2
            );
        }
        assert!(output.contains("failed [REDACTED]"));
        assert!(output.contains(&*t!("core.export.truncated")));
        assert!(!output.contains(&"中".repeat(16001)));
        for omitted in [
            "ghp_child123",
            "rv_child123",
            "sk-child123",
            "orphan",
            "not attached",
        ] {
            assert!(!output.contains(omitted));
        }
    }

    #[test]
    fn redaction_and_markdown_fences_do_not_leak_secrets_or_break_unicode() {
        let text = "中文 sk-example123 rv_example123 ghp_example123 github_pat_example123 Bearer abc.DEF/+== bearer\tsecret_key";
        let result = redact(text);
        assert_eq!(result.matches("[REDACTED]").count(), 6);
        for secret in ["example123", "abc.DEF", "secret_key"] {
            assert!(!result.contains(secret));
        }
        assert_eq!(redact("正常文本"), "正常文本");
        assert_eq!(
            redact("task-based risk-free srv_value"),
            "task-based risk-free srv_value"
        );
        assert_eq!(redact(""), "");
        assert!(truncate("中文测试", 2).starts_with("中文\n"));
        let mut output = String::new();
        fence(&mut output, "```\n</details>\n```");
        assert!(output.starts_with("````\n"));
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("export.md");
        write_new(&path, &result).unwrap();
        assert_eq!(
            write_new(&path, "overwrite").unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), result);
    }

    #[tokio::test]
    async fn markdown_replays_persisted_messages_tools_and_omits_reasoning() {
        rust_i18n::set_locale("zh");
        use crate::session::{SessionTable, entries::SessionOrigin};
        use moc_llm::{Message, ReasoningBlock, ToolCall};
        let root = tempfile::tempdir().unwrap();
        let table = SessionTable::new(root.path().join("sessions"), false);
        let info = table
            .create(
                root.path(),
                "test/model",
                Default::default(),
                Default::default(),
                Some("测试 sk-title123"),
                SessionOrigin::default(),
                true,
            )
            .await
            .unwrap();
        let state =
            SessionState::from_entries(table.read_entries(&info.id).await.unwrap()).unwrap();
        assert!(render(&state).contains(&*t!("core.export.empty")));
        table
            .append_message(&info.id, Message::user_text("问题 ghp_user123"), None, None)
            .await
            .unwrap();
        let mut answer = Message::assistant_text("回答 rv_answer123");
        answer
            .content
            .push(ContentPart::Reasoning(ReasoningBlock::Text {
                text: "private reasoning".into(),
                signature: None,
            }));
        answer.content.push(ContentPart::ToolCall(ToolCall::new(
            "call",
            "bash",
            "{\"command\":\"echo sk-secret123\"}",
        )));
        table
            .append_message(&info.id, answer, None, None)
            .await
            .unwrap();
        table
            .append_message(
                &info.id,
                Message::tool_result(
                    "call",
                    format!("Bearer secret123\n{}", "中".repeat(17000)),
                    false,
                ),
                None,
                None,
            )
            .await
            .unwrap();
        let state =
            SessionState::from_entries(table.read_entries(&info.id).await.unwrap()).unwrap();
        let output = render(&state);
        for expected in [
            "# 测试 [REDACTED]",
            "test/model",
            "问题 [REDACTED]",
            "回答 [REDACTED]",
            "<details>",
            "<summary>bash:",
            "Bearer [REDACTED]",
        ] {
            assert!(output.contains(expected), "{expected}");
        }
        for secret in [
            "sk-title123",
            "ghp_user123",
            "rv_answer123",
            "sk-secret123",
            "secret123",
            "private reasoning",
        ] {
            assert!(!output.contains(secret), "{secret}");
        }
        assert!(output.contains(&*t!("core.export.truncated")));
        assert!(!output.contains(&"中".repeat(16001)));
    }
}

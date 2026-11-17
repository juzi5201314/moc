//! 用户侧 Markdown 导出：不调用模型，不改变会话，不导出思考和系统提示。

use super::{CoreInner, to_rpc};
use crate::{CoreError, session::state::SessionState};
use moc_llm::{ContentPart, Role};
use moc_protocol::{RpcError, SessionExportParams, SessionExportResult};
use rust_i18n::t;
use std::{collections::HashMap, fmt::Write as _, io::Write as _, path::Path};

pub(super) async fn run(
    inner: &CoreInner,
    params: SessionExportParams,
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
            content,
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
    if state.transcript.is_empty() {
        let _ = writeln!(out, "{}\n", t!("core.export.empty"));
    }
    for entry in state.transcript.iter() {
        let message = &entry.message;
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
                let arguments = redact(&call.arguments);
                let summary = truncate(
                    &arguments.split_whitespace().collect::<Vec<_>>().join(" "),
                    120,
                );
                let _ = writeln!(
                    out,
                    "<details>\n<summary>{}: {}</summary>\n",
                    html(&redact(&call.name)),
                    html(&summary)
                );
                let _ = writeln!(out, "{}\n", t!("core.export.arguments"));
                fence(&mut out, &truncate(&arguments, 4096));
                let _ = writeln!(out, "{}\n", t!("core.export.output"));
                let output = results.get(call.id.as_str()).copied().unwrap_or_default();
                fence(&mut out, &truncate(&redact(output), 16000));
                out.push_str("</details>\n\n");
            }
        }
    }
    redact(&out)
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

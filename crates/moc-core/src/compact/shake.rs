//! shake 压缩：保护窗口之外的工具结果只保留开头结尾，调用配对和原始日志地址不变。

use super::{CompactionBudget, messages_tokens, record_compaction};
use crate::session::entries::Entry;
use crate::tools::read::ReadArgs;
use crate::{config::ResolvedConfig, error::CoreError, service::CoreInner};
use moc_llm::{ContentPart, Role};
use std::collections::{HashMap, HashSet};

pub(crate) async fn compact(
    inner: &CoreInner,
    session_id: &str,
    config: &ResolvedConfig,
    model: &crate::ResolvedModel,
    budget: CompactionBudget,
) -> Result<moc_protocol::CompactResult, CoreError> {
    let CompactionBudget {
        tokens_before,
        fixed_tokens,
    } = budget;
    let record = inner.sessions.get(session_id).await?;
    let paths = record.paths.clone();
    let compaction = config.compaction.clone();
    // 在追加锁内重新取状态，裁剪、归档和重写共用同一快照；后台子代理结果只能在
    // 事务前或事务后追加，不能被压缩入口的旧快照覆盖。
    let (state, archive_uri) = inner
        .sessions
        .rewrite_current(session_id, move |state| async move {
            let mut entries = state.log_entries();
            if !prune(&mut entries, &compaction) {
                return Err(CoreError::ContextOverflow(
                    "local pruning found no reclaimable tool output".into(),
                ));
            }
            // 归档必须先于重写，保留本次真正裁剪的完整原文。
            let archive_uri = super::archive::write(&paths, &state).await?;
            Ok((entries, (state, archive_uri)))
        })
        .await?;
    let snapshot = record.state.lock().await.clone();
    let tokens_after = fixed_tokens.saturating_add(messages_tokens(
        &super::replay::model_messages(&snapshot, None, Some(model)),
    ));
    let first_kept = state.first_kept_id.clone().or_else(|| {
        state
            .transcript
            .get(super::replay::visible_history_start(&state))
            .map(|entry| entry.id.clone())
    });
    // 沿用上一份 checkpoint 的 summary 时，必须保留它原本的方法名和生成它的模型：
    // `grok_build` 摘要写成 `shake` 会让回放丢掉「查询+载体+提醒」的整体替换形状。
    let (method, model) = match state.summary.as_ref() {
        Some((method, _)) => (method.clone(), state.summary_model.clone()),
        None => ("shake".to_owned(), None),
    };
    record_compaction(
        inner,
        &state.session.id,
        super::Checkpoint {
            method,
            model,
            summary: state
                .summary
                .as_ref()
                .map(|(_, summary)| summary.clone())
                .unwrap_or_default(),
            // 沿用上一份 checkpoint 时连提醒一起保留，回放时才不会丢掉运行时状态快照。
            reminder: state.summary_reminder.clone(),
            first_kept_id: first_kept,
            tokens_before,
            tokens_after,
            full: false,
            instructions: None,
            archive: super::HistoryArchive::Precomputed(archive_uri),
        },
    )
    .await
}

fn prune(entries: &mut [Entry], config: &crate::config::CompactionConfig) -> bool {
    // read 按完整分页参数判重；ask 的答复是用户亲手做的决定，短小但丢了无法回查，永不裁剪。
    let mut reads: HashMap<String, ReadArgs> = HashMap::new();
    let mut asks: HashSet<String> = HashSet::new();
    for call in entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Message { message, .. } => Some(message),
            _ => None,
        })
        .flat_map(|message| message.tool_calls())
    {
        match call.name.as_str() {
            "read" if config.shake.supersede_reads => {
                if let Ok(args) = serde_json::from_str(&call.arguments) {
                    reads.insert(call.id.clone(), args);
                }
            }
            "ask" => {
                asks.insert(call.id.clone());
            }
            _ => {}
        }
    }
    let mut seen_reads = HashSet::new();
    let mut prompts_ago = 0usize;
    let mut protected_tokens = 0u64;
    let mut changed = false;
    for entry in entries.iter_mut().rev() {
        let Entry::Message {
            message,
            bash: None,
            ..
        } = entry
        else {
            continue;
        };
        if message.role == Role::User {
            prompts_ago += 1;
        }
        if message.role != Role::Tool {
            continue;
        }
        let cost = messages_tokens(std::slice::from_ref(message));
        let protected = prompts_ago < config.shake.keep_last_prompts
            || protected_tokens < config.protect_recent_tool_tokens as u64;
        protected_tokens = protected_tokens.saturating_add(cost);
        for part in &mut message.content {
            let ContentPart::ToolResult(result) = part else {
                continue;
            };
            if asks.contains(&result.tool_call_id) {
                continue;
            }
            let superseded = !result.is_error
                && reads
                    .get(&result.tool_call_id)
                    .is_some_and(|args| !seen_reads.insert(args));
            if protected {
                continue;
            }
            let replacement = if superseded {
                Some("[superseded by a later read]".to_owned())
            } else if prompts_ago >= config.shake.hard_clear_prompts {
                Some("[tool output pruned]".to_owned())
            } else if result.content.chars().count() > config.shake.soft_trim_chars {
                Some(match artifact_path(&result.content) {
                    Some(path) => trim_text(
                        &result.content,
                        config.shake.head_chars,
                        config.shake.tail_chars,
                        path,
                    ),
                    None => "[tool output pruned]".to_owned(),
                })
            } else {
                None
            };
            if let Some(replacement) = replacement
                && replacement != result.content
            {
                result.content = replacement;
                changed = true;
            }
        }
    }
    changed
}

fn artifact_path(text: &str) -> Option<&str> {
    let path = text
        .rsplit_once("full output: ")
        .map(|(_, path)| path.split(['\n', ']']).next().unwrap_or(path).trim())?;
    (path.starts_with("artifact://") || path.starts_with('/')).then_some(path)
}

fn trim_text(text: &str, head: usize, tail: usize, path: &str) -> String {
    let count = text.chars().count();
    if count <= head.saturating_add(tail) {
        return text.to_owned();
    }
    let head_end = text
        .char_indices()
        .nth(head)
        .map_or(text.len(), |(index, _)| index);
    let tail_start = if tail == 0 {
        text.len()
    } else {
        text.char_indices()
            .rev()
            .nth(tail - 1)
            .map_or(0, |(index, _)| index)
    };
    format!(
        "{}\n[… {} chars pruned; full output: {path}]\n{}",
        &text[..head_end],
        count - head - tail,
        &text[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::entries::EntryPayload;
    use moc_llm::{Message, ToolCall};

    fn entry(message: Message, parent: &str) -> Entry {
        Entry::from_payload(
            EntryPayload::Message {
                message,
                tool: None,
                finish_reason: None,
                agent: None,
                goal: None,
            },
            parent.to_owned(),
            2,
        )
    }

    fn earlier_read_after_prune(
        earlier_args: &str,
        later_args: &str,
        later_is_error: bool,
    ) -> String {
        let mut config = crate::config::CompactionConfig::default();
        config.shake.keep_last_prompts = 0;
        config.protect_recent_tool_tokens = 0;
        let read = |id: &str, args: &str| Message {
            role: Role::Assistant,
            content: moc_llm::message_content![ContentPart::ToolCall(ToolCall::new(
                id, "read", args,
            ))],
        };
        let later_output = if later_is_error {
            "E_NOT_FOUND"
        } else {
            "later page"
        };
        let mut entries = [
            entry(read("read1", earlier_args), "root"),
            entry(Message::tool_result("read1", "original page", false), "e1"),
            entry(read("read2", later_args), "e2"),
            entry(
                Message::tool_result("read2", later_output, later_is_error),
                "e3",
            ),
        ];
        prune(&mut entries, &config);
        let Entry::Message { message, .. } = &entries[1] else {
            unreachable!();
        };
        message.tool_results().next().unwrap().content.clone()
    }

    #[test]
    fn prune_keeps_distinct_read_pages() {
        let first_lines = r#"{"path":"large.rs","offset":1,"limit":10}"#;
        let actual = [
            (
                first_lines,
                r#"{"path":"large.rs","offset":100,"limit":10}"#,
            ),
            (first_lines, r#"{"path":"large.rs","offset":1,"limit":5}"#),
            (
                r#"{"path":"large.rs","offset":1,"limit":1,"byte_offset":0}"#,
                r#"{"path":"large.rs","offset":1,"limit":1,"byte_offset":1024}"#,
            ),
        ]
        .map(|(earlier, later)| earlier_read_after_prune(earlier, later, false));
        assert_eq!(actual, ["original page"; 3]);
    }

    #[test]
    fn prune_keeps_successful_read_when_retry_fails() {
        let args = r#"{"path":"large.rs","offset":1,"limit":10}"#;
        assert_eq!(earlier_read_after_prune(args, args, true), "original page");
    }

    #[test]
    fn prune_supersedes_matching_successful_reads() {
        assert_eq!(
            earlier_read_after_prune(
                r#"{"path":"large.rs","offset":1,"limit":10}"#,
                r#"{"limit":10,"offset":1,"path":"large.rs"}"#,
                false,
            ),
            "[superseded by a later read]"
        );
    }

    #[test]
    fn prune_never_clears_ask_answers() {
        let mut config = crate::config::CompactionConfig::default();
        config.shake.hard_clear_prompts = 1;
        config.shake.keep_last_prompts = 0;
        config.protect_recent_tool_tokens = 0;
        let assistant = Message {
            role: Role::Assistant,
            content: moc_llm::message_content![
                ContentPart::ToolCall(ToolCall::new(
                    "ask1",
                    "ask",
                    r#"{"questions":[{"id":"db","question":"用哪种存储？","options":[{"label":"PostgreSQL"},{"label":"SQLite"}]}]}"#,
                )),
                ContentPart::ToolCall(ToolCall::new("b1", "bash", r#"{"command":"ls"}"#)),
            ],
        };
        let mut entries = vec![
            entry(assistant, "root"),
            entry(Message::tool_result("ask1", "PostgreSQL", false), "e1"),
            entry(Message::tool_result("b1", "a.rs\nb.rs", false), "e2"),
            entry(Message::user_text("下一步"), "e3"),
            entry(Message::user_text("继续"), "e4"),
        ];
        assert!(prune(&mut entries, &config));
        let outputs: Vec<&str> = entries
            .iter()
            .flat_map(|entry| match entry {
                Entry::Message { message, .. } => message
                    .tool_results()
                    .map(|result| result.content.as_str())
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        // 越过 hard_clear 后普通工具输出被清，ask 答复原样保留。
        assert!(outputs.contains(&"PostgreSQL"), "{outputs:?}");
        assert!(outputs.contains(&"[tool output pruned]"), "{outputs:?}");
    }

    #[test]
    fn trimming_preserves_unicode_edges_and_original_log() {
        let text = format!("{}{}", "前".repeat(30), "尾".repeat(30));
        let result = trim_text(&text, 5, 5, "/tmp/job.log");
        assert!(result.starts_with("前前前前前"));
        assert!(result.ends_with("尾尾尾尾尾"));
        assert!(result.contains("50 chars pruned; full output: /tmp/job.log"));
    }
}

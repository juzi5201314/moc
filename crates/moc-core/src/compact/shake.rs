//! 在保护窗口之外修剪工具结果，保留调用配对与原始日志地址。

use super::{CompactionBudget, messages_tokens, record_compaction};
use crate::session::{entries::Entry, state::SessionState};
use crate::{config::ResolvedConfig, error::CoreError, service::CoreInner};
use moc_llm::{ContentPart, Role};
use moc_protocol::RequestContext;
use std::collections::{HashMap, HashSet};

pub(crate) async fn compact(
    inner: &CoreInner,
    ctx: &RequestContext,
    state: &SessionState,
    config: &ResolvedConfig,
    model: &crate::ResolvedModel,
    budget: CompactionBudget,
) -> Result<moc_protocol::CompactResult, CoreError> {
    let CompactionBudget {
        tokens_before,
        fixed_tokens,
    } = budget;
    let mut entries = state.entries.clone();
    if !prune(&mut entries, &config.compaction) {
        return Err(CoreError::ContextOverflow(
            "local pruning found no reclaimable tool output".into(),
        ));
    }
    // 归档必须覆盖完整的压缩前历史：整表 rewrite 之后 transcript 只剩剪枝结果，
    // 而 summary 向模型承诺的正是这份压缩前历史。
    let record = inner.sessions.get(&state.session.id).await?;
    let archive_uri =
        super::archive::write(&record.paths, &state.session.id, state).await?;
    inner.sessions.rewrite(&state.session.id, entries).await?;
    let snapshot = record.state.lock().await.clone();
    let tokens_after = fixed_tokens
        .saturating_add(super::native_input_tokens(&snapshot))
        .saturating_add(messages_tokens(&super::replay::model_messages(
            &snapshot,
            None,
            Some(model),
        )));
    let first_kept = state.first_kept_id.clone().or_else(|| {
        state
            .transcript
            .get(super::replay::visible_history_start(state))
            .map(|entry| entry.id.clone())
    });
    // 复用上一份 checkpoint 的 summary/native 时必须保留其真实来源（方法名与产出模型）：
    // `ModelChange` 依赖 `remote` 与产出模型作废不再匹配的原生载荷，写成 `shake`/
    // 当前模型都会让清理失效。
    let (method, model) = match state.summary.as_ref() {
        Some((method, _)) => (method.clone(), state.summary_model.clone()),
        None => ("shake".to_owned(), None),
    };
    record_compaction(
        inner,
        ctx,
        &state.session.id,
        super::Checkpoint {
            method,
            model,
            summary: state
                .summary
                .as_ref()
                .map(|(_, summary)| summary.clone())
                .unwrap_or_default(),
            first_kept_id: first_kept,
            tokens_before,
            tokens_after,
            instructions: None,
            native: state
                .native_input
                .clone()
                .map(|items| Box::new(serde_json::Value::Array(items))),
            archive: super::HistoryArchive::Precomputed(archive_uri),
        },
    )
    .await
}

fn prune(entries: &mut [Entry], config: &crate::config::CompactionConfig) -> bool {
    let reads: HashMap<String, String> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Message { message, .. } => Some(message),
            _ => None,
        })
        .flat_map(|message| message.tool_calls())
        .filter(|call| call.name == "read")
        .filter_map(|call| {
            let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
            Some((call.id.clone(), args.get("path")?.as_str()?.to_owned()))
        })
        .collect();
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
            let superseded = reads
                .get(&result.tool_call_id)
                .is_some_and(|path| !seen_reads.insert(path.clone()));
            if protected {
                continue;
            }
            let replacement = if config.shake.supersede_reads && superseded {
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
    #[test]
    fn trimming_preserves_unicode_edges_and_original_log() {
        let text = format!("{}{}", "前".repeat(30), "尾".repeat(30));
        let result = trim_text(&text, 5, 5, "/tmp/job.log");
        assert!(result.starts_with("前前前前前"));
        assert!(result.ends_with("尾尾尾尾尾"));
        assert!(result.contains("50 chars pruned; full output: /tmp/job.log"));
    }
}

//! 压缩前历史的可回读分段归档。

use std::path::Path;

use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::error::CoreError;
use crate::session::state::SessionState;
use crate::session::store::SessionPaths;

use super::images::strip_images;
use super::replay::visible_history_start;

const MAX_SEGMENT_BYTES: usize = 5 * 1024 * 1024;

pub async fn write(
    paths: &SessionPaths,
    session_id: &str,
    state: &SessionState,
) -> Result<Option<String>, CoreError> {
    let start = visible_history_start(state);
    if start >= state.transcript.len() {
        return Ok(None);
    }
    let directory = paths.compaction();
    fs::create_dir_all(&directory).await?;
    let segment_number = next_segment_number(&directory).await?;
    let filename = format!("segment-{segment_number:04}.md");
    let path = directory.join(&filename);
    let (content, count, first_id, last_id) = render_segment(&state.transcript[start..]);
    if count == 0 {
        return Ok(None);
    }
    fs::write(&path, &content).await?;

    let index_path = directory.join("INDEX.md");
    let mut index = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path)
        .await?;
    if index.metadata().await?.len() == 0 {
        index
            .write_all(b"# Compaction history\n\n| Segment | Entries | IDs | Bytes |\n|---|---:|---|---:|\n")
            .await?;
    }
    index
        .write_all(
            format!(
                "| [{filename}]({filename}) | {count} | `{first_id}` .. `{last_id}` | {} |\n",
                content.len()
            )
            .as_bytes(),
        )
        .await?;
    index.flush().await?;
    Ok(Some(format!("history://{session_id}/compaction/INDEX.md")))
}

async fn next_segment_number(directory: &Path) -> Result<usize, CoreError> {
    let mut entries = fs::read_dir(directory).await?;
    let mut next = 0;
    while let Some(entry) = entries.next_entry().await? {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(number) = name
            .strip_prefix("segment-")
            .and_then(|value| value.strip_suffix(".md"))
            .and_then(|value| value.parse::<usize>().ok())
        else {
            continue;
        };
        next = next.max(number.saturating_add(1));
    }
    Ok(next)
}

fn render_segment(
    entries: &[moc_protocol::methods::TranscriptEntry],
) -> (String, usize, String, String) {
    let mut content = String::with_capacity(entries.len().saturating_mul(256));
    let mut count = 0;
    let first_id = entries
        .iter()
        .find(|entry| entry.in_context())
        .map(|entry| entry.id.clone())
        .unwrap_or_default();
    let mut last_id = first_id.clone();
    content.push_str("# Pre-compaction transcript\n\n");
    for entry in entries {
        let Some(mut message) = entry.context_message() else {
            continue;
        };
        strip_images(&mut message);
        let serialized = serde_json::to_string_pretty(&message).unwrap_or_default();
        let tool = entry
            .tool
            .as_ref()
            .and_then(|value| serde_json::to_string(value).ok())
            .unwrap_or_default();
        let block = format!(
            "## {} ({})\n\n```json\n{}\n```\n{}\n",
            entry.id,
            entry.at_ms,
            serialized,
            if tool.is_empty() {
                String::new()
            } else {
                format!("Tool execution: `{tool}`\n")
            }
        );
        if content.len().saturating_add(block.len()) > MAX_SEGMENT_BYTES {
            content.push_str("\n_[segment truncated at a message boundary; the JSONL transcript remains authoritative.]_\n");
            break;
        }
        content.push_str(&block);
        count += 1;
        last_id = entry.id.clone();
    }
    (content, count, first_id, last_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_llm::Message;
    use moc_protocol::methods::SessionMode;
    use tempfile::tempdir;

    #[tokio::test]
    async fn writes_index_and_segment_for_visible_history() {
        let root = tempdir().unwrap();
        let paths = SessionPaths {
            transcript: root.path().join("transcript.jsonl"),
            sidecar: root.path().join("sidecar"),
        };
        let header = crate::session::entries::Entry::header(
            "ses_archive123".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Normal,
            None,
            "default".into(),
            1,
        );
        let message = crate::session::entries::Entry::from_payload(
            crate::session::entries::EntryPayload::Message {
                message: Message::user_text("preserve this exact detail"),
                tool: None,
                finish_reason: None,
            },
            header.id().into(),
            2,
        );
        let state = SessionState::from_entries(vec![header, message]).unwrap();
        let uri = write(&paths, "ses_archive123", &state).await.unwrap();
        assert_eq!(
            uri.as_deref(),
            Some("history://ses_archive123/compaction/INDEX.md")
        );
        let index = tokio::fs::read_to_string(paths.compaction().join("INDEX.md"))
            .await
            .unwrap();
        assert!(index.contains("segment-0000.md"));
        let segment = tokio::fs::read_to_string(paths.compaction().join("segment-0000.md"))
            .await
            .unwrap();
        assert!(segment.contains("preserve this exact detail"));
    }
}

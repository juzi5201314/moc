//! `session/search` 的扫描实现：按 `session/list` 的新→旧顺序逐行扫各会话
//! transcript，先用原始字节做小写预筛，只解析候选行并抽出消息 `Text` 块
//! 做真正的大小写不敏感匹配。

use std::path::Path;

use moc_protocol::methods::{SessionSearchHit, SessionSearchMatch};

use super::SessionTable;
use super::log::{self, Policy};
use crate::error::CoreError;

/// 单个会话返回的命中上限：取时间最新的几条。
const MATCHES_PER_SESSION: usize = 3;
/// 一次搜索最多扫描的会话数；更早的不扫，由 `truncated` 上报。
const MAX_SCANNED_SESSIONS: usize = 300;
/// 摘录在命中点前后各留的字符数。
const SNIPPET_RADIUS: usize = 60;

/// 每行解析的最小形状；tool、finish_reason 等其余字段不构造对象树。
#[derive(serde::Deserialize)]
struct SearchEntry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    at_ms: u64,
    #[serde(default)]
    message: Option<SearchMessage>,
}

#[derive(serde::Deserialize)]
struct SearchMessage {
    role: moc_llm::Role,
    #[serde(default)]
    content: Vec<serde_json::Value>,
}

impl SessionTable {
    /// 跨会话正文子串搜索；返回按 `session/list` 顺序（`updated_at_ms` 降序）
    /// 的命中与「命中达上限或更早会话超出扫描预算」标记。
    pub(crate) async fn search(
        &self,
        query: &str,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<SessionSearchHit>, bool), CoreError> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return Ok((Vec::new(), false));
        }
        let mut hits = Vec::new();
        let mut truncated = false;
        let mut scanned = 0usize;
        for info in self.list().await? {
            if hits.len() >= limit || scanned >= MAX_SCANNED_SESSIONS {
                truncated = true;
                break;
            }
            if cwd.is_some_and(|cwd| info.cwd != cwd) {
                continue;
            }
            scanned += 1;
            let Some(paths) = self.store.find(&info.id).await? else {
                continue;
            };
            let path = paths.transcript.clone();
            let needle = needle.clone();
            let matches = tokio::task::spawn_blocking(move || scan(&path, &needle))
                .await
                .map_err(|error| CoreError::Session(error.to_string()))?;
            if matches.is_empty() {
                continue;
            }
            hits.push(SessionSearchHit {
                session: info,
                matches,
            });
        }
        Ok((hits, truncated))
    }
}

/// 扫单个 transcript；坏文件或坏帧只丢命中不失败。扫描顺序即时间序，
/// 最后只留最新的 [`MATCHES_PER_SESSION`] 条并反转为新→旧。
fn scan(path: &Path, needle: &str) -> Vec<SessionSearchMatch> {
    // 预筛的前提：搜索词不含会被 JSON 转义的字符（引号、反斜杠、控制字符），
    // 否则命中行的原始字节里没有它的逐字字节，只能逐行解析。
    let prefilter = !needle.chars().any(|c| c < ' ' || c == '"' || c == '\\');
    let mut matches = Vec::new();
    let result = log::for_each_line(path, Policy::SkipFrame, &mut |line| {
        if prefilter && !raw_contains(line, needle) {
            return Ok(());
        }
        collect(line, needle, &mut matches);
        Ok(())
    });
    if let Err(error) = result {
        tracing::warn!(path = %path.display(), %error, "session/search 跳过无法完整读取的 transcript");
    }
    if matches.len() > MATCHES_PER_SESSION {
        matches.drain(..matches.len() - MATCHES_PER_SESSION);
    }
    matches.reverse();
    matches
}

/// 原始行的小写预筛：needle 已小写，两边一致折叠后命中行必然包含它。
fn raw_contains(line: &[u8], needle: &str) -> bool {
    if needle.is_ascii() {
        let lowered = line.to_ascii_lowercase();
        memchr::memmem::find(&lowered, needle.as_bytes()).is_some()
    } else {
        String::from_utf8_lossy(line)
            .to_lowercase()
            .contains(needle)
    }
}

/// 解析候选行，收集 user/assistant 消息顶层 `Text` 块的命中。
fn collect(line: &[u8], needle: &str, matches: &mut Vec<SessionSearchMatch>) {
    let Ok(entry) = serde_json::from_slice::<SearchEntry>(line) else {
        return;
    };
    if entry.kind != "message" {
        return;
    }
    let Some(message) = entry.message else {
        return;
    };
    if !matches!(message.role, moc_llm::Role::User | moc_llm::Role::Assistant) {
        return;
    }
    for part in &message.content {
        let Some(text) = part.get("Text").and_then(serde_json::Value::as_str) else {
            continue;
        };
        // 命中区间：先精确字节查找；只有小写展开字节（如 'İ'）会让位置漂移，
        // 此时区间近似、由 snippet 按 char 边界收紧。
        let (start, end) = match text.find(needle) {
            Some(pos) => (pos, pos + needle.len()),
            None => {
                let lowered = text.to_lowercase();
                let Some(pos) = lowered.find(needle) else {
                    continue;
                };
                let end = if lowered.len() == text.len() {
                    pos + needle.len()
                } else {
                    (pos + needle.len()).min(text.len())
                };
                (pos, end)
            }
        };
        matches.push(SessionSearchMatch {
            entry_id: entry.id.clone(),
            role: message.role.as_str().to_owned(),
            snippet: snippet(text, start, end),
            at_ms: entry.at_ms,
        });
    }
}

/// 命中区间前后各留 [`SNIPPET_RADIUS`] 个字符，空白折叠成单空格。
fn snippet(text: &str, start: usize, end: usize) -> String {
    let start = floor_char_boundary(text, start.min(text.len()));
    let end = ceil_char_boundary(text, end.min(text.len()).max(start));
    let from = text[..start]
        .char_indices()
        .rev()
        .nth(SNIPPET_RADIUS - 1)
        .map(|(index, _)| index)
        .unwrap_or(0);
    let to = text[end..]
        .char_indices()
        .nth(SNIPPET_RADIUS)
        .map(|(index, _)| end + index)
        .unwrap_or(text.len());
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.push_str(
        &text[from..to]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    );
    if to < text.len() {
        out.push('…');
    }
    out
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn line(ty: &str, role: &str, text: &str) -> String {
        format!(
            r#"{{"type":"{ty}","id":"e1","at_ms":7,"message":{{"role":"{role}","content":[{{"Text":{}}}]}}}}"#,
            serde_json::to_string(text).unwrap()
        )
    }

    fn write_transcript(dir: &Path, lines: &[String]) -> std::path::PathBuf {
        let path = dir.join("a.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(file, "{l}").unwrap();
        }
        path
    }

    #[test]
    fn scan_只匹配user与assistant的text块() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            &[
                line("message", "User", "hello Foo world"),
                line("message", "Tool", "Foo in tool"),
                line("message", "Assistant", "answer foo"),
                r#"{"type":"goal_change","id":"g1","at_ms":8}"#.to_owned(),
                "not json foo".to_owned(),
            ],
        );
        let matches = scan(&path, "foo");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].role, "assistant");
        assert_eq!(matches[1].role, "user");
        assert_eq!(matches[1].entry_id, "e1");
    }

    #[test]
    fn 预筛放行后仍需解析验证() {
        // 搜索词出现在非 Text 字段（如 tool 名），解析后不应命中。
        let dir = tempfile::tempdir().unwrap();
        let raw = r#"{"type":"message","id":"e2","at_ms":9,"message":{"role":"Assistant","content":[{"ToolCall":{"id":"c1","name":"foo_tool","arguments":{}}}]}}"#;
        let path = write_transcript(dir.path(), &[raw.to_owned()]);
        assert!(scan(&path, "foo").is_empty());
    }

    #[test]
    fn snippet_按字符留上下文并折叠空白() {
        let text = format!("{}foo{}", "a".repeat(100), "b".repeat(100));
        let out = snippet(&text, 100, 103);
        assert!(out.starts_with('…'));
        assert!(out.ends_with('…'));
        assert_eq!(out.matches('a').count(), SNIPPET_RADIUS);
        assert_eq!(out.matches('b').count(), SNIPPET_RADIUS);
        assert!(out.contains("foo"));
    }
}

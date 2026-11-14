//! 会话的可读投影：先选择记录，再展开正文；原始 JSONL 不参与展示层裁切。

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use serde_json::Value;

use super::{InternalUri, ResourceError};
use crate::error::CoreError;
use crate::session::{log, store::SessionPaths};

pub(crate) enum Selection<'a> {
    Index,
    Role(&'a str),
    Tool(&'a str),
    Entry(&'a str),
}

impl<'a> Selection<'a> {
    pub(crate) fn parse(uri: &'a InternalUri) -> Result<Self, ResourceError> {
        match uri.segments() {
            [] => Ok(Self::Index),
            [kind, role]
                if kind == "role"
                    && matches!(role.as_str(), "user" | "assistant" | "tool" | "custom") =>
            {
                Ok(Self::Role(role))
            }
            [kind, name] if kind == "tool" && safe_component(name) => Ok(Self::Tool(name)),
            [kind, id] if kind == "entry" && safe_component(id) => Ok(Self::Entry(id)),
            _ => Err(ResourceError::new(
                "E_HISTORY_URI",
                "Use history://transcript for an index, history://transcript/role/user, history://transcript/tool/<name>, history://transcript/entry/<id>, or history://raw for the original JSONL.",
            )),
        }
    }

    pub(crate) fn path(&self, paths: &SessionPaths) -> PathBuf {
        let name = match self {
            Self::Index => "view-index.md".into(),
            Self::Role(role) => format!("view-role-{role}.md"),
            Self::Tool(name) => format!("view-tool-{name}.md"),
            Self::Entry(id) => format!("view-entry-{id}.md"),
        };
        paths.history_cache_dir().join(name)
    }

    pub(crate) fn refresh(&self, paths: &SessionPaths) -> Result<bool, CoreError> {
        log::refresh_history_document(paths, &self.path(paths), |out| {
            writeln!(
                out,
                "# Session history\n\nIndex previews are not full records. Read history://transcript/entry/<id> for a record; filter before paging with history://transcript/role/user or history://transcript/tool/<name>. Use read offset/limit to page this view; long lines support limit=1 and byte_offset. Opaque reasoning and binary payloads remain only in history://raw.\n"
            )?;
            let mut calls = HashMap::<String, String>::new();
            let mut found = false;
            log::for_each_line(&paths.transcript, &mut |line| {
                let entry: Value = serde_json::from_slice(line)
                    .map_err(|error| CoreError::Session(error.to_string()))?;
                let id = entry["id"].as_str().unwrap_or_default();
                let exact = matches!(self, Self::Entry(wanted) if *wanted == id);
                if entry["type"] == "child_tool" {
                    let call = &entry["call"];
                    let name = call["name"].as_str().unwrap_or("tool");
                    if exact
                        || matches!(self, Self::Index | Self::Role("tool"))
                        || matches!(self, Self::Tool(wanted) if *wanted == name)
                    {
                        found = true;
                        if exact {
                            self.record(
                                out,
                                id,
                                name,
                                Some("arguments"),
                                call["arguments"].as_str().unwrap_or_default(),
                            )?;
                        }
                        self.record(
                            out,
                            id,
                            name,
                            call["status"].as_str(),
                            call["result"].as_str().unwrap_or_default(),
                        )?;
                    }
                    return Ok(());
                }
                let message = &entry["message"];
                let role = message["role"].as_str().unwrap_or_default();
                let Some(parts) = message["content"].as_array() else {
                    if exact {
                        found = true;
                        writeln!(out, "## {id}\n")?;
                        serde_json::to_writer_pretty(&mut *out, &entry)
                            .map_err(|e| CoreError::Session(e.to_string()))?;
                        writeln!(out)?;
                    }
                    return Ok(());
                };
                for part in parts {
                    if let Some(call) = part.get("ToolCall") {
                        let call_id = call["id"].as_str().unwrap_or_default();
                        let name = call["name"].as_str().unwrap_or("tool");
                        calls.insert(call_id.to_owned(), name.to_owned());
                        if exact || matches!(self, Self::Index | Self::Role("assistant")) {
                            found = true;
                            self.record(
                                out,
                                id,
                                name,
                                Some("arguments"),
                                call["arguments"].as_str().unwrap_or_default(),
                            )?;
                        }
                    } else if let Some(result) = part.get("ToolResult") {
                        let name =
                            calls.remove(result["tool_call_id"].as_str().unwrap_or_default());
                        let name = name.as_deref().unwrap_or("tool");
                        if exact
                            || matches!(self, Self::Index | Self::Role("tool"))
                            || matches!(self, Self::Tool(wanted) if *wanted == name)
                        {
                            found = true;
                            self.record(
                                out,
                                id,
                                name,
                                Some(if result["is_error"] == true {
                                    "error"
                                } else {
                                    "completed"
                                }),
                                result["content"].as_str().unwrap_or_default(),
                            )?;
                        }
                    } else if let Some(text) = part.get("Text").and_then(Value::as_str)
                        && (exact
                            || matches!(self, Self::Index)
                            || matches!(self, Self::Role(wanted) if role.eq_ignore_ascii_case(wanted)))
                    {
                        found = true;
                        self.record(out, id, role, None, text)?;
                    } else if part.get("Reasoning").is_none()
                        && part.get("Text").is_none()
                        && (exact
                            || matches!(self, Self::Index)
                            || matches!(self, Self::Role(wanted) if role.eq_ignore_ascii_case(wanted)))
                    {
                        found = true;
                        writeln!(
                            out,
                            "## {id} · {role}\n\n[Non-text attachment; original payload: history://raw]\n"
                        )?;
                    }
                }
                Ok(())
            })?;
            if !found {
                if matches!(self, Self::Entry(_)) {
                    return Err(CoreError::Session(
                        "history entry has no readable content or does not exist".into(),
                    ));
                }
                writeln!(out, "No matching history records.")?;
            }
            Ok(())
        })
    }

    fn record(
        &self,
        out: &mut dyn Write,
        id: &str,
        kind: &str,
        status: Option<&str>,
        text: &str,
    ) -> Result<(), CoreError> {
        if matches!(self, Self::Index) {
            write!(out, "- history://transcript/entry/{id} · {kind}")?;
            if let Some(status) = status {
                write!(out, " · {status}")?;
            }
            write!(out, " · ")?;
            let mut chars = text.chars();
            for ch in chars.by_ref().take(160) {
                write!(out, "{}", if ch.is_control() { ' ' } else { ch })?;
            }
            if chars.next().is_some() {
                write!(out, " …")?;
            }
            writeln!(out)?;
        } else {
            write!(out, "## {id} · {kind}")?;
            if let Some(status) = status {
                write!(out, " · {status}")?;
            }
            writeln!(out, "\n\n{text}\n")?;
        }
        Ok(())
    }
}

fn safe_component(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projections_filter_before_paging_and_keep_exact_records_and_raw_source() {
        for compressed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let paths = SessionPaths::new(
                root.path().join(if compressed {
                    "session.jsonl.zst"
                } else {
                    "session.jsonl"
                }),
                root.path().join("sidecar"),
            );
            let answer = "完整回答\n".repeat(1000);
            let records = [
                json!({"type":"message","id":"u1","message":{"role":"User","content":[{"Text":answer}]}}),
                json!({"type":"message","id":"a1","message":{"role":"Assistant","content":[{"Reasoning":{"Item":{"encrypted_content":"opaque".repeat(10000)}}},{"Text":"visible answer"},{"ToolCall":{"id":"c1","name":"ask","arguments":"question"}}]}}),
                json!({"type":"message","id":"t1","message":{"role":"Tool","content":[{"ToolResult":{"tool_call_id":"c1","content":"chosen answer","is_error":false}}]}}),
                json!({"type":"child_tool","id":"t2","call":{"name":"read","status":"completed","arguments":"{}","result":"read body".repeat(10000)}}),
            ];
            let raw = records.iter().map(|r| format!("{r}\n")).collect::<String>();
            let bytes = if compressed {
                log::compress_frame(raw.as_bytes()).unwrap()
            } else {
                raw.as_bytes().to_vec()
            };
            std::fs::write(&paths.transcript, &bytes).unwrap();
            let view = |selection: Selection<'_>| {
                assert!(selection.refresh(&paths).unwrap());
                std::fs::read_to_string(selection.path(&paths)).unwrap()
            };
            let index = view(Selection::Index);
            assert!(
                index.contains("history://transcript/entry/u1")
                    && index.contains("history://transcript/entry/t2")
            );
            assert!(index.lines().all(|line| line.len() < 4096));
            assert!(!index.contains("opaqueopaque"));
            let user = view(Selection::Role("user"));
            assert!(user.contains(&answer));
            assert!(!user.contains("read body") && !user.contains("chosen answer"));
            let ask = view(Selection::Tool("ask"));
            assert!(ask.contains("chosen answer") && !ask.contains("read body"));
            let exact = view(Selection::Entry("a1"));
            assert!(exact.contains("visible answer") && exact.contains("question"));
            assert!(!exact.contains("opaqueopaque"));
            assert_eq!(std::fs::read(&paths.transcript).unwrap(), bytes);
            let next = b"{\"type\":\"message\",\"id\":\"u2\",\"message\":{\"role\":\"User\",\"content\":[{\"Text\":\"new answer\"}]}}\n";
            let next = if compressed {
                log::compress_frame(next).unwrap()
            } else {
                next.to_vec()
            };
            std::fs::OpenOptions::new()
                .append(true)
                .open(&paths.transcript)
                .unwrap()
                .write_all(&next)
                .unwrap();
            assert!(view(Selection::Role("user")).contains("new answer"));
            assert!(view(Selection::Entry("u1")).contains(&answer));
        }
    }
}

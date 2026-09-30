//! 原子创建与整体替换文件，拒绝回写已服务锚点。

use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, resolve_tool_path, schema};
use moc_protocol::{ToolDiff, ToolFileDiff};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "新建或整体覆盖文件。若文件行含有最近 read 得到的 hashline 锚点，请用 edit 而不是 write。"
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "write",
            schema(
                serde_json::json!({
                    "path": {"type":"string"},
                    "content": {"type":"string"}
                }),
                &["path", "content"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: WriteArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            if args.path.starts_with("ov://") || args.path.starts_with("viking://") {
                let Some(client) = &ctx.openviking else {
                    return Err(ToolError::new(
                        "E_OPENVIKING_DISABLED",
                        "未配置 OpenViking URL",
                    ));
                };
                let peer = ctx.openviking_peer.as_deref();
                let uri = moc_openviking::OpenVikingUri::parse(&args.path)
                    .map_err(|error| ToolError::new("E_OPENVIKING_URI", error.to_string()))?;
                client
                    .write(uri.as_str(), &args.content, peer)
                    .await
                    .map_err(|error| ToolError::new("E_OPENVIKING_WRITE", error.to_string()))?;
                return Ok(ToolOutcome::success(format!(
                    "已提交 OpenViking 写入 {}",
                    uri.as_str()
                )));
            }
            let supplied = Path::new(&args.path);
            let absolute = if supplied.is_absolute() {
                supplied.to_owned()
            } else {
                ctx.cwd.join(supplied)
            };
            let parent = absolute
                .parent()
                .ok_or_else(|| ToolError::new("E_ACCESS", "无效文件路径"))?;
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
            let path = resolve_tool_path(&ctx.cwd, &args.path)?;
            if args.content.len() as u64 > ctx.config.tools.output.max_file_bytes {
                return Err(ToolError::new("E_TOO_LARGE", "写入内容超过单文件字节上限"));
            }
            reject_served_hash_echo(
                &path,
                &args.content,
                &ctx.anchors,
                ctx.config.tools.write.hash_echo_min_hits,
            )?;
            let old = match tokio::fs::read(&path).await {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(ToolError::new("E_ACCESS", error.to_string())),
            };
            let ending = old.as_deref().map(line_ending).unwrap_or(b"\n");
            let content = normalize_line_endings(&args.content, ending);
            let diff = build_write_diff(&path, old.as_deref(), &content);
            let temporary = stage(&path, content.as_bytes()).await?;
            if let Err(error) = tokio::fs::rename(&temporary, &path).await {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(ToolError::new("E_ACCESS", error.to_string()));
            }
            if let Some(old) = old {
                let metadata = tokio::fs::metadata(&path)
                    .await
                    .map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
                ctx.anchors.push_undo(
                    path.clone(),
                    old,
                    crate::hashline::SnapshotId::from_metadata_and_bytes(
                        &metadata,
                        content.as_bytes(),
                    ),
                );
            }
            ctx.anchors.invalidate(&path);
            let lines = content.lines().count();
            Ok(ToolOutcome {
                content: format!(
                    "已写入 {}（{lines} 行，{} 字节）",
                    path.display(),
                    content.len()
                )
                .into(),
                is_error: false,
                images: Vec::new(),
                control: super::ToolControl::Continue,
                status: None,
                diff,
            })
        })
    }
}

fn reject_served_hash_echo(
    path: &Path,
    content: &str,
    anchors: &crate::hashline::AnchorStore,
    threshold: usize,
) -> Result<(), ToolError> {
    let Some(served) = anchors.served(path) else {
        return Ok(());
    };
    let expected: std::collections::BTreeSet<String> = served
        .hashes
        .iter()
        .flatten()
        .filter_map(|hash| crate::hashline::encode_anchor(*hash).ok())
        .collect();
    let echoes = content
        .lines()
        .filter(|line| {
            let Some((hash, _)) = line.split_once('│') else {
                return false;
            };
            hash.len() == 3
                && hash.bytes().all(|byte| byte.is_ascii_alphanumeric())
                && expected.contains(hash)
        })
        .count();
    if echoes >= threshold && threshold > 0 {
        return Err(ToolError::new(
            "E_WRITE_HASH_ECHO",
            format!("内容中有 {echoes} 行回显已服务锚点；使用 edit 修改这些行"),
        ));
    }
    Ok(())
}

fn line_ending(bytes: &[u8]) -> &'static [u8] {
    if bytes.windows(2).any(|window| window == b"\r\n") {
        b"\r\n"
    } else if bytes.contains(&b'\r') {
        b"\r"
    } else {
        b"\n"
    }
}

/// 计算整体写入的 diff：旧文件（可能不存在）对比归一化后的新内容。
///
/// 旧文件不是合法 UTF-8 或新旧内容完全一致时返回 `None`——前者无法按行
/// 渲染，后者没有可展示的变更。
fn build_write_diff(path: &Path, old: Option<&[u8]>, content: &str) -> Option<ToolDiff> {
    let old_lines: Vec<String> = match old {
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(_) => return None,
        },
        None => Vec::new(),
    };
    let new_lines: Vec<String> = content.lines().map(str::to_owned).collect();
    let old_refs: Vec<&str> = old_lines.iter().map(String::as_str).collect();
    let new_refs: Vec<&str> = new_lines.iter().map(String::as_str).collect();
    let (hunks, truncated) = moc_protocol::build_hunks(&old_refs, &new_refs);
    if hunks.is_empty() {
        return None;
    }
    Some(ToolDiff {
        files: vec![ToolFileDiff {
            path: path.display().to_string(),
            hunks,
        }],
        truncated,
    })
}

/// 授权预览：按工具参数计算 write 将产生的 diff，不落盘。
/// 参数缺失、路径无效或内容超限时返回 `None`，真实错误由执行阶段给出。
pub(crate) async fn preview_write_diff(
    cwd: &Path,
    path_value: &str,
    content: &str,
    max_file_bytes: u64,
) -> Option<ToolDiff> {
    let absolute = resolve_tool_path(cwd, path_value).ok()?;
    if content.len() as u64 > max_file_bytes {
        return None;
    }
    let old = match tokio::fs::read(&absolute).await {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    let ending = old.as_deref().map(line_ending).unwrap_or(b"\n");
    let normalized = normalize_line_endings(content, ending);
    build_write_diff(&absolute, old.as_deref(), &normalized)
}

fn normalize_line_endings(content: &str, ending: &[u8]) -> String {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if ending == b"\n" {
        return normalized;
    }
    let ending = std::str::from_utf8(ending).expect("line ending ASCII");
    normalized.replace('\n', ending)
}

async fn stage(path: &Path, content: &[u8]) -> Result<PathBuf, ToolError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".moc-write-{}.tmp", crate::ids::entry_id()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
    file.write_all(content)
        .await
        .map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
    file.sync_all()
        .await
        .map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
    Ok(temporary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::{AnchorStore, ServedFile, assign_hashes, served::read_snapshot};
    use std::fs;

    #[test]
    fn write_hash_echo_is_rejected_only_for_served_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.rs");
        fs::write(&path, "fn main() {}\nfn other() {}\n").unwrap();
        let snapshot = read_snapshot(&path, 1024).unwrap();
        let anchors = AnchorStore::new();
        anchors.record_served(
            path.clone(),
            &[(0, snapshot.hashes[0]), (1, snapshot.hashes[1])],
            snapshot.clone(),
        );
        let content = format!(
            "{}│one\n{}│two",
            crate::hashline::encode_anchor(snapshot.hashes[0]).unwrap(),
            crate::hashline::encode_anchor(snapshot.hashes[1]).unwrap()
        );
        assert!(reject_served_hash_echo(&path, &content, &anchors, 2).is_err());
        let unserved = assign_hashes(&["new".to_owned()], &[]).unwrap()[0];
        let content = format!(
            "{}│not previously served",
            crate::hashline::encode_anchor(unserved).unwrap()
        );
        assert!(reject_served_hash_echo(&path, &content, &anchors, 2).is_ok());
    }

    #[test]
    fn line_endings_are_preserved_without_normalizing_content_semantics() {
        assert_eq!(normalize_line_endings("a\nb\n", b"\r\n"), "a\r\nb\r\n");
        assert_eq!(line_ending(b"a\r\nb\r\n"), b"\r\n");
        assert!(ServedFile::new().hashes.is_empty());
    }
}

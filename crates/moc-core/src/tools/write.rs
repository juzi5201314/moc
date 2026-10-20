//! write 工具：原子地创建文件或替换全部内容；拒绝把带锚点的行原样写回。

use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, resolve_tool_path, schema};
use crate::resources::ResourceRouter;
use moc_protocol::{ToolDiff, ToolFileDiff};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteArgs {
    pub(crate) path: String,
    pub(crate) content: String,
}

/// 解析 write 参数；授权和执行共用这一个解析函数。
pub(crate) fn parse_args(args: Value) -> Result<WriteArgs, ToolError> {
    serde_json::from_value(args).map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))
}

pub struct WriteTool;

/// 生成逐行 diff 的内容大小上限；更大的内容照样可以整体替换，只是不显示 diff。
///
/// 与 `edit` 的 4 MiB 快照上限一致：逐行拆分只为前端展示，不值得在大文件上付出这个开销。
const WRITE_DIFF_MAX_BYTES: usize = 4 * 1024 * 1024;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Create a file or replace its complete content. For partial changes to a read file, use edit."
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
            let args = parse_args(args)?;
            // 绕过授权入口的调用也要检查：解析路径前先按词法结果判断一次。
            crate::tools::ensure_path_access(ctx, &args.path, crate::sandbox::Access::Write)?;
            ResourceRouter::write(&args.path)
                .map_err(|error| ToolError::new(error.code, error.message))?;
            if args.path.starts_with("ov://") || args.path.starts_with("viking://") {
                let Some(client) = &ctx.openviking else {
                    return Err(ToolError::new(
                        "E_OPENVIKING_DISABLED",
                        "OpenViking URL is not configured",
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
                    "Submitted OpenViking write for {}",
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
                .ok_or_else(|| ToolError::new("E_ACCESS", "invalid file path"))?;
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
            let path = resolve_tool_path(&ctx.cwd, &args.path)?;
            // create_dir_all 可能会创建末尾缺失的目录：按当前解析结果再判断一次边界，
            // 没获授权的越界写入在这里拦下，不会写到磁盘上。
            crate::tools::ensure_path_access(
                ctx,
                &path.to_string_lossy(),
                crate::sandbox::Access::Write,
            )?;
            let max_file_bytes = ctx.config.tools.output.max_file_bytes;
            if args.content.len() as u64 > max_file_bytes {
                return Err(ToolError::new(
                    "E_TOO_LARGE",
                    "content exceeds the per-file byte limit",
                ));
            }
            reject_served_hash_echo(
                &path,
                &args.content,
                &ctx.anchors,
                ctx.config.tools.write.hash_echo_min_hits,
            )?;
            // 覆盖已有文件前先看旧文件大小：超过读写上限就不读旧内容，
            // 因此也不做 diff、不记撤销（几 GB 的旧文件会撑爆内存）。
            let (old, oversized_old) = read_previous(&path, max_file_bytes).await?;
            let ending = old.as_deref().map(line_ending).unwrap_or(b"\n");
            let content = normalize_line_endings(&args.content, ending);
            let diff = write_diff(&path, old.as_deref(), &content, WRITE_DIFF_MAX_BYTES);
            let temporary = stage(&path, content.as_bytes()).await?;
            if let Err(error) = tokio::fs::rename(&temporary, &path).await {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(ToolError::new("E_ACCESS", error.to_string()));
            }
            let mut notes = String::new();
            if oversized_old {
                notes.push_str(
                    "\n[The previous file exceeded the per-file read limit; it was replaced without diff or undo record.]",
                );
            }
            if let Some(old) = old {
                let metadata = tokio::fs::metadata(&path)
                    .await
                    .map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
                let kept = ctx.anchors.push_undo(
                    path.clone(),
                    old,
                    crate::hashline::SnapshotId::from_metadata_and_bytes(
                        &metadata,
                        content.as_bytes(),
                    ),
                );
                if !kept {
                    notes.push_str(
                        "\n[The previous file exceeded the undo byte budget; /undo cannot restore it.]",
                    );
                }
            }
            ctx.anchors.invalidate(&path);
            let lines = content.lines().count();
            Ok(ToolOutcome {
                content: format!(
                    "Wrote {} ({lines} lines, {} bytes){notes}",
                    path.display(),
                    content.len()
                )
                .into(),
                is_error: false,
                structured_content: None,
                error_code: None,
                images: Vec::new(),
                control: super::ToolControl::Continue,
                status: None,
                diff,
            })
        })
    }
}

/// 授权预览：用与执行相同的旧内容和换行归一化算出覆盖后的 diff，不写盘。
pub(crate) async fn preview_arguments(ctx: &ToolCtx, args: &WriteArgs) -> Option<ToolDiff> {
    if args.path.starts_with("ov://") || args.path.starts_with("viking://") {
        return None;
    }
    let path = resolve_tool_path(&ctx.cwd, &args.path).ok()?;
    let max_file_bytes = ctx.config.tools.output.max_file_bytes;
    let (old, oversized) = read_previous(&path, max_file_bytes).await.ok()?;
    if oversized {
        return None;
    }
    let ending = old.as_deref().map(line_ending).unwrap_or(b"\n");
    let content = normalize_line_endings(&args.content, ending);
    write_diff(&path, old.as_deref(), &content, WRITE_DIFF_MAX_BYTES)
}

/// 拒绝把 read/diff 输出里带锚点的行原样写回文件。
///
/// 直接用会话里已登记的令牌校验，不为此重读磁盘：既不会把旧位图和新内容混在一起，
/// 也不会在大文件上无限制地分配内存。
fn reject_served_hash_echo(
    path: &Path,
    content: &str,
    anchors: &crate::hashline::AnchorStore,
    threshold: usize,
) -> Result<(), ToolError> {
    let Some(served) = anchors.served(path) else {
        return Ok(());
    };
    let expected: std::collections::BTreeSet<u64> = served
        .entries
        .iter()
        .flatten()
        .map(|token| crate::hashline::token_key(*token))
        .collect();
    if expected.is_empty() {
        return Ok(());
    }
    let echoes = content
        .lines()
        .filter(|line| {
            let Some((raw, _)) = line.split_once('│') else {
                return false;
            };
            let token = raw.trim_start_matches(['+', '-', '>', ' ']).trim();
            crate::hashline::parse_anchor(token)
                .is_ok_and(|parsed| expected.contains(&crate::hashline::token_key(parsed.token())))
        })
        .count();
    if echoes >= threshold && threshold > 0 {
        return Err(ToolError::new(
            "E_WRITE_HASH_ECHO",
            "content appears to echo anchored read output; use only the intended file text",
        ));
    }
    Ok(())
}

/// 覆盖已有文件前读取旧内容；超过读写上限的旧文件不读。
///
/// 返回 `(旧内容, 是否因超过上限而跳过)`；跳过时不做 diff、不记撤销。
async fn read_previous(
    path: &Path,
    max_file_bytes: u64,
) -> Result<(Option<Vec<u8>>, bool), ToolError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.len() > max_file_bytes => Ok((None, true)),
        Ok(_) => match tokio::fs::read(path).await {
            Ok(bytes) => Ok((Some(bytes), false)),
            Err(error) => Err(ToolError::new("E_ACCESS", error.to_string())),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((None, false)),
        Err(error) => Err(ToolError::new("E_ACCESS", error.to_string())),
    }
}

/// 覆盖写入的 diff：任一侧超过展示上限就返回 `None`，不影响整体替换本身。
fn write_diff(
    path: &Path,
    old: Option<&[u8]>,
    content: &str,
    max_bytes: usize,
) -> Option<ToolDiff> {
    if old.map_or(0, |bytes| bytes.len()) > max_bytes || content.len() > max_bytes {
        return None;
    }
    build_write_diff(path, old, content)
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
/// 旧文件不是合法 UTF-8 或新旧内容完全相同时返回 `None`：前者没法按行显示，后者没有变化可显示。
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

fn normalize_line_endings(content: &str, ending: &[u8]) -> String {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if ending == b"\n" {
        return normalized;
    }
    let ending = std::str::from_utf8(ending).expect("换行符一定是 ASCII");
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
    use crate::hashline::{AnchorStore, anchor_token, served::read_snapshot};
    use std::fs;

    #[test]
    fn write_hash_echo_is_rejected_only_for_served_anchors() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.rs");
        fs::write(&path, "fn main() {}\nfn other() {}\n").expect("写文件");
        let snapshot = read_snapshot(&path, 1024).expect("读取快照");
        let anchors = AnchorStore::new();
        anchors.record_served(path.clone(), &snapshot, &[0..2]);
        let content = format!(
            "{}│one\n{}│two",
            anchor_token(1, &snapshot.lines[0]),
            anchor_token(2, &snapshot.lines[1])
        );
        assert!(reject_served_hash_echo(&path, &content, &anchors, 2).is_err());
        let content = format!("{}│not previously served", anchor_token(3, "new"));
        assert!(reject_served_hash_echo(&path, &content, &anchors, 2).is_ok());
    }

    #[test]
    fn line_endings_are_preserved_without_normalizing_content_semantics() {
        assert_eq!(normalize_line_endings("a\nb\n", b"\r\n"), "a\r\nb\r\n");
        assert_eq!(line_ending(b"a\r\nb\r\n"), b"\r\n");
    }

    #[tokio::test]
    async fn previous_content_is_skipped_above_the_read_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        fs::write(&path, vec![b'x'; 4096]).unwrap();
        let (old, oversized) = read_previous(&path, 1024).await.unwrap();
        assert!(old.is_none() && oversized);
        let (old, oversized) = read_previous(&path, 8192).await.unwrap();
        assert_eq!(old.unwrap().len(), 4096);
        assert!(!oversized);
        let (old, oversized) = read_previous(&dir.path().join("missing.txt"), 1024)
            .await
            .unwrap();
        assert!(old.is_none() && !oversized);
    }

    #[test]
    fn diff_is_skipped_above_the_display_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        let old = b"old\n";
        assert!(write_diff(&path, Some(&old[..]), "new\n", 1024).is_some());
        assert!(write_diff(&path, Some(&old[..]), "a longer body", 4).is_none());
        let big = vec![b'x'; 4096];
        assert!(write_diff(&path, Some(&big), "new\n", 1024).is_none());
    }
}

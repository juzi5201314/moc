//! Hashline 批量编辑解析、验证、搬运与原子提交。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use super::served::{RangeError, read_snapshot, verify_range};
use crate::error::CoreError;
use crate::hashline::{ANCHOR_SPACE, AnchorStore, FileSnapshot, ServedFile};
use moc_protocol::{ToolDiff, ToolFileDiff};

#[derive(Debug, Error)]
pub enum EditError {
    #[error("{0}")]
    Tool(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Core(#[from] CoreError),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditRequest {
    pub path: String,
    pub edits: Vec<[String; 3]>,
    #[serde(default, rename = "move")]
    pub move_lines: Option<MoveRequest>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveRequest {
    pub from: String,
    pub to: String,
    pub target_path: String,
    #[serde(default)]
    pub target_anchor: Option<String>,
}

#[derive(Debug, Clone)]
struct VerifiedEdit {
    start: usize,
    end: usize,
    replacement: Vec<String>,
}

/// 一个待提交文件的内容计划：原始快照 + 计算后的最终行。
#[derive(Debug, Clone)]
pub struct EditPlan {
    /// 源文件绝对路径。
    pub path: PathBuf,
    /// 编辑前的文件快照（含 bytes / 行尾 / BOM，提交阶段需要）。
    pub snapshot: FileSnapshot,
    /// 编辑后的最终行序列。
    pub lines: Vec<String>,
    /// 被删除的原始行号（0-based，`stable_hashes` 需要）。
    pub removed: Vec<usize>,
    /// verify_range 产生的警告（过期锚点已按当前位置处理等）。
    pub warnings: Vec<String>,
    /// 编辑前后内容完全一致（`apply_edit` 直接早退，`preview_edit` 返回空 diff）。
    pub no_changes: bool,
    /// `move` 的目标文件计划。
    pub target: Option<EditPlanTarget>,
}

/// `move` 目标文件的内容计划。
#[derive(Debug, Clone)]
pub struct EditPlanTarget {
    pub path: PathBuf,
    pub snapshot: FileSnapshot,
    pub lines: Vec<String>,
}

/// 验证全部范围并计算文件最终内容；不触碰磁盘、不更新锚点状态。
///
/// [`apply_edit`]（提交）与 [`preview_edit`]（授权 diff 预览）共用同一份计划，
/// 保证「弹窗里预览的」与「实际落盘的」完全一致。
pub fn plan_edit(
    cwd: &Path,
    request: &EditRequest,
    served: &ServedFile,
    target_served: Option<&ServedFile>,
    anchors: &AnchorStore,
    max_edits: usize,
    max_file_bytes: u64,
) -> Result<EditPlan, EditError> {
    if request.edits.is_empty() || request.edits.len() > max_edits {
        return Err(tool_error(
            "E_BAD_PAYLOAD",
            "edits must contain at least one item and must not exceed the configured limit",
        ));
    }
    let source_path = resolve_path(cwd, &request.path)?;
    let mut source_snapshot =
        read_snapshot(&source_path, max_file_bytes).map_err(tool_error_from_io)?;
    anchors.reuse_hashes(&source_path, &mut source_snapshot);
    ensure_snapshot_current(served, &source_snapshot, &source_path)?;

    let mut edits = Vec::with_capacity(request.edits.len());
    let mut warnings = Vec::new();
    let mut ranges = Vec::with_capacity(request.edits.len());
    let mut removed = Vec::new();
    for [from, to, replacement] in &request.edits {
        let (start, end, warning) =
            verify_range(served, &source_snapshot, from, to).map_err(range_tool_error)?;
        if ranges
            .iter()
            .any(|&(left, right)| start <= right && left <= end)
        {
            return Err(tool_error(
                "E_OVERLAP",
                "edit ranges overlap or reuse the same anchor",
            ));
        }
        ranges.push((start, end));
        if let Some(warning) = warning {
            warnings.push(warning);
        }
        let replacement = split_replacement(replacement);
        let (start, end, replacement) = if start == end && !replacement.is_empty() {
            (start + 1, start + 1, replacement)
        } else {
            removed.extend(start..=end);
            (start, end + 1, replacement)
        };
        edits.push(VerifiedEdit {
            start,
            end,
            replacement,
        });
    }
    edits.sort_by_key(|edit| (edit.start, edit.end));

    let mut source_lines = source_snapshot.lines.clone();
    for edit in edits.iter().rev() {
        source_lines.splice(edit.start..edit.end, edit.replacement.clone());
    }

    let mut target = None;
    if let Some(movement) = &request.move_lines {
        let (start, end, warning) =
            verify_range(served, &source_snapshot, &movement.from, &movement.to)
                .map_err(range_tool_error)?;
        if let Some(warning) = warning {
            warnings.push(warning);
        }
        let target_path = resolve_path(cwd, &movement.target_path)?;
        if target_path == source_path {
            return Err(tool_error(
                "E_BAD_PAYLOAD",
                "move target_path must differ from source path",
            ));
        }
        let target_served_for_check = target_served.ok_or_else(|| {
            tool_error(
                "E_UNSERVED_RANGE",
                "target file has not been read; read it before moving lines",
            )
        })?;
        let mut target_snapshot =
            read_snapshot(&target_path, max_file_bytes).map_err(tool_error_from_io)?;
        anchors.reuse_hashes(&target_path, &mut target_snapshot);
        ensure_snapshot_current(target_served_for_check, &target_snapshot, &target_path)?;
        let target_position = if let Some(anchor) = &movement.target_anchor {
            let (position, _, warning) =
                verify_range(target_served_for_check, &target_snapshot, anchor, anchor)
                    .map_err(range_tool_error)?;
            if let Some(warning) = warning {
                warnings.push(warning);
            }
            position + 1
        } else {
            target_snapshot.lines.len()
        };
        let moved_lines = source_snapshot.lines[start..=end].to_vec();
        if ranges
            .iter()
            .any(|&(left, right)| start <= right && left <= end)
        {
            return Err(tool_error(
                "E_OVERLAP",
                "the edit batch overlaps the move source range",
            ));
        }
        removed.extend(start..=end);
        let mut original_source = source_snapshot.lines.clone();
        original_source.splice(start..=end, []);
        let mut final_source = original_source;
        for edit in edits.iter().rev() {
            let offset = usize::from(edit.start > end).saturating_mul(end - start + 1);
            let adjusted_start = edit.start.saturating_sub(offset);
            let adjusted_end = edit.end.saturating_sub(offset);
            final_source.splice(adjusted_start..adjusted_end, edit.replacement.clone());
        }
        source_lines = final_source;
        let mut target_lines = target_snapshot.lines.clone();
        target_lines.splice(target_position..target_position, moved_lines.clone());
        target = Some(EditPlanTarget {
            path: target_path,
            snapshot: target_snapshot,
            lines: target_lines,
        });
    }

    if source_lines.is_empty() && !source_snapshot.lines.is_empty() {
        return Err(tool_error(
            "E_EMPTY_RANGE",
            "the edit would empty the file; use write or explicitly rewrite the complete file",
        ));
    }
    let no_changes = source_lines == source_snapshot.lines && target.is_none();
    Ok(EditPlan {
        path: source_path,
        snapshot: source_snapshot,
        lines: source_lines,
        removed,
        warnings,
        no_changes,
        target,
    })
}

impl EditPlan {
    /// 把计划转为结构化 [`ToolDiff`]（协议 DTO）。
    fn to_tool_diff(&self) -> ToolDiff {
        let mut files = Vec::with_capacity(1 + usize::from(self.target.is_some()));
        let mut truncated =
            push_file_diff(&self.path, &self.snapshot.lines, &self.lines, &mut files);
        if let Some(target) = &self.target {
            truncated |= push_file_diff(
                &target.path,
                &target.snapshot.lines,
                &target.lines,
                &mut files,
            );
        }
        ToolDiff { files, truncated }
    }
}

/// 单文件 diff 计算并压入 `files`；返回该文件是否被截断。
fn push_file_diff(
    path: &Path,
    old_lines: &[String],
    new_lines: &[String],
    files: &mut Vec<ToolFileDiff>,
) -> bool {
    let old_refs: Vec<&str> = old_lines.iter().map(String::as_str).collect();
    let new_refs: Vec<&str> = new_lines.iter().map(String::as_str).collect();
    let (hunks, truncated) = moc_protocol::build_hunks(&old_refs, &new_refs);
    files.push(ToolFileDiff {
        path: path.display().to_string(),
        hunks,
    });
    truncated
}

/// `apply_edit` 的成功输出：给模型看的内容 + 给前端看的结构化 diff。
#[derive(Debug, Clone)]
pub struct EditOutput {
    pub content: String,
    pub diff: Option<ToolDiff>,
}

/// 先验证全部范围并构造文件内容，再原子提交；失败不产生 undo 记录。
pub async fn apply_edit(
    cwd: &Path,
    request: &EditRequest,
    served: &mut ServedFile,
    target_served: Option<&mut ServedFile>,
    anchors: &AnchorStore,
    max_edits: usize,
    max_file_bytes: u64,
) -> Result<EditOutput, EditError> {
    let plan = plan_edit(
        cwd,
        request,
        served,
        target_served.as_deref(),
        anchors,
        max_edits,
        max_file_bytes,
    )?;
    if plan.no_changes {
        return Ok(EditOutput {
            content: "No changes made.".into(),
            diff: None,
        });
    }
    let diff = plan.to_tool_diff();

    let EditPlan {
        path: source_path,
        snapshot: source_snapshot,
        lines: source_lines,
        removed,
        warnings,
        target,
        ..
    } = plan;

    let source_hashes = stable_hashes(&source_snapshot, &source_lines, served, &removed)?;
    let source_bytes = encode_file(&source_snapshot, &source_lines);
    let target_change = match target {
        Some(target) => {
            let target_served = target_served.as_deref().expect("目标已在 plan 阶段验证");
            let hashes = stable_hashes(&target.snapshot, &target.lines, target_served, &[])?;
            let bytes = encode_file(&target.snapshot, &target.lines);
            Some((target.path, target.snapshot, target.lines, hashes, bytes))
        }
        None => None,
    };

    let source_temp = stage_file(&source_path, &source_bytes).await?;
    let target_temp = if let Some((path, _, _, _, bytes)) = &target_change {
        match stage_file(path, bytes).await {
            Ok(path) => Some(path),
            Err(error) => {
                let _ = tokio::fs::remove_file(&source_temp).await;
                return Err(error);
            }
        }
    } else {
        None
    };

    let target_path = target_change.as_ref().map(|(path, ..)| path);
    let source_backup = if target_path.is_some() {
        match stage_file(&source_path, &source_snapshot.bytes).await {
            Ok(backup) => Some(backup),
            Err(error) => {
                cleanup_staged(source_temp, target_temp).await;
                return Err(error);
            }
        }
    } else {
        None
    };
    if let Err(error) = tokio::fs::rename(&source_temp, &source_path).await {
        cleanup_staged(source_temp, target_temp).await;
        if let Some(backup) = source_backup {
            let _ = tokio::fs::remove_file(backup).await;
        }
        return Err(error.into());
    }
    if let (Some(temporary), Some((target_path, ..))) = (target_temp, target_change.as_ref())
        && let Err(error) = tokio::fs::rename(&temporary, target_path).await
    {
        let _ = tokio::fs::remove_file(&temporary).await;
        if let Some(backup) = &source_backup
            && let Err(rollback) = tokio::fs::rename(backup, &source_path).await
        {
            return Err(tool_error(
                "E_BATCH_ABORT",
                &format!(
                    "target commit failed: {error}; source rollback failed: {rollback}; original content is saved at {}",
                    backup.display()
                ),
            ));
        }
        return Err(tool_error(
            "E_BATCH_ABORT",
            &format!("target commit failed; source file was restored: {error}"),
        ));
    }
    if let Some(backup) = source_backup {
        tokio::fs::remove_file(backup).await?;
    }

    let source_after = read_snapshot(&source_path, max_file_bytes).map_err(tool_error_from_io)?;
    anchors.push_undo(
        source_path.clone(),
        source_snapshot.bytes.clone(),
        source_after.epoch,
    );
    let mut output = publish_edit(
        &source_path,
        &source_snapshot,
        source_after,
        source_hashes,
        served,
        anchors,
    );
    if let Some((path, before, _, hashes, _)) = target_change {
        let after = read_snapshot(&path, max_file_bytes).map_err(tool_error_from_io)?;
        anchors.push_undo(path.clone(), before.bytes.clone(), after.epoch);
        output.push('\n');
        output.push_str(&publish_edit(
            &path,
            &before,
            after,
            hashes,
            target_served.expect("目标已验证"),
            anchors,
        ));
    }
    if !warnings.is_empty() {
        output.push_str(&format!("\nWarning: {}", warnings.join("; ")));
    }
    Ok(EditOutput {
        content: output,
        diff: Some(diff),
    })
}

/// 授权预览：计算 edit 将产生的结构化 diff，不落盘、不更新锚点。
///
/// 与 [`apply_edit`] 共用 [`plan_edit`]，预览通过即代表实际执行可用；
/// 返回 [`EditError`]（如 E_UNSERVED_RANGE）时调用方应直接放弃预览，
/// 真实错误由执行阶段给出。
pub fn preview_edit(
    cwd: &Path,
    request: &EditRequest,
    served: &ServedFile,
    target_served: Option<&ServedFile>,
    anchors: &AnchorStore,
    max_edits: usize,
    max_file_bytes: u64,
) -> Result<ToolDiff, EditError> {
    let plan = plan_edit(
        cwd,
        request,
        served,
        target_served,
        anchors,
        max_edits,
        max_file_bytes,
    )?;
    if plan.no_changes {
        return Ok(ToolDiff {
            files: Vec::new(),
            truncated: false,
        });
    }
    Ok(plan.to_tool_diff())
}

pub async fn undo(
    path: &Path,
    anchors: &AnchorStore,
    max_file_bytes: u64,
) -> Result<String, EditError> {
    let Some(previous) = anchors.peek_undo(path) else {
        return Err(tool_error("E_NO_UNDO", "the file has no undoable edit"));
    };
    let stale = || {
        tool_error(
            "E_UNDO_STALE",
            "the file changed externally after the edit; undo was not performed and current content and undo history were preserved",
        )
    };
    let before = tokio::fs::metadata(path).await?;
    if before.len() > max_file_bytes {
        return Err(stale());
    }
    let bytes = tokio::fs::read(path).await?;
    if bytes.len() as u64 > max_file_bytes {
        return Err(stale());
    }
    let after = tokio::fs::metadata(path).await?;
    if super::SnapshotId::from_metadata(&before) != super::SnapshotId::from_metadata(&after) {
        return Err(stale());
    }
    let current = super::SnapshotId::from_metadata_and_bytes(&after, &bytes);
    if current != previous.expected {
        return Err(stale());
    }
    let temporary = stage_file(path, &previous.bytes).await?;
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(temporary).await;
        return Err(error.into());
    }
    let snapshot = read_snapshot(path, max_file_bytes).map_err(tool_error_from_io)?;
    anchors.commit_undo(path, snapshot.epoch);
    anchors.invalidate(path);
    let lines = snapshot.lines.len();
    Ok(format!("Reverted file changes ({lines} lines)"))
}

fn resolve_path(cwd: &Path, value: &str) -> Result<PathBuf, EditError> {
    std::fs::canonicalize(cwd.join(value)).map_err(tool_error_from_io)
}

fn ensure_snapshot_current(
    served: &ServedFile,
    snapshot: &FileSnapshot,
    path: &Path,
) -> Result<(), EditError> {
    if served.epoch != Some(snapshot.epoch) {
        return Err(tool_error(
            "E_UNSERVED_RANGE",
            &format!("{} changed externally after read", path.display()),
        ));
    }
    Ok(())
}

fn split_replacement(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n').map(str::to_owned).collect()
}

fn stable_hashes(
    snapshot: &FileSnapshot,
    lines: &[String],
    served: &ServedFile,
    removed: &[usize],
) -> Result<Vec<u32>, EditError> {
    super::anchor::map_stable_hashes_reserved(
        &snapshot.lines,
        &snapshot
            .hashes
            .iter()
            .copied()
            .map(Some)
            .collect::<Vec<_>>(),
        lines,
        removed,
        &served
            .retired
            .iter()
            .map(|&(hash, _)| hash)
            .collect::<Vec<_>>(),
    )
    .map_err(|error| tool_error("E_ANCHOR_SPACE_EXHAUSTED", &error.to_string()))
}

fn encode_file(snapshot: &FileSnapshot, lines: &[String]) -> Vec<u8> {
    let line_ending = snapshot.line_ending.bytes();
    let mut output = Vec::with_capacity(snapshot.bytes.len());
    if snapshot.bom {
        output.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    }
    for (index, line) in lines.iter().enumerate() {
        output.extend_from_slice(line.as_bytes());
        if index + 1 < lines.len()
            || snapshot.bytes.ends_with(b"\n")
            || snapshot.bytes.ends_with(b"\r")
        {
            output.extend_from_slice(line_ending);
        }
    }
    output
}

async fn stage_file(path: &Path, bytes: &[u8]) -> Result<PathBuf, EditError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(".moc-{}.tmp", crate::ids::entry_id()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .await?;
    let result = async {
        if let Ok(metadata) = tokio::fs::metadata(path).await {
            file.set_permissions(metadata.permissions()).await?;
        }
        file.write_all(bytes).await?;
        file.sync_all().await
    }
    .await;
    if let Err(error) = result {
        drop(file);
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error.into());
    }
    Ok(temp)
}

async fn cleanup_staged(first: PathBuf, second: Option<PathBuf>) {
    let _ = tokio::fs::remove_file(first).await;
    if let Some(second) = second {
        let _ = tokio::fs::remove_file(second).await;
    }
}

fn publish_edit(
    path: &Path,
    before: &FileSnapshot,
    mut after: FileSnapshot,
    hashes: Vec<u32>,
    served: &mut ServedFile,
    anchors: &AnchorStore,
) -> String {
    after.hashes = hashes;
    let (output, shown) = render_diff(path, before, &after);
    // 只记录真实出现过的锚点；定长位图会让每次编辑都分配并清零 238 KiB。
    let served_before: std::collections::HashSet<u32> =
        served.hashes.iter().flatten().copied().collect();
    let mut current: std::collections::HashSet<u32> =
        std::collections::HashSet::with_capacity(after.hashes.len());
    let rows: Vec<_> = after
        .hashes
        .iter()
        .enumerate()
        .filter_map(|(position, &hash)| {
            current.insert(hash);
            debug_assert!(hash < ANCHOR_SPACE);
            (served_before.contains(&hash) || shown.contains(&position)).then_some((position, hash))
        })
        .collect();
    let mut retired = std::mem::take(&mut served.retired);
    retired.extend(
        before
            .hashes
            .iter()
            .enumerate()
            .filter_map(|(position, &hash)| (!current.contains(&hash)).then_some((hash, position))),
    );
    served.record_served(&rows, &after.lines, &after);
    served.retired = retired;
    anchors.commit_edit(path.to_path_buf(), served.clone(), after);
    output
}

fn render_diff(
    path: &Path,
    before: &FileSnapshot,
    after: &FileSnapshot,
) -> (String, std::ops::Range<usize>) {
    let prefix = before
        .lines
        .iter()
        .zip(&after.lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = before.lines[prefix..]
        .iter()
        .rev()
        .zip(after.lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let start = prefix.saturating_sub(2);
    let old_end = before.lines.len() - suffix;
    let new_end = after.lines.len() - suffix;
    let end = (new_end + 2).min(after.lines.len());
    let mut output = format!("[{}]", path.display());
    for index in start..prefix {
        output.push_str(&format!(
            "\n  {}│{}",
            anchor_hash(after.hashes[index]),
            after.lines[index]
        ));
    }
    for index in prefix..old_end {
        output.push_str(&format!(
            "\n- {}│{}",
            anchor_hash(before.hashes[index]),
            before.lines[index]
        ));
    }
    for index in prefix..new_end {
        output.push_str(&format!(
            "\n+ {}│{}",
            anchor_hash(after.hashes[index]),
            after.lines[index]
        ));
    }
    for index in new_end..end {
        output.push_str(&format!(
            "\n  {}│{}",
            anchor_hash(after.hashes[index]),
            after.lines[index]
        ));
    }
    (output, start..end)
}

fn anchor_hash(hash: u32) -> String {
    crate::hashline::encode_anchor(hash).expect("分配的锚点属于 base-62 空间")
}

fn range_tool_error(error: RangeError) -> EditError {
    let message = error.to_string();
    match error {
        RangeError::Unserved { .. } => tool_error("E_UNSERVED_RANGE", &message),
        RangeError::BadAnchor => tool_error("E_BAD_ANCHOR", &message),
        RangeError::StaleRange => tool_error("E_STALE_RANGE", &message),
        RangeError::TooLarge => tool_error("E_RANGE_TOO_LARGE", &message),
        RangeError::StaleAnchor => tool_error("E_STALE_ANCHOR", &message),
        RangeError::AnchorSpaceExhausted => tool_error("E_ANCHOR_SPACE_EXHAUSTED", &message),
    }
}

fn tool_error(code: &str, message: &str) -> EditError {
    EditError::Tool(format!("[{code}] {message}"))
}

fn tool_error_from_io(error: std::io::Error) -> EditError {
    let message = error.to_string();
    let code = if message.contains("E_BINARY") {
        "E_BINARY"
    } else if message.contains("E_TOO_LARGE") {
        "E_TOO_LARGE"
    } else if error.kind() == std::io::ErrorKind::NotFound {
        "E_NOT_FOUND"
    } else {
        "E_ACCESS"
    };
    tool_error(code, &message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::{AnchorStore, ServedFile, encode_anchor};

    fn served_snapshot(path: &Path, text: &str) -> (FileSnapshot, ServedFile) {
        std::fs::write(path, text).expect("写测试文件");
        let snapshot = read_snapshot(path, 4096).expect("读取快照");
        let mut served = ServedFile::new();
        let rows: Vec<_> = (0..snapshot.lines.len())
            .map(|position| (position, snapshot.hashes[position]))
            .collect();
        served.record_served(&rows, &snapshot.lines, &snapshot);
        (snapshot, served)
    }

    #[tokio::test]
    async fn applies_hashline_replacements_and_preserves_crlf_bom() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"\xef\xbb\xbffirst\r\nsecond\r\nthird\r\n").expect("写文件");
        let snapshot = read_snapshot(&path, 4096).expect("读取");
        let mut served = ServedFile::new();
        let rows: Vec<_> = snapshot.hashes.iter().copied().enumerate().collect();
        served.record_served(&rows, &snapshot.lines, &snapshot);
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[
                encode_anchor(snapshot.hashes[1]).unwrap(),
                encode_anchor(snapshot.hashes[1]).unwrap(),
                "replacement".into(),
            ]],
            move_lines: None,
        };
        let anchors = AnchorStore::new();
        apply_edit(dir.path(), &request, &mut served, None, &anchors, 32, 4096)
            .await
            .expect("编辑成功");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xef\xbb\xbffirst\r\nsecond\r\nreplacement\r\nthird\r\n"
        );
    }

    #[tokio::test]
    async fn rejects_unserved_and_overlapping_edits_without_mutating_files() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let (snapshot, mut served) = served_snapshot(&path, "a\nb\nc");
        let anchors = AnchorStore::new();
        let before = std::fs::read(&path).unwrap();
        let mut partial = ServedFile::new();
        partial.record_served(
            &[(0, snapshot.hashes[0]), (2, snapshot.hashes[2])],
            &snapshot.lines,
            &snapshot,
        );
        let denied = EditRequest {
            path: "file.txt".into(),
            edits: vec![[
                encode_anchor(snapshot.hashes[0]).unwrap(),
                encode_anchor(snapshot.hashes[2]).unwrap(),
                "x".into(),
            ]],
            move_lines: None,
        };
        assert!(
            apply_edit(dir.path(), &denied, &mut partial, None, &anchors, 32, 4096)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let overlap = EditRequest {
            path: "file.txt".into(),
            edits: vec![
                [
                    encode_anchor(snapshot.hashes[0]).unwrap(),
                    encode_anchor(snapshot.hashes[1]).unwrap(),
                    "x".into(),
                ],
                [
                    encode_anchor(snapshot.hashes[1]).unwrap(),
                    encode_anchor(snapshot.hashes[2]).unwrap(),
                    "y".into(),
                ],
            ],
            move_lines: None,
        };
        assert!(
            apply_edit(dir.path(), &overlap, &mut served, None, &anchors, 32, 4096)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn preview_matches_applied_content_and_reports_insert_delete_lines() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let (snapshot, mut served) = served_snapshot(&path, "alpha\nbeta\ngamma\n");
        let anchors = AnchorStore::new();
        let anchor = encode_anchor(snapshot.hashes[1]).unwrap();
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[anchor.clone(), anchor, "delta\nbeta".into()]],
            move_lines: None,
        };
        // 预览：在 beta 行后插入 delta、beta 两行（同锚点 + 非空 = 后插入）
        let diff = preview_edit(dir.path(), &request, &served, None, &anchors, 32, 4096)
            .expect("预览成功");
        assert!(!diff.truncated);
        assert_eq!(diff.files.len(), 1);
        let kinds: Vec<_> = diff.files[0]
            .hunks
            .iter()
            .flat_map(|hunk| {
                hunk.lines
                    .iter()
                    .map(|line| (line.kind, line.text.as_str()))
            })
            .collect();
        assert!(kinds.contains(&(moc_protocol::DiffLineKind::Insert, "delta")));
        assert!(kinds.contains(&(moc_protocol::DiffLineKind::Insert, "beta")));
        assert!(
            !kinds
                .iter()
                .any(|(kind, _)| *kind == moc_protocol::DiffLineKind::Delete)
        );
        // 执行：最终内容与预览一致
        let output = apply_edit(dir.path(), &request, &mut served, None, &anchors, 32, 4096)
            .await
            .expect("编辑成功");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha\nbeta\ndelta\nbeta\ngamma\n"
        );
        let applied = output.diff.expect("成功编辑携带 diff");
        let applied_inserts: Vec<&str> = applied.files[0]
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .filter(|line| line.kind == moc_protocol::DiffLineKind::Insert)
            .map(|line| line.text.as_str())
            .collect();
        // 重复行场景下 LCS 对齐存在等价解，这里只断言插入集合与最终内容一致
        assert_eq!(applied_inserts.len(), 2);
        assert!(applied_inserts.contains(&"delta"));
        assert!(applied_inserts.contains(&"beta"));
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use crate::hashline::encode_anchor;

    fn prepare(path: &Path, text: &str, store: &AnchorStore) -> (FileSnapshot, ServedFile) {
        std::fs::write(path, text).unwrap();
        let snapshot = read_snapshot(path, 4096).unwrap();
        let rows: Vec<_> = snapshot.hashes.iter().copied().enumerate().collect();
        store.record_served(path.to_path_buf(), &rows, snapshot.clone());
        (snapshot, store.served(path).unwrap())
    }

    #[tokio::test]
    async fn duplicate_insertions_are_rejected_and_unchanged_anchors_survive_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        let store = AnchorStore::new();
        let (snapshot, mut served) = prepare(&path, "dup\ndup\nlast\n", &store);
        let first = encode_anchor(snapshot.hashes[0]).unwrap();
        let second = encode_anchor(snapshot.hashes[1]).unwrap();
        let mut request = EditRequest {
            path: "source".into(),
            edits: vec![
                [first.clone(), first.clone(), "one".into()],
                [first.clone(), first.clone(), "two".into()],
            ],
            move_lines: None,
        };
        assert!(
            apply_edit(dir.path(), &request, &mut served, None, &store, 32, 4096)
                .await
                .unwrap_err()
                .to_string()
                .contains("E_OVERLAP")
        );
        assert_eq!(std::fs::read(&path).unwrap(), snapshot.bytes);
        assert!(store.latest_undo_path().is_none());
        request.edits = vec![[first.clone(), first, String::new()]];
        apply_edit(dir.path(), &request, &mut served, None, &store, 32, 4096)
            .await
            .unwrap();
        request.edits = vec![[second.clone(), second, "inserted".into()]];
        apply_edit(dir.path(), &request, &mut served, None, &store, 32, 4096)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "dup\ninserted\nlast\n"
        );
        let mut reread = read_snapshot(&path, 4096).unwrap();
        store.reuse_hashes(&path, &mut reread);
        assert_eq!(reread.hashes[0], snapshot.hashes[1]);
        undo(&path, &store, 4096).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "dup\nlast\n");
        undo(&path, &store, 4096).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), snapshot.bytes);
    }

    #[tokio::test]
    async fn move_commits_both_files_and_returns_target_anchors_without_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        let store = AnchorStore::new();
        let (a, mut served_a) = prepare(&source, "move\nkeep\nend\n", &store);
        let (b, mut served_b) = prepare(&target, "target\n", &store);
        let hash = |index| encode_anchor(a.hashes[index]).unwrap();
        let request = EditRequest {
            path: "source".into(),
            edits: vec![[hash(2), hash(2), "after".into()]],
            move_lines: Some(MoveRequest {
                from: hash(0),
                to: hash(0),
                target_path: "target".into(),
                target_anchor: None,
            }),
        };
        let output = apply_edit(
            dir.path(),
            &request,
            &mut served_a,
            Some(&mut served_b),
            &store,
            32,
            4096,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "keep\nend\nafter\n"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "target\nmove\n");
        assert!(output.content.contains(&format!("[{}]", target.display())));
        undo(&source, &store, 4096).await.unwrap();
        undo(&target, &store, 4096).await.unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), a.bytes);
        assert_eq!(std::fs::read(&target).unwrap(), b.bytes);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn undo_rejects_external_changes_without_consuming_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        let store = AnchorStore::new();
        let (snapshot, mut served) = prepare(&path, "first\nlast\n", &store);
        let anchor = encode_anchor(snapshot.hashes[0]).unwrap();
        let request = EditRequest {
            path: "source".into(),
            edits: vec![[anchor.clone(), anchor, "added".into()]],
            move_lines: None,
        };
        apply_edit(dir.path(), &request, &mut served, None, &store, 32, 4096)
            .await
            .unwrap();
        std::fs::write(&path, "external content").unwrap();
        for _ in 0..2 {
            assert!(
                undo(&path, &store, 4096)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("E_UNDO_STALE")
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "external content");
        }
    }

    #[tokio::test]
    async fn complete_read_releases_retired_anchors_for_new_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        let store = AnchorStore::new();
        let (original, mut served) = prepare(&path, "drop\nkeep\n", &store);
        let dropped = encode_anchor(original.hashes[0]).unwrap();
        let deletion = EditRequest {
            path: "source".into(),
            edits: vec![[dropped.clone(), dropped, String::new()]],
            move_lines: None,
        };
        apply_edit(dir.path(), &deletion, &mut served, None, &store, 32, 4096)
            .await
            .unwrap();
        let mut reread = read_snapshot(&path, 4096).unwrap();
        store.reuse_hashes(&path, &mut reread);
        let keep = encode_anchor(reread.hashes[0]).unwrap();
        let rows: Vec<_> = reread.hashes.iter().copied().enumerate().collect();
        store.record_served(path.clone(), &rows, reread);
        let mut served = store.served(&path).unwrap();
        let insertion = EditRequest {
            path: "source".into(),
            edits: vec![[keep.clone(), keep, "drop".into()]],
            move_lines: None,
        };
        apply_edit(dir.path(), &insertion, &mut served, None, &store, 32, 4096)
            .await
            .unwrap();
        let mut current = read_snapshot(&path, 4096).unwrap();
        store.reuse_hashes(&path, &mut current);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep\ndrop\n");
        assert_eq!(current.hashes[1], original.hashes[0]);
    }
}

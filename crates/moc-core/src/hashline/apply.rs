//! Hashline 批量编辑解析、验证、搬运与原子提交。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use super::served::{RangeError, read_snapshot, verify_range};
use crate::error::CoreError;
use crate::hashline::{AnchorStore, FileSnapshot, ServedFile, anchor_line, parse_anchor};
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
    /// 编辑后重定位旧 served 行所需的原行位移表。
    pub moves: Vec<(usize, isize)>,
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
    _anchors: &AnchorStore,
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
    let source_snapshot =
        read_snapshot(&source_path, max_file_bytes).map_err(tool_error_from_io)?;
    ensure_snapshot_current(served, &source_snapshot, &source_path)?;

    let mut edits = Vec::with_capacity(request.edits.len());
    let mut warnings = Vec::new();
    let mut ranges = Vec::with_capacity(request.edits.len());
    for [from, to, replacement] in &request.edits {
        let parsed_from = parse_anchor(from).map_err(|received| {
            range_tool_error(RangeError::BadAnchor {
                field: "from",
                received,
            })
        })?;
        let parsed_to = parse_anchor(to).map_err(|received| {
            range_tool_error(RangeError::BadAnchor {
                field: "to",
                received,
            })
        })?;
        if let Some(trimmed) = parsed_from.trimmed {
            warnings.push(format!(
                "edit[{}]: from cleaned to \"{trimmed}\" (copy only the anchor, not the line content)",
                edits.len()
            ));
        }
        if let Some(trimmed) = parsed_to.trimmed {
            warnings.push(format!(
                "edit[{}]: to cleaned to \"{trimmed}\" (copy only the anchor, not the line content)",
                edits.len()
            ));
        }
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
            (start, end + 1, replacement)
        };

        edits.push(VerifiedEdit {
            start,
            end,
            replacement,
        });
    }
    let moves = build_moves(source_snapshot.lines.len(), &edits);
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
        let target_snapshot =
            read_snapshot(&target_path, max_file_bytes).map_err(tool_error_from_io)?;
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
        moves,
        lines: source_lines,
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
    commit_plan(plan, served, target_served, anchors, max_file_bytes).await
}

/// 提交一份已通过验证的编辑计划；到 rename 前才重读快照确认没有并发写入。
async fn commit_plan(
    plan: EditPlan,
    served: &mut ServedFile,
    target_served: Option<&mut ServedFile>,
    anchors: &AnchorStore,
    max_file_bytes: u64,
) -> Result<EditOutput, EditError> {
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
        moves,
        lines: source_lines,
        warnings,
        target,
        ..
    } = plan;

    let source_bytes = encode_file(&source_snapshot, &source_lines);
    let target_change = match target {
        Some(target) => {
            let bytes = encode_file(&target.snapshot, &target.lines);
            Some((target.path, target.snapshot, target.lines, bytes))
        }
        None => None,
    };

    let source_temp = stage_file(&source_path, &source_bytes).await?;
    let target_temp = if let Some((path, _, _, bytes)) = &target_change {
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
    // 校验与 rename 之间存在窗口：紧邻提交前重读源文件，窗口内被外部写入时放弃。
    if let Err(error) = ensure_epoch(&source_path, source_snapshot.epoch, max_file_bytes).await {
        cleanup_staged(source_temp, target_temp).await;
        if let Some(backup) = &source_backup {
            let _ = tokio::fs::remove_file(backup).await;
        }
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&source_temp, &source_path).await {
        cleanup_staged(source_temp, target_temp).await;
        if let Some(backup) = source_backup {
            let _ = tokio::fs::remove_file(backup).await;
        }
        return Err(error.into());
    }
    if let (Some(temporary), Some((target_path, target_snapshot, ..))) =
        (target_temp, target_change.as_ref())
    {
        if let Err(error) = ensure_epoch(target_path, target_snapshot.epoch, max_file_bytes).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(rollback_source(error, &source_path, source_backup.as_ref()).await);
        }
        if let Err(error) = tokio::fs::rename(&temporary, target_path).await {
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
        &moves,
        served,
        anchors,
    );
    if let Some((path, before, _, _)) = target_change {
        let after = read_snapshot(&path, max_file_bytes).map_err(tool_error_from_io)?;
        anchors.push_undo(path.clone(), before.bytes.clone(), after.epoch);
        output.push('\n');
        output.push_str(&publish_edit(
            &path,
            &before,
            after,
            &[],
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

fn build_moves(line_count: usize, edits: &[VerifiedEdit]) -> Vec<(usize, isize)> {
    let mut moves = Vec::with_capacity(edits.len() * 2 + 1);
    let mut delta = 0isize;
    for edit in edits {
        if edit.start > 0 {
            moves.push((edit.start, delta));
        }
        for position in edit.start..edit.end {
            moves.push((position, isize::MIN));
        }
        let removed = isize::try_from(edit.end.saturating_sub(edit.start))
            .expect("removed line count must fit in isize");
        let replacement = isize::try_from(edit.replacement.len())
            .expect("replacement line count must fit in isize");
        delta += replacement - removed;
        moves.push((edit.end, delta));
    }
    if line_count > 0 {
        moves.push((line_count, delta));
    }
    moves
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

/// rename 前紧邻重读目标文件；epoch 与计划不一致说明窗口内被并发写入。
///
/// 失败错误固定为 `E_STALE_RANGE`：调用方保留磁盘上的外部内容，不覆盖。
async fn ensure_epoch(
    path: &Path,
    expected: super::SnapshotId,
    max_file_bytes: u64,
) -> Result<(), EditError> {
    let stale = || {
        tool_error(
            "E_STALE_RANGE",
            &format!(
                "{} changed externally while the edit was being committed; retry with current anchors",
                path.display()
            ),
        )
    };
    let metadata = tokio::fs::metadata(path).await.map_err(|_| stale())?;
    if metadata.len() > max_file_bytes {
        return Err(stale());
    }
    let bytes = tokio::fs::read(path).await.map_err(|_| stale())?;
    if bytes.len() as u64 > max_file_bytes
        || super::SnapshotId::from_metadata_and_bytes(&metadata, &bytes) != expected
    {
        return Err(stale());
    }
    Ok(())
}

/// 目标提交失败时把源文件恢复回提交前内容；恢复失败单独报告，避免静默丢内容。
async fn rollback_source(
    error: EditError,
    source_path: &Path,
    backup: Option<&PathBuf>,
) -> EditError {
    let Some(backup) = backup else {
        return error;
    };
    match tokio::fs::rename(backup, source_path).await {
        Ok(()) => error,
        Err(rollback) => tool_error(
            "E_BATCH_ABORT",
            &format!(
                "source rollback failed: {rollback}; original content is saved at {}",
                backup.display()
            ),
        ),
    }
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
    after: FileSnapshot,
    moves: &[(usize, isize)],
    served: &mut ServedFile,
    anchors: &AnchorStore,
) -> String {
    let (output, shown) = render_diff(path, before, &after);
    let _dropped = served.rebase(moves, after.lines.len());
    served.epoch = Some(after.epoch);
    served.serve(after.epoch, after.lines.len(), &[shown]);
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
            "\n  {}",
            anchor_line(index + 1, &after.lines[index])
        ));
    }
    for index in prefix..old_end {
        output.push_str(&format!(
            "\n- {}",
            anchor_line(index + 1, &before.lines[index])
        ));
    }
    for index in prefix..new_end {
        output.push_str(&format!(
            "\n+ {}",
            anchor_line(index + 1, &after.lines[index])
        ));
    }
    for index in new_end..end {
        output.push_str(&format!(
            "\n  {}",
            anchor_line(index + 1, &after.lines[index])
        ));
    }
    (output, start..end)
}

fn range_tool_error(error: RangeError) -> EditError {
    let message = error.to_string();
    match error {
        RangeError::Unserved { .. } => tool_error("E_UNSERVED_RANGE", &message),
        RangeError::BadAnchor { .. } => tool_error("E_BAD_ANCHOR", &message),
        RangeError::Stale { .. } => tool_error("E_STALE_ANCHOR", &message),
        RangeError::TooLarge => tool_error("E_RANGE_TOO_LARGE", &message),
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
    use crate::hashline::{AnchorStore, ServedFile, anchor_token};

    fn served_snapshot(path: &Path, text: &str) -> (FileSnapshot, ServedFile) {
        std::fs::write(path, text).expect("写测试文件");
        let snapshot = read_snapshot(path, 4096).expect("读取快照");
        let mut served = ServedFile::new();
        served.serve(
            snapshot.epoch,
            snapshot.lines.len(),
            &[0..snapshot.lines.len()],
        );
        (snapshot, served)
    }

    #[tokio::test]
    async fn applies_replacement_and_preserves_crlf_bom() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"\xef\xbb\xbffirst\r\nsecond\r\nthird\r\n").expect("写文件");
        let snapshot = read_snapshot(&path, 4096).expect("读取");
        let mut served = ServedFile::new();
        served.serve(
            snapshot.epoch,
            snapshot.lines.len(),
            &[0..snapshot.lines.len()],
        );
        let anchor = anchor_token(2, &snapshot.lines[1]);
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[anchor.clone(), anchor, "replacement".into()]],
            move_lines: None,
        };
        apply_edit(
            dir.path(),
            &request,
            &mut served,
            None,
            &AnchorStore::new(),
            32,
            4096,
        )
        .await
        .expect("编辑成功");
        assert_eq!(
            std::fs::read(&path).expect("读取结果"),
            b"\xef\xbb\xbffirst\r\nsecond\r\nreplacement\r\nthird\r\n"
        );
    }

    #[tokio::test]
    async fn edit_then_continue_without_reread() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let (snapshot, mut served) = served_snapshot(&path, "one\ntwo\nthree\n");
        let store = AnchorStore::new();
        let first = anchor_token(1, &snapshot.lines[0]);
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[first.clone(), first, "inserted".into()]],
            move_lines: None,
        };
        let output = apply_edit(dir.path(), &request, &mut served, None, &store, 32, 4096)
            .await
            .expect("首次编辑成功");
        let second = output
            .content
            .lines()
            .find_map(|line| {
                line.strip_prefix("+ ")
                    .filter(|line| line.contains("│inserted"))
                    .and_then(|line| line.split_once('│').map(|(anchor, _)| anchor.to_owned()))
            })
            .expect("diff 应包含可继续编辑的锚点");
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[second.clone(), second, "changed".into()]],
            move_lines: None,
        };
        apply_edit(dir.path(), &request, &mut served, None, &store, 32, 4096)
            .await
            .expect("无需重读即可继续编辑");
        assert_eq!(
            std::fs::read_to_string(&path).expect("读取结果"),
            "one\ninserted\nchanged\ntwo\nthree\n"
        );
    }

    #[tokio::test]
    async fn anchors_track_edits_that_shift_lines() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let (snapshot, mut served) = served_snapshot(&path, "one\ntwo\nthree\n");
        let store = AnchorStore::new();
        let old_third = anchor_token(3, &snapshot.lines[2]);
        let first = anchor_token(1, &snapshot.lines[0]);
        let insert = EditRequest {
            path: "file.txt".into(),
            edits: vec![[first.clone(), first, "inserted".into()]],
            move_lines: None,
        };
        apply_edit(dir.path(), &insert, &mut served, None, &store, 32, 4096)
            .await
            .expect("插入成功");
        let replace = EditRequest {
            path: "file.txt".into(),
            edits: vec![[old_third.clone(), old_third, "shifted".into()]],
            move_lines: None,
        };
        apply_edit(dir.path(), &replace, &mut served, None, &store, 32, 4096)
            .await
            .expect("旧锚点应重定位");
        assert_eq!(
            std::fs::read_to_string(&path).expect("读取结果"),
            "one\ninserted\ntwo\nthree\nshifted\n"
        );
    }

    #[tokio::test]
    async fn reread_expands_the_served_window() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let (snapshot, mut served) = served_snapshot(&path, "one\ntwo\nthree\n");
        served.served.fill(false);
        served.serve(snapshot.epoch, snapshot.lines.len(), &[0..1]);
        let store = AnchorStore::new();
        let from = anchor_token(1, &snapshot.lines[0]);
        let to = anchor_token(3, &snapshot.lines[2]);
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[from.clone(), to.clone(), "rewritten".into()]],
            move_lines: None,
        };
        let error = apply_edit(dir.path(), &request, &mut served, None, &store, 32, 4096)
            .await
            .expect_err("未服务范围必须拒绝");
        assert!(error.to_string().contains("E_UNSERVED_RANGE"));
        served.serve(snapshot.epoch, snapshot.lines.len(), &[0..3]);
        apply_edit(dir.path(), &request, &mut served, None, &store, 32, 4096)
            .await
            .expect("重读后范围可编辑");
    }
}

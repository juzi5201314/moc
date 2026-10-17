//! Hashline 批量编辑解析、验证、搬运与原子提交。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use super::served::{RangeError, read_snapshot, verify_range};
use crate::error::CoreError;
use crate::hashline::{
    AnchorStore, FileSnapshot, ServedFile, anchor_line_token, id_str, parse_anchor,
};
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
    /// 给模型看的 diff 文本（已包含当前锚点）。
    pub output: String,
    /// 给前端看的结构化 diff。
    pub diff: ToolDiff,
    /// 编辑后按位移表重定位并刷新过展示窗口的 served 状态。
    pub served: ServedFile,
    /// 编辑前后内容完全一致（`apply_edit` 直接早退，`preview_edit` 返回空 diff）。
    pub no_changes: bool,
    /// `move` 的目标文件计划。
    pub target: Option<EditPlanTarget>,
}

/// `move` 的目标文件的内容计划。
#[derive(Debug, Clone)]
pub struct EditPlanTarget {
    pub path: PathBuf,
    pub snapshot: FileSnapshot,
    pub lines: Vec<String>,
    pub served: ServedFile,
}

/// 验证全部范围并计算文件最终内容；不触碰磁盘、不更新锚点状态。
///
/// [`apply_edit`]（提交）与 [`preview_edit`]（授权 diff 预览）共用同一份计划，
/// 保证「弹窗里预览的」与「实际落盘的」完全一致。计划同时按位移表重定位
/// served 令牌，并把 diff 展示窗口内的令牌刷新到当前行号。
pub fn plan_edit(
    cwd: &Path,
    request: &EditRequest,
    served: &ServedFile,
    target_served: Option<&ServedFile>,
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

    // `move` 的源删除与目标插入都并入统一位移表：先在原始行号上完成验证，
    // 再与 edits 一起排序、生成一次 moves，避免重基漏掉任一侧的位移。
    let mut target_plan = None;
    let mut all_edits = edits.clone();
    let mut target_moves = Vec::new();
    if let Some(movement) = &request.move_lines {
        let (start, end, warning) =
            verify_range(served, &source_snapshot, &movement.from, &movement.to)
                .map_err(range_tool_error)?;
        if let Some(warning) = warning {
            warnings.push(warning);
        }
        if ranges
            .iter()
            .any(|&(left, right)| start <= right && left <= end)
        {
            return Err(tool_error(
                "E_OVERLAP",
                "the edit batch overlaps the move source range",
            ));
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
        all_edits.push(VerifiedEdit {
            start,
            end: end + 1,
            replacement: Vec::new(),
        });
        target_moves = vec![(
            target_position,
            isize::try_from(moved_lines.len()).expect("moved line count must fit in isize"),
        )];
        let mut target_lines = target_snapshot.lines.clone();
        target_lines.splice(target_position..target_position, moved_lines);
        target_plan = Some((target_path, target_snapshot, target_lines));
    }

    all_edits.sort_by_key(|edit| (edit.start, edit.end));
    let moves = build_moves(&all_edits);
    let mut source_lines = source_snapshot.lines.clone();
    for edit in all_edits.iter().rev() {
        source_lines.splice(edit.start..edit.end, edit.replacement.clone());
    }

    if source_lines.is_empty() && !source_snapshot.lines.is_empty() {
        return Err(tool_error(
            "E_EMPTY_RANGE",
            "the edit would empty the file; use write or explicitly rewrite the complete file",
        ));
    }
    let no_changes = source_lines == source_snapshot.lines && target_plan.is_none();
    if no_changes {
        return Ok(EditPlan {
            path: source_path,
            snapshot: source_snapshot,
            lines: source_lines,
            output: "No changes made.".into(),
            diff: ToolDiff {
                files: Vec::new(),
                truncated: false,
            },
            served: served.clone(),
            no_changes: true,
            target: None,
        });
    }

    let mut source_served = served.rebased(&moves, source_lines.len());
    let mut output = render_diff(
        &source_path,
        &source_snapshot,
        &source_lines,
        served,
        &mut source_served,
    );
    let mut files = Vec::new();
    let mut truncated = push_file_diff(
        &source_path,
        &source_snapshot.lines,
        &source_lines,
        &mut files,
    );
    let target = if let Some((target_path, target_snapshot, target_lines)) = target_plan {
        let before = target_served.expect("move 目标已验证");
        let mut after = before.rebased(&target_moves, target_lines.len());
        let rendered = render_diff(
            &target_path,
            &target_snapshot,
            &target_lines,
            before,
            &mut after,
        );
        output.push('\n');
        output.push_str(&rendered);
        truncated |= push_file_diff(
            &target_path,
            &target_snapshot.lines,
            &target_lines,
            &mut files,
        );
        Some(EditPlanTarget {
            path: target_path,
            snapshot: target_snapshot,
            lines: target_lines,
            served: after,
        })
    } else {
        None
    };
    if !warnings.is_empty() {
        output.push_str(&format!("\nWarning: {}", warnings.join("; ")));
    }
    Ok(EditPlan {
        path: source_path,
        snapshot: source_snapshot,
        lines: source_lines,
        output,
        diff: ToolDiff { files, truncated },
        served: source_served,
        no_changes: false,
        target,
    })
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
    served: &ServedFile,
    target_served: Option<&ServedFile>,
    anchors: &AnchorStore,
    max_edits: usize,
    max_file_bytes: u64,
) -> Result<EditOutput, EditError> {
    let plan = plan_edit(
        cwd,
        request,
        served,
        target_served,
        max_edits,
        max_file_bytes,
    )?;
    commit_plan(plan, anchors, max_file_bytes).await
}

/// 提交一份已通过验证的编辑计划；到 rename 前才重读快照确认没有并发写入。
async fn commit_plan(
    plan: EditPlan,
    anchors: &AnchorStore,
    max_file_bytes: u64,
) -> Result<EditOutput, EditError> {
    if plan.no_changes {
        return Ok(EditOutput {
            content: "No changes made.".into(),
            diff: None,
        });
    }

    let EditPlan {
        path: source_path,
        snapshot: source_snapshot,
        lines: source_lines,
        output,
        diff,
        served: mut source_served,
        target,
        ..
    } = plan;

    let source_bytes = encode_file(&source_snapshot, &source_lines);
    let target_change = match target {
        Some(target) => {
            let bytes = encode_file(&target.snapshot, &target.lines);
            Some((target, bytes))
        }
        None => None,
    };

    let source_temp = stage_file(&source_path, &source_bytes).await?;
    let target_temp = if let Some((target, bytes)) = &target_change {
        match stage_file(&target.path, bytes).await {
            Ok(path) => Some(path),
            Err(error) => {
                let _ = tokio::fs::remove_file(&source_temp).await;
                return Err(error);
            }
        }
    } else {
        None
    };

    let source_backup = if target_change.is_some() {
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
    if let (Some(temporary), Some((target, _))) = (target_temp, target_change.as_ref()) {
        if let Err(error) = ensure_epoch(&target.path, target.snapshot.epoch, max_file_bytes).await
        {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(rollback_source(error, &source_path, source_backup.as_ref()).await);
        }
        if let Err(error) = tokio::fs::rename(&temporary, &target.path).await {
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
    source_served.epoch = Some(source_after.epoch);
    anchors.commit_edit(source_path, source_served);
    if let Some((target, _)) = target_change {
        let after = read_snapshot(&target.path, max_file_bytes).map_err(tool_error_from_io)?;
        anchors.push_undo(
            target.path.clone(),
            target.snapshot.bytes.clone(),
            after.epoch,
        );
        let mut served = target.served;
        served.epoch = Some(after.epoch);
        anchors.commit_edit(target.path, served);
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
    max_edits: usize,
    max_file_bytes: u64,
) -> Result<ToolDiff, EditError> {
    let plan = plan_edit(
        cwd,
        request,
        served,
        target_served,
        max_edits,
        max_file_bytes,
    )?;
    if plan.no_changes {
        return Ok(ToolDiff {
            files: Vec::new(),
            truncated: false,
        });
    }
    Ok(plan.diff)
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
        let message = if served.epoch.is_none() {
            format!(
                "{} has not been read; read it before editing",
                path.display()
            )
        } else {
            format!("{} changed externally after read", path.display())
        };
        return Err(tool_error("E_UNSERVED_RANGE", &message));
    }
    Ok(())
}

fn split_replacement(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n').map(str::to_owned).collect()
}

/// 生成按原行位置升序的位移表。
///
/// 调用方必须先按 `(start, end)` 排序；`rebase` 的二分查找依赖该顺序。
/// 删除区间逐行写入 `isize::MIN`，区间结束位置写入累计位移。
fn build_moves(edits: &[VerifiedEdit]) -> Vec<(usize, isize)> {
    debug_assert!(
        edits
            .windows(2)
            .all(|pair| (pair[0].start, pair[0].end) <= (pair[1].start, pair[1].end)),
        "build_moves 需要已排序且不重叠的编辑"
    );
    let mut moves = Vec::with_capacity(edits.len() * 2);
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

/// 渲染带锚点的 diff 文本。
///
/// 展示窗口内的 after 行会被刷新为当前行号令牌；窗口外已服务行保留旧令牌，
/// 因此旧锚点在位移后仍然可用。被删除行保留展示时的行号与 id（删除后不可编辑）。
fn render_diff(
    path: &Path,
    before: &FileSnapshot,
    after_lines: &[String],
    before_served: &ServedFile,
    after_served: &mut ServedFile,
) -> String {
    let prefix = before
        .lines
        .iter()
        .zip(after_lines)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = before.lines[prefix..]
        .iter()
        .rev()
        .zip(after_lines[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let start = prefix.saturating_sub(2);
    let old_end = before.lines.len() - suffix;
    let new_end = after_lines.len() - suffix;
    let end = (new_end + 2).min(after_lines.len());
    let epoch = after_served.epoch.expect("diff 渲染前必须已有快照纪元");
    let window = start..end;
    let _ = after_served.serve(epoch, after_lines, std::slice::from_ref(&window));

    let mut output = format!("[{}]", path.display());
    for (index, line) in after_lines.iter().enumerate().take(prefix).skip(start) {
        let token = after_served.token(index).expect("展示窗口内必须已登记令牌");
        output.push_str(&format!("\n  {}", anchor_line_token(token, line)));
    }
    for (index, line) in before.lines.iter().enumerate().take(old_end).skip(prefix) {
        let token = before_served.token(index).expect("被删除行必须曾展示");
        output.push_str(&format!("\n- {}{}│{}", index + 1, id_str(token.id), line));
    }
    for (index, line) in after_lines.iter().enumerate().take(new_end).skip(prefix) {
        let token = after_served.token(index).expect("展示窗口内必须已登记令牌");
        output.push_str(&format!("\n+ {}", anchor_line_token(token, line)));
    }
    for (index, line) in after_lines.iter().enumerate().take(end).skip(new_end) {
        let token = after_served.token(index).expect("展示窗口内必须已登记令牌");
        output.push_str(&format!("\n  {}", anchor_line_token(token, line)));
    }
    output
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
        served.serve(snapshot.epoch, &snapshot.lines, &[0..snapshot.lines.len()]);
        (snapshot, served)
    }

    #[tokio::test]
    async fn applies_replacement_and_preserves_crlf_bom() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"\xef\xbb\xbffirst\r\nsecond\r\nthird\r\n").expect("写文件");
        let snapshot = read_snapshot(&path, 4096).expect("读取");
        let mut served = ServedFile::new();
        served.serve(snapshot.epoch, &snapshot.lines, &[0..snapshot.lines.len()]);
        let anchor = served.token(1).expect("第二行令牌").text();
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[anchor.clone(), anchor, "replacement".into()]],
            move_lines: None,
        };
        apply_edit(
            dir.path(),
            &request,
            &served,
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
        let (snapshot, served) = served_snapshot(&path, "one\ntwo\nthree\n");
        let store = AnchorStore::new();
        let first = served.token(0).expect("首行令牌").text();
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[first.clone(), first, "inserted".into()]],
            move_lines: None,
        };
        let output = apply_edit(dir.path(), &request, &served, None, &store, 32, 4096)
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
        let served = store.served(&path).expect("首次编辑已提交 served");
        apply_edit(dir.path(), &request, &served, None, &store, 32, 4096)
            .await
            .expect("无需重读即可继续编辑");
        let _ = snapshot;
        assert_eq!(
            std::fs::read_to_string(&path).expect("读取结果"),
            "one\ninserted\nchanged\ntwo\nthree\n"
        );
    }

    #[tokio::test]
    async fn anchors_track_edits_that_shift_lines() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let text = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
        let (snapshot, served) = served_snapshot(&path, text);
        let store = AnchorStore::new();
        let old_ninth = served.token(8).expect("第九行令牌").text();
        let first = served.token(0).expect("首行令牌").text();
        let insert = EditRequest {
            path: "file.txt".into(),
            edits: vec![[first.clone(), first, "inserted".into()]],
            move_lines: None,
        };
        apply_edit(dir.path(), &insert, &served, None, &store, 32, 4096)
            .await
            .expect("插入成功");
        // 第九行在 diff 展示窗口之外，旧令牌保持有效。
        let served = store.served(&path).expect("插入后 served");
        let delete = EditRequest {
            path: "file.txt".into(),
            edits: vec![[old_ninth.clone(), old_ninth, String::new()]],
            move_lines: None,
        };
        apply_edit(dir.path(), &delete, &served, None, &store, 32, 4096)
            .await
            .expect("旧锚点应重定位");
        assert_eq!(
            std::fs::read_to_string(&path).expect("读取结果"),
            "l1\ninserted\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl10\n"
        );
        let _ = snapshot;
    }

    #[tokio::test]
    async fn reread_expands_the_served_window() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let (snapshot, served) = served_snapshot(&path, "one\ntwo\nthree\n");
        let mut partial = ServedFile::new();
        partial.serve(snapshot.epoch, &snapshot.lines, &[0..1]);
        let store = AnchorStore::new();
        let from = partial.token(0).expect("首行令牌").text();
        let to = anchor_token(3, &snapshot.lines[2]);
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[from.clone(), to.clone(), "rewritten".into()]],
            move_lines: None,
        };
        let error = apply_edit(dir.path(), &request, &partial, None, &store, 32, 4096)
            .await
            .expect_err("未服务范围必须拒绝");
        assert!(error.to_string().contains("E_UNSERVED_RANGE"));
        apply_edit(dir.path(), &request, &served, None, &store, 32, 4096)
            .await
            .expect("重读后范围可编辑");
    }

    #[tokio::test]
    async fn rejects_unserved_and_overlapping_edits_without_mutating_files() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let (snapshot, served) = served_snapshot(&path, "one\ntwo\nthree\n");
        let before = std::fs::read(&path).expect("读取原文件");
        let mut partial = ServedFile::new();
        partial.serve(snapshot.epoch, &snapshot.lines, &[0..1, 2..3]);
        let from = partial.token(0).expect("首行令牌").text();
        let middle = anchor_token(2, &snapshot.lines[1]);
        let denied = EditRequest {
            path: "file.txt".into(),
            edits: vec![[from, middle, "x".into()]],
            move_lines: None,
        };
        assert!(
            apply_edit(
                dir.path(),
                &denied,
                &partial,
                None,
                &AnchorStore::new(),
                32,
                4096
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&path).expect("读取原文件"), before);
        let overlap = EditRequest {
            path: "file.txt".into(),
            edits: vec![
                [
                    served.token(0).expect("首行令牌").text(),
                    served.token(1).expect("第二行令牌").text(),
                    "x".into(),
                ],
                [
                    served.token(1).expect("第二行令牌").text(),
                    served.token(2).expect("第三行令牌").text(),
                    "y".into(),
                ],
            ],
            move_lines: None,
        };
        let error = apply_edit(
            dir.path(),
            &overlap,
            &served,
            None,
            &AnchorStore::new(),
            32,
            4096,
        )
        .await
        .expect_err("重叠范围必须拒绝");
        assert!(error.to_string().contains("E_OVERLAP"));
        let duplicate = EditRequest {
            path: "file.txt".into(),
            edits: vec![
                [
                    served.token(0).expect("首行令牌").text(),
                    served.token(0).expect("首行令牌").text(),
                    "one".into(),
                ],
                [
                    served.token(0).expect("首行令牌").text(),
                    served.token(0).expect("首行令牌").text(),
                    "two".into(),
                ],
            ],
            move_lines: None,
        };
        let error = apply_edit(
            dir.path(),
            &duplicate,
            &served,
            None,
            &AnchorStore::new(),
            32,
            4096,
        )
        .await
        .expect_err("重复锚点必须拒绝");
        assert!(error.to_string().contains("E_OVERLAP"));
        assert_eq!(std::fs::read(&path).expect("读取原文件"), before);
    }

    #[tokio::test]
    async fn preview_matches_applied_content_and_reports_insert_delete_lines() {
        use moc_protocol::DiffLineKind;

        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let (_snapshot, served) = served_snapshot(&path, "alpha\nbeta\ngamma\n");
        let store = AnchorStore::new();
        let anchor = served.token(1).expect("第二行令牌").text();
        let request = EditRequest {
            path: "file.txt".into(),
            edits: vec![[anchor.clone(), anchor, "delta\nbeta".into()]],
            move_lines: None,
        };
        let preview =
            preview_edit(dir.path(), &request, &served, None, 32, 4096).expect("预览成功");
        assert!(!preview.truncated);
        assert_eq!(preview.files.len(), 1);
        let kinds: Vec<_> = preview.files[0]
            .hunks
            .iter()
            .flat_map(|hunk| {
                hunk.lines
                    .iter()
                    .map(|line| (line.kind, line.text.as_str()))
            })
            .collect();
        assert!(kinds.contains(&(DiffLineKind::Insert, "delta")));
        assert!(kinds.contains(&(DiffLineKind::Insert, "beta")));
        assert!(!kinds.iter().any(|(kind, _)| *kind == DiffLineKind::Delete));
        let output = apply_edit(dir.path(), &request, &served, None, &store, 32, 4096)
            .await
            .expect("编辑成功");
        assert_eq!(
            std::fs::read_to_string(&path).expect("读取结果"),
            "alpha\nbeta\ndelta\nbeta\ngamma\n"
        );
        let applied = output.diff.expect("成功编辑携带 diff");
        let inserts: Vec<&str> = applied.files[0]
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .filter(|line| line.kind == DiffLineKind::Insert)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(inserts.len(), 2);
        assert!(inserts.contains(&"delta"));
        assert!(inserts.contains(&"beta"));
    }

    #[tokio::test]
    async fn move_commits_both_files_and_rebases_both_served_tables() {
        let dir = tempfile::tempdir().expect("临时目录");
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        std::fs::write(&source, "a\nb\nc\nd\ne\nf\n").expect("写源文件");
        std::fs::write(&target, "t1\nt2\nt3\nt4\nt5\nt6\nt7\nt8\n").expect("写目标文件");
        let source_snapshot = read_snapshot(&source, 4096).expect("读取源文件");
        let target_snapshot = read_snapshot(&target, 4096).expect("读取目标文件");
        let mut source_served = ServedFile::new();
        source_served.serve(source_snapshot.epoch, &source_snapshot.lines, &[0..2, 5..6]);
        let mut target_served = ServedFile::new();
        target_served.serve(
            target_snapshot.epoch,
            &target_snapshot.lines,
            &[0..target_snapshot.lines.len()],
        );
        let store = AnchorStore::new();
        let first = source_served.token(0).expect("源首行令牌").text();
        let moved = source_served.token(1).expect("源第二行令牌").text();
        let last = source_served.token(5).expect("源末行令牌").text();
        let target_first = target_served.token(0).expect("目标首行令牌").text();
        let seventh = target_served.token(6).expect("目标第七行令牌").text();
        let request = EditRequest {
            path: "source".into(),
            edits: vec![[first.clone(), first, "x".into()]],
            move_lines: Some(MoveRequest {
                from: moved.clone(),
                to: moved,
                target_path: "target".into(),
                target_anchor: Some(target_first),
            }),
        };
        let output = apply_edit(
            dir.path(),
            &request,
            &source_served,
            Some(&target_served),
            &store,
            32,
            4096,
        )
        .await
        .expect("move 成功");
        assert_eq!(
            std::fs::read_to_string(&source).expect("读取源文件"),
            "a\nx\nc\nd\ne\nf\n"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("读取目标文件"),
            "t1\nb\nt2\nt3\nt4\nt5\nt6\nt7\nt8\n"
        );
        assert!(output.content.contains(&format!("[{}]", target.display())));
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("读取目录").count(),
            2,
            "临时文件必须清理"
        );

        let source_served = store.served(&source).expect("提交后的源 served");
        let target_served = store.served(&target).expect("提交后的目标 served");
        let delete_last = EditRequest {
            path: "source".into(),
            edits: vec![[last.clone(), last, String::new()]],
            move_lines: None,
        };
        apply_edit(
            dir.path(),
            &delete_last,
            &source_served,
            None,
            &store,
            32,
            4096,
        )
        .await
        .expect("move 源删除必须进入位移表");
        assert_eq!(
            std::fs::read_to_string(&source).expect("读取源文件"),
            "a\nx\nc\nd\ne\n"
        );
        let delete_seventh = EditRequest {
            path: "target".into(),
            edits: vec![[seventh.clone(), seventh, String::new()]],
            move_lines: None,
        };
        apply_edit(
            dir.path(),
            &delete_seventh,
            &target_served,
            None,
            &store,
            32,
            4096,
        )
        .await
        .expect("move 目标插入必须重基");
        assert_eq!(
            std::fs::read_to_string(&target).expect("读取目标文件"),
            "t1\nb\nt2\nt3\nt4\nt5\nt6\nt8\n"
        );
    }

    /// 白盒核对：乱序编辑排序后的位移表把令牌映射到正确的新位置。
    #[test]
    fn out_of_order_edits_build_a_correct_move_table() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "}\n".repeat(12)).expect("写文件");
        let snapshot = read_snapshot(&path, 4096).expect("读取");
        let mut served = ServedFile::new();
        served.serve(snapshot.epoch, &snapshot.lines, &[1..2, 3..4, 10..11]);
        let old_fourth = served.token(3).expect("第 4 行令牌");
        let mut edits = vec![
            VerifiedEdit {
                start: 10,
                end: 11,
                replacement: Vec::new(),
            },
            VerifiedEdit {
                start: 1,
                end: 2,
                replacement: Vec::new(),
            },
        ];
        edits.sort_by_key(|edit| (edit.start, edit.end));
        let moves = build_moves(&edits);
        let rebased = served.rebased(&moves, 10);
        assert_eq!(rebased.token(2), Some(old_fourth), "第 4 行落到 index 2");
        assert!(rebased.token(1).is_none(), "第 2 行已删除");
        assert!(rebased.token(9).is_none(), "第 11 行已删除");
    }

    #[tokio::test]
    async fn undo_rejects_external_changes_without_consuming_history() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("source");
        let (_snapshot, served) = served_snapshot(&path, "first\nlast\n");
        let store = AnchorStore::new();
        let anchor = served.token(0).expect("首行令牌").text();
        let request = EditRequest {
            path: "source".into(),
            edits: vec![[anchor.clone(), anchor, "added".into()]],
            move_lines: None,
        };
        apply_edit(dir.path(), &request, &served, None, &store, 32, 4096)
            .await
            .expect("编辑成功");
        std::fs::write(&path, "external content").expect("外部改写");
        for _ in 0..2 {
            let error = undo(&path, &store, 4096)
                .await
                .expect_err("外部改写必须拒绝撤销");
            assert!(error.to_string().contains("E_UNDO_STALE"));
            assert_eq!(
                std::fs::read_to_string(&path).expect("读取文件"),
                "external content"
            );
        }
    }

    #[tokio::test]
    async fn commit_rejects_external_writes_that_land_after_planning() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("source");
        let (_snapshot, served) = served_snapshot(&path, "one\ntwo\n");
        let store = AnchorStore::new();
        let anchor = served.token(0).expect("首行令牌").text();
        let request = EditRequest {
            path: "source".into(),
            edits: vec![[anchor.clone(), anchor, "inserted".into()]],
            move_lines: None,
        };
        let plan = plan_edit(dir.path(), &request, &served, None, 32, 4096).expect("计划成功");
        std::fs::write(&path, "external\n").expect("外部改写");
        let error = commit_plan(plan, &store, 4096)
            .await
            .expect_err("计划后外部写入必须拒绝提交");
        assert!(error.to_string().contains("E_STALE_RANGE"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("读取文件"),
            "external\n"
        );
        assert!(store.latest_undo_path().is_none());
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("读取目录").count(),
            1,
            "临时文件与备份必须清理"
        );
    }

    #[tokio::test]
    async fn target_epoch_change_rolls_back_the_committed_source() {
        let dir = tempfile::tempdir().expect("临时目录");
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        std::fs::write(&source, "move\nkeep\n").expect("写源文件");
        std::fs::write(&target, "target\n").expect("写目标文件");
        let source_snapshot = read_snapshot(&source, 4096).expect("读取源文件");
        let target_snapshot = read_snapshot(&target, 4096).expect("读取目标文件");
        let mut source_served = ServedFile::new();
        source_served.serve(
            source_snapshot.epoch,
            &source_snapshot.lines,
            &[0..source_snapshot.lines.len()],
        );
        let mut target_served = ServedFile::new();
        target_served.serve(
            target_snapshot.epoch,
            &target_snapshot.lines,
            &[0..target_snapshot.lines.len()],
        );
        let store = AnchorStore::new();
        let moved = source_served.token(0).expect("源首行令牌").text();
        let keep = source_served.token(1).expect("源第二行令牌").text();
        let request = EditRequest {
            path: "source".into(),
            edits: vec![[keep.clone(), keep, "after".into()]],
            move_lines: Some(MoveRequest {
                from: moved.clone(),
                to: moved,
                target_path: "target".into(),
                target_anchor: None,
            }),
        };
        let plan = plan_edit(
            dir.path(),
            &request,
            &source_served,
            Some(&target_served),
            32,
            4096,
        )
        .expect("计划成功");
        std::fs::write(&target, "external\n").expect("外部改写目标");
        let error = commit_plan(plan, &store, 4096)
            .await
            .expect_err("目标 epoch 变化必须回滚源文件");
        assert!(error.to_string().contains("E_STALE_RANGE"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&source).expect("读取源文件"),
            "move\nkeep\n"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("读取目标文件"),
            "external\n"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("读取目录").count(),
            2,
            "临时文件与备份必须清理"
        );
        assert!(store.latest_undo_path().is_none());
    }
}

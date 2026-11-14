//! Hashline 批量编辑解析、验证、搬运与原子提交。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use super::served::read_snapshot;
use crate::error::CoreError;
use crate::hashline::{
    AnchorStore, FileSnapshot, ServedFile, TagError, numbered_line, parse_tag, tag_text,
};
use moc_protocol::{LineChange, ToolDiff, ToolFileDiff, build_hunks_from_changes};

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
    /// read 或 edit 输出头部 `[path#TAG]` 里的 tag；全部行号都指这个版本。
    pub tag: String,
    #[serde(default)]
    pub edits: Vec<EditOp>,
    #[serde(default, rename = "move")]
    pub move_lines: Option<MoveRequest>,
}

/// 单个编辑操作；行号是 1 基、闭区间，均指请求 tag 对应的版本。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
pub enum EditOp {
    Replace {
        from: usize,
        to: usize,
        text: String,
    },
    Insert {
        after: usize,
        text: String,
    },
    Delete {
        from: usize,
        to: usize,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveRequest {
    pub from: usize,
    pub to: usize,
    pub target_path: String,
    pub target_tag: String,
    /// 目标文件中插入位置之前的行号；`0` 表示开头，省略表示追加到末尾。
    #[serde(default)]
    pub after: Option<usize>,
}

/// 映射到当前坐标后的编辑：半开区间 `start..end`，插入时 `start == end`。
#[derive(Debug, Clone)]
struct VerifiedEdit {
    start: usize,
    end: usize,
    replacement: Vec<String>,
}

/// `move` 的目标文件计划：除了最终内容，还保留插入位置与搬运行，
/// 供结构化 diff 直接按「一次纯插入」构造。
struct MoveTargetPlan {
    path: PathBuf,
    snapshot: FileSnapshot,
    lines: Vec<String>,
    insert_at: usize,
    moved_lines: Vec<String>,
    served: ServedFile,
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
    /// 给模型看的 diff 文本（头部带新 tag）。
    pub output: String,
    /// 给前端看的结构化 diff。
    pub diff: ToolDiff,
    /// 编辑后压入新版本、并登记了 diff 展示窗口的 served 状态。
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

/// 验证全部范围并计算文件最终内容；不触碰磁盘、不更新 served 状态。
///
/// [`apply_edit`]（提交）与 [`preview_edit`]（授权 diff 预览）共用同一份计划，
/// 保证「弹窗里预览的」与「实际落盘的」完全一致。请求里的行号先沿版本链
/// 映射到当前坐标，再统一排序、生成位移表。
pub fn plan_edit(
    cwd: &Path,
    request: &EditRequest,
    served: &ServedFile,
    target_served: Option<&ServedFile>,
    max_edits: usize,
    max_file_bytes: u64,
) -> Result<EditPlan, EditError> {
    if (request.edits.is_empty() && request.move_lines.is_none()) || request.edits.len() > max_edits
    {
        return Err(tool_error(
            "E_BAD_PAYLOAD",
            "edits must not exceed the configured limit; provide edits, move, or both",
        ));
    }
    let tag = parse_tag(&request.tag).ok_or_else(bad_tag)?;
    let source_path = resolve_path(cwd, &request.path)?;
    let source_snapshot =
        read_snapshot(&source_path, max_file_bytes).map_err(tool_error_from_io)?;
    if served.epoch != Some(source_snapshot.epoch) {
        let message = if served.epoch.is_none() {
            format!(
                "{} has no editable snapshot available; read it to establish a current snapshot",
                source_path.display()
            )
        } else {
            format!(
                "{} changed outside this session since it was read",
                source_path.display()
            )
        };
        return Err(tool_error("E_STALE_TAG", &message));
    }
    let version = resolve_tag(served, tag, &source_path)?;

    let mut edits: Vec<VerifiedEdit> = Vec::with_capacity(request.edits.len() + 1);
    for (index, op) in request.edits.iter().enumerate() {
        let field = Some(index);
        let edit = match op {
            EditOp::Replace { from, to, text } => {
                let (start, end) = map_range(served, version, *from, *to, tag, field)?;
                VerifiedEdit {
                    start,
                    end,
                    replacement: split_text(text),
                }
            }
            EditOp::Delete { from, to } => {
                let (start, end) = map_range(served, version, *from, *to, tag, field)?;
                VerifiedEdit {
                    start,
                    end,
                    replacement: Vec::new(),
                }
            }
            EditOp::Insert { after, text } => {
                let gap = map_gap(served, version, *after, tag, field)?;
                VerifiedEdit {
                    start: gap,
                    end: gap,
                    replacement: split_text(text),
                }
            }
        };
        if let Some(other) = edits.iter().position(|existing| overlaps(existing, &edit)) {
            return Err(tool_error(
                "E_OVERLAP",
                &format!("edits[{index}] overlaps edits[{other}]"),
            ));
        }
        edits.push(edit);
    }

    // `move` 的源删除与目标插入都并入统一位移表：先映射到当前坐标完成验证，
    // 再与 edits 一起排序、生成一次 moves，避免版本链漏掉任一侧的位移。
    let mut target_plan: Option<MoveTargetPlan> = None;
    if let Some(movement) = &request.move_lines {
        let (start, end) = map_range(served, version, movement.from, movement.to, tag, None)?;
        let source_range = VerifiedEdit {
            start,
            end,
            replacement: Vec::new(),
        };
        if edits.iter().any(|edit| overlaps(edit, &source_range)) {
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
        let target_tag = parse_tag(&movement.target_tag).ok_or_else(bad_tag)?;
        let target_served = target_served
            .filter(|record| record.epoch.is_some())
            .ok_or_else(|| {
                tool_error(
                    "E_STALE_TAG",
                    "target file has not been read; read it before moving lines",
                )
            })?;
        let target_snapshot =
            read_snapshot(&target_path, max_file_bytes).map_err(tool_error_from_io)?;
        if target_served.epoch != Some(target_snapshot.epoch) {
            return Err(tool_error(
                "E_STALE_TAG",
                "target changed outside this session since it was read",
            ));
        }
        let target_version = resolve_tag(target_served, target_tag, &target_path)?;
        let insert_at = match movement.after {
            None => target_snapshot.lines.len(),
            Some(after) => map_gap(target_served, target_version, after, target_tag, None)?,
        };
        let moved_lines = source_snapshot.lines[start..end].to_vec();
        edits.push(source_range);
        let target_moves = [(
            insert_at,
            isize::try_from(moved_lines.len()).expect("moved line count must fit in isize"),
        )];
        let mut target_lines = target_snapshot.lines.clone();
        target_lines.splice(insert_at..insert_at, moved_lines.iter().cloned());
        let target_after = target_served.advanced(&target_moves, &target_lines);
        target_plan = Some(MoveTargetPlan {
            path: target_path,
            snapshot: target_snapshot,
            lines: target_lines,
            insert_at,
            moved_lines,
            served: target_after,
        });
    }

    // 稳定排序：同一插入点上的多个插入保持请求顺序，并排在从该点开始的替换之前；
    // 逆序 splice 后插入内容按请求顺序出现在替换内容之前。
    edits.sort_by_key(|edit| (edit.start, edit.end));
    let moves = build_moves(&edits);
    let mut source_lines = source_snapshot.lines.clone();
    for edit in edits.iter().rev() {
        source_lines.splice(edit.start..edit.end, edit.replacement.iter().cloned());
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

    let mut source_served = served.advanced(&moves, &source_lines);
    let mut output = render_diff(
        &source_path,
        &source_snapshot,
        &source_lines,
        &mut source_served,
    );
    // edit 的变更范围在规划阶段已经确定：直接按范围构造完整 hunk，
    // 不再对整份文件做 LCS，因此文件再大也不会把少量改动误判成截断。
    let source_refs: Vec<&str> = source_snapshot.lines.iter().map(String::as_str).collect();
    let source_changes: Vec<LineChange<'_>> = edits
        .iter()
        .map(|edit| LineChange {
            start: edit.start,
            end: edit.end,
            replacement: &edit.replacement,
        })
        .collect();
    let mut files = vec![ToolFileDiff {
        path: source_path.display().to_string(),
        hunks: build_hunks_from_changes(&source_refs, &source_changes),
    }];
    let target = if let Some(mut target_plan) = target_plan {
        let rendered = render_diff(
            &target_plan.path,
            &target_plan.snapshot,
            &target_plan.lines,
            &mut target_plan.served,
        );
        output.push('\n');
        output.push_str(&rendered);
        let target_refs: Vec<&str> = target_plan
            .snapshot
            .lines
            .iter()
            .map(String::as_str)
            .collect();
        let change = LineChange {
            start: target_plan.insert_at,
            end: target_plan.insert_at,
            replacement: &target_plan.moved_lines,
        };
        files.push(ToolFileDiff {
            path: target_plan.path.display().to_string(),
            hunks: build_hunks_from_changes(&target_refs, &[change]),
        });
        Some(EditPlanTarget {
            path: target_plan.path,
            snapshot: target_plan.snapshot,
            lines: target_plan.lines,
            served: target_plan.served,
        })
    } else {
        None
    };
    Ok(EditPlan {
        path: source_path,
        snapshot: source_snapshot,
        lines: source_lines,
        output,
        diff: ToolDiff {
            files,
            truncated: false,
        },
        served: source_served,
        no_changes: false,
        target,
    })
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

    // 两个文件都必须先通过大小校验，避免提交后读取快照失败而丢失撤销记录。
    for bytes in std::iter::once(source_bytes.as_slice())
        .chain(target_change.as_ref().map(|(_, bytes)| bytes.as_slice()))
    {
        if bytes.len() as u64 > max_file_bytes {
            return Err(tool_error(
                "E_TOO_LARGE",
                "edited file exceeds the per-file byte limit",
            ));
        }
    }
    let source_temp = stage_file(&source_path, &source_bytes).await?;
    let target_temp = if let Some((target, bytes)) = &target_change {
        match stage_file(&target.path, bytes).await {
            Ok(path) => Some(path),
            Err(error) => {
                let _ = tokio::fs::remove_file(&source_temp).await;
                return Err(error.into());
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
                return Err(error.into());
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

/// 授权预览：计算 edit 将产生的结构化 diff，不落盘、不更新 served 状态。
///
/// 与 [`apply_edit`] 共用 [`plan_edit`]，预览通过即代表实际执行可用；
/// 返回 [`EditError`]（如 E_STALE_TAG）时调用方应直接放弃预览，
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

fn bad_tag() -> EditError {
    tool_error(
        "E_BAD_PAYLOAD",
        "tag must be the 4-hex value from the [path#TAG] header of read or edit output",
    )
}

fn resolve_tag(served: &ServedFile, tag: u16, path: &Path) -> Result<usize, EditError> {
    served.resolve(tag).map_err(|error| {
        let tag = tag_text(tag);
        let message = match error {
            TagError::Unknown => format!("tag {tag} is not a known version of {}", path.display()),
            TagError::Ambiguous => {
                format!("tag {tag} matches several versions of {}", path.display())
            }
        };
        tool_error("E_STALE_TAG", &message)
    })
}

/// 错误消息里的字段名：`edits[i]` 或 `move`。
fn field_name(index: Option<usize>) -> String {
    index.map_or_else(|| "move".to_owned(), |index| format!("edits[{index}]"))
}

fn stale_line(index: Option<usize>, line: usize, tag: u16) -> EditError {
    tool_error(
        "E_STALE_LINE",
        &format!(
            "{}: line {line} of tag {} was changed by a later edit",
            field_name(index),
            tag_text(tag)
        ),
    )
}

fn unseen_line(index: Option<usize>, line: usize) -> EditError {
    tool_error(
        "E_UNSEEN",
        &format!(
            "{}: line {line} has not been shown; read it first",
            field_name(index)
        ),
    )
}

/// 把 tag 版本中的 1 基闭区间 `from..=to` 映射为当前版本的半开区间。
///
/// 区间内每一行都必须仍然存在、在当前版本里保持连续，并且已展示给模型。
fn map_range(
    served: &ServedFile,
    version: usize,
    from: usize,
    to: usize,
    tag: u16,
    index: Option<usize>,
) -> Result<(usize, usize), EditError> {
    let len = served.version_len(version);
    if from == 0 || from > to || to > len {
        return Err(tool_error(
            "E_BAD_PAYLOAD",
            &format!(
                "{}: lines {from}-{to} are outside 1-{len} or reversed",
                field_name(index)
            ),
        ));
    }
    if to - from >= 100_000 {
        return Err(tool_error(
            "E_RANGE_TOO_LARGE",
            &format!(
                "{}: one range may contain at most 100000 lines",
                field_name(index)
            ),
        ));
    }
    let mut first = None;
    let mut last: Option<usize> = None;
    for line in from..=to {
        let position = served
            .map_line(version, line - 1)
            .filter(|position| last.is_none_or(|previous| *position == previous + 1))
            .ok_or_else(|| stale_line(index, line, tag))?;
        if !served.is_seen(position) {
            return Err(unseen_line(index, line));
        }
        first.get_or_insert(position);
        last = Some(position);
    }
    let first = first.expect("非空区间至少映射一行");
    let last = last.expect("非空区间至少映射一行");
    Ok((first, last + 1))
}

/// 把「在 tag 版本第 `after` 行之后插入」映射为当前版本的插入点。
fn map_gap(
    served: &ServedFile,
    version: usize,
    after: usize,
    tag: u16,
    index: Option<usize>,
) -> Result<usize, EditError> {
    let len = served.version_len(version);
    if after > len {
        return Err(tool_error(
            "E_BAD_PAYLOAD",
            &format!("{}: after {after} is outside 0-{len}", field_name(index)),
        ));
    }
    if after == 0 {
        return Ok(0);
    }
    let position = served
        .map_line(version, after - 1)
        .ok_or_else(|| stale_line(index, after, tag))?;
    if !served.is_seen(position) {
        return Err(unseen_line(index, after));
    }
    Ok(position + 1)
}

/// 两个编辑是否冲突：非空区间相交，或插入点落在另一区间内部；同一插入点可共存。
fn overlaps(left: &VerifiedEdit, right: &VerifiedEdit) -> bool {
    match (left.start == left.end, right.start == right.end) {
        (true, true) => false,
        (true, false) => right.start < left.start && left.start < right.end,
        (false, true) => left.start < right.start && right.start < left.end,
        (false, false) => left.start < right.end && right.start < left.end,
    }
}

/// 把 `text` 拆成行：容忍 CRLF，去掉恰好一个末尾换行；空串得到一个空行。
fn split_text(text: &str) -> Vec<String> {
    let text = text
        .strip_suffix('\n')
        .map_or(text, |rest| rest.strip_suffix('\r').unwrap_or(rest));
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect()
}

/// 生成按原行位置升序的位移表。
///
/// 调用方必须先按 `(start, end)` 排序；`shifted_position` 的二分查找依赖该顺序。
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

pub(crate) fn encode_file(snapshot: &FileSnapshot, lines: &[String]) -> Vec<u8> {
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
/// 失败错误固定为 `E_STALE_TAG`：调用方保留磁盘上的外部内容，不覆盖。
async fn ensure_epoch(
    path: &Path,
    expected: super::SnapshotId,
    max_file_bytes: u64,
) -> Result<(), EditError> {
    let stale = || {
        tool_error(
            "E_STALE_TAG",
            &format!(
                "{} changed externally while the edit was being committed; use the current tag",
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

/// write、edit 与 undo 共用的暂存写入；已有文件保留权限，写入失败清理临时文件。
pub(crate) async fn stage_file(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let permissions = match tokio::fs::metadata(path).await {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(".moc-{}.tmp", crate::ids::entry_id()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // 已有文件的暂存副本从创建时就不向其他用户开放；随后恢复目标权限。
    #[cfg(unix)]
    if permissions.is_some() {
        options.mode(0o600);
    }
    let mut file = options.open(&temp).await?;
    let result = async {
        if let Some(permissions) = permissions {
            file.set_permissions(permissions).await?;
        }
        file.write_all(bytes).await?;
        file.sync_all().await
    }
    .await;
    if let Err(error) = result {
        drop(file);
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error);
    }
    Ok(temp)
}

async fn cleanup_staged(first: PathBuf, second: Option<PathBuf>) {
    let _ = tokio::fs::remove_file(first).await;
    if let Some(second) = second {
        let _ = tokio::fs::remove_file(second).await;
    }
}

/// 渲染带新 tag 头部与当前行号的 diff 文本，并把展示窗口登记为已展示。
///
/// 被删除的行只写原文、不带行号：它们已不可寻址。
fn render_diff(
    path: &Path,
    before: &FileSnapshot,
    after_lines: &[String],
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
    let tag = after_served.serve(epoch, after_lines, std::slice::from_ref(&window));

    let mut output = format!("[{}#{}]", path.display(), tag_text(tag));
    for (index, line) in after_lines.iter().enumerate().take(prefix).skip(start) {
        let _ = write!(output, "\n  {}", numbered_line(index + 1, line));
    }
    for line in &before.lines[prefix..old_end] {
        let _ = write!(output, "\n- {line}");
    }
    for (index, line) in after_lines.iter().enumerate().take(new_end).skip(prefix) {
        let _ = write!(output, "\n+ {}", numbered_line(index + 1, line));
    }
    for (index, line) in after_lines.iter().enumerate().take(end).skip(new_end) {
        let _ = write!(output, "\n  {}", numbered_line(index + 1, line));
    }
    output
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
mod tests;

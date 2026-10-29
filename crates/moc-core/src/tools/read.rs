//! read 工具：展示目录和文本文件，给完整显示出来的行分配 hashline 锚点。

use std::fs;
use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::hashline::{anchor_line_token, served::read_snapshot};
use crate::resources::{ResourceBacking, ResourceRouter};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read a text file, directory, or session resource URI; returns hashline anchors for edit and plan edits."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "read",
            schema(
                serde_json::json!({
                    "path": {"type":"string","description":"File, directory, or session-internal URI"},
                    "offset": {"type":"integer","minimum":1,"description":"1-based starting line"},
                    "limit": {"type":"integer","minimum":1,"description":"Number of lines to read"}
                }),
                &["path"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: ReadArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let (path, display) = match ResourceRouter::resolve_read(ctx, &args.path)
                .map_err(|error| ToolError::new(error.code, error.message))?
            {
                Some(resource) => match resource.backing {
                    ResourceBacking::File(path) => (path, resource.uri),
                    ResourceBacking::Namespace { listing, .. } => {
                        return Ok(ToolOutcome::success(render_text_resource(
                            &resource.uri,
                            &listing,
                            args.offset,
                            args.limit.unwrap_or(ctx.config.tools.read.default_limit),
                        )));
                    }
                },
                None => (resolve_path(&ctx.cwd, &args.path)?, args.path.clone()),
            };
            if path.is_dir() {
                return Ok(ToolOutcome::success(list_directory(&path, &ctx.cwd)?));
            }
            let max_bytes = ctx.config.tools.output.max_file_bytes;
            let snapshot =
                read_snapshot(&path, max_bytes).map_err(|error| map_read_error(&display, error))?;
            let offset = args.offset.unwrap_or(1).max(1);
            let limit = args
                .limit
                .unwrap_or(ctx.config.tools.read.default_limit)
                .max(1);
            let start = offset.saturating_sub(1).min(snapshot.lines.len());
            let end = start.saturating_add(limit).min(snapshot.lines.len());
            let mut output = format!("[{display}]\n");
            let mut notices = String::new();
            if snapshot.bytes.len() > 4 * 1024 * 1024 {
                notices.push_str(
                    "\n[File exceeds the 4 MiB snapshot limit; anchored edits are unavailable; use bash or write.]",
                );
            }
            if std::str::from_utf8(&snapshot.bytes).is_err() {
                notices.push_str(
                    "\n[Invalid UTF-8 shown as U+FFFD; editing rewrites the file as UTF-8]",
                );
            }
            let total = snapshot.lines.len();
            let footer_budget = page_footer(total, total.saturating_sub(1), total).len();
            let page_budget = ctx
                .config
                .tools
                .read
                .max_bytes
                .min(super::output::budget(&ctx.config.tools.output))
                .saturating_sub(output.len() + notices.len() + footer_budget);
            let max_line_bytes = ctx
                .config
                .tools
                .read
                .line_max_bytes
                .min(page_budget.saturating_sub(7));
            let anchored = snapshot.bytes.len() <= 4 * 1024 * 1024;
            let mut selected = Vec::new();
            let mut modes = Vec::new();
            let mut rendered_bytes = 0usize;
            let mut has_uneditable_shown = false;
            for (offset, line) in snapshot.lines[start..end].iter().enumerate() {
                let position = start + offset;
                let (mode, rendered_len) = if line.len() > max_line_bytes {
                    let placeholder = placeholder_line(position + 1, line.len(), anchored);
                    let mode = if anchored {
                        LineMode::AnchorPlaceholder
                    } else {
                        LineMode::PlainPlaceholder
                    };
                    (mode, placeholder.len())
                } else if anchored {
                    let digits = digits_of(position + 1);
                    (LineMode::Anchor, digits + 3 + line.len())
                } else {
                    (LineMode::Plain, line.len())
                };
                if rendered_bytes.saturating_add(rendered_len + 1) > page_budget {
                    break;
                }
                rendered_bytes += rendered_len + 1;
                has_uneditable_shown |= mode == LineMode::AnchorPlaceholder;
                selected.push(position);
                modes.push(mode);
            }

            let mut windows = Vec::new();
            let mut window_start = None;
            let mut last_served = None;
            for (position, mode) in selected.iter().zip(&modes) {
                if *mode == LineMode::Anchor {
                    if window_start.is_none() {
                        window_start = Some(*position);
                    }
                    last_served = Some(position + 1);
                } else if let Some(start_window) = window_start.take() {
                    windows.push(start_window..last_served.expect("显示窗口一定有结束行"));
                    last_served = None;
                }
            }
            if let Some(start_window) = window_start {
                windows.push(start_window..last_served.expect("显示窗口一定有结束行"));
            }

            let tokens = if anchored {
                ctx.anchors.record_served(path.clone(), &snapshot, &windows)
            } else {
                Vec::new()
            };
            let mut token_iter = tokens.into_iter();
            let mut rendered = Vec::with_capacity(selected.len());
            for (position, mode) in selected.iter().copied().zip(modes.iter().copied()) {
                let line = &snapshot.lines[position];
                let text = match mode {
                    LineMode::Anchor => {
                        let (token_position, token) =
                            token_iter.next().expect("锚点行一定已登记令牌");
                        debug_assert_eq!(token_position, position);
                        anchor_line_token(token, line)
                    }
                    LineMode::AnchorPlaceholder => placeholder_line(position + 1, line.len(), true),
                    LineMode::Plain => line.clone(),
                    LineMode::PlainPlaceholder => placeholder_line(position + 1, line.len(), false),
                };
                rendered.push(text);
            }
            if rendered.is_empty() && snapshot.lines.is_empty() {
                output.push_str("(empty file)");
            } else {
                output.push_str(&rendered.join("\n"));
            }
            let shown_end = start + rendered.len();
            output.push_str(&page_footer(start + 1, shown_end, total));
            output.push_str(&notices);
            if has_uneditable_shown {
                output.push_str("\nLines shown as a size notice instead of an anchor are not editable; inspect them with bash or rewrite the file with write.");
            }
            if !anchored {
                ctx.anchors.invalidate(&path);
            }
            Ok(ToolOutcome::success(output))
        })
    }
}

/// 单行渲染模式；只有 `Anchor` 行会被登记为可编辑。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineMode {
    Plain,
    Anchor,
    AnchorPlaceholder,
    PlainPlaceholder,
}

fn digits_of(value: usize) -> usize {
    value.checked_ilog10().map_or(1, |digits| {
        usize::try_from(digits).expect("位数一定放得进 usize") + 1
    })
}

/// 超长行的占位文本；可编辑模式带 `N│` 前缀，其它模式不带任何像锚点的前缀。
fn placeholder_line(line_no: usize, bytes: usize, anchored: bool) -> String {
    let notice = format!(
        "[Line {line_no} is {} KB and exceeds the limit; content omitted; use bash to inspect it]",
        bytes.div_ceil(1024)
    );
    if anchored {
        format!("  {line_no}│{notice}")
    } else {
        notice
    }
}

fn render_text_resource(uri: &str, text: &str, offset: Option<usize>, limit: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = offset
        .unwrap_or(1)
        .max(1)
        .saturating_sub(1)
        .min(lines.len());
    let end = start.saturating_add(limit.max(1)).min(lines.len());
    let mut output = format!("[{uri}]\n");
    if lines.is_empty() {
        output.push_str("(empty resource)");
    } else {
        output.push_str(&lines[start..end].join("\n"));
        output.push_str(&page_footer(start + 1, end, lines.len()));
    }
    output
}

fn page_footer(start: usize, end: usize, total: usize) -> String {
    if end < total {
        format!(
            "\n[Showing lines {start}-{end} of {total}. Use offset={} to continue.]",
            end + 1
        )
    } else {
        String::new()
    }
}

fn resolve_path(cwd: &Path, value: &str) -> Result<PathBuf, ToolError> {
    let path = Path::new(value);
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    fs::canonicalize(&joined).map_err(|error| map_read_error(value, error))
}

fn map_read_error(path: &str, error: std::io::Error) -> ToolError {
    let text = error.to_string();
    let code = if text.contains("E_BINARY") {
        "E_BINARY"
    } else if text.contains("E_TOO_LARGE") {
        "E_TOO_LARGE"
    } else if error.kind() == std::io::ErrorKind::NotFound {
        "E_NOT_FOUND"
    } else {
        "E_ACCESS"
    };
    ToolError::new(code, format!("{path}: {text}"))
}

fn list_directory(path: &Path, cwd: &Path) -> Result<String, ToolError> {
    let relative = path.strip_prefix(cwd).unwrap_or(path);
    let mut output = format!("[{}]", relative.display());
    append_directory(path, 0, &mut output)?;
    Ok(output)
}

fn append_directory(path: &Path, depth: usize, output: &mut String) -> Result<(), ToolError> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(path).map_err(|error| map_read_error(&path.display().to_string(), error))?
    {
        let entry = entry.map_err(|error| map_read_error(&path.display().to_string(), error))?;
        let metadata = entry
            .metadata()
            .map_err(|error| map_read_error(&entry.path().display().to_string(), error))?;
        let modified = metadata
            .modified()
            .map_err(|error| map_read_error(&entry.path().display().to_string(), error))?;
        entries.push((
            modified,
            entry.file_name(),
            metadata.len(),
            metadata.is_dir(),
            entry.path(),
        ));
    }
    entries.sort_unstable_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let indent = "  ".repeat(depth);
    if entries.is_empty() {
        output.push_str(&format!("\n{indent}(empty directory)"));
    }
    for (modified, name, size, directory, child) in entries.iter().take(12) {
        let age = modified.elapsed().unwrap_or_default().as_secs();
        output.push_str(&format!(
            "\n{indent}{}{}  {size} B  {age}s ago",
            name.to_string_lossy(),
            if *directory { "/" } else { "" }
        ));
        if *directory && depth == 0 {
            append_directory(child, depth + 1, output)?;
        }
    }
    if entries.len() > 12 {
        output.push_str(&format!(
            "\n{indent}[… {} more; read the subdirectory to continue]",
            entries.len() - 12
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_are_sorted_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..14 {
            fs::write(dir.path().join(format!("{index:02}.txt")), "x").unwrap();
            let file = fs::File::options()
                .write(true)
                .open(dir.path().join(format!("{index:02}.txt")))
                .unwrap();
            file.set_times(
                fs::FileTimes::new().set_modified(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(index + 1),
                ),
            )
            .unwrap();
        }
        let output = list_directory(dir.path(), dir.path()).unwrap();
        assert!(output.contains("13.txt"));
        assert!(!output.contains("00.txt"));
        assert!(output.contains("2 more"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_preview_has_two_levels_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("child/deeper")).unwrap();
        fs::write(dir.path().join("child/file.txt"), "x").unwrap();
        fs::write(dir.path().join("child/deeper/hidden.txt"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        let output = list_directory(dir.path(), dir.path()).unwrap();
        assert!(output.contains("  file.txt"));
        assert!(!output.contains("hidden.txt"));
    }

    #[test]
    fn read_path_rejects_binary_and_nonexistent_files() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("binary");
        fs::write(&binary, [0, 1, 2]).unwrap();
        assert_eq!(
            map_read_error("binary", read_snapshot(&binary, 1024).unwrap_err()).code,
            "E_BINARY"
        );
        assert_eq!(
            resolve_path(dir.path(), "missing").unwrap_err().code,
            "E_NOT_FOUND"
        );
    }
}

//! read 工具：展示目录和文本文件；文件头部带快照 tag，完整显示的行登记为可编辑。

use std::fs;
use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::output::ToolOutput;
use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::hashline::{numbered_line, served::read_snapshot, tag_text};
use crate::resources::{ResourceBacking, ResourceRouter};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    offset: Option<i64>,
    limit: Option<usize>,
}

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read a text file, directory, or session resource URI; file output starts with [path#TAG] and numbers each line as N│text for edit and plan edits. With an explicit limit, a directory lists one level at a time and offset paginates."
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
                    "offset": {"type":"integer","description":"1-based starting line; negative counts from the end (-50 = last 50 lines)"},
                    "limit": {"type":"integer","minimum":1,"description":"Number of lines to read (default 300); oversized pages are truncated by a byte budget"}
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
                            args.limit,
                            ctx.config.tools.read.default_limit,
                        )));
                    }
                },
                None => (resolve_path(&ctx.cwd, &args.path)?, args.path.clone()),
            };
            if path.is_dir() {
                return Ok(ToolOutcome::success(list_directory(
                    &path,
                    &ctx.cwd,
                    args.offset,
                    args.limit,
                )?));
            }
            let max_bytes = ctx.config.tools.output.max_file_bytes;
            let snapshot =
                read_snapshot(&path, max_bytes).map_err(|error| map_read_error(&display, error))?;
            let limit = args
                .limit
                .unwrap_or(ctx.config.tools.read.default_limit)
                .max(1);
            let start = page_start(args.offset, snapshot.lines.len());
            let end = start.saturating_add(limit).min(snapshot.lines.len());
            let anchored = snapshot.bytes.len() <= 4 * 1024 * 1024;
            // tag 固定 4 个字符：先按最终头部长度（含换行）预留预算，登记后再写入头部。
            let header_len = display.len() + if anchored { 8 } else { 3 };
            let mut notices = String::new();
            if !anchored {
                notices.push_str(
                    "\n[File exceeds the 4 MiB snapshot limit; line-number edits are unavailable; use bash or write.]",
                );
            }
            if std::str::from_utf8(&snapshot.bytes).is_err() {
                notices.push_str(
                    "\n[Invalid UTF-8 shown as U+FFFD; editing rewrites the file as UTF-8]",
                );
            }
            let total = snapshot.lines.len();
            // 截断文案最长：limit 取 usize::MAX 时数字位数最多，足作预留上界。
            let footer_budget = page_footer(
                total,
                total.saturating_sub(1),
                total,
                Some(usize::MAX),
                usize::MAX,
            )
            .len();
            let page_budget = page_byte_budget(
                limit,
                super::output::budget(&ctx.config.tools.output),
                ctx.config.tools.read.max_bytes,
            )
            .saturating_sub(header_len + notices.len() + footer_budget);
            let max_line_bytes = ctx
                .config
                .tools
                .read
                .line_max_bytes
                .min(page_budget.saturating_sub(7));
            let mut selected = Vec::new();
            let mut modes = Vec::new();
            let mut rendered_bytes = 0usize;
            let mut has_uneditable_shown = false;
            for (offset, line) in snapshot.lines[start..end].iter().enumerate() {
                let position = start + offset;
                let (mode, rendered_len) = if line.len() > max_line_bytes {
                    let placeholder = placeholder_line(position + 1, line.len());
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

            let mut output = if anchored {
                let tag = ctx.anchors.record_served(path.clone(), &snapshot, &windows);
                format!("[{display}#{}]\n", tag_text(tag))
            } else {
                format!("[{display}]\n")
            };
            debug_assert_eq!(output.len(), header_len);
            let mut rendered = Vec::with_capacity(selected.len());
            for (position, mode) in selected.iter().copied().zip(modes.iter().copied()) {
                let line = &snapshot.lines[position];
                let text = match mode {
                    LineMode::Anchor => numbered_line(position + 1, line),
                    LineMode::AnchorPlaceholder | LineMode::PlainPlaceholder => {
                        placeholder_line(position + 1, line.len())
                    }
                    LineMode::Plain => line.clone(),
                };
                rendered.push(text);
            }
            if rendered.is_empty() && snapshot.lines.is_empty() {
                output.push_str("(empty file)");
            } else {
                output.push_str(&rendered.join("\n"));
            }
            let shown_end = start + rendered.len();
            output.push_str(&page_footer(start + 1, shown_end, total, args.limit, limit));
            output.push_str(&notices);
            if has_uneditable_shown {
                output.push_str("\nLines shown as a size notice instead of N│text are not editable; inspect them with bash or rewrite the file with write.");
            }
            if !anchored {
                ctx.anchors.invalidate(&path);
            }
            Ok(ToolOutcome::success(ToolOutput::Verbatim(output)))
        })
    }
}

/// 页预算随 `limit` 缩放时每行预留的字节数；远大于普通代码行宽，使 limit 近似按字面送达。
const BYTES_PER_REQUESTED_LINE: usize = 512;

/// 页字节预算随请求行数缩放：小窗口保底共享输出预算（避免单行都装不下的退化页），
/// 大窗口上限 tools.read.max_bytes；输出以 Verbatim 豁免共享预算的归档预览。
fn page_byte_budget(limit: usize, output_budget: usize, max_bytes: usize) -> usize {
    limit
        .saturating_mul(BYTES_PER_REQUESTED_LINE)
        .max(output_budget)
        .min(max_bytes)
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

/// 超长行的占位文本；不带 `N│` 前缀，避免被当成可编辑行。
fn placeholder_line(line_no: usize, bytes: usize) -> String {
    format!(
        "[Line {line_no} is {} KB and exceeds the limit; content omitted; use bash to inspect it]",
        bytes.div_ceil(1024)
    )
}

fn render_text_resource(
    uri: &str,
    text: &str,
    offset: Option<i64>,
    limit: Option<usize>,
    default_limit: usize,
) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let effective = limit.unwrap_or(default_limit).max(1);
    let start = page_start(offset, lines.len());
    let end = start.saturating_add(effective).min(lines.len());
    let mut output = format!("[{uri}]\n");
    if lines.is_empty() {
        output.push_str("(empty resource)");
    } else {
        output.push_str(&lines[start..end].join("\n"));
        output.push_str(&page_footer(start + 1, end, lines.len(), limit, effective));
    }
    output
}

/// 页脚只在工具主动截断时出现：显式 limit 的行数全部送达视为未截断，保持静默；
/// 默认页大小截断沿用普通续读提示，字节预算砍在请求行数之前则点名原因。
fn page_footer(
    start: usize,
    end: usize,
    total: usize,
    explicit: Option<usize>,
    limit: usize,
) -> String {
    if end >= total {
        return String::new();
    }
    let served = end + 1 - start;
    if explicit.is_some_and(|n| served >= n) {
        return String::new();
    }
    if served < limit {
        format!(
            "\n[Showing lines {start}-{end} of {total}; byte budget truncated the {limit}-line page. Use offset={} to continue.]",
            end + 1
        )
    } else {
        format!(
            "\n[Showing lines {start}-{end} of {total}. Use offset={} to continue.]",
            end + 1
        )
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

/// 1-based 页起点换算成 0-based 索引：0 视为 1，负数从末尾数（-50 = 最后 50 行）。
fn page_start(offset: Option<i64>, total: usize) -> usize {
    match offset.unwrap_or(1) {
        n if n < 0 => total.saturating_sub(n.unsigned_abs() as usize),
        n => (n.max(1) as usize - 1).min(total),
    }
}

/// 目录项：mtime 是排序键，其余字段供渲染与递归预览。
struct DirEntry {
    modified: std::time::SystemTime,
    name: std::ffi::OsString,
    size: u64,
    directory: bool,
    path: PathBuf,
}

/// 目录项按现有排序（mtime 新→旧，同刻按名字）收集。
fn directory_entries(path: &Path) -> Result<Vec<DirEntry>, ToolError> {
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
        entries.push(DirEntry {
            modified,
            name: entry.file_name(),
            size: metadata.len(),
            directory: metadata.is_dir(),
            path: entry.path(),
        });
    }
    entries.sort_unstable_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

fn list_directory(
    path: &Path,
    cwd: &Path,
    offset: Option<i64>,
    limit: Option<usize>,
) -> Result<String, ToolError> {
    let relative = path.strip_prefix(cwd).unwrap_or(path);
    let mut output = format!("[{}]", relative.display());
    // 显式 limit 时改为单层分页列表；否则保持现有两层预览。
    if let Some(limit) = limit {
        let entries = directory_entries(path)?;
        let start = page_start(offset, entries.len());
        let end = start.saturating_add(limit.max(1)).min(entries.len());
        if entries.is_empty() {
            output.push_str("\n(empty directory)");
        }
        for entry in &entries[start..end] {
            output.push_str(&format!(
                "\n{}{}  {} B",
                entry.name.to_string_lossy(),
                if entry.directory { "/" } else { "" },
                entry.size
            ));
        }
        let remaining = entries.len() - end;
        if remaining > 0 {
            output.push_str(&format!(
                "\n[… {remaining} more; use offset={} to continue]",
                end + 1
            ));
        }
        return Ok(output);
    }
    append_directory(path, 0, &mut output)?;
    Ok(output)
}

fn append_directory(path: &Path, depth: usize, output: &mut String) -> Result<(), ToolError> {
    let entries = directory_entries(path)?;
    let indent = "  ".repeat(depth);
    if entries.is_empty() {
        output.push_str(&format!("\n{indent}(empty directory)"));
    }
    for entry in entries.iter().take(12) {
        let age = entry.modified.elapsed().unwrap_or_default().as_secs();
        output.push_str(&format!(
            "\n{indent}{}{}  {} B  {age}s ago",
            entry.name.to_string_lossy(),
            if entry.directory { "/" } else { "" },
            entry.size
        ));
        if entry.directory && depth == 0 {
            append_directory(&entry.path, depth + 1, output)?;
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
        let output = list_directory(dir.path(), dir.path(), None, None).unwrap();
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
        let output = list_directory(dir.path(), dir.path(), None, None).unwrap();
        assert!(output.contains("  file.txt"));
        assert!(!output.contains("hidden.txt"));
    }

    #[test]
    fn negative_offset_starts_from_the_end() {
        assert_eq!(page_start(Some(-3), 10), 7);
        assert_eq!(page_start(Some(-30), 10), 0);
        assert_eq!(page_start(Some(0), 10), 0);
        assert_eq!(page_start(None, 10), 0);
        assert_eq!(page_start(Some(5), 10), 4);
        let output = render_text_resource("local://x.md", "a\nb\nc\nd\n", Some(-2), Some(50), 300);
        assert_eq!(output, "[local://x.md]\nc\nd");
    }

    #[test]
    fn page_byte_budget_scales_with_requested_lines() {
        // 小窗口保底共享输出预算，与旧行为一致。
        assert_eq!(page_byte_budget(10, 40_960, 262_144), 40_960);
        // 大窗口按每行 512B 缩放，默认 300 行拿到 150KiB。
        assert_eq!(page_byte_budget(300, 40_960, 262_144), 153_600);
        // 顶到 tools.read.max_bytes 硬顶，防一次性注入过多上下文。
        assert_eq!(page_byte_budget(10_000, 40_960, 262_144), 262_144);
        // 用户配置的 max_bytes 低于保底时以配置为准。
        assert_eq!(page_byte_budget(10, 40_960, 8_192), 8_192);
    }

    #[test]
    fn page_footer_only_when_the_tool_cut_the_output() {
        // 显式 limit 全数送达：文件未完也静默。
        assert!(page_footer(626, 695, 2456, Some(70), 70).is_empty());
        assert!(page_footer(1, 300, 2456, Some(300), 300).is_empty());
        // 字节预算砍在请求行数之前：保留行范围前缀，点名截断原因。
        let cut = page_footer(626, 695, 2456, Some(300), 300);
        assert!(cut.contains("Showing lines 626-695 of 2456"), "{cut}");
        assert!(cut.contains("byte budget"), "{cut}");
        assert!(cut.contains("offset=696"), "{cut}");
        // 默认页大小截断：模型没选停点，保留普通续读提示。
        assert_eq!(
            page_footer(1, 300, 2456, None, 300),
            "\n[Showing lines 1-300 of 2456. Use offset=301 to continue.]"
        );
        // 读到文件尾：无页脚。
        assert!(page_footer(2400, 2456, 2456, Some(300), 300).is_empty());
    }

    #[test]
    fn directory_limit_paginates_a_single_level() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("child/deeper")).unwrap();
        for index in 0..5 {
            fs::write(dir.path().join(format!("{index}.txt")), "x").unwrap();
        }
        // 分页是单层列表：子目录出现但不展开。
        let output = list_directory(dir.path(), dir.path(), None, Some(3)).unwrap();
        assert_eq!(output.matches('\n').count(), 4, "{output}");
        assert!(output.contains("more; use offset="), "{output}");
        assert!(!output.contains("deeper"), "{output}");
        // offset 翻页能拿到剩余项且不再有 more 提示。
        let second = list_directory(dir.path(), dir.path(), Some(4), Some(3)).unwrap();
        assert!(!second.contains("more;"), "{second}");
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

//! 展示目录与文本文件，为完整可见行提供 hashline 锚点。

use std::fs;
use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::hashline::{encode_anchor, served::read_snapshot};
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
        "Read a text file, a directory, or a session-internal URI (artifact://, local://, history://, skill://, viking://). Read before editing to obtain current content and hashline anchors. Use grep for content and find for paths; do not use bash or cat as substitutes."
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
            if args.path.starts_with("ov://") || args.path.starts_with("viking://") {
                let Some(client) = &ctx.openviking else {
                    return Err(ToolError::new(
                        "E_OPENVIKING_DISABLED",
                        "OpenViking URL is not configured",
                    ));
                };
                let peer = ctx.openviking_peer.as_deref();
                let offset = args.offset.unwrap_or(1).max(1).saturating_sub(1);
                let limit = args.limit.unwrap_or(ctx.config.tools.read.default_limit) as i64;
                if let Ok(document) = client.read(&args.path, peer, offset, limit).await {
                    return Ok(ToolOutcome::success(format!(
                        "[{}]\n{}",
                        document.uri, document.content
                    )));
                }
                return Err(ToolError::new(
                    "E_OPENVIKING_READ",
                    "Failed to read OpenViking content",
                ));
            }
            let (path, display, immutable) = match ResourceRouter::resolve_read(ctx, &args.path)
                .map_err(|error| ToolError::new(error.code, error.message))?
            {
                Some(resource) => match resource.backing {
                    ResourceBacking::File(path) => (path, resource.uri, true),
                    ResourceBacking::Text(text) => {
                        return Ok(ToolOutcome::success(render_text_resource(
                            &resource.uri,
                            &text,
                            args.offset,
                            args.limit.unwrap_or(ctx.config.tools.read.default_limit),
                        )));
                    }
                },
                None => (
                    resolve_path(&ctx.cwd, &args.path)?,
                    args.path.clone(),
                    false,
                ),
            };
            if path.is_dir() {
                return Ok(ToolOutcome::success(list_directory(&path, &ctx.cwd)?));
            }
            let max_bytes = ctx.config.tools.output.max_file_bytes;
            let mut snapshot =
                read_snapshot(&path, max_bytes).map_err(|error| map_read_error(&display, error))?;
            if !immutable {
                ctx.anchors.reuse_hashes(&path, &mut snapshot);
            }
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
                    "\n[Warning: file contains invalid UTF-8; invalid bytes were replaced with U+FFFD for display; editing rewrites it as UTF-8]",
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
            let mut selected = Vec::new();
            let mut served_rows = Vec::new();
            let mut rendered_bytes = 0usize;
            for position in start..end {
                let line = &snapshot.lines[position];
                let rendered = if line.len() > max_line_bytes {
                    format!(
                        "[Line {} is {} KB and exceeds the limit; content omitted; use bash to inspect it]",
                        position + 1,
                        line.len().div_ceil(1024)
                    )
                } else if immutable {
                    line.clone()
                } else {
                    let anchor = encode_anchor(snapshot.hashes[position]).map_err(|error| {
                        ToolError::new("E_ANCHOR_SPACE_EXHAUSTED", error.to_string())
                    })?;
                    format!("{anchor}│{line}")
                };
                if rendered_bytes.saturating_add(rendered.len() + 1) > page_budget {
                    break;
                }
                rendered_bytes += rendered.len() + 1;
                if !immutable && line.len() <= max_line_bytes {
                    served_rows.push((position, snapshot.hashes[position]));
                }
                selected.push(rendered);
            }
            if selected.is_empty() && snapshot.lines.is_empty() {
                output.push_str("(empty file)");
            } else {
                output.push_str(&selected.join("\n"));
            }
            let shown_end = start + selected.len();
            output.push_str(&page_footer(start + 1, shown_end, total));
            output.push_str(&notices);
            if !immutable && snapshot.bytes.len() <= 4 * 1024 * 1024 {
                ctx.anchors.record_served(path, &served_rows, snapshot);
            } else if !immutable {
                ctx.anchors.invalidate(&path);
            }
            Ok(ToolOutcome::success(output))
        })
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

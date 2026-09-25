//! 展示目录与文本文件，为完整可见行提供 hashline 锚点。

use std::fs;
use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::hashline::{encode_anchor, served::read_snapshot};

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
        "读取本地文本文件或目录。edit 之前必须先 read；文件行会返回三字符 hashline 锚点。"
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
                    "path": {"type":"string"},
                    "offset": {"type":"integer","minimum":1,"description":"1 基起始行"},
                    "limit": {"type":"integer","minimum":1,"description":"读取行数"}
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
            let path = resolve_path(&ctx.cwd, &args.path)?;
            if path.is_dir() {
                return Ok(ToolOutcome::success(list_directory(&path, &ctx.cwd)?));
            }
            let max_bytes = ctx.config.tools.read.max_file_bytes;
            let mut snapshot = read_snapshot(&path, max_bytes)
                .map_err(|error| map_read_error(path.to_string_lossy().as_ref(), error))?;
            ctx.anchors.reuse_hashes(&path, &mut snapshot);
            let offset = args.offset.unwrap_or(1).max(1);
            let limit = args
                .limit
                .unwrap_or(ctx.config.tools.read.default_limit)
                .max(1);
            let start = offset.saturating_sub(1).min(snapshot.lines.len());
            let end = start.saturating_add(limit).min(snapshot.lines.len());
            let max_line_bytes = ctx
                .config
                .tools
                .read
                .line_max_bytes
                .min(ctx.config.tools.read.max_bytes.saturating_sub(7));
            let mut selected = Vec::new();
            let mut served_rows = Vec::new();
            let mut rendered_bytes = 0usize;
            for position in start..end {
                let line = &snapshot.lines[position];
                let rendered = if line.len() > max_line_bytes {
                    format!(
                        "[第 {} 行 {} KB，超出上限，内容未展示；可用 bash 检查该行]",
                        position + 1,
                        line.len().div_ceil(1024)
                    )
                } else {
                    let anchor = encode_anchor(snapshot.hashes[position]).map_err(|error| {
                        ToolError::new("E_ANCHOR_SPACE_EXHAUSTED", error.to_string())
                    })?;
                    format!("{anchor}│{line}")
                };
                if rendered_bytes.saturating_add(rendered.len() + 1)
                    > ctx.config.tools.read.max_bytes
                {
                    break;
                }
                rendered_bytes += rendered.len() + 1;
                if line.len() <= max_line_bytes {
                    served_rows.push((position, snapshot.hashes[position]));
                }
                selected.push(rendered);
            }
            let mut output = format!("[{}]\n", path.display());
            if selected.is_empty() && snapshot.lines.is_empty() {
                output.push_str("(empty file)");
            } else {
                output.push_str(&selected.join("\n"));
            }
            let shown_end = start + selected.len();
            if shown_end < snapshot.lines.len() {
                output.push_str(&format!(
                    "\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
                    start + 1,
                    shown_end,
                    snapshot.lines.len(),
                    shown_end + 1
                ));
            }
            if snapshot.bytes.len() > 4 * 1024 * 1024 {
                output.push_str(
                    "\n[文件超过 4 MiB 快照上限，未授权锚点编辑；请使用 bash 或 write。]",
                );
            }
            if std::str::from_utf8(&snapshot.bytes).is_err() {
                output.push_str(
                    "\n[警告：文件包含无效 UTF-8 字节，展示时已替换为 U+FFFD；编辑会重写为 UTF-8]",
                );
            }
            if snapshot.bytes.len() <= 4 * 1024 * 1024 {
                ctx.anchors.record_served(path, &served_rows, snapshot);
            } else {
                ctx.anchors.invalidate(&path);
            }
            Ok(ToolOutcome::success(output))
        })
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
            "\n{indent}{}{}  {size} B  {age} 秒前",
            name.to_string_lossy(),
            if *directory { "/" } else { "" }
        ));
        if *directory && depth == 0 {
            append_directory(child, depth + 1, output)?;
        }
    }
    if entries.len() > 12 {
        output.push_str(&format!(
            "\n{indent}[… 另有 {} 项，读取子目录继续]",
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
        assert!(output.contains("另有 2 项"));
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

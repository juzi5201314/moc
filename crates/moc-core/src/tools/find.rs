//! 并行遍历本地路径；排序结果使用有界堆，空间不随目录大小增长。

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Sort {
    #[default]
    Mtime,
    Path,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    glob: Vec<String>,
    path: Option<String>,
    hidden: Option<bool>,
    gitignore: Option<bool>,
    limit: Option<usize>,
    #[serde(default)]
    sort: Sort,
}

pub struct FindTool;

impl Tool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }
    fn description(&self) -> &'static str {
        "Find local paths in parallel with glob patterns. Use this for filenames and paths, not bash, find, or ls. Respects .gitignore/.ignore and returns at most 200 results."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "find",
            schema(
                serde_json::json!({
                    "glob":{"type":"array","minItems":1,"items":{"type":"string"}},
                    "path":{"type":"string"},
                    "hidden":{"type":"boolean"},
                    "gitignore":{"type":"boolean"},
                    "limit":{"type":"integer","minimum":1,"maximum":200},
                    "sort":{"type":"string","enum":["mtime","path"]}
                }),
                &["glob"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: FindArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            if let Some(path) = args.path.as_deref()
                && path.contains("://")
                && !path.starts_with("ov://")
                && !path.starts_with("viking://")
            {
                let _ = crate::resources::ResourceRouter::resolve_read(ctx, path)
                    .map_err(|error| ToolError::new(error.code, error.message))?
                    .ok_or_else(|| {
                        ToolError::new(
                            "E_URI_UNSUPPORTED",
                            "resource URI cannot be a find root; use read or grep",
                        )
                    })?;
                return Err(ToolError::new(
                    "E_URI_UNSUPPORTED",
                    "resource URI cannot be a find root; use read or grep",
                ));
            }
            let cancel = ctx.cancel.child_token();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let scan = Scan {
                cwd: ctx.cwd.clone(),
                args,
                cancel,
                limit: ctx.config.tools.find.limit,
                max_entries: ctx.config.workspace.max_scan_entries,
                deadline: Instant::now() + Duration::from_millis(ctx.config.tools.find.timeout_ms),
            };
            let output = tokio::task::spawn_blocking(move || scan.run())
                .await
                .map_err(|error| ToolError::new("E_FIND", error.to_string()))??;
            Ok(ToolOutcome::success(output))
        })
    }
}

struct Scan {
    args: FindArgs,
    cwd: PathBuf,
    cancel: CancellationToken,
    limit: usize,
    max_entries: usize,
    deadline: Instant,
}

impl Scan {
    fn run(&self) -> Result<String, ToolError> {
        if self.args.glob.is_empty() {
            return Err(ToolError::new(
                "E_BAD_GLOB",
                "glob requires at least one pattern",
            ));
        }
        let mut patterns = globset::GlobSetBuilder::new();
        for pattern in &self.args.glob {
            let pattern = if !pattern.contains('/') {
                format!("**/{pattern}")
            } else {
                pattern.clone()
            };
            patterns.add(
                globset::Glob::new(&pattern)
                    .map_err(|error| ToolError::new("E_BAD_GLOB", error.to_string()))?,
            );
        }
        let patterns = patterns
            .build()
            .map_err(|error| ToolError::new("E_BAD_GLOB", error.to_string()))?;
        let root = crate::tools::resolve_scan_root(&self.cwd, self.args.path.as_deref())?;
        let root_is_file = root.is_file();
        let limit = self.args.limit.unwrap_or(self.limit);
        if !(1..=200).contains(&limit) {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "limit must be between 1 and 200",
            ));
        }
        let gitignore = self.args.gitignore.unwrap_or(true);
        let mut walker = ignore::WalkBuilder::new(&root);
        walker
            .hidden(!self.args.hidden.unwrap_or(true))
            .git_ignore(gitignore)
            .git_global(gitignore)
            .git_exclude(gitignore)
            .ignore(gitignore)
            .same_file_system(true)
            .threads(std::thread::available_parallelism().map_or(1, usize::from));
        // 堆顶为当前最差结果；最多 200 项，避免保存整棵目录树。
        let best = Mutex::new(BinaryHeap::with_capacity(limit));
        let visited = AtomicUsize::new(0);
        let matches = AtomicUsize::new(0);
        let incomplete = AtomicBool::new(false);
        walker.build_parallel().run(|| {
            Box::new(|entry| {
                if self.cancel.is_cancelled()
                    || Instant::now() >= self.deadline
                    || visited.fetch_add(1, Ordering::Relaxed) >= self.max_entries
                {
                    incomplete.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        incomplete.store(true, Ordering::Relaxed);
                        return ignore::WalkState::Continue;
                    }
                };
                if !root_is_file && entry.path() == root {
                    return ignore::WalkState::Continue;
                }
                let path = if root_is_file {
                    Path::new(entry.file_name())
                } else {
                    entry.path().strip_prefix(&root).unwrap_or(entry.path())
                };
                if !patterns.is_match(path) {
                    return ignore::WalkState::Continue;
                }
                let modified = match self.args.sort {
                    Sort::Path => 0,
                    Sort::Mtime => match entry
                        .metadata()
                        .and_then(|metadata| metadata.modified().map_err(ignore::Error::Io))
                    {
                        Ok(time) => time
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos(),
                        Err(_) => {
                            incomplete.store(true, Ordering::Relaxed);
                            return ignore::WalkState::Continue;
                        }
                    },
                };
                matches.fetch_add(1, Ordering::Relaxed);
                let path = entry
                    .path()
                    .strip_prefix(&self.cwd)
                    .unwrap_or(entry.path())
                    .to_owned();
                let candidate = (Reverse(modified), path);
                let mut best = best.lock();
                if best.len() < limit {
                    best.push(candidate);
                } else if best.peek().is_some_and(|worst| candidate < *worst) {
                    best.pop();
                    best.push(candidate);
                }
                debug_assert!(best.len() <= limit);
                ignore::WalkState::Continue
            })
        });
        let paths = best.into_inner().into_sorted_vec();
        let mut output: Vec<String> = paths
            .iter()
            .map(|(_, path)| path.display().to_string())
            .collect();
        if output.is_empty() {
            output.push("No files found matching pattern".into());
        }
        if matches.load(Ordering::Relaxed) > limit {
            output.push(format!(
                "[limit {limit} reached; narrow the glob{}]",
                if limit < 200 {
                    " or increase limit"
                } else {
                    ""
                }
            ));
        }
        if incomplete.load(Ordering::Relaxed) {
            output.push("[A time or scan-entry limit was reached, the scan was cancelled, or a path was unreadable; results may be incomplete]".into());
        }
        Ok(output.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scan(root: &Path, args: Value, max_entries: usize) -> Result<String, ToolError> {
        Scan {
            args: serde_json::from_value(args).unwrap(),
            cwd: root.to_owned(),
            cancel: CancellationToken::new(),
            limit: 200,
            max_entries,
            deadline: Instant::now() + Duration::from_secs(5),
        }
        .run()
    }
    #[test]
    fn discovery_is_sorted_bounded_and_relative_to_search_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
        std::fs::write(root.path().join("src/nested/a.rs"), "").unwrap();
        std::fs::write(root.path().join("src/b.rs"), "").unwrap();
        let output = scan(
            root.path(),
            serde_json::json!({"path":"src","glob":["*.rs"],"sort":"path","limit":1}),
            100,
        )
        .unwrap();
        assert!(output.starts_with("src/b.rs\n[limit 1 reached"));
        let file = scan(
            root.path(),
            serde_json::json!({"path":"src/b.rs","glob":["*.rs"],"sort":"path"}),
            100,
        )
        .unwrap();
        assert_eq!(file, "src/b.rs");
    }
    #[test]
    fn scan_limit_counts_nonmatching_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("folder")).unwrap();
        let output = scan(root.path(), serde_json::json!({"glob":["*.rs"]}), 1).unwrap();
        assert!(output.contains("results may be incomplete"));
    }
    #[test]
    fn malformed_glob_is_a_tool_error() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            scan(root.path(), serde_json::json!({"glob":["["]}), 100)
                .unwrap_err()
                .code,
            "E_BAD_GLOB"
        );
    }
}

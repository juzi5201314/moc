//! 遵循 ignore 规则的有界并行搜索；分页顺序由路径决定。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use grep_matcher::Matcher;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use moc_llm::ToolDef;
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::resources::{ResourceBacking, ResourceRouter};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<Vec<String>>,
    case: Option<bool>,
    gitignore: Option<bool>,
    hidden: Option<bool>,
    context_before: Option<usize>,
    context_after: Option<usize>,
    skip: Option<usize>,
    #[serde(default)]
    cross_filesystems: bool,
}

pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search local files with Rust regex in parallel. Absolute paths are allowed; by default scanning does not cross mounts below the root, and cross_filesystems=true expands it. Timeout is cooperative, not a hard kill. Use this for content, not bash, cat, rg, awk, or sed."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "grep",
            schema(
                serde_json::json!({
                    "pattern":{"type":"string"},
                    "path":{"type":"string","description":"Absolute or cwd-relative scan root"},
                    "glob":{"type":"array","items":{"type":"string"}},
                    "case":{"type":"boolean"},
                    "gitignore":{"type":"boolean"},
                    "hidden":{"type":"boolean","description":"Include hidden entries such as dotfiles and .git; default false"},
                    "context_before":{"type":"integer","minimum":0},
                    "context_after":{"type":"integer","minimum":0},
                    "skip":{"type":"integer","minimum":0},
                    "cross_filesystems":{"type":"boolean","description":"Cross filesystem mounts below the scan root; default false"}
                }),
                &["pattern"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: GrepArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            if let Some(path) = args.path.as_deref()
                && let Some(resource) = ResourceRouter::resolve_read(ctx, path)
                    .map_err(|error| ToolError::new(error.code, error.message))?
            {
                let output = match resource.backing {
                    ResourceBacking::File(path) => {
                        search_resource_file(&resource.uri, &path, &args, &ctx.config.tools.grep)?
                    }
                    ResourceBacking::Text(text) => {
                        search_resource_text(&resource.uri, &text, &args, &ctx.config.tools.grep)?
                    }
                };
                return Ok(ToolOutcome::success(output));
            }
            let cancel = ctx.cancel.child_token();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let search = Search::new(args, ctx.cwd.clone(), ctx.config.tools.grep.clone(), cancel)?;
            let result = tokio::task::spawn_blocking(move || search.run())
                .await
                .map_err(|error| ToolError::new("E_GREP", error.to_string()))??;
            Ok(ToolOutcome::success(result))
        })
    }
}

fn search_resource_file(
    uri: &str,
    path: &Path,
    args: &GrepArgs,
    config: &crate::config::GrepConfig,
) -> Result<String, ToolError> {
    let metadata =
        std::fs::metadata(path).map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
    if metadata.len() > config.max_file_bytes {
        return Err(ToolError::new(
            "E_TOO_LARGE",
            format!(
                "resource exceeds the {}-byte search limit",
                config.max_file_bytes
            ),
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
    search_resource_text(uri, &String::from_utf8_lossy(&bytes), args, config)
}

fn search_resource_text(
    uri: &str,
    text: &str,
    args: &GrepArgs,
    config: &crate::config::GrepConfig,
) -> Result<String, ToolError> {
    if args.pattern.is_empty() {
        return Err(ToolError::new("E_BAD_REGEX", "pattern must not be empty"));
    }
    if text.len() as u64 > config.max_file_bytes {
        return Err(ToolError::new(
            "E_TOO_LARGE",
            format!(
                "resource exceeds the {}-byte search limit",
                config.max_file_bytes
            ),
        ));
    }
    let matcher = grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(!args.case.unwrap_or(true))
        .build(&args.pattern)
        .map_err(|error| ToolError::new("E_BAD_REGEX", error.to_string()))?;
    let lines: Vec<&str> = text.lines().collect();
    let mut matches = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if matcher
            .is_match(line.as_bytes())
            .map_err(|error| ToolError::new("E_GREP", error.to_string()))?
        {
            matches.push(index);
        }
    }
    let skip = args.skip.unwrap_or(0);
    let selected = matches
        .into_iter()
        .skip(skip)
        .take(config.max_matches_total);
    let mut rows = std::collections::BTreeSet::new();
    for index in selected {
        let before = args.context_before.unwrap_or(config.context_before);
        let after = args.context_after.unwrap_or(config.context_after);
        let end = index
            .saturating_add(after)
            .min(lines.len().saturating_sub(1));
        rows.extend(index.saturating_sub(before)..=end);
    }
    let mut output = String::new();
    for index in rows {
        let marker = if matcher
            .is_match(lines[index].as_bytes())
            .map_err(|error| ToolError::new("E_GREP", error.to_string()))?
        {
            ':'
        } else {
            '-'
        };
        use std::fmt::Write;
        writeln!(output, "{uri}{marker}{}{marker}{}", index + 1, lines[index])
            .expect("写入 String 不失败");
    }
    if output.is_empty() {
        output.push_str("No matches found");
    }
    Ok(output.trim_end_matches('\n').to_owned())
}

struct Search {
    args: GrepArgs,
    root: PathBuf,
    cwd: PathBuf,
    matcher: grep_regex::RegexMatcher,
    include: globset::GlobSet,
    config: crate::config::GrepConfig,
    cancel: CancellationToken,
    deadline: Instant,
}

impl Search {
    fn new(
        args: GrepArgs,
        cwd: PathBuf,
        config: crate::config::GrepConfig,
        cancel: CancellationToken,
    ) -> Result<Self, ToolError> {
        if args.pattern.is_empty() {
            return Err(ToolError::new("E_BAD_REGEX", "pattern must not be empty"));
        }
        let matcher = grep_regex::RegexMatcherBuilder::new()
            .case_insensitive(!args.case.unwrap_or(true))
            .build(&args.pattern)
            .map_err(|error| ToolError::new("E_BAD_REGEX", error.to_string()))?;
        let mut include = globset::GlobSetBuilder::new();
        for pattern in args.glob.as_deref().unwrap_or_default() {
            include.add(
                globset::Glob::new(pattern)
                    .map_err(|error| ToolError::new("E_BAD_GLOB", error.to_string()))?,
            );
        }
        let include = include
            .build()
            .map_err(|error| ToolError::new("E_BAD_GLOB", error.to_string()))?;
        let root = crate::tools::resolve_scan_root(&cwd, args.path.as_deref())?;
        let deadline = Instant::now() + Duration::from_millis(config.timeout_ms);
        Ok(Self {
            args,
            root,
            cwd,
            matcher,
            include,
            config,
            cancel,
            deadline,
        })
    }

    fn stopped(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }

    fn run(&self) -> Result<String, ToolError> {
        let mut walker = ignore::WalkBuilder::new(&self.root);
        let gitignore = self.args.gitignore.unwrap_or(true);
        walker
            .hidden(!self.args.hidden.unwrap_or(false))
            .git_ignore(gitignore)
            .git_global(gitignore)
            .git_exclude(gitignore)
            .ignore(gitignore)
            .same_file_system(!self.args.cross_filesystems)
            .threads(std::thread::available_parallelism().map_or(1, usize::from));
        let candidates = Mutex::new(Vec::new());
        let incomplete = AtomicBool::new(false);
        walker.build_parallel().run(|| {
            Box::new(|entry| {
                if self.stopped() {
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
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    return ignore::WalkState::Continue;
                }
                let path = entry.path();
                if !self.include.is_empty()
                    && !self
                        .include
                        .is_match(path.strip_prefix(&self.root).unwrap_or(path))
                    && !self.include.is_match(path.file_name().unwrap_or_default())
                {
                    return ignore::WalkState::Continue;
                }
                let mut paths = candidates.lock();
                if paths.len() >= self.config.max_candidate_files {
                    incomplete.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }
                paths.push(entry.into_path());
                ignore::WalkState::Continue
            })
        });
        let mut paths = candidates.into_inner();
        paths.sort_unstable();
        paths.dedup();
        let mut output = String::new();
        let mut matched_files = 0usize;
        let mut shown_files = 0usize;
        let mut shown_matches = 0usize;
        let mut limited = false;
        let mut skipped_large_files = 0usize;
        let skip = self.args.skip.unwrap_or(0);
        // 分块保留并行度，同时使低输出上限无需搜索整个候选集合。
        let width = rayon::current_num_threads().clamp(1, 32);
        'blocks: for block in paths.chunks(width) {
            let results: Vec<_> = block
                .par_iter()
                .map(|path| self.search_file(path))
                .collect();
            for (path, result) in block.iter().zip(results) {
                let result = match result {
                    Ok(result) => result,
                    Err(_) => {
                        incomplete.store(true, Ordering::Relaxed);
                        continue;
                    }
                };
                if result.skipped {
                    skipped_large_files += 1;
                    incomplete.store(true, Ordering::Relaxed);
                    continue;
                }
                if result.matches == 0 {
                    continue;
                }
                matched_files += 1;
                if matched_files <= skip {
                    continue;
                }
                if shown_files >= self.config.max_files
                    || shown_matches >= self.config.max_matches_total
                {
                    limited = true;
                    break 'blocks;
                }
                let allowance = self.config.max_matches_total - shown_matches;
                let count = result.matches.min(allowance);
                let last_match = result
                    .rows
                    .iter()
                    .filter(|row| row.matched)
                    .nth(count - 1)
                    .map(|row| row.number)
                    .unwrap_or(0);
                let last_line = last_match.saturating_add(
                    self.args.context_after.unwrap_or(self.config.context_after) as u64,
                );
                let relative = path.strip_prefix(&self.cwd).unwrap_or(path);
                let mut emitted_matches = 0usize;
                for row in result.rows {
                    if row.number > last_line || (row.matched && emitted_matches == count) {
                        break;
                    }
                    let marker = if row.matched {
                        emitted_matches += 1;
                        ':'
                    } else {
                        '-'
                    };
                    use std::fmt::Write;
                    writeln!(
                        output,
                        "{}{marker}{}{marker}{}",
                        relative.display(),
                        row.number,
                        row.text
                    )
                    .expect("写入 String 不失败");
                }
                shown_files += 1;
                shown_matches += count;
                limited |= result.truncated || result.matches > count;
            }
            if self.stopped() {
                incomplete.store(true, Ordering::Relaxed);
                break;
            }
        }
        if output.is_empty() && !incomplete.load(Ordering::Relaxed) && !self.stopped() {
            output.push_str("No matches found\n");
        }
        if limited {
            output.push_str(&format!(
                "[{shown_files} files, {shown_matches} matches shown; result limit reached, continue with skip={}; narrow pattern when one file hits its limit]\n",
                skip + shown_files
            ));
        }
        if skipped_large_files > 0 {
            output.push_str(&format!(
                "[{skipped_large_files} files exceeded the {}-byte limit and were not searched]\n",
                self.config.max_file_bytes
            ));
        }
        if incomplete.load(Ordering::Relaxed) || self.stopped() {
            output.push_str("[Scan was incomplete because the cooperative timeout, candidate-file limit, cancellation, unreadable path, or oversized-file limit was reached; results may be incomplete and are not a no-match proof. Narrow path or glob, or set cross_filesystems=true to include mounted paths below the root]\n");
        }
        Ok(output.trim_end_matches('\n').to_owned())
    }
    fn search_file(&self, path: &Path) -> std::io::Result<Matches> {
        let mut sink = Matches::default();
        if self.stopped() {
            return Ok(sink);
        }
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > self.config.max_file_bytes {
            sink.skipped = true;
            return Ok(sink);
        }
        let file = std::fs::File::open(path)?;
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .multi_line(false)
            .before_context(
                self.args
                    .context_before
                    .unwrap_or(self.config.context_before),
            )
            .after_context(self.args.context_after.unwrap_or(self.config.context_after))
            .binary_detection(BinaryDetection::quit(0))
            .build();
        // 禁止 mmap；文件在扫描中增长时也不能突破读取字节预算。
        use std::io::Read;
        searcher.search_reader(
            &self.matcher,
            file.take(self.config.max_file_bytes),
            MatchSink {
                search: self,
                result: &mut sink,
            },
        )?;
        Ok(sink)
    }
}

#[derive(Default)]
struct Matches {
    rows: Vec<Row>,
    matches: usize,
    truncated: bool,
    skipped: bool,
}
struct Row {
    number: u64,
    text: String,
    matched: bool,
}
struct MatchSink<'a> {
    search: &'a Search,
    result: &'a mut Matches,
}

impl MatchSink<'_> {
    fn push(&mut self, number: Option<u64>, bytes: &[u8], matched: bool) {
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_end_matches(['\r', '\n']);
        let mut chars = text.chars();
        let mut text: String = chars
            .by_ref()
            .take(self.search.config.line_max_chars)
            .collect();
        if chars.next().is_some() {
            text.push('…');
        }
        self.result.rows.push(Row {
            number: number.expect("搜索器已启用行号"),
            text,
            matched,
        });
    }
}

impl Sink for MatchSink<'_> {
    type Error = std::io::Error;
    fn matched(&mut self, _: &Searcher, line: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.search.stopped() {
            return Ok(false);
        }
        if self.result.matches >= self.search.config.max_matches_per_file {
            self.result.truncated = true;
            return Ok(false);
        }
        self.result.matches += 1;
        self.push(line.line_number(), line.bytes(), true);
        Ok(true)
    }
    fn context(&mut self, _: &Searcher, line: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if self.search.stopped() {
            return Ok(false);
        }
        self.push(line.line_number(), line.bytes(), false);
        Ok(true)
    }
    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, Self::Error> {
        *self.result = Matches::default();
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(root: &Path, args: Value, config: crate::config::GrepConfig) -> String {
        Search::new(
            serde_json::from_value(args).unwrap(),
            root.to_owned(),
            config,
            CancellationToken::new(),
        )
        .unwrap()
        .run()
        .unwrap()
    }
    #[test]
    fn context_colons_do_not_consume_match_budget_and_nested_globs_work() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/a.rs"),
            "context: colon\nhit one\ncontext: colon\nhit two\ntail\n",
        )
        .unwrap();
        std::fs::write(root.path().join("b.txt"), "hit ignored").unwrap();
        let config = crate::config::GrepConfig {
            max_matches_total: 2,
            max_matches_per_file: 2,
            ..Default::default()
        };
        let output = run(
            root.path(),
            serde_json::json!({"pattern":"hit","glob":["src/**/*.rs"],"context_before":1,"context_after":1}),
            config,
        );
        assert_eq!(
            output,
            "src/a.rs-1-context: colon\nsrc/a.rs:2:hit one\nsrc/a.rs-3-context: colon\nsrc/a.rs:4:hit two\nsrc/a.rs-5-tail"
        );
    }
    #[test]
    fn pagination_is_path_ordered_and_binary_files_are_not_returned() {
        let root = tempfile::tempdir().unwrap();
        for path in ["c.rs", "a.rs", "b.rs"] {
            std::fs::write(root.path().join(path), "hit\n").unwrap();
        }
        std::fs::write(root.path().join("0.bin"), b"hit\0\n").unwrap();
        let config = crate::config::GrepConfig {
            max_files: 1,
            ..Default::default()
        };
        let output = run(
            root.path(),
            serde_json::json!({"pattern":"hit","skip":1}),
            config,
        );
        assert!(output.starts_with("b.rs:1:hit\n"));
        assert!(output.contains("skip=2"));
        assert!(!output.contains("a.rs:") && !output.contains("0.bin"));
    }
    #[test]
    fn resource_context_ranges_saturate_instead_of_overflowing() {
        let config = crate::config::GrepConfig::default();
        let args: GrepArgs = serde_json::from_value(serde_json::json!({
            "pattern": "hit",
            "context_before": usize::MAX,
            "context_after": usize::MAX,
        }))
        .unwrap();
        let output = search_resource_text(
            "skill://demo/SKILL.md",
            "first\nhit\nlast\n",
            &args,
            &config,
        )
        .unwrap();
        assert_eq!(
            output,
            "skill://demo/SKILL.md-1-first\nskill://demo/SKILL.md:2:hit\nskill://demo/SKILL.md-3-last"
        );
    }

    #[test]
    fn zero_deadline_reports_incomplete_instead_of_false_negative() {
        let root = tempfile::tempdir().unwrap();
        let config = crate::config::GrepConfig {
            timeout_ms: 0,
            ..Default::default()
        };
        let output = run(root.path(), serde_json::json!({"pattern":"hit"}), config);
        assert!(output.contains("results may be incomplete"));
    }

    #[test]
    fn hidden_entries_are_excluded_by_default() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git/objects")).unwrap();
        std::fs::write(root.path().join(".git/objects/pack"), "hit\n").unwrap();
        std::fs::write(root.path().join(".env"), "hit\n").unwrap();
        std::fs::write(root.path().join("visible.txt"), "hit\n").unwrap();
        let config = crate::config::GrepConfig::default();
        let default = run(
            root.path(),
            serde_json::json!({"pattern":"hit"}),
            config.clone(),
        );
        assert!(default.contains("visible.txt:1:hit"));
        assert!(!default.contains(".env") && !default.contains(".git"));
        let included = run(
            root.path(),
            serde_json::json!({"pattern":"hit","hidden":true}),
            config,
        );
        assert!(included.contains(".env:1:hit"));
        assert!(included.contains(".git/objects/pack:1:hit"));
    }
}

#[cfg(test)]
mod absolute_scan_tests {
    use super::*;

    #[test]
    fn absolute_root_is_resolved_and_cooperative_timeout_is_reported() {
        let directory = tempfile::tempdir().expect("临时目录");
        std::fs::write(directory.path().join("hit.txt"), "hit\n").expect("写入测试文件");
        let config = crate::config::GrepConfig {
            timeout_ms: 0,
            ..Default::default()
        };
        let args: GrepArgs =
            serde_json::from_value(serde_json::json!({"pattern":"hit", "path":directory.path()}))
                .expect("解析参数");
        let output = Search::new(
            args,
            directory.path().to_owned(),
            config,
            CancellationToken::new(),
        )
        .expect("解析绝对扫描根")
        .run()
        .expect("执行扫描");
        assert!(output.contains("incomplete"));
        assert!(!output.contains("No matches found"));
    }
}

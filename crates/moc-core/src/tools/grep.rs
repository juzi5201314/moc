//! grep 工具：遵守 ignore 规则的并行搜索，输出有上限；分页按路径排序。

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
        "Search file contents with Rust regex. Returns matching lines without surrounding context unless context_before/context_after are set. Does not cross mounts below the root unless cross_filesystems=true."
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
                    "path":{"type":"string","description":"Scan root or resource URI; default cwd"},
                    "glob":{"type":"array","items":{"type":"string"}},
                    "case":{"type":"boolean","description":"Case-sensitive; default true"},
                    "gitignore":{"type":"boolean"},
                    "hidden":{"type":"boolean","description":"Include dotfiles and .git; default false"},
                    "context_before":{"type":"integer","minimum":0,"description":"Lines of context before each match; default 0"},
                    "context_after":{"type":"integer","minimum":0,"description":"Lines of context after each match; default 0"},
                    "skip":{"type":"integer","minimum":0,"description":"Matched files to skip, for paging"},
                    "cross_filesystems":{"type":"boolean","description":"Default false"}
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
                    ResourceBacking::Namespace { entries, .. } => {
                        search_resource_namespace(&entries, &args, &ctx.config.tools.grep)?
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
    let matches = collect_resource_matches(uri, text, args, config)?;
    Ok(if matches.is_empty() {
        "No matches found".to_owned()
    } else {
        matches
    })
}

/// 在命名空间的各个目录里逐个文件匹配。命中行的前缀用「URI 前缀 + 相对路径」拼出，
/// 模型看到的是与 read 一致的 URI，而不是 urlfs 或 sidecar 的真实路径。
fn search_resource_namespace(
    entries: &[(String, PathBuf)],
    args: &GrepArgs,
    config: &crate::config::GrepConfig,
) -> Result<String, ToolError> {
    /// 一次命名空间扫描最多检查的文件数：命名空间可以无限大，工具输出必须有上限。
    const MAX_FILES: usize = 200;
    let mut output = String::new();
    let mut notices = Vec::new();
    let mut scanned = 0usize;
    let mut stopped = false;
    'entries: for (prefix, directory) in entries {
        for (relative, path) in directory_files(directory) {
            if scanned >= MAX_FILES {
                stopped = true;
                break 'entries;
            }
            scanned += 1;
            let uri = format!("{prefix}{relative}");
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > config.max_file_bytes {
                notices.push(format!(
                    "[skipped {uri}: {} B exceeds the search limit]",
                    metadata.len()
                ));
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let matches =
                collect_resource_matches(&uri, &String::from_utf8_lossy(&bytes), args, config)?;
            if matches.is_empty() {
                continue;
            }
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&matches);
        }
    }
    if output.is_empty() {
        output.push_str("No matches found");
    }
    for notice in notices {
        output.push('\n');
        output.push_str(&notice);
    }
    if stopped {
        output.push_str(&format!(
            "\n[namespace scan stopped after {MAX_FILES} files]"
        ));
    }
    Ok(output)
}

/// 列出目录下所有文件及其相对路径（用 `/` 分隔，按路径排序），跳过点文件。
fn directory_files(directory: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    let mut stack = vec![(String::new(), directory.to_owned())];
    while let Some((prefix, current)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push((relative, entry.path()));
            } else if file_type.is_file() {
                files.push((relative, entry.path()));
            }
        }
    }
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    files
}

/// 取出一段文本里的命中行；没有命中时返回空串，由调用方决定报告无结果还是继续拼下一段。
fn collect_resource_matches(
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
    let mut matched = vec![false; lines.len()];
    let mut hits = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if matcher
            .is_match(line.as_bytes())
            .map_err(|error| ToolError::new("E_GREP", error.to_string()))?
        {
            matched[index] = true;
            hits.push(index);
        }
    }
    let skip = args.skip.unwrap_or(0);
    let selected = hits.into_iter().skip(skip).take(config.max_matches_total);
    let mut rows = std::collections::BTreeSet::new();
    for index in selected {
        let before = args.context_before.unwrap_or(config.context_before);
        let after = args.context_after.unwrap_or(config.context_after);
        let end = index
            .saturating_add(after)
            .min(lines.len().saturating_sub(1));
        rows.extend(index.saturating_sub(before)..=end);
    }
    let rows: Vec<Row> = rows
        .into_iter()
        .map(|index| Row {
            number: index as u64 + 1,
            text: preview_line(
                lines[index],
                config.line_max_chars,
                matched[index]
                    .then(|| match_span(&matcher, lines[index].as_bytes()))
                    .flatten(),
            ),
            matched: matched[index],
        })
        .collect();
    Ok(render_group(uri, &rows))
}

/// 命中在本行里的字节区间，用来把长行截断的窗口对准命中处。
///
/// 绕回 `None` 时退化为从头截断：正则在这行上没找到（理论上不该发生，可能是
/// 字节与字符边界不一致），或者调用方拿不到命中位置（上下文行）。
fn match_span(matcher: &grep_regex::RegexMatcher, haystack: &[u8]) -> Option<(usize, usize)> {
    matcher
        .find(haystack)
        .ok()
        .flatten()
        .map(|found| (found.start(), found.end()))
}

/// 追加一行提示：已有内容时先补一个换行，空结果（扫描被取消等情况）不会以空行开头。
fn push_notice(output: &mut String, notice: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(notice);
}

/// 一行结果里的单个文件分组：单行文件写成 `路径:行号:正文`，多行文件写成
/// 「路径标题 + 2 空格缩进的行」。路径只写一次是这套格式省 token 的来源；
/// 缩进固定 2 个空格，让标题行与结果行可以按列区分，正文里的 `12:34:56`
/// 也不会被当成行号前缀。
fn render_group(path: &str, rows: &[Row]) -> String {
    use std::fmt::Write;
    let mut output = String::new();
    match rows {
        [] => return output,
        [row] => {
            let marker = if row.matched { ':' } else { '-' };
            write!(output, "{path}{marker}{}{marker}{}", row.number, row.text)
                .expect("写入 String 不会失败");
        }
        rows => {
            output.push_str(path);
            for row in rows {
                let marker = if row.matched { ':' } else { '-' };
                write!(output, "\n  {}{marker}{}", row.number, row.text)
                    .expect("写入 String 不会失败");
            }
        }
    }
    output
}

/// 截断一行结果，并尽量让命中处留在窗口里。
///
/// 命中行以命中位置为中心取窗口（命中前留 `max_chars / 4` 个字符），这样长行
/// 被截断时模型仍能看见命中的那一段；上下文行没有命中位置，退化为从头截断。
/// 省略号计入 `max_chars`，因此返回值不会超过预算。
fn preview_line(text: &str, max_chars: usize, span: Option<(usize, usize)>) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    let head = span
        .and_then(|(start, _)| text.get(..start))
        .map_or(0, |prefix| {
            prefix.chars().count().saturating_sub(max_chars / 4)
        });
    let lead = head > 0;
    let mut room = max_chars.saturating_sub(usize::from(lead));
    let tail = head + room < total;
    room = room.saturating_sub(usize::from(tail));
    let kept: String = text.chars().skip(head).take(room).collect();
    let mut output = String::with_capacity(kept.len() + 6);
    if lead {
        output.push('…');
    }
    output.push_str(&kept);
    if tail {
        output.push('…');
    }
    output
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
        let mut capped = false;
        let mut skipped_large_files = 0usize;
        let skip = self.args.skip.unwrap_or(0);
        // 分块搜索：块内并行，输出上限较低时不必搜完全部候选文件。
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
                let mut rows = Vec::new();
                let mut emitted_matches = 0usize;
                for row in result.rows {
                    if row.number > last_line || (row.matched && emitted_matches == count) {
                        break;
                    }
                    if row.matched {
                        emitted_matches += 1;
                    }
                    rows.push(row);
                }
                let group = render_group(&relative.display().to_string(), &rows);
                let needed = group.len() + usize::from(!output.is_empty());
                // 整组判断，不切半个文件：预算封顶时的 skip 才是精确的。
                if !output.is_empty() && output.len() + needed > self.config.output_bytes {
                    capped = true;
                    break 'blocks;
                }
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&group);
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
            output.push_str("No matches found");
        }
        if capped {
            push_notice(
                &mut output,
                &format!(
                    "[{shown_files} files, {shown_matches} matches shown; output capped at {} bytes. Continue with skip={}; narrow the pattern if a single file hits its limit]",
                    self.config.output_bytes,
                    skip + shown_files
                ),
            );
        } else if limited {
            push_notice(
                &mut output,
                &format!(
                    "[{shown_files} files, {shown_matches} matches shown; limit reached. Continue with skip={}; narrow the pattern if a single file hits its limit]",
                    skip + shown_files
                ),
            );
        }
        if skipped_large_files > 0 {
            push_notice(
                &mut output,
                &format!(
                    "[{skipped_large_files} files exceeded the {}-byte limit and were not searched]",
                    self.config.max_file_bytes
                ),
            );
        }
        if incomplete.load(Ordering::Relaxed) || self.stopped() {
            push_notice(
                &mut output,
                "[Scan incomplete (time limit, candidate-file limit, cancellation, unreadable path, or oversized file); results may be incomplete, so a miss does not prove absence. Narrow path or glob, or set cross_filesystems=true to include mounts below the root]",
            );
        }
        Ok(output)
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
        // 不用 mmap：文件在扫描过程中变大时，读取量也不能超过字节预算。
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
    fn push(
        &mut self,
        number: Option<u64>,
        bytes: &[u8],
        matched: bool,
        span: Option<(usize, usize)>,
    ) {
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_end_matches(['\r', '\n']);
        let text = preview_line(text, self.search.config.line_max_chars, span);
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
        let span = match_span(&self.search.matcher, line.bytes());
        self.push(line.line_number(), line.bytes(), true, span);
        Ok(true)
    }
    fn context(&mut self, _: &Searcher, line: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if self.search.stopped() {
            return Ok(false);
        }
        self.push(line.line_number(), line.bytes(), false, None);
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
            "src/a.rs\n  1-context: colon\n  2:hit one\n  3-context: colon\n  4:hit two\n  5-tail"
        );
    }
    #[test]
    fn multi_row_file_groups_under_one_path_and_single_row_file_stays_inline() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "hit\nhit\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "hit\n").unwrap();
        let config = crate::config::GrepConfig {
            context_before: 0,
            context_after: 0,
            ..Default::default()
        };
        let output = run(root.path(), serde_json::json!({"pattern":"hit"}), config);
        assert_eq!(output, "a.rs\n  1:hit\n  2:hit\nb.rs:1:hit");
    }
    #[test]
    fn long_line_preview_keeps_the_match_visible() {
        let root = tempfile::tempdir().unwrap();
        let line = format!("{}NEEDLE{}", "a".repeat(700), "b".repeat(700));
        std::fs::write(root.path().join("long.txt"), format!("{line}\n")).unwrap();
        let config = crate::config::GrepConfig::default();
        let output = run(root.path(), serde_json::json!({"pattern":"NEEDLE"}), config);
        let row = output.lines().next().expect("至少一行结果");
        assert!(row.contains("NEEDLE"), "命中必须留在窗口里：{row}");
        let text = output.splitn(3, ':').nth(2).expect("预览正文");
        assert!(
            text.chars().count() <= crate::config::GrepConfig::default().line_max_chars,
            "预览超过行长度预算：{}",
            text.chars().count()
        );
        assert!(text.starts_with('…') && text.ends_with('…'), "{text}");
    }
    #[test]
    fn output_budget_caps_whole_files_and_reports_skip() {
        let root = tempfile::tempdir().unwrap();
        for path in ["a.rs", "b.rs"] {
            std::fs::write(root.path().join(path), "hit hit hit hit\n").unwrap();
        }
        let config = crate::config::GrepConfig {
            output_bytes: 40,
            context_before: 0,
            context_after: 0,
            ..Default::default()
        };
        let output = run(
            root.path(),
            serde_json::json!({"pattern":"hit"}),
            config.clone(),
        );
        assert_eq!(
            output,
            "a.rs:1:hit hit hit hit\n[1 files, 1 matches shown; output capped at 40 bytes. Continue with skip=1; narrow the pattern if a single file hits its limit]"
        );
        // 第一个文件哪怕自己就超过预算也要照常输出，否则搜索永远没有进展。
        let tiny = crate::config::GrepConfig {
            output_bytes: 1,
            ..config
        };
        let output = run(root.path(), serde_json::json!({"pattern":"hit"}), tiny);
        assert!(output.starts_with("a.rs:1:hit hit hit hit\n"), "{output}");
        assert!(output.contains("output capped at 1 bytes"), "{output}");
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
            "skill://demo/SKILL.md\n  1-first\n  2:hit\n  3-last"
        );
    }

    /// 命名空间扫描：命中行的前缀必须是和 read 一样的 URI，不能是 sidecar 路径。
    #[test]
    fn namespace_scan_prefixes_rows_with_entry_uris() {
        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("jobs");
        std::fs::create_dir_all(jobs.join("nested")).unwrap();
        std::fs::write(jobs.join("job_1.log"), "alpha\nbeta\nerror: boom\n").unwrap();
        std::fs::write(jobs.join("job_2.log"), "fine\n").unwrap();
        std::fs::write(jobs.join(".hidden.log"), "error: hidden\n").unwrap();
        std::fs::write(jobs.join("nested/job_3.log"), "error: nested\n").unwrap();
        let config = crate::config::GrepConfig::default();
        let args: GrepArgs =
            serde_json::from_value(serde_json::json!({"pattern":"error","context_before":1}))
                .unwrap();
        let entries = vec![("artifact://jobs/".to_owned(), jobs.clone())];
        let output = search_resource_namespace(&entries, &args, &config).unwrap();
        // 显式要一行上文；同一个文件的 URI 只写一次，单行文件仍是一行式。
        assert_eq!(
            output,
            "artifact://jobs/job_1.log\n  2-beta\n  3:error: boom\nartifact://jobs/nested/job_3.log:1:error: nested"
        );
        assert_eq!(
            search_resource_namespace(&[], &args, &config).unwrap(),
            "No matches found"
        );
    }

    /// 命名空间可以无限大，扫描必须有上限：超大的文件跳过并说明，文件数超限就停止。
    #[test]
    fn namespace_scan_is_bounded_and_reports_what_it_skipped() {
        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("jobs");
        std::fs::create_dir_all(&jobs).unwrap();
        std::fs::write(jobs.join("big.log"), "error: too large\n").unwrap();
        let args: GrepArgs =
            serde_json::from_value(serde_json::json!({"pattern":"error"})).unwrap();
        let small = crate::config::GrepConfig {
            max_file_bytes: 4,
            ..Default::default()
        };
        let entries = vec![("artifact://jobs/".to_owned(), jobs.clone())];
        assert_eq!(
            search_resource_namespace(&entries, &args, &small).unwrap(),
            "No matches found\n[skipped artifact://jobs/big.log: 17 B exceeds the search limit]"
        );
        for index in 0..201 {
            std::fs::write(jobs.join(format!("job_{index}.log")), "alpha\n").unwrap();
        }
        let output = search_resource_namespace(&entries, &args, &small).unwrap();
        assert!(
            output.ends_with("[namespace scan stopped after 200 files]"),
            "{output}"
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

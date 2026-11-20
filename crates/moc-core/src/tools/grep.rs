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

use super::output::ToolOutput;
use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::hashline::{AnchorStore, FileSnapshot, SnapshotId, tag_text};
use crate::resources::{ResourceBacking, ResourceRouter};

#[derive(Clone, Deserialize)]
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
    offset: Option<u64>,
    limit: Option<usize>,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    word: bool,
    #[serde(default)]
    files_only: bool,
    #[serde(default)]
    cross_filesystems: bool,
}

pub struct GrepTool;

/// 文件页与文件内续读分开记录；原生文本和 PTC 元数据共用同一份续读参数。
#[derive(Default)]
struct SearchOutput {
    content: String,
    next: Vec<Value>,
    incomplete: bool,
    matches: usize,
}

impl SearchOutput {
    fn into_outcome(self) -> ToolOutcome {
        let mut outcome = ToolOutcome::success(ToolOutput::Verbatim(self.content));
        outcome.structured_content = Some(serde_json::json!({
            "next": self.next,
            "scan_incomplete": self.incomplete,
        }));
        outcome
    }

    fn continue_file(&mut self, path: &str, offset: u64, reason: &str) {
        let next = serde_json::json!({"path": path, "offset": offset, "skip": 0});
        push_notice(
            &mut self.content,
            &format!(
                "[{reason} in {path}; continue with {next}, keeping the same pattern and options]"
            ),
        );
        self.next.push(next);
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents with Rust regex; skip pages files, offset resumes lines in one file. Single-file searches use the total match budget; multi-file searches cap each file for diversity. Local results include [path#TAG] for editing fully shown lines; shortened lines are not editable. Context defaults to zero. Does not cross mounts unless cross_filesystems=true."
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
                    "gitignore":{"type":"boolean","description":"Respect .gitignore, .ignore and git excludes; default true. Set false to search ignored files"},
                    "hidden":{"type":"boolean","description":"Include dotfiles and .git; default false"},
                    "context_before":{"type":"integer","minimum":0,"description":"Lines of context before each match; default 0"},
                    "context_after":{"type":"integer","minimum":0,"description":"Lines of context after each match; default 0"},
                    "skip":{"type":"integer","minimum":0,"description":"Matched files to skip, for paging"},
                    "offset":{"type":"integer","minimum":1,"description":"First line to match (1-based); requires one file or document URI. Use returned next parameters to resume"},
                    "limit":{"type":"integer","minimum":1,"description":"Maximum matches to show (files with files_only); capped by the total match budget"},
                    "literal":{"type":"boolean","description":"Treat pattern as a literal string"},
                    "word":{"type":"boolean","description":"Match whole words only"},
                    "files_only":{"type":"boolean","description":"List matching file paths only"},
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
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", format!(
                    "grep: {error}. Use skip for file pages and offset for lines within one file; read tool://grep for the schema."
                )))?;
            if args.offset == Some(0) {
                return Err(ToolError::new(
                    "E_ARGUMENT",
                    "grep offset is a 1-based line number",
                ));
            }
            if let Some(path) = args.path.as_deref()
                && let Some(resource) = ResourceRouter::resolve_read(ctx, path)
                    .await
                    .map_err(|error| ToolError::new(error.code, error.message))?
            {
                let output = match resource.backing {
                    ResourceBacking::File(path) => {
                        search_resource_file(&resource.uri, &path, &args, &ctx.config.tools.grep)?
                    }
                    ResourceBacking::Text(text) => {
                        search_resource_text(&resource.uri, &text, &args, &ctx.config.tools.grep)?
                    }
                    ResourceBacking::Namespace { entries, .. } => {
                        search_resource_namespace(&entries, &args, &ctx.config.tools.grep)?
                    }
                };
                return Ok(output.into_outcome());
            }
            let cancel = ctx.cancel.child_token();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let search = Search::new(args, ctx.cwd.clone(), ctx.config.tools.grep.clone(), cancel)?;
            let anchors = std::sync::Arc::clone(&ctx.anchors);
            let max_edit_bytes = ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024);
            let result = tokio::task::spawn_blocking(move || search.run(&anchors, max_edit_bytes))
                .await
                .map_err(|error| ToolError::new("E_GREP", error.to_string()))??;
            Ok(result.into_outcome())
        })
    }
}

fn search_resource_file(
    uri: &str,
    path: &Path,
    args: &GrepArgs,
    config: &crate::config::GrepConfig,
) -> Result<SearchOutput, ToolError> {
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

/// Search::new 与资源路径共用的 matcher 构建。
fn build_matcher(args: &GrepArgs) -> Result<grep_regex::RegexMatcher, ToolError> {
    grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(!args.case.unwrap_or(true))
        .fixed_strings(args.literal)
        .word(args.word)
        .build(&args.pattern)
        .map_err(|error| {
            let error = ToolError::new("E_BAD_REGEX", error.to_string());
            if args.literal {
                error
            } else {
                error.with_hint("escape regex metacharacters such as ( [ { or set literal=true")
            }
        })
}

/// 本次调用的总匹配预算：`limit` 参数在全局预算内生效。
fn match_budget(args: &GrepArgs, config: &crate::config::GrepConfig) -> usize {
    args.limit
        .unwrap_or(config.max_matches_total)
        .min(config.max_matches_total)
}

fn search_resource_text(
    uri: &str,
    text: &str,
    args: &GrepArgs,
    config: &crate::config::GrepConfig,
) -> Result<SearchOutput, ToolError> {
    let mut output = collect_resource_matches(uri, text, args, config)?;
    if output.content.is_empty() {
        output.content = if args.skip.unwrap_or(0) > 0 {
            "No more matching files"
        } else {
            "No matches found"
        }
        .into();
    }
    Ok(output)
}

/// 在命名空间的各个目录里逐个文件匹配。命中行的前缀用「URI 前缀 + 相对路径」拼出，
/// 模型看到的是与 read 一致的 URI，而不是 urlfs 或 sidecar 的真实路径。
fn search_resource_namespace(
    entries: &[(String, PathBuf)],
    args: &GrepArgs,
    config: &crate::config::GrepConfig,
) -> Result<SearchOutput, ToolError> {
    if args.offset.is_some() {
        return Err(ToolError::new(
            "E_ARGUMENT",
            "grep offset requires one file, not a namespace",
        ));
    }
    /// 一次命名空间扫描最多检查的文件数：命名空间可以无限大，工具输出必须有上限。
    const MAX_FILES: usize = 200;
    let budget = match_budget(args, config);
    // files_only 只列路径，每页文件数直接用总预算，不走 max_files 的分页档。
    let file_cap = if args.files_only {
        budget
    } else {
        config.max_files
    };
    let mut output = SearchOutput::default();
    let mut notices = Vec::new();
    let mut scanned = 0usize;
    let mut matched_files = 0usize;
    let mut shown_files = 0usize;
    let mut stopped = false;
    let mut file_args = args.clone();
    file_args.skip = None;
    let mut file_config = config.clone();
    'entries: for (prefix, directory) in entries {
        let remaining = MAX_FILES - scanned;
        let (files, truncated, enumeration_notices) =
            directory_files(directory, remaining, prefix);
        notices.extend(enumeration_notices);
        // 截断先置位：分页提前退出时也要报告扫描不完整。
        stopped |= truncated;
        for (relative, path) in files {
            scanned += 1;
            let uri = format!("{prefix}{relative}");
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    notices.push(format!("[skipped {uri}: {error}]"));
                    output.incomplete = true;
                    continue;
                }
            };
            if metadata.len() > config.max_file_bytes {
                notices.push(format!(
                    "[skipped {uri}: {} B exceeds the search limit]",
                    metadata.len()
                ));
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    notices.push(format!("[skipped {uri}: {error}]"));
                    output.incomplete = true;
                    continue;
                }
            };
            file_config.max_matches_total = config
                .max_matches_per_file
                .min(budget.saturating_sub(output.matches))
                .max(1);
            let matches = collect_resource_matches(
                &uri,
                &String::from_utf8_lossy(&bytes),
                &file_args,
                &file_config,
            )?;
            if matches.content.is_empty() {
                continue;
            }
            matched_files += 1;
            if matched_files <= args.skip.unwrap_or(0) {
                continue;
            }
            if shown_files >= file_cap
                || output.matches >= budget
                || (!output.content.is_empty()
                    && output.content.len() + matches.content.len() + 1 > config.output_bytes)
            {
                let skip = args.skip.unwrap_or(0) + shown_files;
                push_notice(
                    &mut output.content,
                    &format!("[More matching files; continue with skip={skip}]"),
                );
                output.next.push(serde_json::json!({"skip": skip}));
                break 'entries;
            }
            push_notice(&mut output.content, &matches.content);
            output.next.extend(matches.next);
            output.matches += matches.matches;
            shown_files += 1;
        }
        if truncated {
            break 'entries;
        }
    }
    if output.content.is_empty() {
        output.content.push_str("No matches found");
    }
    output.incomplete |= stopped || !notices.is_empty();
    for notice in notices {
        push_notice(&mut output.content, &notice);
    }
    if stopped {
        push_notice(
            &mut output.content,
            &format!("[namespace scan stopped after {MAX_FILES} files]"),
        );
    }
    Ok(output)
}

/// 最多列出 limit 个文件及其相对路径（用 `/` 分隔），跳过点文件。
/// 返回（文件, 是否还有文件没枚举到, 枚举失败的提示）；提示用 `uri_prefix` + 相对
/// 路径拼 URI，不暴露真实磁盘路径。只有真遇到第 limit+1 个文件才算截断。
fn directory_files(
    directory: &Path,
    limit: usize,
    uri_prefix: &str,
) -> (Vec<(String, PathBuf)>, bool, Vec<String>) {
    const MAX_NOTICES: usize = 8;
    let mut files = Vec::new();
    let mut notices = Vec::new();
    let mut failed = 0usize;
    let mut stack = vec![(String::new(), directory.to_owned())];
    let mut truncated = false;
    'walk: while let Some((prefix, current)) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(error) => {
                failed += 1;
                if notices.len() < MAX_NOTICES {
                    notices.push(format!("[skipped {uri_prefix}{prefix}: {error}]"));
                }
                continue;
            }
        };
        let mut listing = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => listing.push(entry),
                Err(error) => {
                    failed += 1;
                    if notices.len() < MAX_NOTICES {
                        notices.push(format!("[skipped {uri_prefix}{prefix}: {error}]"));
                    }
                }
            }
        }
        // read_dir 顺序不稳定：逐层排序让达到上限时截取的文件子集可复现。
        listing.sort_unstable_by_key(std::fs::DirEntry::file_name);
        let mut subdirs = Vec::new();
        for entry in listing {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    failed += 1;
                    if notices.len() < MAX_NOTICES {
                        notices.push(format!("[skipped {uri_prefix}{relative}: {error}]"));
                    }
                    continue;
                }
            };
            if file_type.is_dir() {
                subdirs.push((relative, entry.path()));
            } else if file_type.is_file() {
                if files.len() >= limit {
                    truncated = true;
                    break 'walk;
                }
                files.push((relative, entry.path()));
            }
        }
        // 栈是后进先出：倒序压栈让子目录仍按排序序展开。
        for subdir in subdirs.into_iter().rev() {
            stack.push(subdir);
        }
    }
    if failed > notices.len() {
        let omitted = failed - notices.len();
        notices.push(format!("[{omitted} more unreadable entries skipped]"));
    }
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    (files, truncated, notices)
}

/// 取出一段文本里的命中行；没有命中时返回空串，由调用方决定报告无结果还是继续拼下一段。
fn collect_resource_matches(
    uri: &str,
    text: &str,
    args: &GrepArgs,
    config: &crate::config::GrepConfig,
) -> Result<SearchOutput, ToolError> {
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
    let mut output = SearchOutput::default();
    if args.skip.unwrap_or(0) > 0 {
        return Ok(output);
    }
    let matcher = build_matcher(args)?;
    let budget = match_budget(args, config);
    let lines: Vec<&str> = text.lines().collect();
    let mut matched = vec![false; lines.len()];
    let mut hits = Vec::new();
    let first = args.offset.unwrap_or(1).saturating_sub(1);
    for (index, line) in lines.iter().enumerate() {
        if (index as u64) < first {
            continue;
        }
        if matcher
            .is_match(line.as_bytes())
            .map_err(|error| ToolError::new("E_GREP", error.to_string()))?
        {
            if args.files_only {
                output.content = uri.to_owned();
                output.matches = 1;
                return Ok(output);
            }
            matched[index] = true;
            hits.push(index);
            if hits.len() > budget {
                break;
            }
        }
    }
    let truncated = hits.len() > budget;
    hits.truncate(budget);
    output.matches = hits.len();
    let next_line = hits.last().map(|&index| index as u64 + 2);
    let selected = hits;
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
    output.content = render_group(uri, &rows);
    if truncated && let Some(offset) = next_line {
        output.continue_file(uri, offset, "Match budget reached");
    }
    Ok(output)
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
    single_file: bool,
    /// 本次调用的总匹配预算：`limit` 参数在全局预算内生效。
    match_budget: usize,
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
        let matcher = build_matcher(&args)?;
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
        // cwd 与 walker 产出需同形（canonical）才能相对化展示路径；
        // macOS 上 cwd 常是 /var/...，walker 给的是 /private/var/...。
        let cwd = crate::tools::normalized(&cwd);
        let root = crate::tools::resolve_scan_root(&cwd, args.path.as_deref())?;
        let single_file = root.is_file();
        if args.offset.is_some() && !single_file {
            return Err(ToolError::new(
                "E_ARGUMENT",
                "grep offset requires one file; narrow path first",
            ));
        }
        let deadline = Instant::now() + Duration::from_millis(config.timeout_ms);
        let match_budget = match_budget(&args, &config);
        Ok(Self {
            args,
            root,
            cwd,
            matcher,
            include,
            config,
            cancel,
            deadline,
            single_file,
            match_budget,
        })
    }

    fn stopped(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }

    fn run(&self, anchors: &AnchorStore, max_edit_bytes: u64) -> Result<SearchOutput, ToolError> {
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
        let mut page = SearchOutput::default();
        let mut matched_files = 0usize;
        let mut shown_files = 0usize;
        let mut shown_matches = 0usize;
        let mut limited = false;
        let mut capped = false;
        let mut skipped_large_files = 0usize;
        // 逐条提示有上限：不可读文件可能上万个，提示本身不能撑爆输出预算。
        const MAX_SKIPPED_NOTICES: usize = 8;
        let mut skipped_unreadable = 0usize;
        let mut skipped_files = Vec::new();
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
                    Err(error) => {
                        incomplete.store(true, Ordering::Relaxed);
                        skipped_unreadable += 1;
                        if skipped_files.len() < MAX_SKIPPED_NOTICES {
                            let relative = path.strip_prefix(&self.cwd).unwrap_or(path);
                            skipped_files
                                .push(format!("[skipped {}: {error}]", relative.display()));
                        }
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
                // files_only：每个命中文件只输出相对路径一行；每页文件数直接用
                // 总匹配预算（limit 可调），不走 max_files 的分页档。
                if self.args.files_only {
                    if shown_files >= self.match_budget {
                        limited = true;
                        break 'blocks;
                    }
                    let relative = path.strip_prefix(&self.cwd).unwrap_or(path);
                    let line = relative.display().to_string();
                    let needed = line.len() + usize::from(!output.is_empty());
                    if !output.is_empty() && output.len() + needed > self.config.output_bytes {
                        capped = true;
                        break 'blocks;
                    }
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&line);
                    shown_files += 1;
                    shown_matches += 1;
                    continue;
                }
                if shown_files >= self.config.max_files || shown_matches >= self.match_budget {
                    limited = true;
                    break 'blocks;
                }
                let allowance = self.match_budget - shown_matches;
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
                // 只为即将展示的匹配文件取编辑快照；搜索时的文件身份必须仍然一致。
                let snapshot = edit_snapshot(path, result.epoch, max_edit_bytes);
                let relative = relative.display().to_string();
                // tag 固定四位：先计算精确输出预算，确认整组可见后才登记并填入真实 tag。
                let display = if snapshot.is_some() {
                    format!("[{relative}#0000]")
                } else {
                    relative.clone()
                };
                let mut group = render_group(&display, &rows);
                let needed = group.len() + usize::from(!output.is_empty());
                // 整组判断，不切半个文件：预算封顶时的 skip 才是精确的。
                if !output.is_empty() && output.len() + needed > self.config.output_bytes {
                    capped = true;
                    break 'blocks;
                }
                if let Some((path, snapshot)) = snapshot {
                    let windows = editable_windows(&rows, &snapshot);
                    let tag = anchors.record_served(path, &snapshot, &windows);
                    group.replace_range(relative.len() + 2..relative.len() + 6, &tag_text(tag));
                }
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&group);
                shown_files += 1;
                shown_matches += count;
                if result.truncated || result.matches > count {
                    page.continue_file(&relative, last_match + 1, "Match budget reached");
                }
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
                    "[{shown_files} files, {shown_matches} matches shown; output capped at {} bytes. More matching files: continue with skip={}]",
                    self.config.output_bytes,
                    skip + shown_files
                ),
            );
        } else if limited {
            push_notice(
                &mut output,
                &format!(
                    "[{shown_files} files, {shown_matches} matches shown; file-page or total-match budget reached. More matching files: continue with skip={}]",
                    skip + shown_files
                ),
            );
        }
        if capped || limited {
            page.next
                .push(serde_json::json!({"skip": skip + shown_files}));
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
        for notice in &skipped_files {
            push_notice(&mut output, notice);
        }
        if skipped_unreadable > skipped_files.len() {
            push_notice(
                &mut output,
                &format!(
                    "[{} more unreadable files skipped]",
                    skipped_unreadable - skipped_files.len()
                ),
            );
        }
        if incomplete.load(Ordering::Relaxed) || self.stopped() {
            page.incomplete = true;
            push_notice(
                &mut output,
                "[Scan incomplete (time limit, candidate-file limit, cancellation, unreadable path, or oversized file); results may be incomplete, so a miss does not prove absence. Narrow path or glob, or set cross_filesystems=true to include mounts below the root]",
            );
        }
        if !page.content.is_empty() {
            push_notice(&mut output, &page.content);
        }
        page.content = output;
        page.matches = shown_matches;
        Ok(page)
    }
    fn search_file(&self, path: &Path) -> std::io::Result<Matches> {
        let mut sink = Matches::default();
        if self.stopped() {
            return Ok(sink);
        }
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() > self.config.max_file_bytes {
            sink.skipped = true;
            return Ok(sink);
        }
        let epoch = SnapshotId::from_metadata(&metadata);
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
            (&file).take(self.config.max_file_bytes),
            MatchSink {
                search: self,
                result: &mut sink,
            },
        )?;
        if SnapshotId::from_metadata(&file.metadata()?) == epoch {
            sink.epoch = Some(epoch);
        }
        Ok(sink)
    }
}

/// 搜索与 read 使用相同的快照身份；文件变化或超出编辑上限时只返回搜索预览。
fn edit_snapshot(
    path: &Path,
    epoch: Option<SnapshotId>,
    max_bytes: u64,
) -> Option<(PathBuf, FileSnapshot)> {
    let epoch = epoch?;
    let path = std::fs::canonicalize(path).ok()?;
    let snapshot = crate::hashline::served::read_snapshot(&path, max_bytes).ok()?;
    let mut identity = snapshot.epoch;
    identity.content_hash = 0;
    (identity == epoch && std::str::from_utf8(&snapshot.bytes).is_ok()).then_some((path, snapshot))
}

/// 只登记完整且逐字匹配快照的行；长行预览、BOM 和不同换行坐标都不会误授权。
fn editable_windows(rows: &[Row], snapshot: &FileSnapshot) -> Vec<std::ops::Range<usize>> {
    let mut windows: Vec<std::ops::Range<usize>> = Vec::new();
    for row in rows {
        let position = row.number as usize - 1;
        if snapshot.lines.get(position) != Some(&row.text) {
            continue;
        }
        match windows.last_mut() {
            Some(last) if position == last.end => last.end += 1,
            _ => windows.push(position..position + 1),
        }
    }
    windows
}

#[derive(Default)]
struct Matches {
    rows: Vec<Row>,
    matches: usize,
    truncated: bool,
    skipped: bool,
    epoch: Option<SnapshotId>,
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
        if line.line_number().unwrap_or(0) < self.search.args.offset.unwrap_or(1) {
            return Ok(true);
        }
        // files_only 只要知道命中过：首个命中后即停止读取。
        if self.search.args.files_only {
            self.result.matches += 1;
            return Ok(false);
        }
        let limit = if self.search.single_file {
            self.search.match_budget
        } else {
            self.search
                .config
                .max_matches_per_file
                .min(self.search.match_budget)
        };
        if self.result.matches >= limit {
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
    pub(crate) fn run(root: &Path, args: Value, config: crate::config::GrepConfig) -> String {
        Search::new(
            serde_json::from_value(args).unwrap(),
            root.to_owned(),
            config,
            CancellationToken::new(),
        )
        .unwrap()
        .run(&AnchorStore::new(), 4 * 1024 * 1024)
        .unwrap()
        .content
    }

    #[test]
    fn single_file_and_resource_pages_resume_without_losing_matches() {
        let root = tempfile::tempdir().unwrap();
        let text = (1..=205).map(|n| format!("hit {n}\n")).collect::<String>();
        std::fs::write(root.path().join("hits.txt"), &text).unwrap();
        let config = crate::config::GrepConfig::default();
        let args = serde_json::json!({"path":"hits.txt", "pattern":"hit"});
        let page = Search::new(
            serde_json::from_value(args.clone()).unwrap(),
            root.path().to_owned(),
            config.clone(),
            CancellationToken::new(),
        )
        .unwrap()
        .run(&AnchorStore::new(), 4 * 1024 * 1024)
        .unwrap();
        assert!(page.content.contains("200:hit 200"));
        assert!(!page.content.contains("201:hit 201"));
        assert_eq!(
            page.next,
            [serde_json::json!({"path":"hits.txt", "offset":201, "skip":0})]
        );
        let mut next = args;
        next.as_object_mut()
            .unwrap()
            .extend(page.next[0].as_object().unwrap().clone());
        let rest = Search::new(
            serde_json::from_value(next).unwrap(),
            root.path().to_owned(),
            config.clone(),
            CancellationToken::new(),
        )
        .unwrap()
        .run(&AnchorStore::new(), 4 * 1024 * 1024)
        .unwrap();
        assert!(rest.content.contains("201:hit 201") && rest.content.contains("205:hit 205"));
        assert!(!rest.content.contains("200:hit 200"));
        assert!(rest.next.is_empty());

        let uri = "artifact://artifacts/hits.txt";
        let args: GrepArgs = serde_json::from_value(serde_json::json!({"pattern":"hit"})).unwrap();
        let page = search_resource_text(uri, &text, &args, &config).unwrap();
        let mut next = page.next[0].clone();
        next["pattern"] = "hit".into();
        let rest =
            search_resource_text(uri, &text, &serde_json::from_value(next).unwrap(), &config)
                .unwrap();
        assert!(rest.content.contains("201:hit 201") && rest.content.contains("205:hit 205"));
        assert!(rest.next.is_empty());
    }

    #[test]
    fn directory_caps_name_each_file_and_file_pages_do_not_skip_matches() {
        let root = tempfile::tempdir().unwrap();
        let text = "hit\n".repeat(25);
        for name in ["a.txt", "b.txt"] {
            std::fs::write(root.path().join(name), &text).unwrap();
        }
        let config = crate::config::GrepConfig {
            max_files: 1,
            ..Default::default()
        };
        let page = Search::new(
            serde_json::from_value(serde_json::json!({"pattern":"hit"})).unwrap(),
            root.path().to_owned(),
            config.clone(),
            CancellationToken::new(),
        )
        .unwrap()
        .run(&AnchorStore::new(), 4096)
        .unwrap();
        assert_eq!(
            page.next,
            [
                serde_json::json!({"path":"a.txt","offset":21,"skip":0}),
                serde_json::json!({"skip":1})
            ]
        );
        let rest = Search::new(
            serde_json::from_value(serde_json::json!({"pattern":"hit","skip":1})).unwrap(),
            root.path().to_owned(),
            config,
            CancellationToken::new(),
        )
        .unwrap()
        .run(&AnchorStore::new(), 4096)
        .unwrap();
        assert!(rest.content.contains("[b.txt#") && !rest.content.contains("[a.txt#"));
        assert_eq!(
            rest.next,
            [serde_json::json!({"path":"b.txt","offset":21,"skip":0})]
        );
    }
    #[test]
    fn only_emitted_complete_rows_become_editable() {
        use crate::hashline::apply::{EditOp, EditRequest, plan_edit};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.txt");
        std::fs::write(&path, format!("hit\n{}\nend\n", "x".repeat(100))).unwrap();
        std::fs::write(root.path().join("b.txt"), "hit\n").unwrap();
        let config = crate::config::GrepConfig {
            line_max_chars: 16,
            output_bytes: 1,
            ..Default::default()
        };
        let search = Search::new(
            serde_json::from_value(serde_json::json!({"pattern":"hit", "context_after":2}))
                .unwrap(),
            root.path().to_owned(),
            config,
            CancellationToken::new(),
        )
        .unwrap();
        let anchors = AnchorStore::new();
        let output = search.run(&anchors, 4096).unwrap().content;
        // 锚点按 walker 产出的规范化路径登记（macOS /var→/private/var）。
        let served = anchors.served(&path.canonicalize().unwrap()).unwrap();
        let tag = tag_text(served.current_tag().unwrap());
        assert!(output.contains(&format!("[a.txt#{tag}]")));
        assert!(served.is_seen(0) && served.is_seen(2));
        assert!(!served.is_seen(1));
        assert!(
            anchors
                .served(&root.path().join("b.txt").canonicalize().unwrap())
                .is_none()
        );
        let mut request = EditRequest {
            path: "a.txt".into(),
            tag,
            move_lines: None,
            edits: vec![EditOp::Replace {
                from: 3,
                to: 3,
                text: "updated".into(),
            }],
        };
        let plan = plan_edit(root.path(), &request, &served, None, 32, 4096).unwrap();
        assert_eq!(plan.lines[2], "updated");
        request.edits = vec![EditOp::Delete { from: 2, to: 2 }];
        assert!(
            plan_edit(root.path(), &request, &served, None, 32, 4096)
                .unwrap_err()
                .to_string()
                .contains("E_UNSEEN")
        );

        let result = search.search_file(&path).unwrap();
        std::fs::write(&path, "changed after search\n").unwrap();
        assert!(edit_snapshot(&path, result.epoch, 4096).is_none());
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
        assert!(output.contains("[a.rs#"));
        assert!(!output.contains("[b.rs#"));
        assert!(output.contains("skip=1"));
        // 第一个文件哪怕自己就超过预算也要照常输出，否则搜索永远没有进展。
        let tiny = crate::config::GrepConfig {
            output_bytes: 1,
            ..config
        };
        let output = run(root.path(), serde_json::json!({"pattern":"hit"}), tiny);
        assert!(
            output.contains("[a.rs#") && !output.contains("[b.rs#"),
            "{output}"
        );
        assert!(output.contains("output capped at 1 bytes"), "{output}");
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
        .unwrap()
        .content;
        assert_eq!(
            output,
            "skill://demo/SKILL.md\n  1-first\n  2:hit\n  3-last"
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
            search_resource_namespace(&entries, &args, &small)
                .unwrap()
                .content,
            "No matches found\n[skipped artifact://jobs/big.log: 17 B exceeds the search limit]"
        );
        for index in 0..201 {
            std::fs::write(jobs.join(format!("job_{index}.log")), "alpha\n").unwrap();
        }
        let output = search_resource_namespace(&entries, &args, &small)
            .unwrap()
            .content;
        assert!(
            output.ends_with("[namespace scan stopped after 200 files]"),
            "{output}"
        );

        // 恰好 200 个文件时全数搜完，不误报"扫描已停止"；读不出的文件点名提示。
        let exact = root.path().join("exact");
        std::fs::create_dir_all(&exact).unwrap();
        for index in 0..200 {
            std::fs::write(exact.join(format!("f_{index:03}.log")), "beta\n").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let skip = root.path().join("skip");
            std::fs::create_dir_all(&skip).unwrap();
            std::fs::write(skip.join("ok.log"), "beta\n").unwrap();
            let unreadable = skip.join("unreadable.log");
            std::fs::write(&unreadable, "beta\n").unwrap();
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
                .unwrap();
            let output = search_resource_namespace(
                &[("artifact://skip/".to_owned(), skip)],
                &args,
                &small,
            )
            .unwrap()
            .content;
            assert!(
                output.contains("[skipped artifact://skip/unreadable.log:"),
                "{output}"
            );
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        let output = search_resource_namespace(
            &[("artifact://exact/".to_owned(), exact)],
            &args,
            &small,
        )
        .unwrap()
        .content;
        assert!(
            !output.contains("namespace scan stopped"),
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
        assert!(default.contains("[visible.txt#") && default.contains(":1:hit"));
        assert!(!default.contains(".env") && !default.contains(".git"));
        let included = run(
            root.path(),
            serde_json::json!({"pattern":"hit","hidden":true}),
            config,
        );
        assert!(included.contains("[.env#"));
        assert!(included.contains("[.git/objects/pack#"));
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
        .run(&AnchorStore::new(), 4 * 1024 * 1024)
        .expect("执行扫描")
        .content;
        assert!(output.contains("incomplete"));
        assert!(!output.contains("No matches found"));
    }
}

#[cfg(test)]
mod pattern_mode_tests {
    use super::*;
    fn run(root: &Path, args: Value) -> String {
        super::tests::run(root, args, crate::config::GrepConfig::default())
    }

    #[test]
    fn literal_treats_pattern_as_plain_text() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "a.b\naxb\n").unwrap();
        // 不带 literal 时 `a.b` 是正则，两行都中；literal 后只中字面行。
        let regex_out = run(root.path(), serde_json::json!({"pattern":"a.b"}));
        assert!(regex_out.contains("axb"), "{regex_out}");
        let output = run(
            root.path(),
            serde_json::json!({"pattern":"a.b","literal":true}),
        );
        assert!(output.contains(":a.b"), "{output}");
        assert!(!output.contains("axb"), "{output}");
    }

    #[test]
    fn word_only_matches_whole_words() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "cat\ncatapult\nmy cat\n").unwrap();
        let output = run(
            root.path(),
            serde_json::json!({"pattern":"cat","word":true}),
        );
        assert!(output.contains(":cat"), "{output}");
        assert!(output.contains(":my cat"), "{output}");
        assert!(!output.contains("catapult"), "{output}");
    }

    #[test]
    fn files_only_lists_paths_once() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "hit\nhit\nhit\n").unwrap();
        std::fs::write(root.path().join("b.txt"), "nope\n").unwrap();
        std::fs::write(root.path().join("c.txt"), "hit\n").unwrap();
        let output = run(
            root.path(),
            serde_json::json!({"pattern":"hit","files_only":true}),
        );
        let mut lines: Vec<&str> = output.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, ["a.txt", "c.txt"], "{output}");

        // files_only 的每页上限是总匹配预算（默认 200），不再受 max_files(20) 约束。
        for index in 0..25 {
            std::fs::write(root.path().join(format!("many_{index:02}.txt")), "hit\n").unwrap();
        }
        let output = run(
            root.path(),
            serde_json::json!({"pattern":"hit","files_only":true}),
        );
        assert_eq!(output.lines().count(), 27, "{output}");
        // limit 在总预算内截断结果，续页参数照旧。
        let output = run(
            root.path(),
            serde_json::json!({"pattern":"hit","files_only":true,"limit":3}),
        );
        assert_eq!(
            output.lines().filter(|line| !line.starts_with('[')).count(),
            3,
            "{output}"
        );
        assert!(output.contains("skip=3"), "{output}");
    }

    #[test]
    fn resource_files_only_lists_uri() {
        let args: GrepArgs =
            serde_json::from_value(serde_json::json!({"pattern":"hit","files_only":true})).unwrap();
        let output = collect_resource_matches(
            "local://x.md",
            "hit\nmiss\n",
            &args,
            &crate::config::GrepConfig::default(),
        )
        .unwrap()
        .content;
        assert_eq!(output, "local://x.md");
    }
}

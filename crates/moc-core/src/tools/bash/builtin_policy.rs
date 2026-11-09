//! 内置工具优先策略：bash 调用如果只是在读/搜/列/改工作区文件，拒绝并给出
//! 等价的 read/grep/find/edit/write 调用。
//!
//! 判定分两层：整条管道规约到「一个读/搜/列命令」时产出 read/grep/find
//! 建议；逐段的写/改检测（原地编辑器、tee、字面量重定向、解释器内联）在
//! 管道的任何位置都算命中。不可翻译的一律放行，宁缺毋滥。

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use grep_matcher::Matcher;
use serde_json::{Map, Value};

use super::syntax::{self, Arg, Cmd, Pipe, Redirect, Simple};

/// 拒绝时给模型的等价内置工具调用。
pub(crate) struct Suggestion {
    /// read / grep / find / edit / write。
    pub(crate) tool: &'static str,
    pub(crate) args: Map<String, Value>,
    /// 仅 edit/write：被替代的 shell 写法，如 "sed -i"、"redirect"。
    pub(crate) via: Option<&'static str>,
}

/// 一次判定的结果：建议列表 + 是否还有未覆盖的非平凡 shell 步骤。
pub(crate) struct Verdict {
    pub(crate) suggestions: Vec<Suggestion>,
    pub(crate) leftover: bool,
}

/// `None` = 放行；`Some` = 拒绝并给出建议。
///
/// `root` 是会话 cwd（工作区根），`dir` 是 bash 实际 cwd（resolve_cwd 的
/// 结果），`stdin` 表示调用带了 bash 的 stdin 参数。
pub(crate) fn check(command: &str, root: &Path, dir: &Path, stdin: bool) -> Option<Verdict> {
    eval(
        command,
        Scope {
            root: root.to_owned(),
            dir: Some(dir.to_owned()),
            stdin,
        },
    )
}

/// 拒绝消息的渲染：逐条列出等价调用，末尾按需提示剩余步骤另起 bash。
pub(crate) fn render(verdict: &Verdict) -> String {
    let mut out = String::from(
        "This bash command only reads, searches, lists, or edits workspace files; use built-in tools instead:",
    );
    for suggestion in &verdict.suggestions {
        out.push_str(&format!(
            "\n- {} {}",
            suggestion.tool,
            Value::Object(suggestion.args.clone())
        ));
        match (suggestion.tool, suggestion.via) {
            ("edit", Some(via)) => out.push_str(&format!(
                " (instead of {via}: read the file, then edit the anchored lines)"
            )),
            ("write", Some(via)) => {
                out.push_str(&format!(" (instead of {via}: pass the full content)"))
            }
            _ => {}
        }
    }
    if verdict.leftover {
        out.push_str("\nRun the remaining shell steps in a separate bash call.");
    }
    out
}

/// 判定时的环境：工作区根、当前有效目录（`cd` 可更新，未知时为 `None`）、
/// bash 调用是否带 stdin 字面量。
#[derive(Clone)]
struct Scope {
    root: PathBuf,
    dir: Option<PathBuf>,
    stdin: bool,
}

fn eval(command: &str, mut scope: Scope) -> Option<Verdict> {
    let tree = syntax::tree(command)?;
    let mut verdict = Verdict {
        suggestions: Vec::new(),
        leftover: false,
    };
    for pipe in &tree.pipelines {
        eval_pipe(pipe, &mut scope, &mut verdict);
    }
    if verdict.suggestions.is_empty() {
        None
    } else {
        Some(verdict)
    }
}

fn eval_pipe(pipe: &Pipe, scope: &mut Scope, verdict: &mut Verdict) {
    // 顶层 `cd` 更新后续管道的有效目录；`cd` 无参数或参数非静态时，
    // 之后的相对路径一律视为非静态。
    if let [Cmd::Simple(simple)] = pipe.commands.as_slice()
        && verb(simple) == Some("cd")
    {
        scope.dir = match &simple.argv[1..] {
            [arg] if arg.value.is_some() && !arg.glob => {
                resolve_dir(arg.value.as_deref().expect("已检查 Some"), scope)
            }
            _ => None,
        };
        if !writes_file(simple) {
            return;
        }
    }
    let mut simples: Vec<&Simple> = Vec::new();
    let mut all_simple = true;
    for cmd in &pipe.commands {
        match cmd {
            Cmd::Simple(simple) => simples.push(simple),
            Cmd::Compound(inner) => {
                all_simple = false;
                // 体内的 `cd` 不影响外层：按当前目录的副本递归。
                let mut inner_scope = scope.clone();
                for inner in &inner.pipelines {
                    eval_pipe(inner, &mut inner_scope, verdict);
                }
            }
            Cmd::Opaque => all_simple = false,
        }
    }
    // 逐段的写/改检测；记录每段是否被建议覆盖。
    let mut covered = Vec::with_capacity(simples.len());
    for (index, simple) in simples.iter().enumerate() {
        let before = verdict.suggestions.len();
        detect_edit(simple, index, &simples, scope, verdict);
        covered.push(verdict.suggestions.len() > before);
    }
    // 字面量生产者喂给已覆盖下游段时，它本身也算被覆盖（cat <<EOF | tee F）。
    for index in (1..simples.len()).rev() {
        if covered[index] && is_literal_producer(simples[index - 1], index - 1, scope) {
            covered[index - 1] = true;
        }
    }
    // 整条管道规约到读/搜/列命令时才翻译。
    let translated = all_simple
        && translate_pipe(&simples, scope).is_some_and(|suggestions| {
            verdict.suggestions.extend(suggestions);
            true
        });
    if !translated {
        let mut index = 0usize;
        for cmd in &pipe.commands {
            match cmd {
                Cmd::Simple(simple) => {
                    if !covered[index] && nontrivial(simple) {
                        verdict.leftover = true;
                    }
                    index += 1;
                }
                // 无法判定的结构按还有 shell 步骤处理。
                Cmd::Opaque => verdict.leftover = true,
                // 复合体内部各管道已各自结算。
                Cmd::Compound(_) => {}
            }
        }
    }
}

/// 未命中时判断是否还有值得单独跑的 shell 步骤：cd/echo/printf/true/:
/// 且无文件写入重定向的命令，以及只剩赋值/重定向的空命令，都是平凡的。
fn nontrivial(simple: &Simple) -> bool {
    if writes_file(simple) {
        return true;
    }
    match verb(simple) {
        Some("cd" | "echo" | "printf" | "true" | ":") => false,
        Some(_) => true,
        None => false,
    }
}

/// 命令的 stdout（fd 1）是否重定向到文件。
fn writes_file(simple: &Simple) -> bool {
    simple
        .redirects
        .iter()
        .any(|r| matches!(r, Redirect::Output { fd: 1, .. }))
}

fn verb(simple: &Simple) -> Option<&str> {
    simple.argv.first()?.value.as_deref().map(basename)
}

fn basename(word: &str) -> &str {
    Path::new(word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(word)
}

/// 静态路径操作数的判定结果。
enum Operand {
    /// 规范化后在工作区内的绝对路径。
    Workspace(PathBuf),
    /// 注册过的资源 URI（read/grep 可翻译，find 不接受）。
    Uri(String),
    /// 工作区外（含 /tmp）或不是静态词：不翻译。
    Other,
}

/// 路径操作数：必须静态且不含 glob；相对路径基于当前有效目录；规范化后
/// 落在 root 下才算工作区内。资源 URI 原样保留。
fn operand(arg: &Arg, scope: &Scope) -> Operand {
    if arg.glob {
        return Operand::Other;
    }
    let Some(text) = arg.value.as_deref() else {
        return Operand::Other;
    };
    if crate::resources::is_resource_uri(text) {
        return Operand::Uri(text.to_owned());
    }
    let path = Path::new(text);
    let absolute = if path.is_absolute() {
        normalize(path)
    } else {
        let Some(dir) = &scope.dir else {
            return Operand::Other;
        };
        normalize(&dir.join(path))
    };
    if absolute.starts_with(&scope.root) {
        Operand::Workspace(absolute)
    } else {
        Operand::Other
    }
}

/// 词法规范化：折叠 `.`/`..`，不做 canonicalize，不要求路径存在。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            part => out.push(part.as_os_str()),
        }
    }
    out
}

/// `cd` 参数解析成新的有效目录；相对路径基于当前目录，目录未知则无法解析。
fn resolve_dir(arg: &str, scope: &Scope) -> Option<PathBuf> {
    if crate::resources::is_resource_uri(arg) {
        return None;
    }
    let path = Path::new(arg);
    Some(if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&scope.dir.as_ref()?.join(path))
    })
}

/// 操作数对应的 path 参数：工作区内取相对 root 的路径（等于 root 时用
/// `"."`），URI 原样；`Other` 返回 `None`。
fn operand_path(op: &Operand, scope: &Scope) -> Option<String> {
    match op {
        Operand::Workspace(path) => Some(relative(&scope.root, path)),
        Operand::Uri(uri) => Some(uri.clone()),
        Operand::Other => None,
    }
}

/// 搜索根的 path 参数：等于 root 时返回 `Some(None)` 表示省略参数；
/// URI 只在 `uri_ok`（read/grep）时接受。
fn scan_root_arg(op: &Operand, scope: &Scope, uri_ok: bool) -> Option<Option<String>> {
    match op {
        Operand::Workspace(path) if path == &scope.root => Some(None),
        Operand::Workspace(path) => Some(Some(relative(&scope.root, path))),
        Operand::Uri(uri) if uri_ok => Some(Some(uri.clone())),
        _ => None,
    }
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        rel.display().to_string()
    }
}

// ---------- argv 标志切分 ----------

/// argv 中标志与操作数的划分。
enum Tok<'a> {
    /// 短簇或长标志；`name` 不带前导 `-`（长为单词，短为单字符；数字开头
    /// 的簇在 `num_flag` 时整体为一个标志，如 grep `-NUM`、head `-N`）。
    Flag { name: String, value: Option<String> },
    /// 位置操作数。
    Operand(&'a Arg),
}

/// 切分 args：`--` 之后全是操作数；`long_values`/`short_values` 是会吃掉
/// 下一个参数（或 `=` 后缀、簇内剩余）的标志名。
fn tokens<'a>(
    args: &'a [Arg],
    long_values: &[&str],
    short_values: &str,
    num_flag: bool,
) -> Vec<Tok<'a>> {
    let mut out = Vec::new();
    let mut operands_only = false;
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if operands_only {
            out.push(Tok::Operand(arg));
            continue;
        }
        let Some(text) = arg.value.as_deref() else {
            out.push(Tok::Operand(arg));
            continue;
        };
        if text == "--" {
            operands_only = true;
            continue;
        }
        if let Some(long) = text.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (long, None),
            };
            let value = if inline.is_none() && long_values.contains(&name) {
                iter.next().and_then(|next| next.value.clone())
            } else {
                inline
            };
            out.push(Tok::Flag {
                name: name.to_owned(),
                value,
            });
            continue;
        }
        if let Some(cluster) = text.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            if num_flag && cluster.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                out.push(Tok::Flag {
                    name: cluster.to_owned(),
                    value: None,
                });
                continue;
            }
            let mut chars = cluster.chars();
            while let Some(c) = chars.next() {
                if short_values.contains(c) {
                    let rest: String = chars.collect();
                    let value = if rest.is_empty() {
                        iter.next().and_then(|next| next.value.clone())
                    } else {
                        Some(rest)
                    };
                    out.push(Tok::Flag {
                        name: c.to_string(),
                        value,
                    });
                    break;
                }
                out.push(Tok::Flag {
                    name: c.to_string(),
                    value: None,
                });
            }
            continue;
        }
        out.push(Tok::Operand(arg));
    }
    out
}

fn operands<'a>(toks: &[Tok<'a>]) -> Vec<&'a Arg> {
    toks.iter()
        .filter_map(|tok| match tok {
            Tok::Operand(arg) => Some(*arg),
            _ => None,
        })
        .collect()
}

fn flag_names<'a>(toks: &'a [Tok<'a>]) -> impl Iterator<Item = &'a str> + 'a {
    toks.iter().filter_map(|tok| match tok {
        Tok::Flag { name, .. } => Some(name.as_str()),
        _ => None,
    })
}

// ---------- 读 / 搜 / 列翻译 ----------

/// 整条管道的读/搜/列翻译；不可规约返回 `None`。
fn translate_pipe(simples: &[&Simple], scope: &Scope) -> Option<Vec<Suggestion>> {
    if simples.is_empty() {
        return None;
    }
    // 这些命令的 stdout 不能重定向到文件（`Dup` 与 fd≠1 的 Output 允许）。
    if simples.iter().any(|s| writes_file(s)) {
        return None;
    }
    let mut segs: Vec<&Simple> = simples.to_vec();
    // stdin 源归一化：首段 `cat F` / `nl` / `nl -ba F`（单个可翻译文件、无
    // 其它 flag）去掉，把 F 当作第二段的输入文件。
    let mut input = None;
    let mut first = true;
    if segs.len() >= 2
        && let Some(file) = stdin_source(segs[0])
    {
        input = Some(operand(file, scope));
        segs.remove(0);
        first = false;
    }
    // `X < F`：首段输入重定向同样是文件输入；其它段的 `<` 让管道无法规约。
    for (index, seg) in segs.iter().enumerate() {
        for redirect in &seg.redirects {
            match redirect {
                Redirect::Input { target } if index == 0 && input.is_none() => {
                    input = Some(match target {
                        Some(text) => operand(
                            &Arg {
                                value: Some(text.clone()),
                                glob: false,
                            },
                            scope,
                        ),
                        None => Operand::Other,
                    });
                }
                Redirect::Input { .. } => return None,
                _ => {}
            }
        }
    }
    // 尾随截断：搜索/列举类后接的 head 是可忽略的输出截断。
    let mut head = None;
    if segs.len() >= 2
        && let Some(n) = head_lines(segs[segs.len() - 1])
    {
        head = Some(n);
        segs.pop();
    }
    let &[seg] = segs.as_slice() else {
        return None;
    };
    // 管道下游且无归一化输入、或首段带 stdin 参数：无文件操作数的命令
    // 读的是 stdin 而不是文件。
    let downstream = input.is_none() && (!first || scope.stdin);
    let (kind, suggestions) = translate_command(seg, input.as_ref(), downstream, head, scope)?;
    // head 截断只对搜索/列举类有效；`ls | head` 之类不翻译。
    if head.is_some() && matches!(kind, Kind::Read) {
        return None;
    }
    Some(suggestions)
}

/// 读/搜/列翻译的类别；`Find` 族可吃 head 的 limit，`Grep` 族忽略。
enum Kind {
    Read,
    Grep,
    Find,
}

/// 首段是否为「单个可翻译文件作为 stdin 源」：`cat F`、`nl F`、`nl -ba F`；
/// F 不能是 flag 或 `-`（stdin 标记）。
fn stdin_source(simple: &Simple) -> Option<&Arg> {
    fn file(arg: &Arg) -> Option<&Arg> {
        if arg.value.as_deref().is_some_and(|v| !v.starts_with('-')) {
            Some(arg)
        } else {
            None
        }
    }
    let args = &simple.argv[1..];
    match verb(simple)? {
        "cat" => match args {
            [arg] => file(arg),
            _ => None,
        },
        "nl" => match args {
            [arg] => file(arg),
            [flag, arg] if flag.value.as_deref() == Some("-ba") => file(arg),
            [flag, value, arg]
                if flag.value.as_deref() == Some("-b") && value.value.as_deref() == Some("a") =>
            {
                file(arg)
            }
            _ => None,
        },
        _ => None,
    }
}

/// `head`/`head -N`/`head -n N`/`--lines[=N]` 视为输出截断，返回 N；默认 10。
/// 其它 flag 或带操作数都不算。
fn head_lines(simple: &Simple) -> Option<usize> {
    if verb(simple)? != "head" {
        return None;
    }
    let mut n = 10usize;
    for tok in tokens(&simple.argv[1..], &["lines"], "n", true) {
        match tok {
            Tok::Operand(_) => return None,
            Tok::Flag { name, value } => match name.as_str() {
                "n" | "lines" => n = value.as_deref()?.parse().ok()?,
                digits if digits.chars().all(|c| c.is_ascii_digit()) => {
                    n = digits.parse().ok()?;
                }
                _ => return None,
            },
        }
    }
    Some(n)
}

fn translate_command(
    seg: &Simple,
    input: Option<&Operand>,
    downstream: bool,
    head: Option<usize>,
    scope: &Scope,
) -> Option<(Kind, Vec<Suggestion>)> {
    let name = verb(seg)?;
    let (kind, suggestions) = match name {
        "cat" => (Kind::Read, cat_read(seg, input, scope)?),
        "nl" => (Kind::Read, nl_read(seg, input, scope)?),
        "head" => (Kind::Read, head_read(seg, input, scope)?),
        "tail" => (Kind::Read, tail_read(seg, input, scope)?),
        "sed" => (Kind::Read, sed_print(seg, input, scope)?),
        "ls" => (Kind::Read, ls_read(seg, scope)?),
        "rg" => rg_translate(seg, input, downstream, head, scope)?,
        "grep" | "egrep" | "fgrep" => (
            Kind::Grep,
            grep_translate(seg, name, input, downstream, scope)?,
        ),
        "find" => (Kind::Find, find_translate(seg, head, scope)?),
        "fd" | "fdfind" => (Kind::Find, fd_translate(seg, head, scope)?),
        _ => return None,
    };
    Some((kind, suggestions))
}

/// read 类翻译共用：每个文件操作数一条 read；无操作数时吃归一化输入。
fn read_suggestions(
    files: &[&Arg],
    input: Option<&Operand>,
    extra: Map<String, Value>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
    let mut out = Vec::new();
    let sources: Vec<Option<String>> = if files.is_empty() {
        vec![input.and_then(|op| operand_path(op, scope))]
    } else {
        files
            .iter()
            .map(|arg| operand_path(&operand(arg, scope), scope))
            .collect()
    };
    for path in sources {
        let mut args = extra.clone();
        args.insert("path".into(), path?.into());
        out.push(Suggestion {
            tool: "read",
            args,
            via: None,
        });
    }
    Some(out)
}

/// `cat [-n] F...`；其它 flag 不翻译。
fn cat_read(seg: &Simple, input: Option<&Operand>, scope: &Scope) -> Option<Vec<Suggestion>> {
    let toks = tokens(&seg.argv[1..], &[], "", false);
    if flag_names(&toks).any(|name| name != "n") {
        return None;
    }
    read_suggestions(&operands(&toks), input, Map::new(), scope)
}

/// `nl` / `nl -ba F` / `nl -b a F`；`-b` 只接受值 `a`，其它 flag 不翻译。
fn nl_read(seg: &Simple, input: Option<&Operand>, scope: &Scope) -> Option<Vec<Suggestion>> {
    let toks = tokens(&seg.argv[1..], &[], "b", false);
    for tok in &toks {
        match tok {
            Tok::Flag { name, value } if name == "b" && value.as_deref() == Some("a") => {}
            Tok::Flag { .. } => return None,
            Tok::Operand(_) => {}
        }
    }
    read_suggestions(&operands(&toks), input, Map::new(), scope)
}

/// `head [-n N | -N | --lines=N] F...` → read limit N（默认 10）；`-c` 等不翻译。
fn head_read(seg: &Simple, input: Option<&Operand>, scope: &Scope) -> Option<Vec<Suggestion>> {
    let toks = tokens(&seg.argv[1..], &["lines"], "n", true);
    let mut limit = 10usize;
    for tok in &toks {
        match tok {
            Tok::Flag { name, value } => match name.as_str() {
                "n" | "lines" => limit = value.as_deref()?.parse().ok()?,
                digits if digits.chars().all(|c| c.is_ascii_digit()) => {
                    limit = digits.parse().ok()?;
                }
                _ => return None,
            },
            Tok::Operand(_) => {}
        }
    }
    let mut extra = Map::new();
    extra.insert("limit".into(), limit.into());
    read_suggestions(&operands(&toks), input, extra, scope)
}

/// `tail [-n N | -N] F` → read offset -N；`-n +K` → offset K；`-f/-F` 等不翻译。
fn tail_read(seg: &Simple, input: Option<&Operand>, scope: &Scope) -> Option<Vec<Suggestion>> {
    let toks = tokens(&seg.argv[1..], &["lines"], "n", true);
    let mut offset = -10i64;
    for tok in &toks {
        match tok {
            Tok::Flag { name, value } => match name.as_str() {
                "n" | "lines" => offset = tail_offset(value.as_deref()?)?,
                digits if digits.chars().all(|c| c.is_ascii_digit()) => {
                    offset = -(digits.parse::<i64>().ok()?);
                }
                _ => return None,
            },
            Tok::Operand(_) => {}
        }
    }
    let mut extra = Map::new();
    extra.insert("offset".into(), offset.into());
    read_suggestions(&operands(&toks), input, extra, scope)
}

fn tail_offset(value: &str) -> Option<i64> {
    if let Some(plus) = value.strip_prefix('+') {
        plus.parse().ok()
    } else {
        value.parse::<i64>().ok().map(|n| -n)
    }
}

/// `sed -n SCRIPT F`：只认 `Np`、`A,Bp`、`A,$p`。
fn sed_print(seg: &Simple, input: Option<&Operand>, scope: &Scope) -> Option<Vec<Suggestion>> {
    let toks = tokens(&seg.argv[1..], &[], "e", false);
    let mut quiet = false;
    let mut script = None;
    for tok in &toks {
        match tok {
            Tok::Flag { name, value } => match name.as_str() {
                "n" | "quiet" => quiet = true,
                "e" => {
                    if script.is_some() {
                        return None;
                    }
                    script = Some(value.clone()?);
                }
                _ => return None,
            },
            Tok::Operand(_) => {}
        }
    }
    if !quiet {
        return None;
    }
    let mut files = operands(&toks);
    let script = match script {
        Some(script) => script,
        None => {
            if files.is_empty() {
                return None;
            }
            files.remove(0).value.clone()?
        }
    };
    let (offset, limit) = sed_range(&script)?;
    let mut extra = Map::new();
    extra.insert("offset".into(), offset.into());
    if let Some(limit) = limit {
        extra.insert("limit".into(), limit.into());
    }
    read_suggestions(&files, input, extra, scope)
}

/// `Np` → (N, 1)；`A,Bp` → (A, B-A+1)；`A,$p` → (A, 无 limit)。
fn sed_range(script: &str) -> Option<(u64, Option<u64>)> {
    let range = script.strip_suffix('p')?;
    let parse = |s: &str| s.parse::<u64>().ok();
    match range.split_once(',') {
        None => parse(range).map(|n| (n, Some(1))),
        Some((a, "$")) => parse(a).map(|a| (a, None)),
        Some((a, b)) => Some((parse(a)?, Some(parse(b)?.saturating_sub(parse(a)?) + 1))),
    }
}

/// `ls [flags] [D...]`：操作数为空（=有效目录）或全是工作区内现存目录。
fn ls_read(seg: &Simple, scope: &Scope) -> Option<Vec<Suggestion>> {
    const FLAGS: &[&str] = &["a", "A", "l", "h", "1", "F", "t", "r", "p", "G"];
    // --color 只接受 `=WHEN` 形式，不吃下一个参数。
    let toks = tokens(&seg.argv[1..], &[], "", false);
    for name in flag_names(&toks) {
        if !FLAGS.contains(&name) && name != "color" {
            return None;
        }
    }
    let dirs = operands(&toks);
    let mut out = Vec::new();
    if dirs.is_empty() {
        let dir = scope.dir.as_ref()?;
        if !dir.is_dir() {
            return None;
        }
        out.push(ls_suggestion(relative(&scope.root, dir)));
        return Some(out);
    }
    for dir in dirs {
        let Operand::Workspace(path) = operand(dir, scope) else {
            return None;
        };
        if !path.is_dir() {
            return None;
        }
        out.push(ls_suggestion(relative(&scope.root, &path)));
    }
    Some(out)
}

fn ls_suggestion(path: String) -> Suggestion {
    let mut args = Map::new();
    args.insert("limit".into(), 200.into());
    args.insert("path".into(), path.into());
    Suggestion {
        tool: "read",
        args,
        via: None,
    }
}

// ---------- 搜索翻译（rg / grep）----------

/// 搜索命令归一出的参数形状。
#[derive(Default)]
struct GrepShape {
    case: Option<bool>,
    smart: bool,
    literal: bool,
    word: bool,
    files_only: bool,
    before: Option<u64>,
    after: Option<u64>,
    globs: Vec<String>,
    hidden: Option<bool>,
    gitignore: Option<bool>,
    patterns: Vec<String>,
}

/// `rg`：`-e` 可多次、位置参数首个为 pattern、其余为路径。
fn rg_translate(
    seg: &Simple,
    input: Option<&Operand>,
    downstream: bool,
    head: Option<usize>,
    scope: &Scope,
) -> Option<(Kind, Vec<Suggestion>)> {
    // `rg --files` 是独立的 find 翻译。
    if seg.argv[1..]
        .iter()
        .any(|arg| arg.value.as_deref() == Some("--files"))
    {
        return rg_files(seg, head, scope).map(|s| (Kind::Find, s));
    }
    let toks = tokens(
        &seg.argv[1..],
        &[
            "after-context",
            "before-context",
            "context",
            "regexp",
            "glob",
            "type",
            "max-count",
            "threads",
        ],
        "ABCegtmj",
        false,
    );
    let mut shape = GrepShape::default();
    let mut u_count = 0;
    for tok in &toks {
        let Tok::Flag { name, value } = tok else {
            continue;
        };
        match name.as_str() {
            "i" | "ignore-case" => shape.case = Some(false),
            "s" | "case-sensitive" => shape.case = Some(true),
            "S" | "smart-case" => shape.smart = true,
            "F" | "fixed-strings" => shape.literal = true,
            "w" | "word-regexp" => shape.word = true,
            "l" | "files-with-matches" => shape.files_only = true,
            "A" | "after-context" => shape.after = Some(value.as_deref()?.parse().ok()?),
            "B" | "before-context" => shape.before = Some(value.as_deref()?.parse().ok()?),
            "C" | "context" => {
                let n = value.as_deref()?.parse().ok()?;
                shape.before = Some(n);
                shape.after = Some(n);
            }
            "g" | "glob" => {
                let glob = value.as_deref()?;
                if glob.starts_with('!') {
                    return None;
                }
                shape.globs.push(glob.to_owned());
            }
            "t" | "type" => shape.globs.extend(type_globs(value.as_deref()?)?),
            "hidden" | "." => shape.hidden = Some(true),
            "no-ignore" | "u" => {
                u_count += 1;
                shape.gitignore = Some(false);
                if u_count >= 2 {
                    shape.hidden = Some(true);
                }
            }
            "e" | "regexp" => shape.patterns.push(value.clone()?),
            "P" | "pcre2" => {}
            // 不影响语义的输出/性能类标志。
            "n" | "N" | "line-number" | "no-line-number" | "H" | "with-filename"
            | "no-filename" | "I" | "no-heading" | "heading" | "color" | "colors" | "p"
            | "pretty" | "column" | "trim" | "m" | "max-count" | "j" | "threads" => {}
            _ => return None,
        }
    }
    let mut files = operands(&toks);
    let mut literal = shape.literal;
    let pattern = if !shape.patterns.is_empty() {
        combine_patterns(std::mem::take(&mut shape.patterns), &mut literal)
    } else {
        if files.is_empty() {
            return None;
        }
        files.remove(0).value.clone()?
    };
    let case = match shape.case {
        Some(case) => Some(case),
        None if shape.smart && !pattern.chars().any(|c| c.is_uppercase()) => Some(false),
        None => None,
    };
    let paths = search_roots(&files, input, downstream, scope)?;
    grep_suggestions(&pattern, &paths, shape, case, literal).map(|s| (Kind::Grep, s))
}

/// `grep`/`egrep`/`fgrep`：目录操作数需要 -r；BRE 需要转 Rust 正则。
fn grep_translate(
    seg: &Simple,
    name: &str,
    input: Option<&Operand>,
    downstream: bool,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
    let toks = tokens(
        &seg.argv[1..],
        &[
            "after-context",
            "before-context",
            "context",
            "regexp",
            "include",
            "max-count",
        ],
        "ABCem",
        true,
    );
    let mut shape = GrepShape {
        literal: name == "fgrep",
        ..Default::default()
    };
    // egrep 隐含 -E；fgrep 隐含 -F（literal 时不做 BRE 转换）。
    let mut bre = name != "egrep";
    let mut recursive = false;
    for tok in &toks {
        let Tok::Flag { name, value } = tok else {
            continue;
        };
        match name.as_str() {
            "i" | "y" | "ignore-case" => shape.case = Some(false),
            "E" | "extended-regexp" => bre = false,
            "G" | "basic-regexp" => bre = true,
            "F" | "fixed-strings" => shape.literal = true,
            "P" | "perl-regexp" => bre = false,
            "w" | "word-regexp" => shape.word = true,
            "l" | "files-with-matches" => shape.files_only = true,
            "r" | "R" | "recursive" | "dereference-recursive" => recursive = true,
            "A" | "after-context" => shape.after = Some(value.as_deref()?.parse().ok()?),
            "B" | "before-context" => shape.before = Some(value.as_deref()?.parse().ok()?),
            "C" | "context" => {
                let n = value.as_deref()?.parse().ok()?;
                shape.before = Some(n);
                shape.after = Some(n);
            }
            digits if digits.chars().all(|c| c.is_ascii_digit()) => {
                let n = digits.parse().ok()?;
                shape.before = Some(n);
                shape.after = Some(n);
            }
            "e" | "regexp" => shape.patterns.push(value.clone()?),
            "include" => shape.globs.push(value.clone()?),
            // 不影响语义的输出/性能类标志。
            "n" | "H" | "h" | "s" | "I" | "color" | "colour" | "m" | "max-count"
            | "line-number" | "with-filename" | "no-filename" | "no-messages" | "ignore-errors" => {
            }
            _ => return None,
        }
    }
    let mut files = operands(&toks);
    let mut literal = shape.literal;
    let pattern = if !shape.patterns.is_empty() {
        if bre && !literal {
            for pattern in &mut shape.patterns {
                *pattern = bre_to_regex(pattern);
            }
        }
        combine_patterns(std::mem::take(&mut shape.patterns), &mut literal)
    } else {
        if files.is_empty() {
            return None;
        }
        let pattern = files.remove(0).value.clone()?;
        // BRE → Rust 正则（grep 且无 -E/-F/-P；egrep 已是 -E）。
        if bre && !literal {
            bre_to_regex(&pattern)
        } else {
            pattern
        }
    };
    let paths = if recursive {
        search_roots(&files, input, downstream, scope)?
    } else {
        // 无 -r 时只接受文件操作数；无操作数则读 stdin，不翻译。
        let mut paths = Vec::new();
        for file in &files {
            match operand(file, scope) {
                Operand::Workspace(path) if !path.is_dir() => {
                    paths.push(Some(relative(&scope.root, &path)));
                }
                Operand::Uri(uri) => paths.push(Some(uri)),
                _ => return None,
            }
        }
        if files.is_empty() {
            match input {
                Some(Operand::Workspace(path)) => {
                    paths.push(Some(relative(&scope.root, path)));
                }
                Some(Operand::Uri(uri)) => paths.push(Some(uri.clone())),
                _ => return None,
            }
        }
        paths
    };
    let case = shape.case;
    grep_suggestions(&pattern, &paths, shape, case, literal)
}

/// 搜索根的归一化：有路径操作数逐个解析；无路径时用归一化输入、
/// 否则有效目录（下游/stdin 场景读的是 stdin，不可翻译）。
fn search_roots(
    files: &[&Arg],
    input: Option<&Operand>,
    downstream: bool,
    scope: &Scope,
) -> Option<Vec<Option<String>>> {
    if !files.is_empty() {
        return files
            .iter()
            .map(|arg| scan_root_arg(&operand(arg, scope), scope, true))
            .collect();
    }
    if let Some(input) = input {
        return scan_root_arg(input, scope, true).map(|root| vec![root]);
    }
    if downstream {
        return None;
    }
    let dir = scope.dir.as_ref()?;
    scan_root_arg(&Operand::Workspace(dir.clone()), scope, true).map(|root| vec![root])
}

/// 用与建议相同的 case/word/fixed_strings 设置编译 pattern，编译不过不翻译。
fn grep_suggestions(
    pattern: &str,
    paths: &[Option<String>],
    shape: GrepShape,
    case: Option<bool>,
    literal: bool,
) -> Option<Vec<Suggestion>> {
    grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(case == Some(false))
        .fixed_strings(literal)
        .word(shape.word)
        .build(pattern)
        .ok()?;
    let mut out = Vec::new();
    for path in paths {
        let mut args = Map::new();
        if let Some(case) = case {
            args.insert("case".into(), case.into());
        }
        if let Some(after) = shape.after {
            args.insert("context_after".into(), after.into());
        }
        if let Some(before) = shape.before {
            args.insert("context_before".into(), before.into());
        }
        if shape.files_only {
            args.insert("files_only".into(), true.into());
        }
        if !shape.globs.is_empty() {
            args.insert("glob".into(), shape.globs.clone().into());
        }
        if let Some(gitignore) = shape.gitignore {
            args.insert("gitignore".into(), gitignore.into());
        }
        if let Some(hidden) = shape.hidden {
            args.insert("hidden".into(), hidden.into());
        }
        if literal {
            args.insert("literal".into(), true.into());
        }
        if let Some(path) = path {
            args.insert("path".into(), path.clone().into());
        }
        args.insert("pattern".into(), pattern.into());
        if shape.word {
            args.insert("word".into(), true.into());
        }
        out.push(Suggestion {
            tool: "grep",
            args,
            via: None,
        });
    }
    Some(out)
}

/// 多个 -e pattern 拼成 `(?:a)|(?:b)`；literal 时先转义再拼并去掉 literal。
fn combine_patterns(patterns: Vec<String>, literal: &mut bool) -> String {
    if patterns.len() == 1 {
        return patterns.into_iter().next().unwrap_or_default();
    }
    let escape = *literal;
    *literal = false;
    patterns
        .iter()
        .map(|pattern| {
            format!(
                "(?:{})",
                if escape {
                    regex_escape(pattern)
                } else {
                    pattern.clone()
                }
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// grep BRE → Rust 正则：`\(` `\)` `\{` `\}` `\|` `\+` `\?` 去反斜杠，
/// 裸 `( ) { } | + ?` 加反斜杠；其余原样。
fn bre_to_regex(pattern: &str) -> String {
    const META: &str = "(){}|+?";
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) if META.contains(next) => out.push(next),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            }
        } else if META.contains(c) {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

/// `rg -t/--type` 的类型名展开成 glob；未知类型返回 `None`。
fn type_globs(name: &str) -> Option<Vec<String>> {
    static TYPES: LazyLock<ignore::types::Types> = LazyLock::new(|| {
        let mut builder = ignore::types::TypesBuilder::new();
        builder.add_defaults();
        builder.build().expect("默认类型表必须构建")
    });
    TYPES
        .definitions()
        .iter()
        .find(|def| def.name() == name)
        .map(|def| def.globs().to_vec())
}

// ---------- 列举翻译（find / fd / rg --files）----------

/// `find [P...] [expr]`：只允许 `-name G`、`-path G`、`-type f`、`-print`
/// 以及 `-o` 连接的上述谓词与包裹它们的括号。
fn find_translate(seg: &Simple, head: Option<usize>, scope: &Scope) -> Option<Vec<Suggestion>> {
    let args = &seg.argv[1..];
    // 路径列表：直到第一个标志或表达式记号。
    let mut index = 0usize;
    let mut roots = Vec::new();
    while index < args.len() {
        let arg = &args[index];
        match arg.value.as_deref() {
            Some(text)
                if !text.starts_with('-') && !matches!(text, "(" | ")" | "!" | "," | ";" | "+") =>
            {
                roots.push(arg);
                index += 1;
            }
            _ => break,
        }
    }
    let root_texts: Vec<String> = roots.iter().filter_map(|arg| arg.value.clone()).collect();
    // 表达式：`-o` 分隔的组，每组至多一个 -name/-path 谓词，其余谓词只认
    // 可忽略的 -type f 与 -print。
    let mut globs = Vec::new();
    let mut group_named = false;
    let mut saw_or = false;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        let flag = arg.value.as_deref()?;
        match flag {
            "-name" | "-path" => {
                if group_named {
                    return None;
                }
                let pattern = args.get(index)?;
                index += 1;
                if pattern.glob {
                    return None;
                }
                let mut glob = pattern.value.clone()?;
                if flag == "-path" {
                    glob = strip_path_prefix(&glob, &root_texts);
                }
                globs.push(glob);
                group_named = true;
            }
            "-type" => {
                if args.get(index).and_then(|arg| arg.value.as_deref()) != Some("f") {
                    return None;
                }
                index += 1;
            }
            "-print" | "(" | ")" => {}
            "-o" | "-or" => {
                if !group_named {
                    return None;
                }
                group_named = false;
                saw_or = true;
            }
            _ => return None,
        }
    }
    // `-o` 的最后一组也必须有 -name/-path 谓词。
    if saw_or && !group_named {
        return None;
    }
    let globs = if globs.is_empty() {
        vec!["*".to_owned()]
    } else {
        globs
    };
    find_suggestions(&roots, globs, head, Map::new(), scope)
}

/// `-path` 的 glob 去掉开头的 `./` 或 `P/` 前缀。
fn strip_path_prefix(glob: &str, roots: &[String]) -> String {
    if let Some(rest) = glob.strip_prefix("./") {
        return rest.to_owned();
    }
    for root in roots {
        if let Some(rest) = glob.strip_prefix(&format!("{root}/")) {
            return rest.to_owned();
        }
    }
    glob.to_owned()
}

/// find 族建议共用：P 为空时取有效目录；每个工作区内根一条建议。
fn find_suggestions(
    roots: &[&Arg],
    globs: Vec<String>,
    head: Option<usize>,
    extra: Map<String, Value>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
    let paths: Vec<Option<String>> = if roots.is_empty() {
        let dir = scope.dir.as_ref()?;
        vec![scan_root_arg(
            &Operand::Workspace(dir.clone()),
            scope,
            false,
        )?]
    } else {
        roots
            .iter()
            .map(|arg| scan_root_arg(&operand(arg, scope), scope, false))
            .collect::<Option<_>>()?
    };
    let mut out = Vec::new();
    for path in paths {
        let mut args = extra.clone();
        args.insert("glob".into(), globs.clone().into());
        if let Some(limit) = head.map(|n| n.min(200)) {
            args.insert("limit".into(), limit.into());
        }
        if let Some(path) = path {
            args.insert("path".into(), path.into());
        }
        out.push(Suggestion {
            tool: "find",
            args,
            via: None,
        });
    }
    Some(out)
}

/// `fd`/`fdfind`：`-e EXT`、`-g/--glob P`、`-t f`、`-H`、`-I` 与
/// `^[A-Za-z0-9_-]+$` 的 PATTERN。
fn fd_translate(seg: &Simple, head: Option<usize>, scope: &Scope) -> Option<Vec<Suggestion>> {
    // `-g/--glob` 是不带值的开关：开启后第一个位置参数就是 glob 模式。
    let toks = tokens(&seg.argv[1..], &["extension", "type"], "et", false);
    let mut globs = Vec::new();
    let mut hidden = None;
    let mut gitignore = None;
    let mut glob_mode = false;
    for tok in &toks {
        let Tok::Flag { name, value } = tok else {
            continue;
        };
        match name.as_str() {
            "e" | "extension" => globs.push(format!("*.{}", value.as_deref()?)),
            "g" | "glob" => glob_mode = true,
            "t" | "type" => {
                if value.as_deref()? != "f" {
                    return None;
                }
            }
            "H" | "hidden" => hidden = Some(true),
            "I" | "no-ignore" => gitignore = Some(false),
            _ => return None,
        }
    }
    let mut files = operands(&toks);
    if let Some(pattern) = files.first() {
        // 模式与 `-e` 是交集语义，glob 数组表达不了 → 不翻译。
        if !globs.is_empty() {
            return None;
        }
        if glob_mode {
            // `-g` 时第一个位置参数直接是 glob；未加引号的 glob 会被 shell 先展开。
            if pattern.glob {
                return None;
            }
            globs.push(pattern.value.clone()?);
        } else {
            // 不带 `-g` 的 PATTERN 只在 ^[A-Za-z0-9_-]+$ 时 → *PATTERN*。
            let text = pattern.value.as_deref()?;
            if text.is_empty()
                || !text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return None;
            }
            globs.push(format!("*{text}*"));
        }
        files.remove(0);
    }
    for file in &files {
        if !matches!(operand(file, scope), Operand::Workspace(_)) {
            return None;
        }
    }
    let globs = if globs.is_empty() {
        vec!["*".to_owned()]
    } else {
        globs
    };
    let mut extra = Map::new();
    if let Some(hidden) = hidden {
        extra.insert("hidden".into(), hidden.into());
    }
    if let Some(gitignore) = gitignore {
        extra.insert("gitignore".into(), gitignore.into());
    }
    find_suggestions(&files, globs, head, extra, scope)
}

/// `rg --files [PATH...] [-g G...] [-t T] [--hidden] [-u...]` → find。
fn rg_files(seg: &Simple, head: Option<usize>, scope: &Scope) -> Option<Vec<Suggestion>> {
    let toks = tokens(&seg.argv[1..], &["glob", "type"], "gt", false);
    let mut globs = Vec::new();
    let mut hidden = None;
    let mut gitignore = None;
    let mut u_count = 0;
    for tok in &toks {
        let Tok::Flag { name, value } = tok else {
            continue;
        };
        match name.as_str() {
            "files" => {}
            "g" | "glob" => {
                let glob = value.as_deref()?;
                if glob.starts_with('!') {
                    return None;
                }
                globs.push(glob.to_owned());
            }
            "t" | "type" => globs.extend(type_globs(value.as_deref()?)?),
            "hidden" | "." => hidden = Some(true),
            "no-ignore" | "u" => {
                u_count += 1;
                gitignore = Some(false);
                if u_count >= 2 {
                    hidden = Some(true);
                }
            }
            _ => return None,
        }
    }
    let files = operands(&toks);
    for file in &files {
        if !matches!(operand(file, scope), Operand::Workspace(_)) {
            return None;
        }
    }
    let globs = if globs.is_empty() {
        vec!["*".to_owned()]
    } else {
        globs
    };
    let mut extra = Map::new();
    if let Some(hidden) = hidden {
        extra.insert("hidden".into(), hidden.into());
    }
    if let Some(gitignore) = gitignore {
        extra.insert("gitignore".into(), gitignore.into());
    }
    find_suggestions(&files, globs, head, extra, scope)
}

// ---------- 写 / 改判定 ----------

/// 逐段的写/改检测；命中往 `verdict.suggestions` 追加。
fn detect_edit(seg: &Simple, index: usize, segs: &[&Simple], scope: &Scope, verdict: &mut Verdict) {
    match verb(seg) {
        // `bash|sh|zsh -c STR`：递归判定子串（同一有效目录），结果并入。
        Some("bash" | "sh" | "zsh") => {
            if let Some(code) = dash_c(&seg.argv[1..])
                && let Some(inner) = eval(&code, scope.clone())
            {
                verdict.leftover |= inner.leftover;
                verdict.suggestions.extend(inner.suggestions);
            }
        }
        // 包装动词：只做尾部原地编辑器检测。
        Some(
            "sudo" | "env" | "command" | "nohup" | "nice" | "timeout" | "stdbuf" | "time" | "xargs",
        ) => {
            if let Some(tail) = editor_tail(&seg.argv[1..]) {
                emit_editor(tail, scope, &mut verdict.suggestions);
            }
        }
        Some("find") => {
            if let Some(tail) = find_exec_tail(&seg.argv[1..]) {
                emit_editor(tail, scope, &mut verdict.suggestions);
            }
        }
        // 原地编辑器与解释器内联（perl 无 -i 时走解释器分支）。
        Some(name @ ("sed" | "perl" | "awk" | "gawk" | "ed" | "ex")) => {
            if editor_hit(name, &seg.argv[1..]) {
                emit_editor(&seg.argv, scope, &mut verdict.suggestions);
                return;
            }
            literal_or_interp(seg, index, segs, scope, verdict);
        }
        Some(name @ ("git" | "patch")) => {
            if let Some(via) = patch_from_stdin(name, seg, index == 0 && scope.stdin) {
                verdict.suggestions.push(Suggestion {
                    tool: "edit",
                    args: Map::new(),
                    via: Some(via),
                });
            }
        }
        _ => literal_or_interp(seg, index, segs, scope, verdict),
    }
}

fn literal_or_interp(
    seg: &Simple,
    index: usize,
    segs: &[&Simple],
    scope: &Scope,
    verdict: &mut Verdict,
) {
    if let Some(suggestions) = literal_write(seg, index, scope) {
        verdict.suggestions.extend(suggestions);
        return;
    }
    if let Some(suggestions) = tee_write(seg, index, segs, scope) {
        verdict.suggestions.extend(suggestions);
        return;
    }
    if let Some(via) = interp_writes(seg) {
        verdict.suggestions.push(Suggestion {
            tool: "edit",
            args: Map::new(),
            via: Some(via),
        });
    }
}

/// `bash|sh|zsh` 的 `-c STR`：允许 `-l`/`-e`/`-x` 等前置 flag 与 `-lc`
/// 这类簇；STR 必须静态。
fn dash_c(args: &[Arg]) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        let text = arg.value.as_deref()?;
        if text == "--" {
            return None;
        }
        let Some(cluster) = text
            .strip_prefix('-')
            .filter(|rest| !rest.is_empty() && !rest.starts_with('-'))
        else {
            // 第一个非 flag 参数就是脚本文件路径，不是 -c 子串。
            return None;
        };
        if let Some(pos) = cluster.find('c') {
            let rest = &cluster[pos + 1..];
            return if rest.is_empty() {
                args.get(index + 1)?.value.clone()
            } else {
                Some(rest.to_owned())
            };
        }
    }
    None
}

/// argv 中第一个 basename 为原地编辑器的位置起的切片。
fn editor_tail(args: &[Arg]) -> Option<&[Arg]> {
    let pos = args.iter().position(|arg| {
        arg.value
            .as_deref()
            .is_some_and(|v| matches!(basename(v), "sed" | "perl" | "awk" | "gawk" | "ed" | "ex"))
    })?;
    Some(&args[pos..])
}

/// `find` 的 `-exec`/`-execdir`/`-ok` 尾部（到 `;`/`+` 为止）里的编辑器段。
fn find_exec_tail(args: &[Arg]) -> Option<&[Arg]> {
    let start = args.iter().position(|arg| {
        matches!(
            arg.value.as_deref(),
            Some("-exec" | "-execdir" | "-ok" | "-okdir")
        )
    })? + 1;
    let tail = &args[start..];
    let end = tail
        .iter()
        .position(|arg| matches!(arg.value.as_deref(), Some(";" | "+")))
        .unwrap_or(tail.len());
    editor_tail(&tail[..end])
}

/// 该 argv 是否命中原地编辑器规则（不含目标解析）。
fn editor_hit(name: &str, args: &[Arg]) -> bool {
    match name {
        "sed" => args.iter().any(|arg| {
            let Some(text) = arg.value.as_deref() else {
                return false;
            };
            if let Some(rest) = text.strip_prefix("--in-place") {
                return rest.is_empty() || rest.starts_with('=');
            }
            let Some(cluster) = text
                .strip_prefix('-')
                .filter(|rest| !rest.is_empty() && !rest.starts_with('-'))
            else {
                return false;
            };
            // `-[nErsuz]*i…`：含 `-i`、`-i.bak`、`-Ei`、`-ni`。
            match cluster.find('i') {
                Some(pos) => cluster[..pos].chars().all(|c| "nErsuz".contains(c)),
                None => false,
            }
        }),
        "perl" => args.iter().any(|arg| {
            let Some(text) = arg.value.as_deref() else {
                return false;
            };
            let Some(cluster) = text
                .strip_prefix('-')
                .filter(|rest| !rest.is_empty() && !rest.starts_with('-'))
            else {
                return false;
            };
            // `i` 之前只有 [0-9aFlnpsw]：`-pi`、`-pie`、`-i.bak`、`-0pi`、`-lpi`。
            match cluster.find('i') {
                Some(pos) => cluster[..pos]
                    .chars()
                    .all(|c| c.is_ascii_digit() || "aFlnpsw".contains(c)),
                None => false,
            }
        }),
        "awk" | "gawk" => {
            let mut iter = args.iter().peekable();
            while let Some(arg) = iter.next() {
                match arg.value.as_deref() {
                    Some("-i") => {
                        if iter.peek().and_then(|arg| arg.value.as_deref()) == Some("inplace") {
                            return true;
                        }
                    }
                    Some("-iinplace") | Some("--include=inplace") => return true,
                    _ => {}
                }
            }
            false
        }
        "ed" | "ex" => args
            .iter()
            .any(|arg| arg.value.as_deref().is_some_and(|v| !v.starts_with('-'))),
        _ => false,
    }
}

/// 命中原地编辑器时产出 edit 建议：只有恰好一个目标、该目标静态且在工作
/// 区内时才产出 `edit {"path":...}`；其它情况一律放行：目标未知（动态、
/// glob、`{}`、无文件操作数）、两个及以上目标、或唯一目标在工作区外。
fn emit_editor(argv: &[Arg], scope: &Scope, suggestions: &mut Vec<Suggestion>) {
    let Some(name) = argv
        .first()
        .and_then(|arg| arg.value.as_deref())
        .map(basename)
    else {
        return;
    };
    let (via, targets) = editor_targets(name, &argv[1..]);
    let [target] = targets.as_slice() else {
        return;
    };
    let Operand::Workspace(path) = operand(target, scope) else {
        return;
    };
    let mut args = Map::new();
    args.insert("path".into(), relative(&scope.root, &path).into());
    suggestions.push(Suggestion {
        tool: "edit",
        args,
        via: Some(via),
    });
}

/// 拆出编辑器命令的文件操作数；返回 via 文案与目标参数。
fn editor_targets<'a>(name: &str, args: &'a [Arg]) -> (&'static str, Vec<&'a Arg>) {
    match name {
        "sed" => (
            "sed -i",
            file_args(
                args,
                &["expression", "file"],
                "ef",
                &["e", "f", "expression", "file"],
            ),
        ),
        "perl" => ("perl -i", file_args(args, &[], "eEMI", &["e", "E"])),
        "awk" | "gawk" => ("awk -i inplace", file_args(args, &[], "eifvF", &["e", "f"])),
        // ed/ex 的非 flag 参数都是文件。
        _ => ("ed", file_args(args, &[], "p", &[])),
    }
}

/// 取非 flag 参数中的文件操作数：脚本标志（`script_flags`，如 -e/-f）已提供
/// 程序文本时全部操作数都是文件；否则第一个操作数是脚本，其余是文件。
/// `script_flags` 为空时所有非 flag 参数都算文件。
fn file_args<'a>(
    args: &'a [Arg],
    long_values: &[&str],
    short_values: &str,
    script_flags: &[&str],
) -> Vec<&'a Arg> {
    let toks = tokens(args, long_values, short_values, false);
    let scripted = flag_names(&toks).any(|name| script_flags.contains(&name));
    let mut files = operands(&toks);
    if !script_flags.is_empty() && !scripted && !files.is_empty() {
        files.remove(0);
    }
    files
}

/// `git apply` / `patch` 且补丁来自 heredoc/here-string/bash stdin 参数：
/// 无补丁文件操作数、无 `<` 文件输入。
fn patch_from_stdin(name: &str, seg: &Simple, stdin_literal: bool) -> Option<&'static str> {
    let (via, args) = match name {
        "git" if seg.argv.get(1).and_then(|a| a.value.as_deref()) == Some("apply") => {
            ("git apply", &seg.argv[2..])
        }
        "patch" => ("patch", &seg.argv[1..]),
        _ => return None,
    };
    // 补丁文件来自 `<` 输入时不是字面量。
    if seg
        .redirects
        .iter()
        .any(|r| matches!(r, Redirect::Input { .. }))
    {
        return None;
    }
    if args
        .iter()
        .any(|arg| !arg.value.as_deref().is_some_and(|v| v.starts_with('-')))
    {
        return None;
    }
    if seg
        .redirects
        .iter()
        .any(|r| matches!(r, Redirect::HereDoc { .. } | Redirect::HereString { .. }))
        || stdin_literal
    {
        Some(via)
    } else {
        None
    }
}

/// 字面量内容写入：echo/printf 全静态、空命令/`:`/`true`、或 cat 无文件
/// 操作数且 stdin 为字面量时，fd 1 的 Output 重定向：
/// `>`/`>`→ write via "redirect"，`>>`→ edit via "append redirect"。
/// 目标必须静态且在工作区内；动态目标放行。
fn literal_write(seg: &Simple, index: usize, scope: &Scope) -> Option<Vec<Suggestion>> {
    let literal = match verb(seg) {
        // 空命令（无命令名，如 `> f`）才算截断；argv[0] 非静态（`"$CMD" > f`）
        // 不是字面量写入。
        None => seg.argv.is_empty(),
        Some("echo" | "printf") => seg.argv[1..].iter().all(|a| a.value.is_some()),
        Some(":" | "true") => true,
        Some("cat") => cat_stdin_literal(seg, index, scope),
        _ => return None,
    };
    if !literal {
        return None;
    }
    let mut out = Vec::new();
    for redirect in &seg.redirects {
        let Redirect::Output {
            fd: 1,
            target,
            append,
        } = redirect
        else {
            continue;
        };
        let Some(target) = target else {
            continue;
        };
        let Operand::Workspace(path) = operand(
            &Arg {
                value: Some(target.clone()),
                glob: false,
            },
            scope,
        ) else {
            continue;
        };
        out.push(Suggestion {
            tool: if *append { "edit" } else { "write" },
            args: {
                let mut args = Map::new();
                args.insert("path".into(), relative(&scope.root, &path).into());
                args
            },
            via: Some(if *append {
                "append redirect"
            } else {
                "redirect"
            }),
        });
    }
    if out.is_empty() { None } else { Some(out) }
}

/// cat 的 stdin 是字面量：自身 heredoc/here-string，或管道首段且调用带
/// stdin 参数；有文件操作数（`-` 除外）则读文件不是字面量。
fn cat_stdin_literal(seg: &Simple, index: usize, scope: &Scope) -> bool {
    if seg.argv[1..].iter().any(|arg| match arg.value.as_deref() {
        Some("-") => false,
        Some(v) => !v.starts_with('-'),
        None => true,
    }) {
        return false;
    }
    seg.redirects
        .iter()
        .any(|r| matches!(r, Redirect::HereDoc { .. } | Redirect::HereString { .. }))
        || (index == 0 && scope.stdin)
}

/// `tee [-a] F...`：stdin 为字面量时每个工作区内 F 一条建议；
/// 上游是程序则放行。
fn tee_write(
    seg: &Simple,
    index: usize,
    segs: &[&Simple],
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
    if verb(seg)? != "tee" {
        return None;
    }
    let literal_stdin = seg
        .redirects
        .iter()
        .any(|r| matches!(r, Redirect::HereDoc { .. } | Redirect::HereString { .. }))
        || (index == 0 && scope.stdin)
        || index
            .checked_sub(1)
            .is_some_and(|up| is_literal_producer(segs[up], up, scope));
    if !literal_stdin {
        return None;
    }
    let toks = tokens(&seg.argv[1..], &["append"], "", false);
    if flag_names(&toks).any(|name| name != "a" && name != "append") {
        return None;
    }
    let append = flag_names(&toks).any(|name| name == "a" || name == "append");
    let mut out = Vec::new();
    for file in operands(&toks) {
        if let Operand::Workspace(path) = operand(file, scope) {
            out.push(Suggestion {
                tool: if append { "edit" } else { "write" },
                args: {
                    let mut args = Map::new();
                    args.insert("path".into(), relative(&scope.root, &path).into());
                    args
                },
                via: Some(if append { "tee -a" } else { "tee" }),
            });
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// 上游段是否为字面量生产者：echo/printf 全静态、空命令、`:`、`true`、
/// 或 cat 无文件操作数且 stdin 为字面量。
fn is_literal_producer(seg: &Simple, index: usize, scope: &Scope) -> bool {
    match verb(seg) {
        None => seg.argv.is_empty(),
        Some("echo" | "printf") => seg.argv[1..].iter().all(|a| a.value.is_some()),
        Some(":" | "true") => true,
        Some("cat") => cat_stdin_literal(seg, index, scope),
        _ => false,
    }
}

/// 解释器内联写文件：python*/pypy*/node/deno/bun/perl/ruby 的代码文本
/// 命中写文件模式 → edit（无 path）。
fn interp_writes(seg: &Simple) -> Option<&'static str> {
    let name = verb(seg)?;
    let family = if name.starts_with("python") || name.starts_with("pypy") {
        "python"
    } else if matches!(name, "node" | "deno" | "bun") {
        "node"
    } else if name == "perl" {
        "perl"
    } else if name == "ruby" {
        "ruby"
    } else {
        return None;
    };
    let code = interp_code(seg, family)?;
    // 正则按语言族分开：python 0-2、node 族 3-4、perl 5、ruby 6。
    let patterns = match family {
        "python" => &INTERP_WRITE[..3],
        "node" => &INTERP_WRITE[3..5],
        "perl" => &INTERP_WRITE[5..6],
        "ruby" => &INTERP_WRITE[6..],
        _ => return None,
    };
    if patterns
        .iter()
        .any(|re| re.is_match(code.as_bytes()).unwrap_or(false))
    {
        Some(family)
    } else {
        None
    }
}

/// 内联代码来源：`-c`（python）、`-e`/`--eval`/`-E`、或无脚本操作数
/// （或为 `-`）时的 heredoc/here-string；bash stdin 参数内容不可见，不判定。
fn interp_code(seg: &Simple, family: &str) -> Option<String> {
    let (long_values, short_values, code_flags): (&[&str], &str, &[&str]) = match family {
        "python" => (&["command", "module"], "cmWX", &["c", "command"]),
        _ => (&["eval"], "eE", &["e", "E", "eval"]),
    };
    let toks = tokens(&seg.argv[1..], long_values, short_values, false);
    for tok in &toks {
        if let Tok::Flag { name, value } = tok {
            if code_flags.contains(&name.as_str()) {
                return value.clone();
            }
            // `python -m module` 以模块为脚本来源，不是内联代码。
            if family == "python" && (name == "m" || name == "module") {
                return None;
            }
        }
    }
    for tok in &toks {
        if let Tok::Operand(arg) = tok {
            if arg.value.as_deref() == Some("-") {
                break;
            }
            // 有脚本文件：不是内联代码。
            return None;
        }
    }
    for redirect in &seg.redirects {
        match redirect {
            Redirect::HereDoc { body } => return Some(body.clone()),
            Redirect::HereString { value } => return value.clone(),
            // stdin 是文件时不是内联代码。
            Redirect::Input { .. } => return None,
            _ => {}
        }
    }
    // stdin 来自 bash 的 stdin 参数时内容不可见，不判定。
    None
}

/// 解释器代码里的写文件模式（python/node/deno/bun/perl/ruby 共用）。
static INTERP_WRITE: LazyLock<Vec<grep_regex::RegexMatcher>> = LazyLock::new(|| {
    [
        r#"\bopen\s*\([^)]*,\s*(?:mode\s*=\s*)?['"][rbt]*[wax+]"#,
        r#"\.write_(?:text|bytes)\s*\("#,
        r#"inplace\s*=\s*True"#,
        r#"\b(?:writeFileSync|writeFile|appendFileSync|appendFile|createWriteStream)\s*\("#,
        r#"Deno\.writeTextFile|Bun\.write\s*\("#,
        r#"\bopen\s*\(?[^;]*['"]\s*\+?>"#,
        r#"File\.(?:write|open\s*\([^)]*['"][wa])"#,
    ]
    .iter()
    .map(|pattern| grep_regex::RegexMatcher::new(pattern).expect("内联写文件正则必须编译"))
    .collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个临时工作区：src/（a.rs、b.rs、main.rs）与 tests/。
    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("临时目录");
        let root = dir.path().to_owned();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        for file in ["a.rs", "b.rs", "main.rs"] {
            std::fs::write(root.join("src").join(file), "fn x() {}\n").unwrap();
        }
        (dir, root)
    }

    /// 渲染后的建议列表；`None` 表示放行。
    fn blocked(root: &Path, dir: &Path, stdin: bool, command: &str) -> Option<Vec<String>> {
        check(command, root, dir, stdin).map(|verdict| {
            verdict
                .suggestions
                .iter()
                .map(|s| format!("{} {}", s.tool, Value::Object(s.args.clone())))
                .collect()
        })
    }

    fn hit(command: &str) -> Option<Vec<String>> {
        let (_dir, root) = workspace();
        blocked(&root, &root, false, command)
    }

    /// 断言命令被拦截且建议列表与期望一致；args 按 JSON 值比较，键序随
    /// `serde_json/preserve_order` 特性统一而变，不能按字符串序断言。
    fn check_case(command: &str, expected: &[&str]) {
        let parse = |s: &str| {
            let (tool, args) = s.split_once(' ').unwrap();
            (tool.to_string(), serde_json::from_str::<Value>(args).unwrap())
        };
        let expected: Vec<_> = expected.iter().map(|s| parse(s)).collect();
        let actual = hit(command).map(|items| items.iter().map(|s| parse(s)).collect::<Vec<_>>());
        assert_eq!(actual, Some(expected), "{command}");
    }

    fn miss(root: &Path, command: &str) {
        assert_eq!(
            blocked(root, root, false, command),
            None,
            "应放行: {command}"
        );
    }

    #[test]
    fn read_family_translations() {
        let cases: &[(&str, &[&str])] = &[
            ("cat src/main.rs", &["read {\"path\":\"src/main.rs\"}"]),
            (
                "cat -n src/a.rs src/b.rs",
                &[
                    "read {\"path\":\"src/a.rs\"}",
                    "read {\"path\":\"src/b.rs\"}",
                ],
            ),
            (
                "head -n 40 src/a.rs",
                &["read {\"limit\":40,\"path\":\"src/a.rs\"}"],
            ),
            (
                "head -20 src/a.rs",
                &["read {\"limit\":20,\"path\":\"src/a.rs\"}"],
            ),
            (
                "tail -n 30 src/a.rs",
                &["read {\"offset\":-30,\"path\":\"src/a.rs\"}"],
            ),
            (
                "tail -n +100 src/a.rs",
                &["read {\"offset\":100,\"path\":\"src/a.rs\"}"],
            ),
            (
                "sed -n '10,40p' src/a.rs",
                &["read {\"limit\":31,\"offset\":10,\"path\":\"src/a.rs\"}"],
            ),
            (
                "sed -n 5p src/a.rs",
                &["read {\"limit\":1,\"offset\":5,\"path\":\"src/a.rs\"}"],
            ),
            (
                "nl -ba src/a.rs | sed -n '1,120p'",
                &["read {\"limit\":120,\"offset\":1,\"path\":\"src/a.rs\"}"],
            ),
            (
                "cat src/a.rs | head -n 5",
                &["read {\"limit\":5,\"path\":\"src/a.rs\"}"],
            ),
            (
                "cat src/a.rs | grep -n foo",
                &["grep {\"path\":\"src/a.rs\",\"pattern\":\"foo\"}"],
            ),
            (
                "grep foo < src/a.rs",
                &["grep {\"path\":\"src/a.rs\",\"pattern\":\"foo\"}"],
            ),
            ("cd src && cat a.rs", &["read {\"path\":\"src/a.rs\"}"]),
            ("nl -b a src/a.rs", &["read {\"path\":\"src/a.rs\"}"]),
            ("ls", &["read {\"limit\":200,\"path\":\".\"}"]),
            ("ls -la src", &["read {\"limit\":200,\"path\":\"src\"}"]),
            (
                "cat artifact://jobs/x.log",
                &["read {\"path\":\"artifact://jobs/x.log\"}"],
            ),
            (
                "tail -n 50 artifact://jobs/x.log",
                &["read {\"offset\":-50,\"path\":\"artifact://jobs/x.log\"}"],
            ),
        ];
        for (command, expected) in cases {
            check_case(command, expected);
        }
    }

    #[test]
    fn grep_family_translations() {
        let cases: &[(&str, &[&str])] = &[
            ("rg foo", &["grep {\"pattern\":\"foo\"}"]),
            (
                "rg -n -i 'fn main' src",
                &["grep {\"case\":false,\"path\":\"src\",\"pattern\":\"fn main\"}"],
            ),
            (
                "rg -F 'a.b(' -t rust",
                &["grep {\"glob\":[\"*.rs\"],\"literal\":true,\"pattern\":\"a.b(\"}"],
            ),
            (
                "rg -l TODO src tests",
                &[
                    "grep {\"files_only\":true,\"path\":\"src\",\"pattern\":\"TODO\"}",
                    "grep {\"files_only\":true,\"path\":\"tests\",\"pattern\":\"TODO\"}",
                ],
            ),
            (
                "rg -C 3 foo src | head -50",
                &[
                    "grep {\"context_after\":3,\"context_before\":3,\"path\":\"src\",\"pattern\":\"foo\"}",
                ],
            ),
            (
                "grep -rn \"foo\\|bar\" src/",
                &["grep {\"path\":\"src\",\"pattern\":\"foo|bar\"}"],
            ),
            (
                "grep -rniw --include='*.ts' handler .",
                &[
                    "grep {\"case\":false,\"glob\":[\"*.ts\"],\"pattern\":\"handler\",\"word\":true}",
                ],
            ),
            (
                "grep -E 'a+b' -r src",
                &["grep {\"path\":\"src\",\"pattern\":\"a+b\"}"],
            ),
            (
                "grep 'a+b' -r src",
                &["grep {\"path\":\"src\",\"pattern\":\"a\\\\+b\"}"],
            ),
            (
                "cargo build && rg foo src",
                &["grep {\"path\":\"src\",\"pattern\":\"foo\"}"],
            ),
        ];
        for (command, expected) in cases {
            check_case(command, expected);
        }
    }

    #[test]
    fn find_family_translations() {
        let cases: &[(&str, &[&str])] = &[
            ("find . -name '*.rs'", &["find {\"glob\":[\"*.rs\"]}"]),
            (
                "find src -type f -name '*.toml' -o -name '*.json'",
                &["find {\"glob\":[\"*.toml\",\"*.json\"],\"path\":\"src\"}"],
            ),
            ("fd -e rs", &["find {\"glob\":[\"*.rs\"]}"]),
            (
                "fd -g '*.rs' src",
                &["find {\"glob\":[\"*.rs\"],\"path\":\"src\"}"],
            ),
            ("rg --files -g '*.md'", &["find {\"glob\":[\"*.md\"]}"]),
            (
                "find . -name '*.rs' | head -20",
                &["find {\"glob\":[\"*.rs\"],\"limit\":20}"],
            ),
        ];
        for (command, expected) in cases {
            check_case(command, expected);
        }
    }

    #[test]
    fn write_and_edit_detection() {
        let cases: &[(&str, &[&str])] = &[
            (
                "sed -i 's/a/b/' src/a.rs",
                &["edit {\"path\":\"src/a.rs\"}"],
            ),
            (
                "sed -i.bak -e 's/a/b/' src/a.rs",
                &["edit {\"path\":\"src/a.rs\"}"],
            ),
            (
                "sed -Ei 's/a/b/' src/a.rs",
                &["edit {\"path\":\"src/a.rs\"}"],
            ),
            (
                "perl -pi -e 's/a/b/' src/a.rs",
                &["edit {\"path\":\"src/a.rs\"}"],
            ),
            (
                "cat > src/new.rs <<'EOF'\nfn main() {}\nEOF",
                &["write {\"path\":\"src/new.rs\"}"],
            ),
            (
                "echo 'x = 1' >> config.toml",
                &["edit {\"path\":\"config.toml\"}"],
            ),
            (
                "printf '%s\\n' a b > notes.txt",
                &["write {\"path\":\"notes.txt\"}"],
            ),
            (
                "cat <<EOF | tee src/a.rs\nhi\nEOF",
                &["write {\"path\":\"src/a.rs\"}"],
            ),
            ("tee src/a.rs <<< 'hi'", &["write {\"path\":\"src/a.rs\"}"]),
            (": > src/a.rs", &["write {\"path\":\"src/a.rs\"}"]),
            (
                "python3 - <<'EOF'\nfrom pathlib import Path\np = Path('src/a.rs'); p.write_text(p.read_text().replace('a', 'b'))\nEOF",
                &["edit {}"],
            ),
            ("python -c \"open('out.txt','w').write('x')\"", &["edit {}"]),
            (
                "node -e \"require('fs').writeFileSync('a.json','{}')\"",
                &["edit {}"],
            ),
            ("bash -c 'cat src/a.rs'", &["read {\"path\":\"src/a.rs\"}"]),
            ("git apply <<'EOF'\n--- a/x\n+++ b/x\nEOF", &["edit {}"]),
            (
                "sudo sed -i 's/a/b/' src/a.rs",
                &["edit {\"path\":\"src/a.rs\"}"],
            ),
            (
                "echo hi > ./src/../out.txt",
                &["write {\"path\":\"out.txt\"}"],
            ),
            (
                "echo == a == && cat src/a.rs",
                &["read {\"path\":\"src/a.rs\"}"],
            ),
        ];
        for (command, expected) in cases {
            check_case(command, expected);
        }
        // stdin 参数让首段 tee 拿到字面量输入。
        let (_dir, root) = workspace();
        assert_eq!(
            blocked(&root, &root, true, "tee src/a.rs"),
            Some(vec!["write {\"path\":\"src/a.rs\"}".to_string()])
        );
    }

    #[test]
    fn verdict_leftover_and_render() {
        let (_dir, root) = workspace();
        let verdict = check("cargo build && rg foo src", &root, &root, false).expect("应拦截");
        assert!(verdict.leftover);
        assert!(
            render(&verdict).ends_with("Run the remaining shell steps in a separate bash call.")
        );
        let verdict = check("echo == a == && cat src/a.rs", &root, &root, false).expect("应拦截");
        assert!(!verdict.leftover);
        let text = render(&verdict);
        assert!(text.starts_with(
            "This bash command only reads, searches, lists, or edits workspace files; use built-in tools instead:"
        ));
        assert!(text.contains("- read {\"path\":\"src/a.rs\"}"));
        let verdict = check("sed -i 's/a/b/' src/a.rs", &root, &root, false).expect("应拦截");
        assert!(render(&verdict).contains(
            "- edit {\"path\":\"src/a.rs\"} (instead of sed -i: read the file, then edit the anchored lines)"
        ));
        let verdict =
            check("cat > src/new.rs <<'EOF'\nx\nEOF", &root, &root, false).expect("应拦截");
        assert!(render(&verdict).contains(
            "- write {\"path\":\"src/new.rs\"} (instead of redirect: pass the full content)"
        ));
    }

    #[test]
    fn legitimate_shell_usage_is_left_alone() {
        let (_dir, root) = workspace();
        for command in [
            "git log | rg foo",
            "git diff | grep -c x",
            "cargo test 2>&1 | rg FAIL",
            "ps aux | grep node",
            "cargo test 2>&1 | tail -n 20",
            "git log --oneline | head -20",
            "cargo build > /tmp/build.log 2>&1",
            "cargo run > out.json",
            "echo hi > /dev/null",
            "echo hi >&2",
            "make 2>/dev/null",
            "echo hello",
            "cat <<EOF\nhi\nEOF",
            "cat /tmp/x.log",
            "cat ~/.bashrc",
            "head -n 5 /etc/os-release",
            "cat *.rs",
            "cat \"$f\"",
            "cat src/a.rs src/b.rs > c.rs",
            "\"$CMD\" > src/out.txt",
            "$GEN | tee src/a.rs",
            "sed 's/a/b/' src/a.rs",
            "sed -n '/foo/p' src/a.rs",
            "awk '{print $1}' src/a.rs",
            "rg -c foo src",
            "rg -v foo src",
            "rg -o 'v\\d+' src",
            "rg -U 'a\nb' src",
            "rg -g '!target' foo",
            "grep -c foo src/a.rs",
            "grep -rv foo src",
            "grep -r --exclude-dir=node_modules foo .",
            "rg -P '(?<=a)b' src",
            "head -c 100 src/a.rs",
            "tail -f log.txt",
            "ls -R",
            "ls src/a.rs",
            "ls /tmp",
            "find . -name '*.o' -delete",
            "find . -mtime -1",
            "find . -maxdepth 1 -name '*.rs'",
            "find . -type d -name target",
            "find /tmp -name x",
            "fd 'ma.n'",
            "fd foo -e rs",
            "nl -bt src/a.rs",
            "cp src/a.rs b.rs",
            "mv a b",
            "touch src/a.rs",
            "mkdir -p x",
            "rm -rf target",
            "cargo fmt",
            "git apply fix.patch",
            "patch -p1 < fix.patch",
            "sed -i 's/a/b/' /tmp/x.conf",
            "sed -i 's/a/b/' src/a.rs src/b.rs",
            "sed -i 's/a/b/' src/*.rs",
            "perl -pi -e 's/a/b/' $(rg -l a)",
            "rg -l foo | xargs sed -i 's/foo/bar/g'",
            "find . -name '*.rs' -exec sed -i 's/a/b/' {} +",
            "for f in src/*.rs; do sed -i 's/a/b/' \"$f\"; done",
            "echo x > /tmp/scratch.txt",
            "cat > /tmp/t.py <<EOF\nprint(1)\nEOF",
            "python3 -c 'print(1)'",
            "python3 gen.py",
            "python - <<EOF\nimport json; print(json.load(open('a.json')))\nEOF",
            "python3 - <<'EOF'\nwith open('a.txt') as f: print(f.read() + '>')\nEOF",
            "python -m json.tool <<'EOF'\n{\"a\": \"open(x, 'w')\"}\nEOF",
            "jq . package.json",
            "cat package.json | jq .name",
            "wc -l src/a.rs",
            "cargo test 2>&1 | tee test.log",
            "bash -c \"$CMD\"",
        ] {
            miss(&root, command);
        }
        // stdin 参数下无操作数的搜索读的是字面量 stdin。
        assert_eq!(blocked(&root, &root, true, "rg foo"), None);
        assert_eq!(blocked(&root, &root, true, "grep foo"), None);
        // bash cwd 在工作区外时相对路径不翻译。
        let outside = tempfile::tempdir().expect("临时目录");
        assert_eq!(blocked(&root, outside.path(), false, "cat a.rs"), None);
    }
}

//! 内置工具优先策略：bash 调用如果只是在读/搜/列/改工作区文件，拒绝并给出
//! 等价的 read/grep/find/edit/write 调用。
//!
//! 判定分两层：整条管道规约到「一个读/搜/列命令」时产出 read/grep/find
//! 建议；逐段的写/改检测（原地编辑器、tee、字面量重定向、解释器内联）在
//! 管道的任何位置都算命中。不可翻译的一律放行，宁缺毋滥。

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

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

mod operands;
use operands::operand;
use operands::resolve_dir;
use operands::tokens;
mod read;
use read::cat_read;
use read::head_read;
use read::ls_read;
use read::nl_read;
use read::sed_print;
use read::tail_read;
mod search;
use search::grep_translate;
use search::rg_translate;
mod find;
use find::fd_translate;
use find::find_translate;
mod writes;
use writes::detect_edit;
use writes::is_literal_producer;

// ---------- argv 标志切分 ----------

/// argv 中标志与操作数的划分。
enum Tok<'a> {
    /// 短簇或长标志；`name` 不带前导 `-`（长为单词，短为单字符；数字开头
    /// 的簇在 `num_flag` 时整体为一个标志，如 grep `-NUM`、head `-N`）。
    Flag { name: String, value: Option<String> },
    /// 位置操作数。
    Operand(&'a Arg),
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
mod tests;

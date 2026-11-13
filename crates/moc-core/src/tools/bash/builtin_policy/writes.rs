use super::super::syntax::Arg;
use super::super::syntax::Redirect;
use super::super::syntax::Simple;
use super::INTERP_WRITE;
use super::Operand;
use super::Scope;
use super::Suggestion;
use super::Tok;
use super::Verdict;
use super::basename;
use super::eval;
use super::operands::flag_names;
use super::operands::operand;
use super::operands::operands;
use super::operands::relative;
use super::operands::tokens;
use super::verb;
use grep_matcher::Matcher;
use serde_json::Map;

// ---------- 写 / 改判定 ----------

/// 逐段的写/改检测；命中往 `verdict.suggestions` 追加。
pub(super) fn detect_edit(
    seg: &Simple,
    index: usize,
    segs: &[&Simple],
    scope: &Scope,
    verdict: &mut Verdict,
) {
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

pub(super) fn literal_or_interp(
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
pub(super) fn dash_c(args: &[Arg]) -> Option<String> {
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
pub(super) fn editor_tail(args: &[Arg]) -> Option<&[Arg]> {
    let pos = args.iter().position(|arg| {
        arg.value
            .as_deref()
            .is_some_and(|v| matches!(basename(v), "sed" | "perl" | "awk" | "gawk" | "ed" | "ex"))
    })?;
    Some(&args[pos..])
}

/// `find` 的 `-exec`/`-execdir`/`-ok` 尾部（到 `;`/`+` 为止）里的编辑器段。
pub(super) fn find_exec_tail(args: &[Arg]) -> Option<&[Arg]> {
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
pub(super) fn editor_hit(name: &str, args: &[Arg]) -> bool {
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
pub(super) fn emit_editor(argv: &[Arg], scope: &Scope, suggestions: &mut Vec<Suggestion>) {
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
pub(super) fn editor_targets<'a>(name: &str, args: &'a [Arg]) -> (&'static str, Vec<&'a Arg>) {
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
pub(super) fn file_args<'a>(
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
pub(super) fn patch_from_stdin(
    name: &str,
    seg: &Simple,
    stdin_literal: bool,
) -> Option<&'static str> {
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
pub(super) fn literal_write(seg: &Simple, index: usize, scope: &Scope) -> Option<Vec<Suggestion>> {
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
pub(super) fn cat_stdin_literal(seg: &Simple, index: usize, scope: &Scope) -> bool {
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
pub(super) fn tee_write(
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
pub(super) fn is_literal_producer(seg: &Simple, index: usize, scope: &Scope) -> bool {
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
pub(super) fn interp_writes(seg: &Simple) -> Option<&'static str> {
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
pub(super) fn interp_code(seg: &Simple, family: &str) -> Option<String> {
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

use super::super::syntax::Arg;
use super::super::syntax::Simple;
use super::Operand;
use super::Scope;
use super::Suggestion;
use super::Tok;
use super::operands::flag_names;
use super::operands::operand;
use super::operands::operand_path;
use super::operands::operands;
use super::operands::relative;
use super::operands::tokens;
use serde_json::Map;
use serde_json::Value;

/// read 类翻译共用：每个文件操作数一条 read；无操作数时吃归一化输入。
pub(super) fn read_suggestions(
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
pub(super) fn cat_read(
    seg: &Simple,
    input: Option<&Operand>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
    let toks = tokens(&seg.argv[1..], &[], "", false);
    if flag_names(&toks).any(|name| name != "n") {
        return None;
    }
    read_suggestions(&operands(&toks), input, Map::new(), scope)
}

/// `nl` / `nl -ba F` / `nl -b a F`；`-b` 只接受值 `a`，其它 flag 不翻译。
pub(super) fn nl_read(
    seg: &Simple,
    input: Option<&Operand>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
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
pub(super) fn head_read(
    seg: &Simple,
    input: Option<&Operand>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
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
pub(super) fn tail_read(
    seg: &Simple,
    input: Option<&Operand>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
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

pub(super) fn tail_offset(value: &str) -> Option<i64> {
    if let Some(plus) = value.strip_prefix('+') {
        plus.parse().ok()
    } else {
        value.parse::<i64>().ok().map(|n| -n)
    }
}

/// `sed -n SCRIPT F`：只认 `Np`、`A,Bp`、`A,$p`。
pub(super) fn sed_print(
    seg: &Simple,
    input: Option<&Operand>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
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
pub(super) fn sed_range(script: &str) -> Option<(u64, Option<u64>)> {
    let range = script.strip_suffix('p')?;
    let parse = |s: &str| s.parse::<u64>().ok();
    match range.split_once(',') {
        None => parse(range).map(|n| (n, Some(1))),
        Some((a, "$")) => parse(a).map(|a| (a, None)),
        Some((a, b)) => Some((parse(a)?, Some(parse(b)?.saturating_sub(parse(a)?) + 1))),
    }
}

/// `ls [flags] [D...]`：操作数为空（=有效目录）或全是工作区内现存目录。
pub(super) fn ls_read(seg: &Simple, scope: &Scope) -> Option<Vec<Suggestion>> {
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

pub(super) fn ls_suggestion(path: String) -> Suggestion {
    let mut args = Map::new();
    args.insert("limit".into(), 200.into());
    args.insert("path".into(), path.into());
    Suggestion {
        tool: "read",
        args,
        via: None,
    }
}

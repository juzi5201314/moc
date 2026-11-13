use super::super::syntax::Arg;
use super::super::syntax::Simple;
use super::Operand;
use super::Scope;
use super::Suggestion;
use super::Tok;
use super::operands::operand;
use super::operands::operands;
use super::operands::scan_root_arg;
use super::operands::tokens;
use super::search::type_globs;
use serde_json::Map;
use serde_json::Value;

// ---------- 列举翻译（find / fd / rg --files）----------

/// `find [P...] [expr]`：只允许 `-name G`、`-path G`、`-type f`、`-print`
/// 以及 `-o` 连接的上述谓词与包裹它们的括号。
pub(super) fn find_translate(
    seg: &Simple,
    head: Option<usize>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
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
pub(super) fn strip_path_prefix(glob: &str, roots: &[String]) -> String {
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
pub(super) fn find_suggestions(
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
pub(super) fn fd_translate(
    seg: &Simple,
    head: Option<usize>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
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
pub(super) fn rg_files(
    seg: &Simple,
    head: Option<usize>,
    scope: &Scope,
) -> Option<Vec<Suggestion>> {
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

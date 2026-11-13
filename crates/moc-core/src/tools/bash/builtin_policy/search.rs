use super::super::syntax::Arg;
use super::super::syntax::Simple;
use super::GrepShape;
use super::Kind;
use super::Operand;
use super::Scope;
use super::Suggestion;
use super::Tok;
use super::find::rg_files;
use super::operands::operand;
use super::operands::operands;
use super::operands::relative;
use super::operands::scan_root_arg;
use super::operands::tokens;
use serde_json::Map;
use std::sync::LazyLock;

/// `rg`：`-e` 可多次、位置参数首个为 pattern、其余为路径。
pub(super) fn rg_translate(
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
pub(super) fn grep_translate(
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
pub(super) fn search_roots(
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
pub(super) fn grep_suggestions(
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
pub(super) fn combine_patterns(patterns: Vec<String>, literal: &mut bool) -> String {
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

pub(super) fn regex_escape(text: &str) -> String {
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
pub(super) fn bre_to_regex(pattern: &str) -> String {
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
pub(super) fn type_globs(name: &str) -> Option<Vec<String>> {
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

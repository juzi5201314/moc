use super::super::syntax::Arg;
use super::Operand;
use super::Scope;
use super::Tok;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// 路径操作数：必须静态且不含 glob；相对路径基于当前有效目录；规范化后
/// 落在 root 下才算工作区内。资源 URI 原样保留。
pub(super) fn operand(arg: &Arg, scope: &Scope) -> Operand {
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
pub(super) fn normalize(path: &Path) -> PathBuf {
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
pub(super) fn resolve_dir(arg: &str, scope: &Scope) -> Option<PathBuf> {
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
pub(super) fn operand_path(op: &Operand, scope: &Scope) -> Option<String> {
    match op {
        Operand::Workspace(path) => Some(relative(&scope.root, path)),
        Operand::Uri(uri) => Some(uri.clone()),
        Operand::Other => None,
    }
}

/// 搜索根的 path 参数：等于 root 时返回 `Some(None)` 表示省略参数；
/// URI 只在 `uri_ok`（read/grep）时接受。
pub(super) fn scan_root_arg(op: &Operand, scope: &Scope, uri_ok: bool) -> Option<Option<String>> {
    match op {
        Operand::Workspace(path) if path == &scope.root => Some(None),
        Operand::Workspace(path) => Some(Some(relative(&scope.root, path))),
        Operand::Uri(uri) if uri_ok => Some(Some(uri.clone())),
        _ => None,
    }
}

pub(super) fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        rel.display().to_string()
    }
}

/// 切分 args：`--` 之后全是操作数；`long_values`/`short_values` 是会吃掉
/// 下一个参数（或 `=` 后缀、簇内剩余）的标志名。
pub(super) fn tokens<'a>(
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

pub(super) fn operands<'a>(toks: &[Tok<'a>]) -> Vec<&'a Arg> {
    toks.iter()
        .filter_map(|tok| match tok {
            Tok::Operand(arg) => Some(*arg),
            _ => None,
        })
        .collect()
}

pub(super) fn flag_names<'a>(toks: &'a [Tok<'a>]) -> impl Iterator<Item = &'a str> + 'a {
    toks.iter().filter_map(|tok| match tok {
        Tok::Flag { name, .. } => Some(name.as_str()),
        _ => None,
    })
}

//! workspace 沙盒内直接执行的 bash 命令判定。
//!
//! 白名单命令和用户配置的正则规则只决定命令能否在 workspace 沙盒中执行；
//! 无法确认安全的命令须经用户批准，并在沙盒外执行一次以尊重用户决定。

use std::path::{Component, Path, PathBuf};

use grep_matcher::Matcher;

use crate::{config::BashConfig, tools::ToolError};

pub(crate) fn reject_unscoped_scan(
    command: &str,
    cwd: &Path,
    workspace: &Path,
) -> Result<(), ToolError> {
    let Some(segments) = split_segments(command) else {
        return Ok(());
    };
    for segment in segments {
        let Ok(words) = shell_words::split(&segment) else {
            continue;
        };
        let Some((name, args)) = command_words(&words) else {
            continue;
        };
        if !matches!(name, "find" | "grep" | "rg") {
            continue;
        }
        if !cwd.starts_with(workspace)
            || crate::tools::is_slow_filesystem_path(cwd)
            || crate::tools::is_filesystem_mount(cwd)
            || args.iter().any(|arg| {
                let looks_like_path = arg == "."
                    || arg == ".."
                    || arg.starts_with('/')
                    || arg.starts_with("./")
                    || arg.starts_with("../")
                    || arg.starts_with("~/");
                looks_like_path && !path_within(arg, cwd, workspace)
            })
        {
            return Err(ToolError::new(
                "E_SCAN_SCOPE",
                "recursive scans are restricted to the current workspace and local filesystem",
            ));
        }
    }
    Ok(())
}

/// 返回命令是否可在 workspace 沙盒中直接执行。
pub(crate) fn allowed_in_sandbox(
    config: &BashConfig,
    command: &str,
    cwd: &Path,
    workspace: &Path,
) -> Result<bool, ToolError> {
    if !cwd.starts_with(workspace) {
        return Ok(false);
    }

    if configured_allow(config, command)? {
        return Ok(!mentions_external_path(command, cwd, workspace));
    }

    let Some(segments) = split_segments(command) else {
        return Ok(false);
    };
    if segments.is_empty() {
        return Ok(false);
    }

    for segment in segments {
        if contains_unsafe_shell_syntax(&segment) {
            return Ok(false);
        }
        let Ok(words) = shell_words::split(&segment) else {
            return Ok(false);
        };
        let Some((name, args)) = command_words(&words) else {
            return Ok(false);
        };
        if !builtin_allowed(name, args) {
            return Ok(false);
        }
        if builtin_writes(name, args) && mentions_external_path_words(args, cwd, workspace) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn configured_allow(config: &BashConfig, command: &str) -> Result<bool, ToolError> {
    config
        .allow_patterns
        .iter()
        .try_fold(false, |matched, pattern| {
            if matched {
                return Ok(true);
            }
            let expression = format!("^(?:{pattern})$");
            let matcher = grep_regex::RegexMatcher::new(&expression).map_err(|error| {
                ToolError::new(
                    "E_CONFIG",
                    format!("bash allowlist rule is invalid: {error}"),
                )
            })?;
            Ok(matcher.is_match(command.as_bytes()).unwrap_or(false))
        })
}

fn split_segments(command: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            current.push(ch);
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            current.push(ch);
            if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            ';' | '\n' | '|' => {
                push_segment(&mut segments, &mut current);
                if ch == '|' && chars.peek() == Some(&'|') {
                    chars.next();
                }
            }
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    push_segment(&mut segments, &mut current);
                } else {
                    return None;
                }
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    push_segment(&mut segments, &mut current);
    Some(segments)
}

fn push_segment(segments: &mut Vec<String>, current: &mut String) {
    if !current.trim().is_empty() {
        segments.push(std::mem::take(current));
    }
}

fn contains_unsafe_shell_syntax(segment: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    let mut chars = segment.chars().peekable();
    while let Some(ch) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '>' | '<' | '`' | '$' | '(' | ')' | '{' | '}' => return true,
            _ => {}
        }
    }
    false
}

fn command_words(words: &[String]) -> Option<(&str, &[String])> {
    let mut index = 0;
    while let Some(word) = words.get(index)
        && is_assignment(word)
    {
        index += 1;
    }
    let command = words.get(index)?;
    let name = Path::new(command).file_name()?.to_str()?;
    Some((name, &words[index + 1..]))
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte == b'_' || byte.is_ascii_alphabetic()
            } else {
                byte == b'_' || byte.is_ascii_alphanumeric()
            }
        })
}

fn builtin_allowed(name: &str, args: &[String]) -> bool {
    match name {
        "basename" | "cat" | "cd" | "date" | "df" | "diff" | "dirname" | "du" | "echo" | "file"
        | "head" | "id" | "jq" | "ls" | "mkdir" | "mktemp" | "nl" | "printf" | "pwd"
        | "readlink" | "realpath" | "seq" | "sleep" | "sort" | "stat" | "tail" | "test"
        | "touch" | "true" | "false" | "type" | "uniq" | "uname" | "wc" | "which" | "whoami" => {
            true
        }
        "chmod" | "cp" | "install" | "ln" | "mv" | "tee" => true,
        "find" => !args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir"
            )
        }),
        "grep" | "rg" => true,
        "sed" => true,
        "git" => git_allowed(args),
        "cargo" => cargo_allowed(args),
        "go" => go_allowed(args),
        _ => false,
    }
}

fn builtin_writes(name: &str, args: &[String]) -> bool {
    match name {
        "chmod" | "cp" | "install" | "ln" | "mkdir" | "mktemp" | "mv" | "tee" | "touch" => true,
        "sed" => args.iter().any(|arg| arg == "-i" || arg.starts_with("-i")),
        "git" => git_subcommand(args)
            .is_some_and(|subcommand| matches!(subcommand, "add" | "branch" | "commit" | "tag")),
        "cargo" => cargo_allowed(args),
        "go" => go_allowed(args),
        _ => false,
    }
}

fn git_allowed(args: &[String]) -> bool {
    let Some(subcommand) = git_subcommand(args) else {
        return false;
    };
    match subcommand {
        "add" | "commit" => true,
        "branch" | "tag" => !args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-d" | "-D" | "--delete" | "-m" | "-M" | "--move"
            )
        }),
        "diff" | "log" | "ls-files" | "remote" | "rev-parse" | "show" | "status" => true,
        _ => false,
    }
}

fn git_subcommand(args: &[String]) -> Option<&str> {
    args.iter().map(String::as_str).find(|arg| {
        matches!(
            *arg,
            "add"
                | "branch"
                | "commit"
                | "diff"
                | "log"
                | "ls-files"
                | "remote"
                | "rev-parse"
                | "show"
                | "status"
                | "tag"
        )
    })
}

fn cargo_allowed(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "build" | "check" | "clippy" | "fmt" | "metadata" | "test"
        )
    })
}

fn go_allowed(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "build" | "fmt" | "test" | "vet"))
}

fn mentions_external_path(command: &str, cwd: &Path, workspace: &Path) -> bool {
    let Some(segments) = split_segments(command) else {
        return true;
    };
    segments.into_iter().any(|segment| {
        let Ok(words) = shell_words::split(&segment) else {
            return true;
        };
        let Some((_, args)) = command_words(&words) else {
            return true;
        };
        mentions_external_path_words(args, cwd, workspace)
    })
}

fn mentions_external_path_words(words: &[String], cwd: &Path, workspace: &Path) -> bool {
    words.iter().any(|word| {
        let candidate = word
            .strip_prefix("--git-dir=")
            .or_else(|| word.strip_prefix("--work-tree="))
            .or_else(|| word.strip_prefix("--manifest-path="))
            .unwrap_or(word);
        if candidate == "~" || candidate.starts_with("~/") || candidate.starts_with('$') {
            return true;
        }
        let looks_like_path = candidate == "."
            || candidate == ".."
            || candidate.starts_with('/')
            || candidate.starts_with("./")
            || candidate.starts_with("../")
            || candidate.contains('/');
        looks_like_path && !path_within(candidate, cwd, workspace)
    })
}

fn path_within(candidate: &str, cwd: &Path, workspace: &Path) -> bool {
    let path = Path::new(candidate);
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    canonicalize_with_missing_tail(&absolute)
        .is_some_and(|normalized| normalized.starts_with(workspace))
}

fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = path;
    while !existing.exists() {
        missing.push(existing.file_name()?.to_owned());
        existing = existing.parent()?;
    }
    let mut result = std::fs::canonicalize(existing).ok()?;
    for component in missing.into_iter().rev() {
        result.push(component);
    }
    Some(normalize_path(result))
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BashConfig {
        BashConfig::default()
    }

    #[test]
    fn common_workspace_commands_bypass_approval() {
        let workspace = tempfile::tempdir().expect("workspace");
        let cwd = workspace.path().join("src");
        std::fs::create_dir(&cwd).expect("创建 cwd");
        let config = config();
        for command in [
            "mkdir -p generated",
            "cp src.txt generated/src.txt",
            "git add src/main.rs",
            "git commit -m 'save change'",
            "cargo test --quiet",
            "sed -i 's/old/new/' src/main.rs",
            "git status --short",
            "cat /etc/hosts",
        ] {
            assert!(
                allowed_in_sandbox(&config, command, &cwd, workspace.path()).expect("判定命令"),
                "命令应留在沙盒：{command}"
            );
        }
    }

    #[test]
    fn unknown_commands_and_external_writes_require_approval() {
        let workspace = tempfile::tempdir().expect("workspace");
        let config = config();
        assert!(
            !allowed_in_sandbox(
                &config,
                "printf x > output.txt",
                workspace.path(),
                workspace.path()
            )
            .expect("判定重定向")
        );
        assert!(
            !allowed_in_sandbox(
                &config,
                "cp input.txt /tmp/output.txt",
                workspace.path(),
                workspace.path()
            )
            .expect("判定外部路径")
        );
        assert!(
            !allowed_in_sandbox(
                &config,
                "python3 build.py",
                workspace.path(),
                workspace.path()
            )
            .expect("判定未知命令")
        );
    }

    #[test]
    fn configured_patterns_extend_builtin_allowlist() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut config = config();
        config.allow_patterns = vec![r"python3\s+build\.py".into()];
        assert!(
            allowed_in_sandbox(
                &config,
                "python3 build.py",
                workspace.path(),
                workspace.path()
            )
            .expect("判定配置命令")
        );
    }
}

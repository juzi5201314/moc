//! workspace 沙盒内直接执行的 bash 命令判定。
//!
//! 白名单命令和用户配置的正则规则只决定命令能否在 workspace 沙盒中执行；
//! 无法确认安全的命令须经用户批准，并在沙盒外执行一次以尊重用户决定。

use std::fs;
use std::path::{Component, Path, PathBuf};

use grep_matcher::Matcher;

use crate::{config::BashConfig, tools::ToolError};

/// 允许核实的 sed 脚本文件上限；超过或不可读时按「写工作区外」处理。
const MAX_SED_SCRIPT_BYTES: u64 = 64 * 1024;

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
        if name == "sed" && sed_escapes_workspace(args, cwd, workspace) {
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
            } else if active == '"' && matches!(ch, '$' | '`') {
                // 双引号内同样会展开命令替换与参数展开（echo "$(id)"），
                // 单引号内的 `$`/`` ` `` 是字面量，保持放行。
                return true;
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
        "sed" => args
            .iter()
            .any(|arg| arg.starts_with("-i") || arg.starts_with("--in-place")),
        "git" => git_subcommand(args)
            .is_some_and(|subcommand| matches!(subcommand, "add" | "branch" | "commit" | "tag")),
        "cargo" => cargo_allowed(args),
        "go" => go_allowed(args),
        _ => false,
    }
}

/// sed 脚本的写目标与执行命令都会绕开白名单语义：`w`/`W` 可能写到工作区外，
/// `e` 命令、`s///e` 标志与解析不了的脚本等于在白名单命令里藏一段任意 shell。
fn sed_escapes_workspace(args: &[String], cwd: &Path, workspace: &Path) -> bool {
    let escapes = |script: &str| {
        let (targets, opaque) = scan_sed_script(script);
        opaque
            || targets
                .iter()
                .any(|target| !path_within(target, cwd, workspace))
    };
    let (scripts, files) = sed_scripts(args);
    if scripts.iter().any(|script| escapes(script)) {
        return true;
    }
    files
        .iter()
        .any(|file| match readable_script(file, cwd, workspace) {
            Some(text) => escapes(&text),
            // 脚本文件在工作区外、不可读或过大时无从核实，按外部写入交给审批。
            None => true,
        })
}

/// 拆分 sed 参数为内联脚本与 `-f` 脚本文件：给了 `-e`/`-f` 后，位置参数是输入文件。
fn sed_scripts(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut scripts = Vec::new();
    let mut files = Vec::new();
    let mut explicit = false;
    let mut positional = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        index += 1;
        if let Some(rest) = arg.strip_prefix("--expression=") {
            scripts.push(rest.to_owned());
            explicit = true;
        } else if let Some(rest) = arg.strip_prefix("--file=") {
            files.push(rest.to_owned());
            explicit = true;
        } else if matches!(arg.as_str(), "-e" | "--expression") {
            scripts.extend(args.get(index).cloned());
            index += 1;
            explicit = true;
        } else if matches!(arg.as_str(), "-f" | "--file") {
            files.extend(args.get(index).cloned());
            index += 1;
            explicit = true;
        } else if let Some(rest) = arg.strip_prefix("-e").filter(|rest| !rest.is_empty()) {
            scripts.push(rest.to_owned());
            explicit = true;
        } else if let Some(rest) = arg.strip_prefix("-f").filter(|rest| !rest.is_empty()) {
            files.push(rest.to_owned());
            explicit = true;
        } else if arg.starts_with('-') {
            // 其它选项（-n/-E/-i/--in-place 等）不携带脚本。
        } else if !explicit && !positional {
            scripts.push(arg.clone());
            positional = true;
        }
    }
    (scripts, files)
}

/// 读取工作区内的 sed 脚本文件；为假说明内容不可核实。
fn readable_script(file: &str, cwd: &Path, workspace: &Path) -> Option<String> {
    let path = Path::new(file);
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let canonical = fs::canonicalize(absolute).ok()?;
    if !canonical.starts_with(workspace) {
        return None;
    }
    let metadata = fs::metadata(&canonical).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SED_SCRIPT_BYTES {
        return None;
    }
    fs::read_to_string(&canonical).ok()
}

/// 扫描一段 sed 脚本，返回 `w`/`W`/`s///w` 的写目标，以及脚本是否含执行 shell
/// 或无法解析的命令：这两种情况都不允许直放沙盒。
fn scan_sed_script(script: &str) -> (Vec<String>, bool) {
    let chars: Vec<char> = script.chars().collect();
    let mut targets = Vec::new();
    let mut opaque = false;
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            ';' | '\n' | ' ' | '\t' | '{' | '}' => {
                index += 1;
                continue;
            }
            '#' => {
                index = line_end(&chars, index);
                continue;
            }
            _ => {}
        }
        index = skip_sed_address(&chars, index);
        index = skip_blanks(&chars, index);
        if chars.get(index) == Some(&'!') {
            index = skip_blanks(&chars, index + 1);
        }
        let Some(&command) = chars.get(index) else {
            break;
        };
        index += 1;
        match command {
            'w' | 'W' => {
                // GNU sed 把 `w` 的文件名读到行尾，其余内容不属于后续命令。
                let (target, _) = script_token(&chars, index);
                targets.push(target);
                index = line_end(&chars, index);
            }
            'e' => {
                opaque = true;
                index = line_end(&chars, index);
            }
            's' => {
                let (next, target, shell) = scan_sed_substitute(&chars, index);
                opaque |= shell;
                targets.extend(target);
                index = next;
            }
            'y' => {
                index = scan_sed_delimited_parts(&chars, index, 2);
            }
            // `a`/`i`/`c` 的文本与 `r`/`R` 的文件名都读到行尾；标签读到行尾前一个分号。
            'a' | 'i' | 'c' | 'r' | 'R' => index = line_end(&chars, index),
            'b' | 't' | 'T' | ':' => index = command_end(&chars, index),
            'd' | 'D' | 'F' | 'g' | 'G' | 'h' | 'H' | 'l' | 'n' | 'N' | 'p' | 'P' | 'q' | 'Q'
            | 'v' | 'x' | 'z' | '=' => {}
            // 解析不了的命令按不可核实处理：脚本语义无法确认时一律交出审批。
            _ => {
                tracing::warn!("sed 脚本包含无法解析的命令，交由审批确认");
                return (targets, true);
            }
        }
    }
    (targets, opaque)
}

/// 解析 `s` 命令：返回（下一个下标、`w` 标志的写目标、是否含执行 shell 的标志）。
fn scan_sed_substitute(chars: &[char], start: usize) -> (usize, Option<String>, bool) {
    let Some(&delimiter) = chars.get(start) else {
        return (chars.len(), None, false);
    };
    if delimiter == '\\' || delimiter == '\n' {
        return (line_end(chars, start), None, false);
    }
    let next = scan_sed_delimited_parts(chars, start, 2);
    let mut cursor = next;
    let mut target = None;
    let mut executes = false;
    while let Some(&flag) = chars.get(cursor) {
        if flag.is_whitespace() || flag == ';' {
            break;
        }
        match flag {
            'w' => {
                let (found, _) = script_token(chars, cursor + 1);
                target = Some(found);
                cursor = line_end(chars, cursor);
                break;
            }
            'e' => {
                executes = true;
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    (cursor, target, executes)
}

/// 跳过分隔符包围的 `count` 段；未闭合时视为读到了脚本末尾。
fn scan_sed_delimited_parts(chars: &[char], start: usize, count: usize) -> usize {
    let Some(&delimiter) = chars.get(start) else {
        return chars.len();
    };
    if delimiter == '\\' || delimiter == '\n' {
        return line_end(chars, start);
    }
    let mut cursor = start + 1;
    for _ in 0..count {
        let mut closed = false;
        while cursor < chars.len() {
            match chars[cursor] {
                '\\' => cursor += 2,
                ch if ch == delimiter => {
                    cursor += 1;
                    closed = true;
                    break;
                }
                _ => cursor += 1,
            }
        }
        if !closed {
            return chars.len();
        }
    }
    cursor
}

/// 跳过 sed 地址：行号、`$`、`/正则/`、`\c正则c`、`,`/`+`/`~` 与 `!` 前导组合。
fn skip_sed_address(chars: &[char], start: usize) -> usize {
    let mut cursor = skip_blanks(chars, start);
    loop {
        match chars.get(cursor) {
            Some(ch) if ch.is_ascii_digit() => {
                while chars.get(cursor).is_some_and(char::is_ascii_digit) {
                    cursor += 1;
                }
                if chars.get(cursor) == Some(&'~') {
                    cursor += 1;
                    while chars.get(cursor).is_some_and(char::is_ascii_digit) {
                        cursor += 1;
                    }
                }
            }
            Some('$') => cursor += 1,
            Some('/') => {
                cursor += 1;
                while cursor < chars.len() {
                    match chars[cursor] {
                        '\\' => cursor += 2,
                        '/' => {
                            cursor += 1;
                            break;
                        }
                        _ => cursor += 1,
                    }
                }
                cursor = cursor.min(chars.len());
            }
            Some('\\') => {
                let Some(&delimiter) = chars.get(cursor + 1) else {
                    return chars.len();
                };
                cursor += 2;
                while cursor < chars.len() {
                    match chars[cursor] {
                        '\\' => cursor += 2,
                        ch if ch == delimiter => {
                            cursor += 1;
                            break;
                        }
                        _ => cursor += 1,
                    }
                }
                cursor = cursor.min(chars.len());
            }
            _ => return cursor,
        }
        cursor = skip_blanks(chars, cursor);
        match chars.get(cursor) {
            Some(',') => {
                cursor = skip_blanks(chars, cursor + 1);
                if chars.get(cursor) == Some(&'+') {
                    cursor += 1;
                    while chars.get(cursor).is_some_and(char::is_ascii_digit) {
                        cursor += 1;
                    }
                }
            }
            _ => return cursor,
        }
    }
}

fn script_token(chars: &[char], start: usize) -> (String, usize) {
    let mut cursor = skip_blanks(chars, start);
    let begin = cursor;
    while chars
        .get(cursor)
        .is_some_and(|ch| !ch.is_whitespace() && *ch != ';' && *ch != '\n')
    {
        cursor += 1;
    }
    (chars[begin..cursor].iter().collect(), cursor)
}

fn skip_blanks(chars: &[char], start: usize) -> usize {
    let mut cursor = start;
    while chars
        .get(cursor)
        .is_some_and(|ch| *ch == ' ' || *ch == '\t')
    {
        cursor += 1;
    }
    cursor
}

fn line_end(chars: &[char], start: usize) -> usize {
    let mut cursor = start;
    while cursor < chars.len() && chars[cursor] != '\n' {
        cursor += 1;
    }
    cursor
}

fn command_end(chars: &[char], start: usize) -> usize {
    let mut cursor = start;
    while cursor < chars.len() && chars[cursor] != '\n' && chars[cursor] != ';' {
        cursor += 1;
    }
    cursor
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
    fn quoted_substitution_and_sed_writes_require_approval() {
        let workspace = tempfile::tempdir().expect("workspace");
        let config = config();
        let outside = tempfile::tempdir().expect("工作区外目录");
        let script = outside.path().join("write.sed");
        std::fs::write(&script, "w /etc/passwd\n").expect("工作区外脚本");
        std::fs::write(workspace.path().join("inside.sed"), "w out.txt\n").expect("工作区脚本");
        let denied = |command: &str| {
            assert!(
                !allowed_in_sandbox(&config, command, workspace.path(), workspace.path())
                    .expect("判定命令"),
                "命令应要求审批：{command}"
            );
        };
        for command in [
            // 双引号内的命令替换与参数展开都不得再被白名单放过。
            r#"echo "$(id)""#,
            "echo \"`id`\"",
            r#"cat "$HOME/.moc/config.toml""#,
            // sed 的写脚本与执行命令躲在白名单命令里。
            "sed -n 'w /etc/passwd' x",
            "sed -n 's/a/b/w /etc/passwd' x",
            "sed '1e id' x",
            "sed 's/a/$(id)/e' x",
            // 无法核实的脚本文件（不存在或在工作区外）同样交给审批。
            "sed -f missing.sed x",
        ] {
            denied(command);
        }
        denied(&format!("sed -f {} x", script.display()));
        // 解析不了的命令按不可核实处理，同样不得直放沙盒。
        denied("sed -n '1,2.w /tmp/leak' x");
        denied("sed -n '1w /tmp/leak' x");
    }

    #[test]
    fn read_only_quotes_and_local_sed_scripts_stay_in_the_sandbox() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("inside.sed"), "w out.txt\n").expect("工作区脚本");
        let config = config();
        for command in [
            "echo 'literal $(id)'",
            "grep 'a b' src/main.rs",
            "sed -n '1,5p' src/main.rs",
            "sed 's/w/x/' src/main.rs",
            "sed -n '/w /p' src/main.rs",
            "sed -n '1,5w out.txt' src/main.rs",
            "sed -n 's/a/b/w out.txt' src/main.rs",
            "sed -i 's/old/new/' src/main.rs",
            "sed -f inside.sed src/main.rs",
        ] {
            assert!(
                allowed_in_sandbox(&config, command, workspace.path(), workspace.path())
                    .expect("判定命令"),
                "命令应留在沙盒：{command}"
            );
        }
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

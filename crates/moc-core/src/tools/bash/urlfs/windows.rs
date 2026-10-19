//! PowerShell / cmd 的裸词扫描与路径引用。
//!
//! brush-parser 只理解 POSIX；PowerShell 和 cmd 的引号、注释、here-string 与变量
//! 展开规则都不同，所以这里只做最低限度的词法扫描：找出完整的、未被引号或转义保护
//! 的词，跳过重定向/赋值目标和动态词。命令结构不需要完整解析。

use std::iter::Peekable;
use std::str::CharIndices;

use super::super::syntax::BareWord;
use crate::shell::ShellKind;

/// 扫描过程中积累的一个词。
struct Token {
    start: usize,
    end: usize,
    bare: bool,
    skipped: bool,
}

/// 把字符并入当前词；`bare` 只会从 true 变成 false。
fn touch(token: &mut Option<Token>, start: usize, end: usize, bare: bool, skip_next: &mut bool) {
    let token = token.get_or_insert(Token {
        start,
        end,
        bare,
        skipped: *skip_next,
    });
    token.end = end;
    token.bare &= bare;
}

/// 结束当前词；只有完整的裸词才进入结果。
fn finish(
    command: &str,
    token: &mut Option<Token>,
    words: &mut Vec<BareWord>,
    skip_next: &mut bool,
) {
    let Some(token) = token.take() else {
        return;
    };
    if token.bare && !token.skipped && token.end > token.start {
        words.push(BareWord {
            text: command[token.start..token.end].to_owned(),
            start: token.start,
            end: token.end,
        });
    }
    *skip_next = false;
}

/// 扫描 PowerShell / cmd 命令里的裸词。
pub(crate) fn scan(command: &str, kind: ShellKind) -> Vec<BareWord> {
    match kind {
        ShellKind::PowerShell => scan_powershell(command),
        ShellKind::Cmd => scan_cmd(command),
        _ => Vec::new(),
    }
}

/// 按 shell 方言引用路径前缀；glob 尾部由调用方原样接在后面。
pub(crate) fn quote(kind: ShellKind, text: &str) -> String {
    match kind {
        ShellKind::PowerShell => format!("'{}'", text.replace('\'', "''")),
        ShellKind::Cmd => quote_cmd(text),
        _ => super::quote_posix(text),
    }
}

fn scan_powershell(command: &str) -> Vec<BareWord> {
    let mut words = Vec::new();
    let mut chars = command.char_indices().peekable();
    let mut token = None;
    let mut skip_next = false;

    while let Some((index, ch)) = chars.next() {
        let end = index + ch.len_utf8();
        match ch {
            ' ' | '\t' | '\r' => finish(command, &mut token, &mut words, &mut skip_next),
            '\n' => {
                finish(command, &mut token, &mut words, &mut skip_next);
                skip_next = false;
            }
            ';' | '|' | '&' | ',' | '(' | ')' | '{' | '}' => {
                finish(command, &mut token, &mut words, &mut skip_next);
                skip_next = false;
            }
            '#' if token.is_none() => {
                skip_line(&mut chars);
                skip_next = false;
            }
            '>' | '<' => {
                finish(command, &mut token, &mut words, &mut skip_next);
                skip_next = true;
            }
            '=' => {
                finish(command, &mut token, &mut words, &mut skip_next);
                skip_next = true;
            }
            '\'' => {
                touch(&mut token, index, end, false, &mut skip_next);
                consume_single_quoted(&mut chars, &mut token, &mut skip_next);
            }
            '"' => {
                touch(&mut token, index, end, false, &mut skip_next);
                consume_double_quoted(&mut chars, &mut token, &mut skip_next);
            }
            '`' => {
                touch(&mut token, index, end, false, &mut skip_next);
                if let Some((next, next_ch)) = chars.next() {
                    touch(
                        &mut token,
                        next,
                        next + next_ch.len_utf8(),
                        false,
                        &mut skip_next,
                    );
                }
            }
            '$' => touch(&mut token, index, end, false, &mut skip_next),
            '@' if token.is_none() && powershell_here_string(command, index) => {
                skip_powershell_here_string(&mut chars, command, index);
                skip_next = false;
            }
            _ => touch(&mut token, index, end, true, &mut skip_next),
        }
    }
    finish(command, &mut token, &mut words, &mut skip_next);
    words
}

fn scan_cmd(command: &str) -> Vec<BareWord> {
    let mut words = Vec::new();
    let mut chars = command.char_indices().peekable();
    let mut token = None;
    let mut skip_next = false;
    let mut at_command_start = true;
    let mut at_line_start = true;

    while let Some((index, ch)) = chars.next() {
        let end = index + ch.len_utf8();
        match ch {
            ' ' | '\t' | '\r' => finish(command, &mut token, &mut words, &mut skip_next),
            '\n' => {
                finish(command, &mut token, &mut words, &mut skip_next);
                skip_next = false;
                at_command_start = true;
                at_line_start = true;
            }
            '&' | '|' | '(' => {
                finish(command, &mut token, &mut words, &mut skip_next);
                skip_next = false;
                at_command_start = true;
                at_line_start = false;
            }
            ')' => {
                finish(command, &mut token, &mut words, &mut skip_next);
                skip_next = false;
                at_command_start = false;
                at_line_start = false;
            }
            '>' | '<' => {
                finish(command, &mut token, &mut words, &mut skip_next);
                skip_next = true;
                at_line_start = false;
            }
            '=' => {
                finish(command, &mut token, &mut words, &mut skip_next);
                skip_next = true;
                at_line_start = false;
            }
            '"' => {
                touch(&mut token, index, end, false, &mut skip_next);
                consume_cmd_quoted(&mut chars, &mut token, &mut skip_next);
                at_command_start = false;
                at_line_start = false;
            }
            '^' => {
                touch(&mut token, index, end, false, &mut skip_next);
                if let Some((next, next_ch)) = chars.next() {
                    touch(
                        &mut token,
                        next,
                        next + next_ch.len_utf8(),
                        false,
                        &mut skip_next,
                    );
                }
                at_command_start = false;
                at_line_start = false;
            }
            '%' => {
                touch(&mut token, index, end, false, &mut skip_next);
                at_command_start = false;
                at_line_start = false;
            }
            ':' if token.is_none() && at_line_start && command[index..].starts_with("::") => {
                skip_line(&mut chars);
                skip_next = false;
                at_command_start = true;
                at_line_start = true;
            }
            _ if token.is_none() && at_command_start && cmd_comment(command, index) => {
                skip_line(&mut chars);
                skip_next = false;
                at_command_start = true;
                at_line_start = true;
            }
            _ => {
                touch(&mut token, index, end, true, &mut skip_next);
                at_command_start = false;
                at_line_start = false;
            }
        }
    }
    finish(command, &mut token, &mut words, &mut skip_next);
    words
}

fn consume_single_quoted(
    chars: &mut Peekable<CharIndices<'_>>,
    token: &mut Option<Token>,
    skip_next: &mut bool,
) {
    while let Some((index, ch)) = chars.next() {
        let end = index + ch.len_utf8();
        touch(token, index, end, false, skip_next);
        if ch != '\'' {
            continue;
        }
        if matches!(chars.peek(), Some((_, '\''))) {
            if let Some((next, next_ch)) = chars.next() {
                touch(token, next, next + next_ch.len_utf8(), false, skip_next);
            }
        } else {
            break;
        }
    }
}

fn consume_double_quoted(
    chars: &mut Peekable<CharIndices<'_>>,
    token: &mut Option<Token>,
    skip_next: &mut bool,
) {
    while let Some((index, ch)) = chars.next() {
        let end = index + ch.len_utf8();
        touch(token, index, end, false, skip_next);
        match ch {
            '`' => {
                if let Some((next, next_ch)) = chars.next() {
                    touch(token, next, next + next_ch.len_utf8(), false, skip_next);
                }
            }
            '"' => {
                if matches!(chars.peek(), Some((_, '"'))) {
                    if let Some((next, next_ch)) = chars.next() {
                        touch(token, next, next + next_ch.len_utf8(), false, skip_next);
                    }
                } else {
                    break;
                }
            }
            _ => {}
        }
    }
}

fn consume_cmd_quoted(
    chars: &mut Peekable<CharIndices<'_>>,
    token: &mut Option<Token>,
    skip_next: &mut bool,
) {
    while let Some((index, ch)) = chars.next() {
        let end = index + ch.len_utf8();
        touch(token, index, end, false, skip_next);
        if ch == '"' {
            if matches!(chars.peek(), Some((_, '"'))) {
                if let Some((next, next_ch)) = chars.next() {
                    touch(token, next, next + next_ch.len_utf8(), false, skip_next);
                }
            } else {
                break;
            }
        }
    }
}

/// PowerShell here-string 的开头：`@'` 或 `@"` 必须单独占一行。
fn powershell_here_string(command: &str, index: usize) -> bool {
    let rest = &command[index..];
    let Some(rest) = rest.strip_prefix("@'").or_else(|| rest.strip_prefix("@\"")) else {
        return false;
    };
    rest.starts_with('\n') || rest.starts_with("\r\n")
}

fn skip_powershell_here_string(chars: &mut Peekable<CharIndices<'_>>, command: &str, open: usize) {
    let quote = command[open + 1..].chars().next().unwrap_or('\'');
    let mut line_start = false;
    while let Some((index, ch)) = chars.next() {
        if ch == '\n' {
            line_start = true;
            continue;
        }
        if line_start {
            if ch == quote && matches!(chars.peek(), Some((_, '@'))) && {
                let after = index + ch.len_utf8() + 1;
                let rest = &command[after..];
                rest.is_empty() || rest.starts_with('\n') || rest.starts_with("\r\n")
            } {
                chars.next();
                break;
            }
            line_start = false;
        }
    }
}

fn skip_line(chars: &mut Peekable<CharIndices<'_>>) {
    while let Some(&(_, ch)) = chars.peek() {
        if ch == '\n' {
            break;
        }
        chars.next();
    }
}

/// cmd 的 `rem` 注释只在命令位置生效；前导 `@` 不影响识别。
fn cmd_comment(command: &str, index: usize) -> bool {
    let rest = &command[index..];
    let rest = rest.strip_prefix('@').unwrap_or(rest);
    let mut chars = rest.chars();
    let head: String = chars.by_ref().take(3).collect();
    if !head.eq_ignore_ascii_case("rem") {
        return false;
    }
    chars
        .next()
        .is_none_or(|ch| matches!(ch, ' ' | '\t' | '\r' | '\n'))
}

/// cmd 用双引号；路径末尾的反斜杠必须留在引号外，否则 C 运行时的参数解析会把
/// `\"` 当成转义引号，参数边界被破坏。
fn quote_cmd(text: &str) -> String {
    let trimmed = text.trim_end_matches('\\');
    let trailing = &text[trimmed.len()..];
    format!("\"{trimmed}\"{trailing}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::ShellKind::{Cmd, GitBash, PowerShell};

    fn uris(command: &str, kind: ShellKind) -> Vec<String> {
        scan(command, kind)
            .into_iter()
            .filter(|word| word.text.contains("://"))
            .map(|word| word.text)
            .collect()
    }

    fn assert_none(command: &str, kind: ShellKind) {
        assert!(
            uris(command, kind).is_empty(),
            "不应扫描出 URI：{command} -> {:?}",
            uris(command, kind)
        );
    }

    #[test]
    fn powershell_collects_only_bare_uri_words() {
        assert_eq!(
            uris(
                "Get-Content artifact://jobs/*.log; local://a.md",
                PowerShell
            ),
            vec!["artifact://jobs/*.log", "local://a.md"]
        );
        assert_eq!(
            uris("$root = skill://demo/SKILL.md", PowerShell),
            Vec::<String>::new()
        );
        assert_eq!(
            uris(
                "Get-Content artifact://jobs/a.log | Select-String ok",
                PowerShell
            ),
            vec!["artifact://jobs/a.log"]
        );
    }

    #[test]
    fn powershell_keeps_quoted_commented_redirected_and_dynamic_words_literal() {
        for command in [
            "Get-Content 'artifact://jobs/x.log'",
            "Get-Content \"artifact://jobs/x.log\"",
            "# artifact://jobs/x.log",
            "Get-Content > artifact://jobs/x.log",
            "Get-Content >artifact://jobs/x.log",
            "Set-Content -Path=artifact://jobs/x.log",
            "$path = artifact://jobs/x.log",
            "Get-Content $root/artifact://jobs/x.log",
            "Get-Content `artifact://jobs/x.log",
            "@'\nartifact://jobs/x.log\n'@",
            "@\"\nartifact://jobs/x.log\n\"@",
        ] {
            assert_none(command, PowerShell);
        }
    }

    #[test]
    fn cmd_collects_bare_uris_after_separators() {
        assert_eq!(
            uris("type artifact://jobs/x.log & echo local://a.md", Cmd),
            vec!["artifact://jobs/x.log", "local://a.md"]
        );
        assert_eq!(
            uris("dir artifact://jobs/*.log", Cmd),
            vec!["artifact://jobs/*.log"]
        );
        // 参数位置的 rem 不是注释。
        assert_eq!(
            uris("type rem artifact://jobs/x.log", Cmd),
            vec!["artifact://jobs/x.log"]
        );
    }

    #[test]
    fn cmd_keeps_quoted_commented_redirected_and_dynamic_words_literal() {
        for command in [
            "type \"artifact://jobs/x.log\"",
            "rem artifact://jobs/x.log",
            "@rem artifact://jobs/x.log",
            ":: artifact://jobs/x.log",
            "echo hi > artifact://jobs/x.log",
            "echo hi >artifact://jobs/x.log",
            "set X=artifact://jobs/x.log",
            "type %TEMP%\\artifact://jobs/x.log",
            "type ^artifact://jobs/x.log",
        ] {
            assert_none(command, Cmd);
        }
    }

    #[test]
    fn second_line_commands_are_scanned_again() {
        assert_eq!(
            uris("rem artifact://jobs/x.log\n\ntype local://a.md", Cmd),
            vec!["local://a.md"]
        );
        assert_eq!(
            uris(
                "# artifact://jobs/x.log\nGet-Content local://a.md",
                PowerShell
            ),
            vec!["local://a.md"]
        );
    }

    #[test]
    fn multibyte_prefix_keeps_byte_offsets() {
        let command = "echo 中文 artifact://jobs/x.log";
        for kind in [PowerShell, Cmd] {
            let word = scan(command, kind)
                .into_iter()
                .find(|word| word.text == "artifact://jobs/x.log")
                .expect("找到 URI 词");
            assert_eq!(&command[word.start..word.end], "artifact://jobs/x.log");
        }
    }

    #[test]
    fn quoting_matches_shell_rules_and_keeps_globs_callable() {
        assert_eq!(quote(PowerShell, "C:\\a b\\it's\\"), "'C:\\a b\\it''s\\'");
        assert_eq!(quote(Cmd, "C:\\a b\\jobs\\"), "\"C:\\a b\\jobs\"\\");
        assert_eq!(
            quote(Cmd, "C:\\a b\\jobs\\*.log"),
            "\"C:\\a b\\jobs\\*.log\""
        );
        assert_eq!(quote(GitBash, "C:/a b/it's"), "'C:/a b/it'\\''s'");
    }
}

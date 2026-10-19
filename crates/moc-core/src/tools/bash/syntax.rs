//! 用 brush-parser 的 AST 替代手写 shell 分词。
//!
//! 这里只回答两个问题：命令位置上的静态 argv 是什么，以及是否存在无法静态判定
//! 的结构（重定向、复合命令、命令替换、`&`、`!` 等）。policy 与 urlfs 共用同一
//! 棵树，避免各自维护一套易错的引号/转义规则。

use std::io::Cursor;

use brush_parser::ast::{self, CommandPrefixOrSuffixItem, SeparatorOperator};
use brush_parser::word::{self, WordPiece, WordPieceWithSource};
use brush_parser::{Parser, ParserOptions};

/// 一条命令段的静态视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedCommand {
    /// 命令名与参数；已去引号，但未做变量/命令替换展开。
    pub(crate) argv: Vec<String>,
    /// 含重定向、复合命令、函数、进程替换、动态词或 `time`：不能直接进沙盒。
    pub(crate) unsafe_syntax: bool,
    /// 以 `&` 结尾。
    pub(crate) asynchronous: bool,
}

/// 未加引号、无转义的词，以及它在命令里的字节范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BareWord {
    pub(crate) text: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// 一次解析的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) commands: Vec<ParsedCommand>,
    pub(crate) bare_words: Vec<BareWord>,
}

/// 解析命令；语法错误或解析器无法处理时返回 `None`，调用方按“不能证明安全”处理。
pub(crate) fn scan(command: &str) -> Option<Scan> {
    let options = ParserOptions::default();
    let mut parser = Parser::new(Cursor::new(command.as_bytes()), &options);
    let program = parser.parse_program().ok()?;
    let offsets = Offsets::new(command);
    let mut scan = Scan {
        commands: Vec::new(),
        bare_words: Vec::new(),
    };
    for complete in &program.complete_commands {
        for item in &complete.0 {
            let asynchronous = matches!(item.1, SeparatorOperator::Async);
            for (_, pipeline) in item.0.iter() {
                for command in &pipeline.seq {
                    let mut parsed = ParsedCommand {
                        argv: Vec::new(),
                        unsafe_syntax: pipeline.bang || pipeline.timed.is_some(),
                        asynchronous,
                    };
                    match command {
                        ast::Command::Simple(simple) => {
                            collect_simple(
                                simple,
                                &options,
                                &offsets,
                                &mut parsed,
                                &mut scan.bare_words,
                            );
                        }
                        // 函数定义、复合命令、扩展测试都不能直接进沙盒。
                        _ => parsed.unsafe_syntax = true,
                    }
                    scan.commands.push(parsed);
                }
            }
        }
    }
    Some(scan)
}

fn collect_simple(
    simple: &ast::SimpleCommand,
    options: &ParserOptions,
    offsets: &Offsets,
    parsed: &mut ParsedCommand,
    bare: &mut Vec<BareWord>,
) {
    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            match item {
                CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                    if static_value(word, options).is_none() {
                        parsed.unsafe_syntax = true;
                    }
                }
                // 普通词出现在 prefix 里不是常见形态；保守起见交给审批。
                CommandPrefixOrSuffixItem::Word(_) => parsed.unsafe_syntax = true,
                CommandPrefixOrSuffixItem::IoRedirect(_)
                | CommandPrefixOrSuffixItem::ProcessSubstitution(..) => {
                    parsed.unsafe_syntax = true;
                }
            }
        }
    }
    let Some(name) = &simple.word_or_name else {
        parsed.unsafe_syntax = true;
        return;
    };
    let Some(value) = static_value(name, options) else {
        parsed.unsafe_syntax = true;
        return;
    };
    parsed.argv.push(value);
    push_bare(name, options, offsets, bare);
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            match item {
                CommandPrefixOrSuffixItem::Word(word) => {
                    let Some(value) = static_value(word, options) else {
                        parsed.unsafe_syntax = true;
                        continue;
                    };
                    parsed.argv.push(value);
                    push_bare(word, options, offsets, bare);
                }
                CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                    if static_value(word, options).is_none() {
                        parsed.unsafe_syntax = true;
                    }
                }
                CommandPrefixOrSuffixItem::IoRedirect(_)
                | CommandPrefixOrSuffixItem::ProcessSubstitution(..) => {
                    parsed.unsafe_syntax = true;
                }
            }
        }
    }
}

/// 把词展开成静态字符串；遇到变量、命令替换、算术、ANSI-C 引号或波浪号时返回
/// `None`。单引号、转义和双引号里的纯文本可以安全拼接。
fn static_value(word: &ast::Word, options: &ParserOptions) -> Option<String> {
    let pieces = word::parse(&word.value, options).ok()?;
    let mut value = String::new();
    for piece in pieces {
        push_piece(piece.piece, &mut value)?;
    }
    Some(value)
}

fn push_piece(piece: WordPiece, out: &mut String) -> Option<()> {
    match piece {
        WordPiece::Text(text)
        | WordPiece::SingleQuotedText(text)
        | WordPiece::EscapeSequence(text) => out.push_str(&text),
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
            for piece in inner {
                push_piece(piece.piece, out)?;
            }
        }
        WordPiece::TildeExpansion(_)
        | WordPiece::ParameterExpansion(_)
        | WordPiece::CommandSubstitution(_)
        | WordPiece::BackquotedCommandSubstitution(_)
        | WordPiece::ArithmeticExpression(_)
        | WordPiece::AnsiCQuotedText(_) => return None,
    }
    Some(())
}

/// 只收集未加引号、无转义的词；heredoc 正文、注释、重定向目标和赋值值都不在内。
fn push_bare(
    word: &ast::Word,
    options: &ParserOptions,
    offsets: &Offsets,
    bare: &mut Vec<BareWord>,
) {
    let Some(location) = word.loc.as_ref() else {
        return;
    };
    let Ok(pieces) = word::parse(&word.value, options) else {
        return;
    };
    let [
        WordPieceWithSource {
            piece: WordPiece::Text(text),
            ..
        },
    ] = pieces.as_slice()
    else {
        return;
    };
    if text != &word.value {
        return;
    }
    let (Some(start), Some(end)) = (
        offsets.byte(location.start.index),
        offsets.byte(location.end.index),
    ) else {
        return;
    };
    bare.push(BareWord {
        text: text.clone(),
        start,
        end,
    });
}

/// 字符索引到字节偏移的映射。brush-parser 的 `SourcePosition.index` 是字符索引，
/// 而重写命令需要 UTF-8 字节偏移；命令里出现中文或 emoji 时两者不相等。
struct Offsets(Vec<usize>);

impl Offsets {
    fn new(source: &str) -> Self {
        let mut offsets = Vec::with_capacity(source.chars().count() + 1);
        offsets.extend(source.char_indices().map(|(offset, _)| offset));
        offsets.push(source.len());
        Self(offsets)
    }

    fn byte(&self, char_index: usize) -> Option<usize> {
        self.0.get(char_index).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(command: &str) -> Vec<String> {
        scan(command).expect("解析").commands[0].argv.clone()
    }

    #[test]
    fn quotes_are_stripped_and_dynamic_words_are_flagged() {
        let scan = scan("echo 'a b' \"c\" $HOME").expect("解析");
        assert_eq!(scan.commands[0].argv, vec!["echo", "a b", "c"]);
        assert!(scan.commands[0].unsafe_syntax);
    }

    #[test]
    fn simple_command_is_static() {
        let scan = scan("cargo test --workspace").expect("解析");
        assert_eq!(scan.commands.len(), 1);
        assert_eq!(scan.commands[0].argv, vec!["cargo", "test", "--workspace"]);
        assert!(!scan.commands[0].unsafe_syntax);
        assert!(!scan.commands[0].asynchronous);
    }

    #[test]
    fn pipeline_and_and_or_are_split_into_segments() {
        let scan = scan("git status | rg '^M' && echo ok").expect("解析");
        assert_eq!(scan.commands.len(), 3);
        assert_eq!(scan.commands[0].argv, vec!["git", "status"]);
        assert_eq!(scan.commands[1].argv, vec!["rg", "^M"]);
        assert_eq!(scan.commands[2].argv, vec!["echo", "ok"]);
    }

    #[test]
    fn redirection_and_background_are_unsafe() {
        assert!(scan("echo hi 2>&1").expect("解析").commands[0].unsafe_syntax);
        assert!(scan("echo hi &").expect("解析").commands[0].asynchronous);
        assert!(scan("echo hi > out").expect("解析").commands[0].unsafe_syntax);
    }

    #[test]
    fn compound_commands_are_unsafe() {
        assert!(scan("(echo hi)").expect("解析").commands[0].unsafe_syntax);
        assert!(scan("! ls").expect("解析").commands[0].unsafe_syntax);
        assert!(scan("for x in a; do echo $x; done").expect("解析").commands[0].unsafe_syntax);
    }

    #[test]
    fn assignments_are_skipped_but_dynamic_values_are_unsafe() {
        assert_eq!(argv("FOO=1 ls"), vec!["ls"]);
        assert!(scan("FOO=$BAR ls").expect("解析").commands[0].unsafe_syntax);
    }

    #[test]
    fn heredoc_body_is_not_a_word() {
        let scan = scan("cat <<EOF\nartifact://jobs/x.log\nEOF").expect("解析");
        assert!(
            !scan
                .bare_words
                .iter()
                .any(|word| word.text.contains("artifact://")),
            "heredoc 正文不能被当成词：{:?}",
            scan.bare_words
        );
    }

    #[test]
    fn quoted_uri_is_not_bare() {
        let scan = scan("echo 'artifact://jobs/x.log'").expect("解析");
        assert!(
            !scan
                .bare_words
                .iter()
                .any(|word| word.text.contains("artifact://")),
            "引号里的 URI 不是裸词：{:?}",
            scan.bare_words
        );
    }

    #[test]
    fn bare_word_offsets_are_bytes_even_after_multibyte_text() {
        let command = "echo 中文 artifact://jobs/x.log";
        let scan = scan(command).expect("解析");
        let word = scan
            .bare_words
            .iter()
            .find(|word| word.text == "artifact://jobs/x.log")
            .expect("找到 URI 词");
        assert_eq!(&command[word.start..word.end], "artifact://jobs/x.log");
        assert!(
            word.start > command.find("中文").expect("中文位置"),
            "字节偏移必须越过中文"
        );
    }
}

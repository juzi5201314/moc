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
    /// 命令名在源码中的字节范围，包含引号；危险规则不能把该位置当数据豁免。
    pub(crate) name_span: Option<(usize, usize)>,
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
                        name_span: None,
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
    parsed.name_span = name.loc.as_ref().and_then(|location| {
        Some((
            offsets.byte(location.start.index)?,
            offsets.byte(location.end.index)?,
        ))
    });
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

/// 命令的富视图：管道按源码顺序展开，`;`、`&&`、`||`、`&` 的区别被抹平。
/// builtin_policy 靠它判断一条命令是否只是在读/搜/列/改工作区文件。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Tree {
    pub(crate) pipelines: Vec<Pipe>,
}

/// 一条管道：`a | b | c`。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Pipe {
    pub(crate) commands: Vec<Cmd>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Cmd {
    Simple(Simple),
    /// subshell/brace/for/while/until/if/case：体内全部管道按源码顺序展开；
    /// 复合命令自身的重定向被忽略。
    Compound(Tree),
    /// 函数定义、`[[ ]]`、算术、coproc：内部对策略不透明。
    Opaque,
}

/// 一条简单命令的静态视图。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Simple {
    /// 命令名与参数；前缀赋值 `FOO=1` 忽略。
    pub(crate) argv: Vec<Arg>,
    pub(crate) redirects: Vec<Redirect>,
}

/// 一个词参数。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Arg {
    /// 静态值；含展开（变量、命令替换、波浪号等）时为 `None`。
    pub(crate) value: Option<String>,
    /// 未加引号的 Text 片段含 `*`、`?`、`[` 或 `{`。
    pub(crate) glob: bool,
}

/// 重定向的静态视图；写文件所需的形态被细化，其余归入 `Dup`。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Redirect {
    /// `>`、`>>`、`>|`、`&>`、`&>>`；fd 默认 1，`&>`/`&>>` 记为 1。
    Output {
        fd: i32,
        target: Option<String>,
        append: bool,
    },
    /// `<`。
    Input { target: Option<String> },
    /// `<<` / `<<-`；body 取 `doc.value` 原文。
    HereDoc { body: String },
    /// `<<<`。
    HereString { value: Option<String> },
    /// `>&`、`<&`、`2>&1`、fd/Duplicate 目标、进程替换目标、`<>`。
    Dup,
}

/// 解析成富视图；语法错误或解析器无法处理时返回 `None`，调用方按放行处理。
pub(crate) fn tree(command: &str) -> Option<Tree> {
    let options = ParserOptions::default();
    let mut parser = Parser::new(Cursor::new(command.as_bytes()), &options);
    let program = parser.parse_program().ok()?;
    let mut pipelines = Vec::new();
    for complete in &program.complete_commands {
        collect_list(complete, &options, &mut pipelines);
    }
    Some(Tree { pipelines })
}

fn collect_list(list: &ast::CompoundList, options: &ParserOptions, pipelines: &mut Vec<Pipe>) {
    for item in &list.0 {
        for (_, pipeline) in item.0.iter() {
            let commands = pipeline.seq.iter().map(|c| node(c, options)).collect();
            pipelines.push(Pipe { commands });
        }
    }
}

fn node(command: &ast::Command, options: &ParserOptions) -> Cmd {
    match command {
        ast::Command::Simple(simple) => Cmd::Simple(rich_simple(simple, options)),
        ast::Command::Compound(compound, _) => {
            compound_tree(compound, options).map_or(Cmd::Opaque, Cmd::Compound)
        }
        _ => Cmd::Opaque,
    }
}

/// 复合命令体内的全部管道按源码顺序展开；不透明形态返回 `None`。
fn compound_tree(compound: &ast::CompoundCommand, options: &ParserOptions) -> Option<Tree> {
    let mut pipelines = Vec::new();
    match compound {
        ast::CompoundCommand::Subshell(c) => collect_list(&c.list, options, &mut pipelines),
        ast::CompoundCommand::BraceGroup(c) => collect_list(&c.list, options, &mut pipelines),
        ast::CompoundCommand::ForClause(c) => collect_list(&c.body.list, options, &mut pipelines),
        ast::CompoundCommand::WhileClause(c) | ast::CompoundCommand::UntilClause(c) => {
            collect_list(&c.0, options, &mut pipelines);
            collect_list(&c.1.list, options, &mut pipelines);
        }
        ast::CompoundCommand::IfClause(c) => {
            collect_list(&c.condition, options, &mut pipelines);
            collect_list(&c.then, options, &mut pipelines);
            if let Some(elses) = &c.elses {
                for clause in elses {
                    if let Some(condition) = &clause.condition {
                        collect_list(condition, options, &mut pipelines);
                    }
                    collect_list(&clause.body, options, &mut pipelines);
                }
            }
        }
        ast::CompoundCommand::CaseClause(c) => {
            for case in &c.cases {
                if let Some(cmd) = &case.cmd {
                    collect_list(cmd, options, &mut pipelines);
                }
            }
        }
        ast::CompoundCommand::Arithmetic(_)
        | ast::CompoundCommand::ArithmeticForClause(_)
        | ast::CompoundCommand::Coprocess(_) => return None,
    }
    Some(Tree { pipelines })
}

fn rich_simple(simple: &ast::SimpleCommand, options: &ParserOptions) -> Simple {
    let mut parsed = Simple {
        argv: Vec::new(),
        redirects: Vec::new(),
    };
    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            if let CommandPrefixOrSuffixItem::IoRedirect(redirect) = item {
                parsed.redirects.push(rich_redirect(redirect, options));
            }
        }
    }
    if let Some(name) = &simple.word_or_name {
        parsed.argv.push(rich_arg(name, options));
    }
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            match item {
                CommandPrefixOrSuffixItem::Word(word) => {
                    parsed.argv.push(rich_arg(word, options));
                }
                CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    parsed.redirects.push(rich_redirect(redirect, options));
                }
                // 后缀赋值与进程替换都不是静态参数。
                _ => {}
            }
        }
    }
    parsed
}

fn rich_arg(word: &ast::Word, options: &ParserOptions) -> Arg {
    Arg {
        value: static_value(word, options),
        glob: unquoted_glob(word, options),
    }
}

/// 未加引号的 Text 片段是否含 `*`、`?`、`[` 或 `{`；引号内与转义片段都不算。
fn unquoted_glob(word: &ast::Word, options: &ParserOptions) -> bool {
    let Ok(pieces) = word::parse(&word.value, options) else {
        return false;
    };
    pieces.iter().any(|piece| match &piece.piece {
        WordPiece::Text(text) => text.contains(['*', '?', '[', '{']),
        _ => false,
    })
}

fn rich_redirect(redirect: &ast::IoRedirect, options: &ParserOptions) -> Redirect {
    use ast::{IoFileRedirectKind as Kind, IoFileRedirectTarget as Target};
    match redirect {
        ast::IoRedirect::File(fd, kind, target) => match (kind, target) {
            (Kind::Write | Kind::Append | Kind::Clobber, Target::Filename(word)) => {
                Redirect::Output {
                    fd: fd.unwrap_or(1),
                    target: static_value(word, options),
                    append: matches!(kind, Kind::Append),
                }
            }
            (Kind::Read, Target::Filename(word)) => Redirect::Input {
                target: static_value(word, options),
            },
            _ => Redirect::Dup,
        },
        ast::IoRedirect::HereDocument(_, doc) => Redirect::HereDoc {
            body: doc.doc.value.clone(),
        },
        ast::IoRedirect::HereString(_, word) => Redirect::HereString {
            value: static_value(word, options),
        },
        ast::IoRedirect::OutputAndError(word, append) => Redirect::Output {
            fd: 1,
            target: static_value(word, options),
            append: *append,
        },
    }
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
    fn command_name_spans_include_quotes_and_use_byte_offsets() {
        let source = "echo 中文; 'mkfs.ext4' /dev/sda1";
        let scan = scan(source).expect("解析");
        let (start, end) = scan.commands[1].name_span.expect("命令名范围");
        assert_eq!(&source[start..end], "'mkfs.ext4'");
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
    fn tree_flattens_lists_and_splits_pipelines() {
        let tree = tree("a | b && c; d & e").expect("解析");
        assert_eq!(tree.pipelines.len(), 4);
        assert_eq!(tree.pipelines[0].commands.len(), 2);
        let Cmd::Simple(first) = &tree.pipelines[0].commands[0] else {
            panic!("应是简单命令");
        };
        assert_eq!(first.argv[0].value.as_deref(), Some("a"));
    }

    #[test]
    fn tree_models_redirects() {
        let tree = tree("cmd 2>&1 > out >> log < in").expect("解析");
        let Cmd::Simple(cmd) = &tree.pipelines[0].commands[0] else {
            panic!("应是简单命令");
        };
        assert_eq!(cmd.redirects[0], Redirect::Dup);
        assert_eq!(
            cmd.redirects[1],
            Redirect::Output {
                fd: 1,
                target: Some("out".into()),
                append: false,
            }
        );
        assert_eq!(
            cmd.redirects[2],
            Redirect::Output {
                fd: 1,
                target: Some("log".into()),
                append: true,
            }
        );
        assert_eq!(
            cmd.redirects[3],
            Redirect::Input {
                target: Some("in".into())
            }
        );
    }

    #[test]
    fn tree_keeps_heredoc_body() {
        let tree = tree("cat <<EOF\nline one\nEOF").expect("解析");
        let Cmd::Simple(cmd) = &tree.pipelines[0].commands[0] else {
            panic!("应是简单命令");
        };
        let Redirect::HereDoc { body } = &cmd.redirects[0] else {
            panic!("应是 heredoc");
        };
        assert!(body.contains("line one"), "{body:?}");
    }

    #[test]
    fn tree_expands_compound_bodies() {
        let parsed = tree("for f in src/*.rs; do sed -i 's/a/b/' \"$f\"; done").expect("解析");
        let Cmd::Compound(inner) = &parsed.pipelines[0].commands[0] else {
            panic!("for 循环应是 Compound");
        };
        let Cmd::Simple(cmd) = &inner.pipelines[0].commands[0] else {
            panic!("循环体应是简单命令");
        };
        assert_eq!(cmd.argv[0].value.as_deref(), Some("sed"));
        assert!(matches!(
            tree("((x++))").expect("解析").pipelines[0].commands[0],
            Cmd::Opaque
        ));
        assert!(matches!(
            tree("f() { cat a; }").expect("解析").pipelines[0].commands[0],
            Cmd::Opaque
        ));
    }

    #[test]
    fn tree_marks_unquoted_globs() {
        let tree = tree("cat *.rs '*.rs' a?x").expect("解析");
        let Cmd::Simple(cmd) = &tree.pipelines[0].commands[0] else {
            panic!("应是简单命令");
        };
        assert!(cmd.argv[1].glob);
        assert!(!cmd.argv[2].glob);
        assert!(cmd.argv[3].glob);
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

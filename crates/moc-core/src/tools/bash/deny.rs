//! 危险命令规则的匹配层：正则命中只是“候选”，还要区分引号内字面量与可执行上下文。
//!
//! 判定规则：
//! - 匹配段完整落在字面量区间（'..'、".."、$'..'、$".."、${..}、# 行注释）内，
//!   且命令行里没有执行型动词：是数据，不拦——`grep "rm -rf /"` 放行。
//! - 匹配段有任何一部分在字面量外：是真代码，拦——`rm -rf "/var"` 的 `rm` 在
//!   引号外，引号里的只是路径参数。
//! - 命令行存在执行型动词（sh/bash/eval/ssh/python 等），或命令无法解析时：
//!   引号内容可能经 -c、eval、管道或 stdin 被执行，一律拦——`bash -c "rm -rf /"`。
//!
//! 本模块只回答“命中了哪条规则”；命中后是申请还是拒绝由调用方决定。

use std::path::Path;

use grep_matcher::Matcher;

use super::syntax;
use crate::tools::ToolError;

/// 字符串参数、管道输入或脚本体即代码的动词（basename，小写）。
const EVAL_VERBS: &[&str] = &[
    // POSIX shell
    "sh",
    "bash",
    "zsh",
    "fish",
    "ksh",
    "mksh",
    "dash",
    "ash",
    "csh",
    "tcsh",
    "jsh",
    "yash",
    // 求值与远程/交互执行
    "eval",
    "su",
    "ssh",
    "mosh",
    "at",
    "batch",
    "watch",
    "script",
    "tmux",
    "screen",
    "expect",
    "unbuffer",
    "osascript",
    // Windows 命令解释器
    "cmd",
    "powershell",
    "pwsh",
    // 解释型语言：-c/-e 参数即代码
    "perl",
    "ruby",
    "node",
    "deno",
    "bun",
    "php",
    "lua",
    "rscript",
    "julia",
    "tclsh",
    "wish",
    // 脚本内可达 system()/e 命令执行
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
];

/// argv 形式转发命令的包装动词：扫其后非选项参数寻找执行型动词。
const WRAPPER_VERBS: &[&str] = &[
    "sudo",
    "doas",
    "pkexec",
    "run0",
    "env",
    "command",
    "builtin",
    "exec",
    "nice",
    "ionice",
    "nohup",
    "timeout",
    "time",
    "stdbuf",
    "setsid",
    "taskset",
    "chrt",
    "numactl",
    "flock",
    "chroot",
    "unshare",
    "nsenter",
    "systemd-run",
    "strace",
    "ltrace",
    "perf",
    "gdb",
    "lldb",
    "rr",
    "xargs",
    "parallel",
    "find",
    "busybox",
    "sshpass",
    "wsl",
    "bwrap",
    "nsjail",
    "firejail",
];

/// 返回命中规则的序号；引号内字面量且无可执行上下文时不算命中。
pub(crate) fn denied_rule(patterns: &[String], command: &str) -> Result<Option<usize>, ToolError> {
    let eval = has_eval_invocation(command);
    let literals = literal_spans(command);
    for (index, pattern) in patterns.iter().enumerate() {
        let matcher = grep_regex::RegexMatcher::new(pattern).map_err(|error| {
            ToolError::new(
                "E_CONFIG",
                format!("dangerous-command rule is invalid: {error}"),
            )
        })?;
        let mut offset = 0usize;
        while let Some(matched) = matcher.find_at(command.as_bytes(), offset).unwrap_or(None) {
            // 匹配可以覆盖字面量自身的定界字符（正则边界类会把包围文本的引号
            // 吃进匹配，如 `"rm -rf /"` 的匹配含结尾引号），定界符不算代码内容。
            let quoted = literals.iter().any(|(start, end)| {
                matched.start() >= start.saturating_sub(1) && matched.end() <= end + 1
            });
            if !quoted || eval {
                return Ok(Some(index));
            }
            offset = matched.end().max(matched.start() + 1);
        }
    }
    Ok(None)
}

/// 命令行是否存在执行型动词：命令名是解释器/求值器，或经 sudo/env 等包装后
/// 转发到执行型动词。复合命令与动态命令名看不见动词，按可执行处理。
fn has_eval_invocation(command: &str) -> bool {
    let Some(scan) = syntax::scan(command) else {
        return true;
    };
    scan.commands
        .iter()
        .any(|command| match command.argv.first() {
            None => true,
            Some(head) if eval_verb(head) => true,
            Some(head) => {
                wrapper_verb(head)
                    && command.argv[1..]
                        .iter()
                        .any(|arg| !arg.starts_with('-') && eval_verb(arg))
            }
        })
}

fn eval_verb(word: &str) -> bool {
    let name = verb_name(word);
    name.starts_with("python") || name.starts_with("pypy") || EVAL_VERBS.contains(&name.as_str())
}

fn wrapper_verb(word: &str) -> bool {
    WRAPPER_VERBS.contains(&verb_name(word).as_str())
}

/// 取命令 basename 并小写化；剥掉 Windows 可执行扩展名。
fn verb_name(word: &str) -> String {
    let name = Path::new(word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(word)
        .to_lowercase();
    for suffix in [".exe", ".bat", ".cmd", ".com"] {
        if let Some(name) = name.strip_suffix(suffix) {
            return name.to_owned();
        }
    }
    name
}

/// 收集字面量区间（字节偏移，左闭右开）：'..'、".."、$'..'、$".."、${..} 的内容
/// 与 # 行注释都算数据；$( ) 与反引号命令替换内部仍是代码，递归按代码上下文处理。
fn literal_spans(command: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    scan_code(command.as_bytes(), 0, command.len(), None, &mut spans);
    spans
}

/// 代码上下文扫描。`term` 为配平结束符（`b')'` 或 `b'}'`）时，在嵌套深度归零处
/// 返回该结束符的位置；否则扫到 end。返回值为调用方应继续的位置。
fn scan_code(
    bytes: &[u8],
    mut index: usize,
    end: usize,
    term: Option<u8>,
    spans: &mut Vec<(usize, usize)>,
) -> usize {
    let mut depth = 0usize;
    while index < end {
        match bytes[index] {
            b'\\' => index += 2,
            b'\'' => {
                // 单引号内部没有转义，到下一个 ' 全是字面量。
                let close = bytes[index + 1..end]
                    .iter()
                    .position(|&byte| byte == b'\'')
                    .map(|offset| index + 1 + offset)
                    .unwrap_or(end);
                spans.push((index + 1, close));
                index = close + 1;
            }
            b'"' => index = scan_quoted(bytes, index + 1, end, b'"', spans),
            b'`' => {
                let close = bytes[index + 1..end]
                    .iter()
                    .position(|&byte| byte == b'`')
                    .map(|offset| index + 1 + offset)
                    .unwrap_or(end);
                scan_code(bytes, index + 1, close, None, spans);
                index = close + 1;
            }
            b'#' if comment_boundary(bytes, index) => {
                let close = bytes[index..end]
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .map(|offset| index + offset)
                    .unwrap_or(end);
                spans.push((index, close));
                index = close + 1;
            }
            b'$' => index = scan_dollar(bytes, index, end, spans),
            b'{' if term == Some(b'}') => {
                depth += 1;
                index += 1;
            }
            b'}' if term == Some(b'}') => {
                if depth == 0 {
                    return index;
                }
                depth -= 1;
                index += 1;
            }
            b'(' if term == Some(b')') => {
                depth += 1;
                index += 1;
            }
            b')' if term == Some(b')') => {
                if depth == 0 {
                    return index;
                }
                depth -= 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    index.min(end)
}

/// 引用/展开上下文扫描（" 与 ${ 共用）：内容整体是字面量，但 $( )、` `、${ }
/// 内部仍是代码，递归处理。返回结束符之后的位置。
fn scan_quoted(
    bytes: &[u8],
    mut index: usize,
    end: usize,
    term: u8,
    spans: &mut Vec<(usize, usize)>,
) -> usize {
    let mut lit = index;
    let mut depth = 0usize;
    while index < end {
        match bytes[index] {
            b'\\' => index += 2,
            b'`' => {
                push_span(spans, lit, index);
                let close = bytes[index + 1..end]
                    .iter()
                    .position(|&byte| byte == b'`')
                    .map(|offset| index + 1 + offset)
                    .unwrap_or(end);
                scan_code(bytes, index + 1, close, None, spans);
                index = close + 1;
                lit = index;
            }
            b'$' if bytes.get(index + 1) == Some(&b'(') => {
                push_span(spans, lit, index);
                let close = scan_code(bytes, index + 2, end, Some(b')'), spans);
                index = close + 1;
                lit = index;
            }
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                push_span(spans, lit, index);
                index = scan_quoted(bytes, index + 2, end, b'}', spans);
                lit = index;
            }
            b'{' if term == b'}' => {
                depth += 1;
                index += 1;
            }
            b'}' if term == b'}' && depth > 0 => {
                depth -= 1;
                index += 1;
            }
            byte if byte == term => {
                push_span(spans, lit, index);
                return index + 1;
            }
            _ => index += 1,
        }
    }
    push_span(spans, lit, end);
    end
}

/// $ 展开调度：$( ) 与反引号是代码，${ } 与 $'..'、$".." 是数据；其余按普通字符。
fn scan_dollar(bytes: &[u8], index: usize, end: usize, spans: &mut Vec<(usize, usize)>) -> usize {
    match bytes.get(index + 1) {
        Some(b'(') => scan_code(bytes, index + 2, end, Some(b')'), spans) + 1,
        Some(b'{') => scan_quoted(bytes, index + 2, end, b'}', spans),
        Some(b'\'') => {
            // ANSI-C $'..'：反斜线转义有效，内容仍是字面量。
            let mut j = index + 2;
            while j < end {
                match bytes[j] {
                    b'\\' => j += 2,
                    b'\'' => break,
                    _ => j += 1,
                }
            }
            spans.push((index + 2, j));
            j + 1
        }
        Some(b'"') => scan_quoted(bytes, index + 2, end, b'"', spans),
        _ => index + 1,
    }
}

/// `#` 只有在词边界（命令开头、空白或分隔符之后）才是注释。
fn comment_boundary(bytes: &[u8], index: usize) -> bool {
    index == 0
        || matches!(
            bytes[index - 1],
            b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'(' | b'`' | b'{' | b'}'
        )
}

fn push_span(spans: &mut Vec<(usize, usize)>, start: usize, end: usize) {
    if start < end {
        spans.push((start, end));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(patterns: &[String], command: &str) -> bool {
        denied_rule(patterns, command).unwrap().is_some()
    }

    fn defaults() -> Vec<String> {
        crate::config::BashConfig::default().deny_patterns
    }

    #[test]
    fn invalid_pattern_reports_config_error() {
        assert!(denied_rule(&["(".into()], "ls").is_err());
    }

    #[test]
    fn dangerous_patterns_preempt_all_execution_modes() {
        let patterns = vec![r"rm\s+-rf\s+/(\s|$)".into()];
        assert!(denied(&patterns, "rm -rf /"));
        assert!(!denied(&patterns, "printf safe"));
    }

    #[test]
    fn quoted_literal_is_data_not_code() {
        let patterns = defaults();
        for command in [
            "grep \"rm -rf /\"",
            "grep 'rm -rf /' file.txt",
            "rg -n 'rm -rf /' src/",
            "echo \"rm -rf /\"",
            "printf 'rm -rf /\\n'",
            "git commit -m \"remove rm -rf / usage\"",
            "sed 's/a/rm/' file",
            "man rm",
            "cat script.sh",
            "sudo grep 'rm -rf /' /var/log/syslog",
            "grep 'rm -rf /' access.log > out.txt",
            "grep 'rm -rf /' access.log | wc -l",
            "echo x # rm -rf /",
            "echo '$(rm -rf /)'",
        ] {
            assert!(!denied(&patterns, command), "不应拦截: {command}");
        }
    }

    #[test]
    fn eval_invocation_turns_quotes_into_code() {
        let patterns = defaults();
        for command in [
            "bash -c \"rm -rf /\"",
            "sh -c 'rm -rf /'",
            "zsh -c 'rm -rf /'",
            "eval \"rm -rf /\"",
            "exec rm -rf /",
            "ssh host 'rm -rf /'",
            "su -c 'rm -rf /'",
            "watch 'rm -rf /'",
            "tmux send-keys 'rm -rf /' Enter",
            "at now <<EOF\nrm -rf /\nEOF",
            "python -c 'import os; os.system(\"rm -rf /\")'",
            "perl -e 'system(\"rm -rf /\")'",
            "node -e 'require(\"child_process\").exec(\"rm -rf /\")'",
            "awk 'BEGIN{system(\"rm -rf /\")}'",
            "cmd /c \"del c:\\data\"",
            "powershell -Command \"Remove-Item C:\\x -Recurse\"",
            "pwsh -c 'rm -rf /'",
            "wsl bash -c 'rm -rf /'",
            "sudo bash -c 'rm -rf /'",
            "sudo -u root bash -c 'rm -rf /'",
            "env FOO=1 bash -c 'rm -rf /'",
            "nohup bash -c 'rm -rf /'",
            "xargs -I{} bash -c 'rm -rf /'",
            "printf 'rm -rf /' | sh",
            "echo 'rm -rf /' | bash",
            "printf 'rm -rf /' | sudo sh",
            "busybox sh -c 'rm -rf /'",
        ] {
            assert!(denied(&patterns, command), "应拦截: {command}");
        }
    }

    #[test]
    fn quoted_targets_of_real_verbs_still_deny() {
        let patterns = defaults();
        // 动词在引号外，引号只是参数写法：仍按危险命令处理。
        for command in [
            "rm -rf \"/var\"",
            "rm -rf '/opt'",
            "chmod -R 777 \"/\"",
            "find \"/\" -delete",
            "\"rm\" -rf /",
            "rm \"-rf\" /",
        ] {
            assert!(denied(&patterns, command), "应拦截: {command}");
        }
    }

    #[test]
    fn substitution_and_unparseable_commands_stay_guarded() {
        let patterns = defaults();
        for command in [
            "echo \"$(rm -rf /)\"",
            "$(rm -rf /)",
            "`rm -rf /`",
            "FOO=$(rm -rf /)",
            "bash <<EOF\nrm -rf /\nEOF",
            "echo x; rm -rf /",
            "( rm -rf / )",
            "if true; then rm -rf /; fi",
            // heredoc 正文未建模为字面量：写入/打印危险脚本按保守处理。
            "cat <<EOF\nrm -rf /\nEOF",
        ] {
            assert!(denied(&patterns, command), "应拦截: {command}");
        }
    }

    #[test]
    fn default_deny_patterns_are_valid_and_block_forkbombs() {
        let patterns = defaults();
        assert!(denied(&patterns, ":(){ :|:& };:"));
        assert!(denied(&patterns, "SHUTDOWN /S"));
        assert!(denied(&patterns, "FORMAT C:"));
        assert!(denied(&patterns, "Remove-Item C:\\data -Recurse"));
        assert!(!denied(&patterns, "printf safe"));
    }

    #[test]
    fn default_deny_patterns_block_whole_environment_targets() {
        let patterns = defaults();
        for command in [
            "rm -rf /",
            "rm -rf /*",
            "rm -fr ~",
            "rm -rf $HOME",
            "rm -rf ${HOME}",
            "rm -rf ~root",
            "rm -r -f /etc",
            "rm --recursive --force /usr",
            "rm -v -rf /var",
            "rm / -rf",
            "sudo rm -rf /home",
            "rm -rf /home/user",
            "rm -rf /mnt/data",
            "rm -rf /dev",
            "rm -rf /etc/nginx",
            "rm -rf /tmp",
            "find / -delete",
            "find ~ -name '*.tmp' -delete",
            "find /etc -type f -exec rm {} \\;",
            "find /tmp -delete",
            "find / | xargs rm",
            "find /home/user -name x -delete",
            "chmod -R 777 /",
            "chmod -R u+w ~",
            "chown -R user /etc",
            "chmod --recursive 000 /usr",
            "chgrp -R staff /mnt/disk",
            "mkfs.ext4 /dev/sda1",
            "mke2fs /dev/sda1",
            "dd if=x of=/dev/sda",
            "dd of=/dev/nvme0n1",
            "echo x > /dev/mmcblk0",
            "cat img > /dev/sda",
            "tee /dev/sda",
            "cp img.raw /dev/sda",
            "mv img.raw /dev/sda",
            "tar -cf /dev/sda files",
            "shred /dev/sda",
            "wipefs -a /dev/sda",
            "fdisk /dev/sda",
            "sfdisk /dev/sda",
            "parted -s /dev/sda mklabel gpt",
            "sgdisk -Z /dev/sda",
            "> /etc/shadow",
            "echo x >> /etc/sudoers",
            "tee /etc/passwd",
            "shutdown -h now",
            "sudo poweroff",
            "doas reboot",
            "systemctl poweroff",
            "systemctl --user reboot",
            "crontab -r",
            "kill -9 -1",
            "kill -s KILL -1",
            "pkill -f .",
            "lvremove -f vg/lv",
            "vgremove vg",
            "userdel -r root",
            "zpool destroy tank",
            "zfs destroy -r tank/data",
            "cryptsetup luksFormat /dev/sda1",
            "iptables -F",
            "ip6tables -F",
            "nft flush ruleset",
            "swapoff -a",
        ] {
            assert!(denied(&patterns, command), "应拦截: {command}");
        }
    }

    #[test]
    fn default_deny_patterns_allow_scoped_operations() {
        let patterns = defaults();
        for command in [
            "rm -rf ./dist",
            "rm -rf node_modules",
            "rm -rf dir/",
            "rm -rf a/b",
            "rm -rf /tmp/scratch",
            "rm -rf ~/project/build",
            "rm -rf $HOME/cache",
            "rm -rf /etc/nginx/conf.d/site.conf",
            "rm -rf /home/user/project",
            "rm -rf /var/lib/mysql/data",
            "rm -v file.txt",
            "rm /etc/nginx/x.conf",
            "rm -rf ../sibling",
            "find . -name '*.o' -delete",
            "find /tmp/scratch -delete",
            "find /usr/lib/foo -name x -delete",
            "find . | xargs rm",
            "chmod -R u+w .",
            "chmod -R 755 ./src",
            "chown -R u:g /tmp/x",
            "chmod 777 /etc/passwd",
            "dd if=/dev/sda of=backup.img",
            "dd if=x of=/dev/null",
            "echo hi > /dev/null",
            "fdisk -l",
            "fdisk -l /dev/sda",
            "parted /dev/sda print",
            "sgdisk -p /dev/sda",
            "shred ./secret.txt",
            "wipefs /dev/sda",
            "cp /dev/sda backup.img",
            "tar -xf /dev/sda",
            "tar -cf out.tar /dev/sda",
            "kill -1 1234",
            "kill -9 1234",
            "pkill -f node",
            "systemctl restart nginx",
            "systemctl status sshd",
            "iptables -L",
            "swapoff /dev/sda3",
            "crontab -l",
            "crontab -e",
            "clang-format -i main.c",
            "grep -r format src/",
            "cargo fmt",
            "ls /home",
            "ls ~",
            "apt install curl",
            "printf safe",
        ] {
            assert!(!denied(&patterns, command), "不应拦截: {command}");
        }
    }
}

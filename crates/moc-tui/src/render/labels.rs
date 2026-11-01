//! 界面固定文案的唯一出处：符号、状态词、耗时、键位。
//!
//! 约定：
//! - 行内截断用 `…`（[`super::wrap::ELLIPSIS`]），整行/整块省略提示用 [`OMITTED`]。
//! - 状态词与块标题一律英文：块标题首字母大写（`Thought`、`Tasks`），状态词小写
//!   （`running`、`done`）。说明性文案（提示、错误原文）不在此列。
//! - 耗时统一 [`duration`]：`161ms` / `54.1s` / `1m02s` / `1h05m`，不加前缀词。
//! - 键位统一 [`key`]：修饰键首字母大写、`+` 相连（`Ctrl+P`、`Shift+Enter`）。
//!
//! 渲染层不得再手写这些字面量；改文案只改这里。

/// 整行或整块被略去时的引导符（`⋯ 还有 3 行`）。
pub const OMITTED: &str = "⋯";
/// 同一行内并列信息的分隔符。
pub const SEP: &str = " · ";
/// token 速率单位。
pub const RATE_UNIT: &str = "tok/s";

/// 「⋯ 还有 N 行」：行级省略提示。
pub fn more_lines(count: usize) -> String {
    format!("{OMITTED} 还有 {count} 行")
}

/// 「⋯ 还有 N 条」：列表项省略提示。
pub fn more_items(count: usize) -> String {
    format!("{OMITTED} 还有 {count} 条")
}

/// 块标题。
pub mod title {
    pub const THOUGHT: &str = "Thought";
    pub const TASKS: &str = "Tasks";
    pub const IRC: &str = "IRC";
    pub const SIDEKICK: &str = "Sidekick";
}

/// 状态词（小写）。
pub mod status {
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const DONE: &str = "done";
    pub const STOPPED: &str = "stopped";
    pub const FAILED: &str = "failed";
    pub const IDLE: &str = "idle";
    pub const CANCELLED: &str = "cancelled";
    pub const DENIED: &str = "denied";
    pub const NOT_RUN: &str = "not run";
    pub const INJECTED: &str = "injected";
    pub const WOKEN: &str = "woken";
    pub const DELIVERED: &str = "delivered";
    pub const BROADCAST: &str = "broadcast";
    pub const CONNECTING: &str = "connecting";
    pub const WAITING: &str = "waiting";
    pub const GENERATING: &str = "generating";
    pub const DRAFT: &str = "draft";
    pub const IN_REVIEW: &str = "in review";
    pub const REVISING: &str = "revising";
    pub const SUPERSEDED: &str = "superseded";
    pub const APPROVED: &str = "approved";
    pub const REJECTED: &str = "rejected";
}

/// 耗时：`161ms`、`54.1s`、`1m02s`、`1h05m`。
pub fn duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}.{}s", ms / 1_000, (ms % 1_000) / 100)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1_000)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    }
}

/// 耗时尾注：`⏱ 1.2s`（工具卡尾注、批次成员行尾、助手回复尾注共用）。
pub fn elapsed(ms: u64) -> String {
    format!("⏱ {}", duration(ms))
}

/// 单个绑定的展示名：`ctrl+p` → `Ctrl+P`，`shift+tab` → `Shift+Tab`，`j` → `j`。
///
/// 单字母只在带修饰键时大写（裸 `j`/`k` 大写会被读成 Shift）。
pub fn key(binding: &str) -> String {
    let lower = binding.trim().to_lowercase();
    let (modifiers, code) = lower.rsplit_once('+').unwrap_or(("", &lower));
    let mut out = String::new();
    for modifier in modifiers.split('+').filter(|part| !part.is_empty()) {
        out.push_str(modifier_name(modifier));
        out.push('+');
    }
    out.push_str(&code_name(code, !modifiers.is_empty()));
    out
}

/// 一组绑定的紧凑展示：同一键码的修饰键合并（`Shift/Alt+Enter`），不同键码以 ` / ` 分隔。
pub fn keys(bindings: &[String]) -> String {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for binding in bindings {
        let shown = key(binding);
        let (modifiers, code) = match shown.rsplit_once('+') {
            Some((modifiers, code)) if !code.is_empty() => (modifiers.to_owned(), code.to_owned()),
            _ => (String::new(), shown),
        };
        match groups.iter_mut().find(|(group, _)| *group == code) {
            Some((_, alternatives)) => alternatives.push(modifiers),
            None => groups.push((code, vec![modifiers])),
        }
    }
    groups
        .into_iter()
        .map(|(code, modifiers)| {
            let modifiers: Vec<_> = modifiers.into_iter().filter(|m| !m.is_empty()).collect();
            if modifiers.is_empty() {
                code
            } else {
                format!("{}+{code}", modifiers.join("/"))
            }
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 操作提示条：`[(键, 动作)]` → `Enter 确认 · Esc 关闭`。
///
/// 键写配置口径（`enter`、`ctrl+s`、`pgup/pgdn`、`↑↓`），逐段经 [`key`] 规范；
/// 键为空时只显示动作（`输入搜索`）。放不下 `width` 时**从左往右**丢项，最后一项
/// 恒保留（再放不下才截断）：调用方把最可省的项写在前面、关闭/确认写在末尾。
pub fn hints(items: &[(&str, &str)], width: usize) -> String {
    let shown: Vec<String> = items
        .iter()
        .map(|(binding, action)| {
            if binding.is_empty() {
                (*action).to_owned()
            } else {
                let binding = binding.split('/').map(key).collect::<Vec<_>>().join("/");
                format!("{binding} {action}")
            }
        })
        .collect();
    let mut start = 0;
    loop {
        let text = shown[start..].join(SEP);
        if start + 1 >= shown.len() || super::wrap::str_width(&text) <= width {
            return super::wrap::truncate(text, width).into_owned();
        }
        start += 1;
    }
}

fn modifier_name(modifier: &str) -> &'static str {
    match modifier {
        "ctrl" | "control" => "Ctrl",
        "alt" => "Alt",
        "shift" => "Shift",
        "super" => "Super",
        _ => "?",
    }
}

fn code_name(code: &str, modified: bool) -> String {
    let named = match code {
        "enter" => "Enter",
        "tab" => "Tab",
        "esc" | "escape" => "Esc",
        "space" | " " => "Space",
        "backspace" => "Backspace",
        "delete" | "del" => "Del",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "PgUp",
        "pagedown" | "pgdn" => "PgDn",
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_owned();
    }
    if code.len() > 1 && code.starts_with('f') && code[1..].parse::<u8>().is_ok() {
        return code.to_uppercase();
    }
    if modified {
        code.to_uppercase()
    } else {
        code.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{duration, hints, key, keys};

    #[test]
    fn duration_has_one_format() {
        assert_eq!(duration(161), "161ms");
        assert_eq!(duration(54_100), "54.1s");
        assert_eq!(duration(62_000), "1m02s");
        assert_eq!(duration(3_900_000), "1h05m");
    }

    #[test]
    fn keys_use_title_case_modifiers() {
        assert_eq!(key("ctrl+p"), "Ctrl+P");
        assert_eq!(key("Shift+Tab"), "Shift+Tab");
        assert_eq!(key("alt+up"), "Alt+↑");
        assert_eq!(key("ctrl+shift+p"), "Ctrl+Shift+P");
        assert_eq!(key("f2"), "F2");
        assert_eq!(key("j"), "j", "裸字母保持小写");
        assert_eq!(key("esc"), "Esc");
        let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            keys(&owned(&["shift+enter", "alt+enter"])),
            "Shift/Alt+Enter"
        );
        assert_eq!(keys(&owned(&["home", "ctrl+a"])), "Home / Ctrl+A");
        assert_eq!(keys(&owned(&[])), "");
    }

    #[test]
    fn hints_drop_leading_items_and_keep_the_last() {
        let items = [
            ("↑↓", "移动"),
            ("", "输入搜索"),
            ("enter", "确认"),
            ("esc", "关闭"),
        ];
        assert_eq!(
            hints(&items, 80),
            "↑↓ 移动 · 输入搜索 · Enter 确认 · Esc 关闭"
        );
        assert_eq!(hints(&items, 30), "Enter 确认 · Esc 关闭", "先丢最左项");
        assert_eq!(hints(&items, 7), "Esc 关…", "只剩末项仍放不下才截断");
        assert_eq!(
            hints(&[("pgup/pgdn", "翻页"), ("shift+x", "跳过")], 80),
            "PgUp/PgDn 翻页 · Shift+X 跳过"
        );
    }
}

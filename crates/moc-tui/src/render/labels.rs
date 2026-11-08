//! 界面固定文案的唯一出处：符号、状态词、耗时、键位。
//!
//! 约定：
//! - 行内截断用 `…`（[`super::wrap::ELLIPSIS`]），整行/整块省略提示用 [`OMITTED`]。
//! - 状态词与块标题一律英文：块标题首字母大写（`Thought`、`Sidekick`），状态词小写
//!   （`running`、`done`）。说明性文案（提示、错误原文）不在此列。
//! - 耗时统一 [`duration`]：`161ms` / `54.1s` / `1m02s` / `1h05m`，不加前缀词。
//! - 键位统一 [`key`]：修饰键首字母大写、`+` 相连（`Ctrl+P`、`Shift+Enter`）。
//!
//! 渲染层不得再手写这些字面量；改文案只改这里。

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;

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
    pub const ACTIVE: &str = "active";
    pub const PAUSED: &str = "paused";
    pub const BLOCKED: &str = "blocked";
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

/// 耗时尾注：`◷ 1.2s`（批次成员行尾、助手回复尾注、statusline 共用）。
///
/// 图标必须是单宽非 emoji 字符：`⏱` 在 Windows Terminal 下渲染成双宽彩色
/// emoji，会与后面的数字重叠。
pub fn elapsed(ms: u64) -> String {
    format!("◷ {}", duration(ms))
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
    hint_layout(items, width).0
}

/// 提示条上一段可点击的「键 动作」：列区间相对提示条起点，点中等价于按下该键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HintKey {
    pub start: u16,
    pub width: u16,
    pub modifiers: KeyModifiers,
    pub code: KeyCode,
}

/// 一块屏幕区域等价于一次按键（提示条上的键位、图例按钮）；渲染每帧重建。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHit {
    pub area: Rect,
    pub modifiers: KeyModifiers,
    pub code: KeyCode,
}

impl HintKey {
    /// 提示条画在 `(x, y)` 起的一行上时，这一段的屏幕命中区。
    pub fn at(self, x: u16, y: u16) -> KeyHit {
        KeyHit {
            area: Rect::new(x.saturating_add(self.start), y, self.width, 1),
            modifiers: self.modifiers,
            code: self.code,
        }
    }
}

/// [`hints`] 的排版，同时给出每个可点击项（整段「键 动作」）的列区间。
///
/// 只有单一按键的项可点：`↑↓`、`1-9`、`pgup/pgdn`、`e/c` 这类多键或多义的项点了
/// 不知道该按哪个，保持纯文字；截断后不完整的项也不登记。
pub fn hint_layout(items: &[(&str, &str)], width: usize) -> (String, Vec<HintKey>) {
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
    while start + 1 < shown.len() && super::wrap::str_width(&shown[start..].join(SEP)) > width {
        start += 1;
    }
    let text = super::wrap::truncate(shown[start..].join(SEP), width).into_owned();
    let limit = super::wrap::str_width(&text);
    let mut keys = Vec::new();
    let mut column = 0usize;
    for (item, (binding, _)) in shown[start..].iter().zip(&items[start..]) {
        let item_width = super::wrap::str_width(item);
        if column + item_width <= limit
            && let Some((modifiers, code)) = hint_key(binding)
            && let (Ok(start), Ok(width)) = (u16::try_from(column), u16::try_from(item_width))
        {
            keys.push(HintKey {
                start,
                width,
                modifiers,
                code,
            });
        }
        column += item_width + super::wrap::str_width(SEP);
    }
    (text, keys)
}

/// 提示条键位 → 终端真实上报的按键：`shift+tab` 是 `BackTab`，`shift+x` 是大写 `X`，
/// 与键盘处理分支的匹配口径一致。
fn hint_key(binding: &str) -> Option<(KeyModifiers, KeyCode)> {
    if binding.is_empty() || (binding != "/" && binding.contains('/')) {
        return None;
    }
    let binding = match binding {
        "del" => "delete",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        other => other,
    };
    let (modifiers, code) = crate::config::parse_key(binding).ok()?;
    Some(match code {
        KeyCode::Tab if modifiers == KeyModifiers::SHIFT => (modifiers, KeyCode::BackTab),
        KeyCode::Char(character) if modifiers.contains(KeyModifiers::SHIFT) => {
            (modifiers, KeyCode::Char(character.to_ascii_uppercase()))
        }
        _ => (modifiers, code),
    })
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
    use super::{HintKey, duration, hint_layout, hints, key, keys};
    use crossterm::event::{KeyCode, KeyModifiers};

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

    /// 点击区间必须与画出来的文字逐列对齐，且只登记单键项；键位按终端真实上报口径。
    #[test]
    fn hint_layout_maps_single_key_items_to_columns() {
        let items = [
            ("↑↓", "移动"),
            ("shift+x", "跳过"),
            ("e/c", "展收"),
            ("shift+tab", "上一问"),
            ("esc", "关闭"),
        ];
        let (text, keys) = hint_layout(&items, 80);
        assert_eq!(text, hints(&items, 80));
        let key = |start, width, modifiers, code| HintKey {
            start,
            width,
            modifiers,
            code,
        };
        // 「↑↓ 移动 · 」占 10 列；「Shift+X 跳过 · e/c 展收 · 」之后才是 Shift+Tab。
        assert_eq!(
            keys,
            vec![
                key(10, 12, KeyModifiers::SHIFT, KeyCode::Char('X')),
                key(36, 16, KeyModifiers::SHIFT, KeyCode::BackTab),
                key(55, 8, KeyModifiers::NONE, KeyCode::Esc),
            ]
        );
        // 截断到只剩末项的半截：不完整的项不可点。
        let (text, keys) = hint_layout(&items, 5);
        assert_eq!(text, "Esc…");
        assert!(keys.is_empty());
        // 丢掉左侧项后区间从新的起点重算。
        let (_, keys) = hint_layout(&items, 10);
        assert_eq!(keys, vec![key(0, 8, KeyModifiers::NONE, KeyCode::Esc)]);
    }
}

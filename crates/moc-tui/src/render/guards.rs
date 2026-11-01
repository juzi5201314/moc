//! 防复发守卫：扫描 `src/` 的产品代码，禁止绕开共享原语的写法重新出现。
//!
//! 规则与原语的对应：
//! - 浮层不透明底只走 [`super::overlay::paint_opaque`]，不得直接用 `Clear`（它不处理左邻宽字符）。
//! - 行级省略提示只走 [`super::labels::more_lines`] / [`super::labels::more_items`]。
//! - 状态词只走 [`super::labels::status`]，渲染处不写中文状态字面量。
//! - 已合并进 `wrap` / `labels` 的旧 helper 不得以新名字之外的形式复活。
//! - 滚动条只走 [`super::overlay::paint_scrollbar`]，不得手画 `▐`/`█` 轨道。
//! - 设置页不用 `Paragraph`：它把超宽文本静默裁掉，文本一律经 `label`/`prose` 截断。
//!
//! 只扫产品代码：每个文件截掉 `#[cfg(test)] mod tests` 之后的部分，跳过 `*_tests.rs`。

use std::path::{Path, PathBuf};

/// 允许出现 `Clear` 的文件：浮层原语本身与终端清屏。
const CLEAR_ALLOWED: &[&str] = &["render/overlay.rs", "terminal.rs"];

/// 已删除、不得复活的旧 helper。
const RETIRED_HELPERS: &[&str] = &[
    "fn truncate_to_width(",
    "fn ellipsize(",
    "fn ellipsize_tail(",
    "fn pad_to(",
    "fn format_duration(",
    "fn display_keys(",
];

/// 状态词的旧中文写法：渲染层一律改用 `labels::status`。
const RETIRED_STATUS_WORDS: &[&str] = &[
    "\"排队中\"",
    "\"运行中\"",
    "\"已完成\"",
    "\"已注入\"",
    "\"已唤醒\"",
    "\"生成中\"",
    "\"未完成\"",
];

/// `src/` 下的产品代码：`(相对路径, 截掉测试模块后的源码)`。
fn product_sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("src 目录可读") {
            let path: PathBuf = entry.expect("目录项可读").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(&root)
                .expect("文件位于 src 下")
                .to_string_lossy()
                .replace('\\', "/");
            if path.extension().is_none_or(|ext| ext != "rs")
                || relative.ends_with("_tests.rs")
                || relative == "render/guards.rs"
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("源码可读");
            let product = match source.find("#[cfg(test)]\nmod tests") {
                Some(end) => source[..end].to_owned(),
                None => source,
            };
            files.push((relative, product));
        }
    }
    // 扫描为空时所有守卫都会假通过：钉住两处已知的渲染入口
    for known in ["render/scrollback.rs", "render/modal.rs"] {
        assert!(
            files.iter().any(|(path, _)| path == known),
            "守卫未扫到 {known}"
        );
    }
    files
}

/// 非注释行（行首 `//` 的整行注释与文档注释不算代码）。
fn code_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .map(|(index, line)| (index + 1, line))
}

/// `word` 在 `line` 中以完整标识符出现。
fn has_word(line: &str, word: &str) -> bool {
    let is_ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    line.match_indices(word).any(|(start, _)| {
        let before = line[..start].chars().next_back();
        let after = line[start + word.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

/// 逐条收集违规，失败时一次列全，便于集中修。
fn violations(check: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let mut found = Vec::new();
    for (path, source) in product_sources() {
        for (line_no, line) in code_lines(&source) {
            if check(&path, line) {
                found.push(format!("{path}:{line_no}: {}", line.trim()));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn overlays_never_use_clear_directly() {
    let found = violations(|path, line| !CLEAR_ALLOWED.contains(&path) && has_word(line, "Clear"));
    assert!(
        found.is_empty(),
        "浮层改用 overlay::paint_opaque：\n{}",
        found.join("\n")
    );
}

#[test]
fn omission_rows_come_from_labels() {
    let found = violations(|path, line| path != "render/labels.rs" && line.contains("⋯ 还有"));
    assert!(
        found.is_empty(),
        "省略提示改用 labels::more_lines / more_items：\n{}",
        found.join("\n")
    );
}

#[test]
fn status_words_come_from_labels() {
    let found = violations(|_, line| RETIRED_STATUS_WORDS.iter().any(|word| line.contains(word)));
    assert!(
        found.is_empty(),
        "状态词改用 labels::status：\n{}",
        found.join("\n")
    );
}

#[test]
fn retired_helpers_stay_removed() {
    let found = violations(|_, line| RETIRED_HELPERS.iter().any(|helper| line.contains(helper)));
    assert!(
        found.is_empty(),
        "折行/截断/耗时/键位只走 wrap 与 labels：\n{}",
        found.join("\n")
    );
}

#[test]
fn scrollbars_come_from_overlay() {
    let found = violations(|path, line| {
        path != "render/overlay.rs"
            && (line.contains("\"▐\"") || line.contains("'▐'") || line.contains("\"█\""))
    });
    assert!(
        found.is_empty(),
        "滚动条改用 overlay::paint_scrollbar：\n{}",
        found.join("\n")
    );
}

#[test]
fn settings_never_clip_silently() {
    let found =
        violations(|path, line| path.starts_with("settings/") && has_word(line, "Paragraph"));
    assert!(
        found.is_empty(),
        "设置页文本改用 label / prose（超宽以 … 收尾）：\n{}",
        found.join("\n")
    );
}

#[test]
fn word_match_respects_identifier_boundaries() {
    assert!(has_word("use ratatui::widgets::{Block, Clear};", "Clear"));
    assert!(has_word("frame.render_widget(Clear, area);", "Clear"));
    assert!(!has_word("state.clear_filter();", "Clear"));
    assert!(!has_word("ClearType::All", "Clear"));
}

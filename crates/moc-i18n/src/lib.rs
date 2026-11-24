//! moc 工作区统一本地化目录与轻量查表运行时。
//!
//! 全部 `locales/*.toml` 在构建期线性解析一次并按 key 字典序固化进 `.rodata`
//! 静态切片 [`CATALOG`]：
//! - 零运行时外部依赖、零启动期堆分配（无需构造 `HashMap`）。
//! - 全工作区共用一份目录，不再由各 crate 重复解析和重复内嵌。
//! - `zh` 为源语言兼最终回退；无占位符查询零分配返回 `Cow::Borrowed`。

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::fmt::{Display, Write as _};
use std::sync::atomic::{AtomicU8, Ordering};

include!(concat!(env!("OUT_DIR"), "/catalog.rs"));

const LOCALE_ZH: u8 = 0;
const LOCALE_EN: u8 = 1;

/// 当前进程语言；单字节原子读写，无锁无分配。默认 `zh`。
static CURRENT_LOCALE: AtomicU8 = AtomicU8::new(LOCALE_ZH);

#[inline]
fn is_english(locale: &str) -> bool {
    locale == "en" || locale.starts_with("en-") || locale.starts_with("en_")
}

/// 设置当前进程语言（`en` / `en-*` / `en_*` 切英文，其余回落源语言 `zh`）。
#[inline]
pub fn set_locale(locale: &str) {
    let code = if is_english(locale) {
        LOCALE_EN
    } else {
        LOCALE_ZH
    };
    CURRENT_LOCALE.store(code, Ordering::Relaxed);
}

/// 读取当前进程语言标识（`"zh"` 或 `"en"`）。
#[inline]
pub fn locale() -> &'static str {
    match CURRENT_LOCALE.load(Ordering::Relaxed) {
        LOCALE_EN => "en",
        _ => "zh",
    }
}

/// 按指定语言尝试查表；英文缺失时回落 `zh`，键不存在返回 `None`。
///
/// 选择静态排序切片 + 二分查找而非 `HashMap` / `phf`：
/// 目录编译期由 `BTreeMap` 按 key 严格升序生成（约 2600 键，最多 12 次比较），
/// 无需第三方依赖与启动初始化，不变量由单元测试校验。
#[inline]
pub fn try_translate(locale: &str, key: &str) -> Option<&'static str> {
    let index = CATALOG.binary_search_by_key(&key, |(k, _, _)| *k).ok()?;
    let (_, zh, en) = CATALOG[index];
    if is_english(locale) && !en.is_empty() {
        Some(en)
    } else {
        Some(zh)
    }
}

/// 按当前进程语言尝试查表；键不存在返回 `None`（不返回 `locale.key` 兜底）。
#[inline]
pub fn try_t(key: impl AsRef<str>) -> Option<Cow<'static, str>> {
    try_translate(locale(), key.as_ref()).map(Cow::Borrowed)
}

/// 按当前进程语言查表；键不存在时返回 `"{locale}.{key}"`。
#[inline]
pub fn translate(key: impl AsRef<str>) -> Cow<'static, str> {
    translate_str(key.as_ref())
}

fn translate_str(key: &str) -> Cow<'static, str> {
    let current = locale();
    match try_translate(current, key) {
        Some(text) => Cow::Borrowed(text),
        None => Cow::Owned(format!("{current}.{key}")),
    }
}

/// 按当前进程语言查表并单趟替换 `%{name}` 占位符。
///
/// 相比逐占位符 `str::replace`：
/// - 无占位符时零分配直接借用静态字符串；
/// - 有占位符时单次扫描直接写入目标缓冲，无中间 `String` 分配，且参数值内含 `%{...}` 不会被二次替换。
#[inline]
pub fn translate_fmt(key: impl AsRef<str>, args: &[(&str, &dyn Display)]) -> Cow<'static, str> {
    translate_fmt_str(key.as_ref(), args)
}

fn translate_fmt_str(key: &str, args: &[(&str, &dyn Display)]) -> Cow<'static, str> {
    let template = translate_str(key);
    if args.is_empty() || !template.contains("%{") {
        return template;
    }
    let mut out = String::with_capacity(template.len() + args.len() * 8);
    let mut rest = template.as_ref();
    while let Some(start) = rest.find("%{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Cow::Owned(out);
        };
        let name = &after[..end];
        if let Some((_, value)) = args.iter().find(|(k, _)| *k == name) {
            let _ = write!(out, "{value}");
        } else {
            out.push_str(&rest[start..start + 2 + end + 1]);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// 翻译查询宏：`t!("key")` 或 `t!("key", name = value, ...)`，返回 `Cow<'static, str>`。
///
/// 用块级 `let` 收口求值，确保 `&dyn Display` 临时切片在宏返回前立即释放，
/// 避免在 `async fn(..., t!(...)).await` 中把 `!Send` 临时值带过 `.await` 边界。
#[macro_export]
macro_rules! t {
    ($key:expr $(,)?) => {{
        let __moc_i18n_out = $crate::translate($key);
        __moc_i18n_out
    }};
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {{
        let __moc_i18n_out = $crate::translate_fmt(
            $key,
            &[$((stringify!($name), &$value as &dyn ::core::fmt::Display)),+],
        );
        __moc_i18n_out
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_sorted_non_empty_and_resolves_both_locales() {
        assert!(!CATALOG.is_empty());
        assert!(CATALOG.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let zh = try_translate("zh", "tui.status.running").expect("存在中文键");
        let en = try_translate("en", "tui.status.running").expect("存在英文键");
        assert_ne!(zh, en);
        assert!(try_translate("zh", "definitely.missing.key").is_none());
    }

    #[test]
    fn translate_fmt_substitutes_once_without_double_expansion() {
        let rendered = translate_fmt_str(
            "tui.mcp.config_error_fmt",
            &[("err", &"bad %{err} literal" as &dyn Display)],
        );
        assert!(rendered.contains("bad %{err} literal"));
        assert_eq!(
            t!("definitely.missing.key", a = 1).as_ref(),
            format!("{}.definitely.missing.key", locale())
        );
    }

    #[test]
    fn macro_temporaries_do_not_break_send_futures() {
        async fn sink(_text: String) {}
        fn assert_send<F: std::future::Future + Send>(_f: F) {}
        assert_send(async {
            sink(t!("tui.mcp.config_error_fmt", err = "boom").into_owned()).await;
        });
    }
}

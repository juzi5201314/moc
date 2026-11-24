//! 用户界面文案本地化。
//!
//! catalog 在 workspace 根的 `locales/`（TOML，`_version = 2`），由 `moc-i18n`
//! 编译期单份线性解析并内嵌；`zh` 是源语言兼最终回退，缺键在 en locale 下回落中文。
//! 只覆盖给人看的文本：系统提示词、运行时提醒、工具描述按规则保持英文，不走这里。

use std::borrow::Cow;

/// 按 `language` 配置初始化进程语言：`auto`（或空值）跟随 `LC_ALL`/`LC_MESSAGES`/`LANG`
/// 环境变量，`zh*` 进中文、其余进英文；显式 `zh`/`en` 直接生效。
/// 前端进程在入口尽早调用；配置加载后可再以解析值调用一次覆盖 auto 探测。
pub fn init(language: &str) {
    moc_i18n::set_locale(resolve(language));
}

pub(crate) fn resolve(language: &str) -> &'static str {
    match language {
        "zh" => "zh",
        "en" => "en",
        _ => detect(),
    }
}

fn detect() -> &'static str {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(var)
            && !value.is_empty()
        {
            return if value.starts_with("zh") { "zh" } else { "en" };
        }
    }
    "en"
}

/// 尝试取翻译，整条 locale 回退链都找不到时返回 `None`（区别于 `t!` 的 `locale.key` 兜底）。
/// 数据化查表（如配置目录）用它判断键是否存在。
pub(crate) fn try_t(key: &str) -> Option<Cow<'static, str>> {
    moc_i18n::try_t(key)
}

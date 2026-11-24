//! 用户界面文案本地化。
//!
//! catalog 在 workspace 根的 `locales/`（TOML，`_version = 2`），由 `moc-i18n`
//! 编译期单份线性解析并内嵌；`zh` 是源语言兼最终回退，缺键在 en locale 下回落中文。
//! 只覆盖给人看的文本：系统提示词、运行时提醒、工具描述按规则保持英文，不走这里。

use std::borrow::Cow;
use std::sync::atomic::{AtomicU8, Ordering};

const HINT_NONE: u8 = 0;
const HINT_ZH: u8 = 1;
const HINT_EN: u8 = 2;

/// 前端握手上报的界面语言（`initialize.locale`）；单字节原子，无则按环境变量探测。
static CLIENT_HINT: AtomicU8 = AtomicU8::new(HINT_NONE);

/// 按 `language` 配置初始化进程语言：显式 `zh`/`en` 直接生效；`auto`（或空值）先看
/// 前端上报的界面语言，没有再跟随 `LC_ALL`/`LC_MESSAGES`/`LANG`，`zh*` 进中文、其余进英文。
/// 前端进程在入口尽早调用；配置加载、握手上报或保存配置后再以配置值调用一次。
pub fn init(language: &str) {
    moc_i18n::set_locale(resolve(language));
}

/// 记录前端上报的界面语言（BCP 47），随后按 `language` 配置重新生效。
pub(crate) fn set_client_locale(locale: &str, language: &str) {
    let hint = if locale.starts_with("zh") {
        HINT_ZH
    } else {
        HINT_EN
    };
    CLIENT_HINT.store(hint, Ordering::Relaxed);
    init(language);
}

pub(crate) fn resolve(language: &str) -> &'static str {
    match language {
        "zh" => "zh",
        "en" => "en",
        _ => match CLIENT_HINT.load(Ordering::Relaxed) {
            HINT_ZH => "zh",
            HINT_EN => "en",
            _ => detect(),
        },
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

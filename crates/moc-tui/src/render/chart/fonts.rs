//! 图表字体与测宽：基于 `resvg::usvg::fontdb`，系统等宽字体优先，内嵌 CJK 子集兜底。
//!
//! 测宽与 `resvg` 的 `<text>` 排版共用同一个 [`fontdb::Database`] 实例：
//! - 默认优先系统 `monospace` 与常见系统 CJK 字体，使拉丁字母/数字比例贴近终端正文；
//! - 支持通过 `MOC_CHART_FONT` 指定首选字体族；
//! - 随 crate 内嵌的 `Noto Sans Mono CJK SC` 子集始终注入 `fontdb`，无系统字体环境下也能正常出字。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use resvg::usvg::fontdb::{Database, Family, ID, Query, Source};

/// 内嵌字体子集（Noto Sans Mono CJK SC，CFF/OTF，含 GB2312 汉字与拉丁区）。
///
/// 子集更新必须重新运行 `scripts/subset-font.sh` 并提交产物；许可证见同目录
/// `OFL.txt`。
pub const EMBEDDED_FONT: &[u8] =
    include_bytes!("../../../assets/fonts/NotoSansMonoCJKsc-Regular.subset.otf");

/// 字符 advance 缓存上限；超过整表清空。
const ADVANCE_CACHE_LIMIT: usize = 8192;

/// 字体集：共享 `fontdb::Database`、CSS `font-family` 回退链与字符宽度缓存。
pub struct Fonts {
    db: Arc<Database>,
    css_family: String,
    face_order: Vec<ID>,
    advance_cache: HashMap<(char, u32), u32>,
}

impl Fonts {
    /// 加载系统字体库并注入内嵌兜底字体（进程内由 [`with_fonts`] 单例复用）。
    pub fn new() -> Self {
        let mut db = Database::new();
        db.load_system_fonts();
        let embedded_ids = db.load_font_source(Source::Binary(Arc::new(EMBEDDED_FONT)));
        let embedded_family = embedded_ids
            .first()
            .and_then(|&id| db.face(id))
            .and_then(|face| face.families.first())
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "Noto Sans Mono CJK SC".to_owned());
        if db
            .query(&Query {
                families: &[Family::Monospace],
                ..Default::default()
            })
            .is_none()
        {
            db.set_monospace_family(&embedded_family);
        }
        let custom = std::env::var("MOC_CHART_FONT")
            .ok()
            .and_then(|raw| sanitize_font_name(&raw));
        let css_family = build_css_family(custom.as_deref(), &embedded_family);
        let face_order = resolve_face_order(&db, custom.as_deref(), &embedded_family);
        Self {
            db: Arc::new(db),
            css_family,
            face_order,
            advance_cache: HashMap::new(),
        }
    }

    /// 供 `resvg::usvg::Options` 复用的字体库句柄（零拷贝 `Arc`）。
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.db)
    }

    /// 供 SVG `<text font-family="...">` 使用的字体回退链。
    pub fn css_family(&self) -> &str {
        &self.css_family
    }

    /// 文本宽度：逐字符累加整数像素 advance。
    pub fn width(&mut self, text: &str, px: u32) -> u32 {
        let mut pen = 0u32;
        for ch in text.chars() {
            pen += self.advance_of(ch, px);
        }
        pen
    }

    /// 按像素宽度截断文本，超宽时以 `…` 结尾（`…` 自身的宽度计入预算）。
    pub fn truncate(&mut self, text: &str, max_px: u32, px: u32) -> String {
        if self.width(text, px) <= max_px {
            return text.to_string();
        }
        let ellipsis = self.width("…", px);
        let mut out = String::new();
        let mut used = 0u32;
        for ch in text.chars() {
            let advance = self.advance_of(ch, px);
            if used + advance + ellipsis > max_px {
                break;
            }
            out.push(ch);
            used += advance;
        }
        out.push('…');
        out
    }

    /// 单字符在指定字号下的整数像素推进量。
    fn advance_of(&mut self, ch: char, px: u32) -> u32 {
        let key = (ch, px);
        if let Some(&cached) = self.advance_cache.get(&key) {
            return cached;
        }
        let measured = self.measure_advance(ch, px);
        if self.advance_cache.len() >= ADVANCE_CACHE_LIMIT {
            self.advance_cache.clear();
        }
        self.advance_cache.insert(key, measured);
        measured
    }

    /// 按与 SVG 回退链一致的优先级在 `fontdb` 中测量字符 advance。
    fn measure_advance(&self, ch: char, px: u32) -> u32 {
        for &id in &self.face_order {
            if let Some(advance) = face_char_advance(&self.db, id, ch, px) {
                return advance;
            }
        }
        for face in self.db.faces() {
            if let Some(advance) = face_char_advance(&self.db, face.id, ch, px) {
                return advance;
            }
        }
        if ch.is_ascii() { px / 2 } else { px }
    }
}

/// `new()` 会扫描系统字体库并注入内嵌字体，手写 `Default` 转调它。
impl Default for Fonts {
    fn default() -> Self {
        Self::new()
    }
}

/// 进程内字体单例：光栅化与测宽都通过它拿共享字体库。
pub fn with_fonts<R>(f: impl FnOnce(&mut Fonts) -> R) -> R {
    static FONTS: LazyLock<Mutex<Fonts>> = LazyLock::new(|| Mutex::new(Fonts::new()));
    let mut fonts = match FONTS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut fonts)
}

/// 清理 `MOC_CHART_FONT` 中的引号、分号及 XML/CSS 特殊字符。
fn sanitize_font_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|ch| !ch.is_control() && !matches!(ch, '\'' | '"' | ';' | '<' | '>' | '&'))
        .collect();
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// 构造 CSS `font-family` 回退链：可选用户指定字体 → 系统 `monospace` → CJK 回退 → 内嵌字体。
fn build_css_family(custom: Option<&str>, embedded: &str) -> String {
    let mut parts = Vec::with_capacity(10);
    if let Some(name) = custom {
        parts.push(format!("'{name}'"));
    }
    parts.push("monospace".to_owned());
    for family in [
        "PingFang SC",
        "Microsoft YaHei",
        "Noto Sans CJK SC",
        "WenQuanYi Zen Hei Mono",
        "WenQuanYi Zen Hei",
        embedded,
    ] {
        parts.push(format!("'{family}'"));
    }
    parts.push("sans-serif".to_owned());
    parts.join(", ")
}

/// 按与 CSS 回退链同序解析 `fontdb::ID` 列表，供精确测宽。
fn resolve_face_order(db: &Database, custom: Option<&str>, embedded: &str) -> Vec<ID> {
    let mut families: Vec<Family<'_>> = Vec::with_capacity(10);
    if let Some(name) = custom {
        families.push(Family::Name(name));
    }
    families.push(Family::Monospace);
    for name in [
        "PingFang SC",
        "Microsoft YaHei",
        "Noto Sans CJK SC",
        "WenQuanYi Zen Hei Mono",
        "WenQuanYi Zen Hei",
        embedded,
    ] {
        families.push(Family::Name(name));
    }
    families.push(Family::SansSerif);

    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(families.len());
    for family in families {
        if let Some(id) = db.query(&Query {
            families: &[family],
            ..Default::default()
        }) && seen.insert(id)
        {
            out.push(id);
        }
    }
    out
}

/// 用 `ttf_parser` 读取指定字体面中 `ch` 的水平 advance（像素，四舍五入）。
fn face_char_advance(db: &Database, id: ID, ch: char, px: u32) -> Option<u32> {
    db.with_face_data(id, |data, index| {
        let face = ttf_parser::Face::parse(data, index).ok()?;
        let gid = face.glyph_index(ch)?;
        let raw = face.glyph_hor_advance(gid)?;
        let upem = u32::from(face.units_per_em());
        if upem == 0 {
            return None;
        }
        let scaled = f64::from(raw) * f64::from(px) / f64::from(upem);
        Some(scaled.round().max(0.0) as u32)
    })
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 宽度随文本变长单调增加，空串为 0（ASCII 与中文同一规则）。
    #[test]
    fn width_grows_with_text() {
        with_fonts(|fonts| {
            assert_eq!(fonts.width("", 16), 0);
            let one = fonts.width("A", 16);
            let two = fonts.width("AA", 16);
            let four = fonts.width("AAAA", 16);
            assert!(one > 0 && one < two && two < four);
            let cjk_one = fonts.width("中", 16);
            let cjk_two = fonts.width("中文", 16);
            assert!(cjk_one > 0 && cjk_one < cjk_two);
        });
    }

    /// 截断结果不超预算、以省略号结尾；放得下时原样返回。
    #[test]
    fn truncate_fits_and_ends_with_ellipsis() {
        with_fonts(|fonts| {
            let text = "一段时间内的错误率";
            let full = fonts.width(text, 16);
            assert_eq!(fonts.truncate(text, full, 16), text, "放得下不截断");
            let max = full / 2;
            let cut = fonts.truncate(text, max, 16);
            assert!(cut.ends_with('…'), "截断必须以省略号结尾：{cut}");
            assert!(
                fonts.width(&cut, 16) <= max,
                "截断后宽度 {} 不得超过预算 {max}",
                fonts.width(&cut, 16)
            );
        });
    }

    /// 共享字体库包含内嵌 CJK 兜底字体，无系统字体环境也能覆盖常用汉字与拉丁。
    #[test]
    fn database_covers_embedded_cjk_fallback() {
        with_fonts(|fonts| {
            let db = fonts.database();
            assert!(!db.is_empty());
            assert!(fonts.css_family().contains("monospace"));
            let has_cjk = db.faces().any(|face| {
                db.with_face_data(face.id, |data, idx| {
                    ttf_parser::Face::parse(data, idx)
                        .ok()
                        .and_then(|f| f.glyph_index('中'))
                        .is_some()
                }) == Some(true)
            });
            assert!(has_cjk, "fontdb 必须能覆盖中文汉字");
        });
    }
}

//! 图表字体与测宽：基于 `resvg::usvg::fontdb`，系统等宽字体优先，内嵌 CJK 子集兜底。
//!
//! 测宽与 `resvg` 的 `<text>` 排版共用同一个 [`fontdb::Database`] 实例：
//! - 默认优先系统 `monospace` 与常见系统 CJK 字体，使拉丁字母/数字比例贴近终端正文；
//! - 支持通过 `MOC_CHART_FONT` 指定首选字体族；
//! - 随 crate 内嵌的 `Noto Sans Mono CJK SC` 子集始终注入 `fontdb`，无系统字体环境下也能正常出字。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use resvg::usvg::fontdb::{Database, Family, Query, Source};
use unicode_segmentation::UnicodeSegmentation;

use super::draw::SvgBuilder;

/// 内嵌字体子集（Noto Sans Mono CJK SC，CFF/OTF，含 GB2312 汉字与拉丁区）。
///
/// 子集更新必须重新运行 `scripts/subset-font.sh` 并提交产物；许可证见同目录
/// `OFL.txt`。
pub const EMBEDDED_FONT: &[u8] =
    include_bytes!("../../../assets/fonts/NotoSansMonoCJKsc-Regular.subset.otf");

/// 整段文本测宽缓存上限；字号变化或达到上限时清空。
const WIDTH_CACHE_LIMIT: usize = 1024;

/// 字体库和整段文本测宽缓存；布局与绘制都交给 usvg，避免另写一套字形回退规则。
pub struct Fonts {
    db: Arc<Database>,
    css_family: String,
    width_px: u32,
    width_cache: HashMap<String, u32>,
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
        Self {
            db: Arc::new(db),
            css_family,
            width_px: 0,
            width_cache: HashMap::new(),
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

    /// 用实际 SVG 排版测量整段文本，只在最终边界向上取整。
    pub fn width(&mut self, text: &str, px: u32) -> u32 {
        if text.is_empty() {
            return 0;
        }
        if self.width_px != px || self.width_cache.len() >= WIDTH_CACHE_LIMIT {
            self.width_cache.clear();
            self.width_px = px;
        }
        if let Some(&width) = self.width_cache.get(text) {
            return width;
        }
        let mut svg = SvgBuilder::new(1, 1, &self.css_family, px);
        svg.text(0, 0, text, [0; 3]);
        let options = resvg::usvg::Options {
            fontdb: self.database(),
            ..Default::default()
        };
        let tree = resvg::usvg::Tree::from_str(&svg.finish(), &options)
            .expect("SVG 构建器已转义文本并替换非法 XML 字符");
        // 文本包围盒非负；像素预算向上取整，超出 u32 的文本按最大宽度处理。
        let width = tree.root().bounding_box().width().ceil() as u32;
        self.width_cache.insert(text.to_owned(), width);
        width
    }

    /// 截断只在字素边界进行；每个候选整体排版，保留连字、回退字体与小数推进量。
    pub fn truncate(&mut self, text: &str, max_px: u32, px: u32) -> String {
        if self.width(text, px) <= max_px {
            return text.to_owned();
        }
        if self.width("…", px) > max_px {
            return String::new();
        }
        let boundaries: Vec<usize> = text.grapheme_indices(true).map(|(at, _)| at).collect();
        let mut low = 0;
        let mut high = boundaries.len();
        let mut candidate = String::with_capacity(text.len() + '…'.len_utf8());
        while low + 1 < high {
            let mid = low + (high - low) / 2;
            candidate.clear();
            candidate.push_str(&text[..boundaries[mid]]);
            candidate.push('…');
            if self.width(&candidate, px) <= max_px {
                low = mid;
            } else {
                high = mid;
            }
        }
        candidate.clear();
        candidate.push_str(&text[..boundaries[low]]);
        candidate.push('…');
        candidate
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_advances_round_only_after_shaping_the_whole_label() {
        let mut db = Database::new();
        let ids = db.load_font_source(Source::Binary(Arc::new(EMBEDDED_FONT)));
        let family = db.face(ids[0]).expect("内嵌字体").families[0].0.clone();
        let mut fonts = Fonts {
            db: Arc::new(db),
            css_family: format!("'{family}'"),
            width_px: 0,
            width_cache: HashMap::new(),
        };
        let face = ttf_parser::Face::parse(EMBEDDED_FONT, 0).expect("内嵌字体面");
        let glyph = face.glyph_index('A').expect("拉丁字形");
        let advance = f64::from(face.glyph_hor_advance(glyph).expect("水平推进量")) * 13.0
            / f64::from(face.units_per_em());
        assert_ne!(advance.fract(), 0.0, "该字号应覆盖非整数推进量");
        let text = "A".repeat(40);
        assert_eq!(f64::from(fonts.width(&text, 13)), (advance * 40.0).ceil());
        for budget in [0, 5, 40, 100, 200] {
            let cut = fonts.truncate("中文ABC123与长标签ABCDEFGHIJKLMNOPQRSTUVWXYZ", budget, 13);
            assert!(fonts.width(&cut, 13) <= budget);
        }
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

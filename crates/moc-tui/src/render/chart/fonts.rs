//! 字体与字形光栅化：内嵌 CJK 等宽字体为主，缺字才懒回退系统字体。
//!
//! 内嵌字体随 crate 提交（见 `assets/fonts/`），因此图表的拉丁与中文标签
//! 在任何机器上都是同一副字形，不受系统字体差异影响。解析子集里全部字形
//! 几何是一次性重活（约九千字形），[`Fonts`] 由 [`with_fonts`] 的进程单例
//! 惰性构造一次，绝不每帧重建。
//!
//! 缺字回退只在真正遇到内嵌字体没有的字符时发生：先扫一遍系统字体库的
//! cmap，解析第一个覆盖该字的 face，并按 face id 缓存；没命中就返回 `None`，
//! 由 [`crate::render::chart::text::draw`] 画方框占位。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

/// 内嵌字体子集（Noto Sans Mono CJK SC，CFF/OTF，含 GB2312 汉字与拉丁区）。
///
/// 子集更新必须重新运行 `scripts/subset-font.sh` 并提交产物；许可证见同目录
/// `OFL.txt`。
pub const EMBEDDED_FONT: &[u8] =
    include_bytes!("../../../assets/fonts/NotoSansMonoCJKsc-Regular.subset.otf");

/// 字形缓存上限；超过整表清空。
const GLYPH_CACHE_LIMIT: usize = 8192;

/// 已光栅化的字形位图。
///
/// 坐标沿用 fontdue 的约定：`left`（`xmin`）是位图左缘相对笔位的像素偏移，
/// `top`（`ymin`）是位图**下缘**相对基线的偏移（向上为正）。整数像素落位
/// 由 [`crate::render::chart::text::draw`] 做——`fonts` 只报告度量，不管排版。
///
/// 字形与颜色无关：同一字形缓存可被不同颜色复用，颜色由绘制调用传入。
pub struct RenderedGlyph {
    /// 位图宽（像素）。
    pub w: u32,
    /// 位图高（像素）。
    pub h: u32,
    /// 位图左缘相对笔位的偏移（可负）。
    pub left: i32,
    /// 位图下缘相对基线的偏移，向上为正。
    pub top: i32,
    /// 推进量（像素，未取整；取整规则见 [`glyph_advance`]）。
    pub advance: f32,
    /// 覆盖度，行优先，长度恰为 `w * h`。
    pub coverage: Vec<u8>,
}

/// 字形缓存键：`(字符, 字号)`。
type GlyphKey = (char, u32);
/// 缓存槽：要么是字形，要么是"确认缺字"（`None`）。
type GlyphSlot = Option<Arc<RenderedGlyph>>;

/// 字体集：内嵌主字体 + 按需加载的系统回退字体 + 字形缓存。
pub struct Fonts {
    primary: fontdue::Font,
    /// 系统回退字体：按 face id 缓存已解析的那一个（只为真正用到的缺字付费）。
    fallback_fonts: HashMap<fontdb::ID, fontdue::Font>,
    /// 字形缓存：键是稀疏的 `(字符, 字号)` 组合，用 `HashMap` 而非按字符码
    /// 开稠密二维表；图表只在切换时重绘，超过上限整表清空的代价可忽略。
    cache: HashMap<GlyphKey, GlyphSlot>,
}

impl Fonts {
    /// 创建字体集：解析内嵌字体（进程内只做一次）。
    ///
    /// 内嵌资产是构建期固定的产物，解析失败说明构建产物损坏——直接 `expect`
    /// 暴露，不给运行时留静默降级路径。
    pub fn new() -> Self {
        let primary = fontdue::Font::from_bytes(EMBEDDED_FONT, font_settings(0))
            .expect("moc-tui 内嵌字体解析失败：构建产物损坏");
        Self {
            primary,
            fallback_fonts: HashMap::new(),
            cache: HashMap::new(),
        }
    }

    /// 取字符在指定字号下的字形；内嵌字体缺字时回退系统字体，仍缺则 `None`。
    pub fn glyph(&mut self, ch: char, px: u32) -> Option<Arc<RenderedGlyph>> {
        let key: GlyphKey = (ch, px);
        if let Some(slot) = self.cache.get(&key) {
            return slot.clone();
        }
        let rendered = if self.primary.lookup_glyph_index(ch) != 0 {
            Some(Arc::new(rasterize(&self.primary, ch, px)))
        } else {
            self.fallback_glyph(ch, px).map(Arc::new)
        };
        if self.cache.len() >= GLYPH_CACHE_LIMIT {
            self.cache.clear();
        }
        self.cache.insert(key, rendered.clone());
        rendered
    }

    /// 系统字体回退：按 `fontdb` 的 face 顺序找第一个覆盖 `ch` 的字体。
    fn fallback_glyph(&mut self, ch: char, px: u32) -> Option<RenderedGlyph> {
        let db = system_fonts()?;
        for face in db.faces() {
            match self.fallback_fonts.get(&face.id) {
                Some(font) if font.lookup_glyph_index(ch) != 0 => {
                    return Some(rasterize(font, ch, px));
                }
                Some(_) => continue,
                None => {}
            }
            // 先只解析 cmap 判定覆盖，命中才把整张字体交给 fontdue 解析。
            let covered = db.with_face_data(face.id, |data, index| {
                ttf_parser::Face::parse(data, index)
                    .ok()
                    .and_then(|f| f.glyph_index(ch))
                    .is_some()
            });
            if covered != Some(true) {
                continue;
            }
            let font = db.with_face_data(face.id, |data, index| {
                fontdue::Font::from_bytes(data, font_settings(index))
            });
            let Some(Ok(font)) = font else {
                continue;
            };
            let glyph = rasterize(&font, ch, px);
            self.fallback_fonts.insert(face.id, font);
            return Some(glyph);
        }
        None
    }

    /// 文本宽度：逐字符累加推进量（规则见 [`glyph_advance`]）。
    pub fn width(&mut self, text: &str, px: u32) -> u32 {
        let mut pen = 0u32;
        for ch in text.chars() {
            pen += self.advance_of(ch, px);
        }
        pen
    }

    /// 单字符推进量：命中字形用其 advance，缺字用方框宽度。
    fn advance_of(&mut self, ch: char, px: u32) -> u32 {
        match self.glyph(ch, px) {
            Some(glyph) => glyph_advance(&glyph),
            None => missing_box_width(px),
        }
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
}

/// `new()` 会解析内嵌字体资产（有副作用），不是各字段默认值的简单拼装，
/// 因此手写 `Default` 转调它，而不是 derive。
impl Default for Fonts {
    fn default() -> Self {
        Self::new()
    }
}

/// 进程内字体单例：光栅化与测量都通过它拿字体。
///
/// 内嵌字体解析一次即驻留；锁中毒（某次绘制 panic）时取回内部值继续用，
/// 不让一次 panic 传染后续所有绘制。
pub fn with_fonts<R>(f: impl FnOnce(&mut Fonts) -> R) -> R {
    static FONTS: LazyLock<Mutex<Fonts>> = LazyLock::new(|| Mutex::new(Fonts::new()));
    let mut fonts = match FONTS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut fonts)
}

/// 字形推进量的唯一取整规则：四舍五入到整数像素。
///
/// [`Fonts::width`] 与 [`crate::render::chart::text::draw`] 必须共用它：
/// 量宽与落位各算一套，累加几字后就会差 1px，截断与右对齐全部错位。
pub(crate) fn glyph_advance(glyph: &RenderedGlyph) -> u32 {
    // 像素数学：advance 由 fontdue 按字号给出，非负；浮点转整点饱和。
    glyph.advance.round().max(0.0) as u32
}

/// 缺字方框的宽度（同时也是它的推进量）。
pub(crate) fn missing_box_width(px: u32) -> u32 {
    px / 2
}

/// fontdue 解析参数。
///
/// 图表只做单字符顺序排版，用不到 GSUB 连字；关掉替换字形加载，避免把 CJK
/// 子集里的竖排/变体字形也解析一遍（解析量直接翻倍）。
fn font_settings(collection_index: u32) -> fontdue::FontSettings {
    fontdue::FontSettings {
        collection_index,
        load_substitutions: false,
        ..fontdue::FontSettings::default()
    }
}

/// 把字形光栅化成位图与度量。
fn rasterize(font: &fontdue::Font, ch: char, px: u32) -> RenderedGlyph {
    let (metrics, coverage) = font.rasterize(ch, px as f32); // 字号是像素数，精度足够
    RenderedGlyph {
        w: u32::try_from(metrics.width).expect("字形位图宽度超出 u32"),
        h: u32::try_from(metrics.height).expect("字形位图高度超出 u32"),
        left: metrics.xmin,
        top: metrics.ymin,
        advance: metrics.advance_width,
        coverage,
    }
}

/// 进程内系统字体库：只在首次遇到内嵌字体缺字时扫描一次目录。
fn system_fonts() -> Option<&'static fontdb::Database> {
    static DB: LazyLock<Option<fontdb::Database>> = LazyLock::new(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        (!db.is_empty()).then_some(db)
    });
    DB.as_ref()
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

    /// 位图尺寸与覆盖度长度严格一致，中文有实体像素。
    #[test]
    fn glyph_coverage_matches_dimensions() {
        with_fonts(|fonts| {
            let glyph = fonts.glyph('中', 16).expect("内嵌字体必须覆盖常用汉字");
            assert!(glyph.w > 0 && glyph.h > 0);
            assert_eq!(
                usize::try_from(glyph.w * glyph.h).expect("位图尺寸在 usize 内"),
                glyph.coverage.len()
            );
            assert!(glyph.coverage.iter().any(|c| *c > 0), "字形不能是空白");
        });
    }

    /// 同一 `(字符, 字号)` 两次调用返回同一个 `Arc`（缓存命中）。
    #[test]
    fn glyph_cache_shares_arc() {
        with_fonts(|fonts| {
            let first = fonts.glyph('字', 14).expect("内嵌字体覆盖该字");
            let second = fonts.glyph('字', 14).expect("第二次应从缓存返回");
            assert!(Arc::ptr_eq(&first, &second), "缓存必须复用同一字形");
        });
    }

    /// 内嵌子集与常见系统字体都不覆盖的字符返回 `None`，不 panic。
    #[test]
    fn missing_glyph_returns_none() {
        with_fonts(|fonts| {
            // U+10FFFD 是第 16 平面私用区的替换字符，任何常规字体都不映射。
            assert!(fonts.glyph('\u{10FFFD}', 16).is_none());
            // 缺字也要有推进量（按方框宽度算），否则后续文字会叠在一起。
            assert_eq!(fonts.width("\u{10FFFD}", 16), missing_box_width(16));
        });
    }
}

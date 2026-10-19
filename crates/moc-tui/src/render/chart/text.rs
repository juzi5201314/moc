//! 文本绘制：把字形覆盖度按 5 档 alpha 预混成调色板像素。
//!
//! 排版只做 LTR 顺序累加 advance（无 kerning、无连字、无 bidi）：图表标签是
//! 短文本，这些能力换不来可读性，却会让"量出来的宽"与"画出来的位"难以对齐。
//! 落位规则与 [`crate::render::chart::fonts::Fonts::width`] 共用同一套 advance
//! 取整（[`crate::render::chart::fonts::glyph_advance`]），保证调用方按量出的
//! 宽度做右对齐时逐像素吻合。
//!
//! 覆盖度不直接写进 RGB，而是先按 0/64/128/192/255 五档与底色 `under` 预混，
//! 把 5 个结果登记进画布调色板；每个文本像素只需一次索引写入，调色板规模
//! 也不会随覆盖率增长。

use super::fonts::{Fonts, glyph_advance, missing_box_width};
use crate::render::canvas::Canvas;

/// 覆盖度分档对应的 alpha：0 档不落墨，其余 4 档参与预混。
const ALPHAS: [u32; 5] = [0, 64, 128, 192, 255];

/// 在画布上画一行文本，返回结束笔位（`x + 文本宽度`）。
///
/// `baseline_y` 是基线所在像素行（可为负，表示整行在画布上缘之外）；
/// `color` 是文字色，`under` 是文字覆盖区域的底色（α 预混用，必须是该区域
/// 恒定的背景色，否则字形边缘会出现底色光晕）。字形按整数像素落位，越界
/// 像素由画布裁剪；内嵌字体缺字的字符画一个 1px 方框占位，不中断整串。
///
/// 参数是契约签名（画布 / 位置 / 文本 / 字号 / 前景 / 底色 / 字体集），
/// 不再抽配置结构体。
#[expect(
    clippy::too_many_arguments,
    reason = "契约签名：画布/位置/文本/字号/前景/底色/字体集，调用点全部传常量"
)]
pub fn draw(
    canvas: &mut Canvas,
    x: u32,
    baseline_y: i32,
    text: &str,
    px: u32,
    color: [u8; 3],
    under: [u8; 3],
    fonts: &mut Fonts,
) -> u32 {
    let mut ink = [0u8; 5];
    for (tier, alpha) in ALPHAS.iter().enumerate().skip(1) {
        ink[tier] = canvas.color(blend(under, color, *alpha));
    }
    let height = i64::from(canvas.h());
    let mut pen = x;
    for ch in text.chars() {
        let Some(glyph) = fonts.glyph(ch, px) else {
            let width = missing_box_width(px);
            draw_missing_box(canvas, pen, baseline_y, width, px, ink[4]);
            pen += width;
            continue;
        };
        let origin_x = i64::from(pen) + i64::from(glyph.left);
        // `top` 是位图下缘相对基线的偏移（向上为正），换算成位图首行：
        // 首行 = 基线 - 下缘偏移 - 位图高。
        let origin_y = i64::from(baseline_y) - i64::from(glyph.top) - i64::from(glyph.h);
        if glyph.w > 0 && glyph.h > 0 {
            let stride = glyph.w as usize; // 像素数学：位图宽远小于 usize 上限
            for (row, line) in glyph.coverage.chunks_exact(stride).enumerate() {
                let y = origin_y + row as i64; // 像素数学：位图行数很小
                if !(0..height).contains(&y) {
                    continue;
                }
                for (col, coverage) in line.iter().enumerate() {
                    if *coverage == 0 {
                        continue;
                    }
                    let gx = origin_x + col as i64; // 像素数学：位图列数很小
                    if gx < 0 {
                        continue;
                    }
                    // gx/y 非负；超出画布的部分由 `set` 裁剪。
                    canvas.set(gx as u32, y as u32, ink[coverage_tier(*coverage)]);
                }
            }
        }
        pen += glyph_advance(&glyph);
    }
    pen
}

/// 覆盖度 → 5 档索引：1..=64 为 1 档，…，193..=255 为 4 档（纯色）。
fn coverage_tier(coverage: u8) -> usize {
    usize::from(coverage).div_ceil(64)
}

/// 按 8 位 alpha 把 `over` 预混到 `under` 上（整数四舍五入）。
fn blend(under: [u8; 3], over: [u8; 3], alpha: u32) -> [u8; 3] {
    let channel = |u: u8, o: u8| -> u8 {
        // 分子最大 255 * 255，除法结果 ≤ 255，窄化不截断。
        ((u32::from(u) * (255 - alpha) + u32::from(o) * alpha + 127) / 255) as u8
    };
    [
        channel(under[0], over[0]),
        channel(under[1], over[1]),
        channel(under[2], over[2]),
    ]
}

/// 缺字方框：宽 `w`、高 `h` 的 1px 描边矩形，竖直方向贴着基线上方的字符格。
fn draw_missing_box(canvas: &mut Canvas, x: u32, baseline_y: i32, w: u32, h: u32, color: u8) {
    if w == 0 || h == 0 {
        return;
    }
    let left = i64::from(x);
    let top = i64::from(baseline_y) - i64::from(h);
    let right = left + i64::from(w) - 1;
    let bottom = top + i64::from(h) - 1;
    for xx in left..=right {
        put(canvas, xx, top, color);
        put(canvas, xx, bottom, color);
    }
    for yy in top..=bottom {
        put(canvas, left, yy, color);
        put(canvas, right, yy, color);
    }
}

/// 整点落像素；负坐标（伸出画布上缘/左缘）跳过。
fn put(canvas: &mut Canvas, x: i64, y: i64, color: u8) {
    if x < 0 || y < 0 {
        return;
    }
    // 像素数学：非负 i64 转 u32，超界部分由 `set` 裁剪。
    canvas.set(x as u32, y as u32, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::chart::fonts::with_fonts;

    /// 测试底色。
    const BG: [u8; 3] = [10, 10, 12];

    /// 返回值严格等于 `x + fonts.width(text, px)`（同一套 advance 取整）。
    #[test]
    fn draw_returns_x_plus_width() {
        with_fonts(|fonts| {
            let mut canvas = Canvas::new(160, 40, BG);
            let end = draw(&mut canvas, 3, 30, "A中x", 16, [240, 240, 240], BG, fonts);
            assert_eq!(end, 3 + fonts.width("A中x", 16));
        });
    }

    /// 纯背景画布上画字母必留墨迹（至少一个非背景像素）。
    #[test]
    fn draw_paints_ink() {
        with_fonts(|fonts| {
            let mut canvas = Canvas::new(80, 32, BG);
            draw(&mut canvas, 4, 24, "A", 16, [255, 255, 255], BG, fonts);
            assert!(
                canvas.index().iter().any(|i| *i != 0),
                "字母 A 必须留下像素"
            );
        });
    }

    /// 文本伸出右缘/上缘只裁剪，不 panic；结束笔位照常累加。
    #[test]
    fn draw_clips_at_edges() {
        with_fonts(|fonts| {
            let mut canvas = Canvas::new(12, 12, BG);
            let end = draw(&mut canvas, 10, 11, "WWWW", 16, [255, 255, 255], BG, fonts);
            assert!(end > 12, "文本确实伸出画布右缘");
            // 基线抬到画布上方：逐行判定全部落在界外，什么都不画。
            let mut above = Canvas::new(12, 12, BG);
            let end_above = draw(&mut above, 10, -20, "WW", 16, [255, 255, 255], BG, fonts);
            assert!(end_above > 10);
            assert!(above.index().iter().all(|i| *i == 0));
        });
    }

    /// 缺字画方框占位，结束笔位按方框宽度前进。
    #[test]
    fn missing_glyph_draws_box() {
        with_fonts(|fonts| {
            let mut canvas = Canvas::new(48, 32, BG);
            let end = draw(
                &mut canvas,
                4,
                24,
                "\u{10FFFD}",
                16,
                [255, 255, 255],
                BG,
                fonts,
            );
            assert_eq!(end, 4 + missing_box_width(16));
            let painted = canvas.index().iter().filter(|i| **i != 0).count();
            assert!(painted > 0, "缺字必须画方框，不能是空白");
        });
    }
}

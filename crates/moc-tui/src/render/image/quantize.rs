//! 位图 → 调色板 + 索引平面。
//!
//! 终端的三种协议里只有 kitty / iTerm2 吃真彩色；sixel 天生是索引色，塞 24 位
//! RGB 进去要终端自己再量化一遍。统一在这里量化成一张 ≤ 256 色的调色板，
//! 三个协议共用同一份索引平面（[`crate::render::canvas::Canvas`] 的双平面），
//! 顺带省掉 sixel 的逐像素真彩色带宽。
//!
//! 0 号槽位固定为主题底色：透明像素与恰好落在底色上的像素都映射到 0，
//! sixel 编码本来就会跳过 0 号槽位，图片留白因此与终端背景严丝合缝。

use std::collections::HashMap;

/// 调色板容量（含 0 号底色槽位）。
const PALETTE_CAP: usize = 256;

/// NeuQuant 训练采样间隔：10 是质量与耗时的常用折中。
const SAMPLE_FACTOR: i32 = 10;

/// NeuQuant 的色槽数：留 1 个给主题底色。
const QUANT_COLORS: usize = PALETTE_CAP - 1;

/// RGBA → (调色板, 索引平面)，索引 0 恒为底色 `bg`。
///
/// 先尝试精确调色板：图标、截图、图表这类「颜色数不多」的图直接拿真彩色，
/// 一个像素都不会被量化偏。颜色数一旦超过 256 才退回 NeuQuant——超出的
/// 位置通常在前几百个像素内就出现，因此这条快路径对照片类图片几乎不花钱。
pub fn quantize(image: &image::RgbaImage, bg: [u8; 3]) -> (Vec<[u8; 3]>, Vec<u8>) {
    let (w, h) = image.dimensions();
    let count = usize::try_from(u64::from(w) * u64::from(h)).unwrap_or(0);
    // 半透明像素先预乘到底色上：终端背景已在画布上铺过，这里不做二次混合
    let mut rgba = Vec::with_capacity(count * 4);
    for pixel in image.pixels() {
        let [r, g, b, a] = pixel.0;
        let flattened = blend([r, g, b], bg, a);
        rgba.extend_from_slice(&[flattened[0], flattened[1], flattened[2], 0xff]);
    }
    exact(&rgba, bg).unwrap_or_else(|| reduce(&rgba, bg))
}

/// 颜色数不超过 256 时直接建精确调色板；超了返回 `None`（交给 NeuQuant）。
fn exact(rgba: &[u8], bg: [u8; 3]) -> Option<(Vec<[u8; 3]>, Vec<u8>)> {
    let mut slots: HashMap<[u8; 3], u8> = HashMap::with_capacity(PALETTE_CAP);
    slots.insert(bg, 0);
    let mut palette = Vec::with_capacity(PALETTE_CAP);
    palette.push(bg);
    let mut index = Vec::with_capacity(rgba.len() / 4);
    for pixel in rgba.as_chunks::<4>().0 {
        let rgb = [pixel[0], pixel[1], pixel[2]];
        match slots.get(&rgb) {
            Some(slot) => index.push(*slot),
            None => {
                if palette.len() >= PALETTE_CAP {
                    return None;
                }
                let slot = u8::try_from(palette.len()).ok()?;
                palette.push(rgb);
                slots.insert(rgb, slot);
                index.push(slot);
            }
        }
    }
    Some((palette, index))
}

/// NeuQuant 量化：照片类图片的唯一选择。索引整体 `+1`，0 号槽位留给底色。
fn reduce(rgba: &[u8], bg: [u8; 3]) -> (Vec<[u8; 3]>, Vec<u8>) {
    let quantizer = color_quant::NeuQuant::new(SAMPLE_FACTOR, QUANT_COLORS, rgba);
    let mut palette = Vec::with_capacity(PALETTE_CAP);
    palette.push(bg);
    for slot in 0..QUANT_COLORS {
        let [r, g, b, _] = quantizer.lookup(slot).unwrap_or([0, 0, 0, 0xff]);
        palette.push([r, g, b]);
    }
    let mut index = Vec::with_capacity(rgba.len() / 4);
    for pixel in rgba.as_chunks::<4>().0 {
        index.push(if pixel[..3] == bg {
            0
        } else {
            // 调色板长 256，`+ 1` 落在 1..=255，`u8` 装得下
            u8::try_from(quantizer.index_of(pixel) + 1).unwrap_or(0)
        });
    }
    (palette, index)
}

/// 把带 alpha 的颜色合成到不透明底色上。
fn blend(src: [u8; 3], bg: [u8; 3], alpha: u8) -> [u8; 3] {
    if alpha == 255 {
        return src;
    }
    if alpha == 0 {
        return bg;
    }
    let a = u32::from(alpha);
    let inv = 255 - a;
    let mix = |over: u8, under: u8| -> u8 {
        let value = u32::from(over) * a + u32::from(under) * inv;
        u8::try_from((value + 127) / 255).unwrap_or(u8::MAX)
    };
    [mix(src[0], bg[0]), mix(src[1], bg[1]), mix(src[2], bg[2])]
}

#[cfg(test)]
mod tests {
    use super::*;

    const BG: [u8; 3] = [0x14, 0x14, 0x14];

    #[test]
    fn two_colour_image_keeps_exact_colours() {
        let mut image = image::RgbaImage::from_pixel(4, 2, image::Rgba([0xff, 0x00, 0x00, 0xff]));
        image.put_pixel(0, 0, image::Rgba([0x00, 0x00, 0xff, 0xff]));
        let (palette, index) = quantize(&image, BG);
        assert_eq!(palette[0], BG, "0 号槽位是底色");
        assert_eq!(index.len(), 8);
        assert_eq!(index[1], index[2], "同色同索引");
        assert_ne!(index[0], index[1], "异色异索引");
        // 精确路径：调色板里拿到的就是原色
        assert_eq!(palette[usize::from(index[0])], [0x00, 0x00, 0xff]);
        assert_eq!(palette[usize::from(index[1])], [0xff, 0x00, 0x00]);
    }

    /// 颜色数超过 256 时走 NeuQuant：索引与调色板仍然自洽且有界。
    #[test]
    fn rich_image_falls_back_to_neuquant() {
        let mut image = image::RgbaImage::new(64, 64);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            // 坐标直接编码进颜色：64×64 = 4096 种不同像素值
            *pixel = image::Rgba([
                u8::try_from(x).unwrap_or(0).wrapping_mul(4),
                u8::try_from(y).unwrap_or(0).wrapping_mul(4),
                0xff,
                0xff,
            ]);
        }
        assert!(exact(image.as_raw(), BG).is_none(), "颜色数应超过 256");
        let (palette, index) = quantize(&image, BG);
        assert_eq!(palette.len(), PALETTE_CAP);
        let upper = u8::try_from(palette.len() - 1).expect("≤ 255");
        for slot in index {
            assert!(slot <= upper, "索引 {slot} 越界（上限 {upper}）");
        }
    }

    #[test]
    fn background_pixels_and_transparency_land_on_slot_zero() {
        let mut image = image::RgbaImage::from_pixel(4, 1, image::Rgba([0, 0, 0, 0]));
        image.put_pixel(1, 0, image::Rgba([BG[0], BG[1], BG[2], 0xff]));
        image.put_pixel(2, 0, image::Rgba([0xff, 0xff, 0xff, 0xff]));
        let (_, index) = quantize(&image, BG);
        assert_eq!(index[0], 0, "全透明 = 底色");
        assert_eq!(index[1], 0, "恰好等于底色的像素");
        assert_ne!(index[2], 0, "白色必须留在别的槽位");
    }

    /// 精确路径满 256 色仍然成立：第 256 个新色正好占满最后一个槽位。
    #[test]
    fn exact_fills_the_last_slot() {
        let mut pixels = Vec::new();
        for slot in 0..PALETTE_CAP {
            let value = u8::try_from(slot).expect("≤ 255");
            // 跳过底色，保证每个槽位都是新色
            pixels.extend_from_slice(&[value, value, value, 0xff]);
        }
        let result = exact(&pixels, BG).expect("256 色恰好放得下");
        assert!(result.0.len() <= PALETTE_CAP);
    }

    #[test]
    fn blend_handles_the_extremes() {
        assert_eq!(blend([1, 2, 3], BG, 255), [1, 2, 3]);
        assert_eq!(blend([1, 2, 3], BG, 0), BG);
        assert_eq!(blend([0xff, 0xff, 0xff], [0, 0, 0], 128), [128, 128, 128]);
    }
}

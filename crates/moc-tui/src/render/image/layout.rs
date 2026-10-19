//! 图片落位：源图像素 → 终端格数 + 像素框 + 框内居中位置。
//!
//! 两条不变量与图表共用：不放大（scale ≤ 1，放大只会糊），像素尺寸恒等于
//! 「格数 × 单元格尺寸」（终端按整数格放置，任何零头都会被重采样）。
//! 估高（预留几行）与贴图（生成多大的位图）走的是同一份公式，不会错位。
//!
//! 全程整数运算：`src` ≤ 8192、框 ≤ 720px，交叉相乘的最大值远在 `u64` 内，
//! 不需要浮点，也就没有「四舍五入到 0 像素」这类边界。

use crate::render::graphics::CellPixels;

/// 图片位图的像素上限：与图表共用 720px 的信息密度上限。
pub const MAX_IMAGE_PX: u32 = 720;

/// 一张图最多占用的行数：再高就把整屏挤掉了。
pub const MAX_IMAGE_ROWS: u16 = 24;

/// 一张图的版式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLayout {
    pub cols: u16,
    pub rows: u16,
    /// 位图像素尺寸（= 格数 × 单元格尺寸）。
    pub px: (u32, u32),
    /// 图片在像素框内的居中落位 `(x, y, w, h)`。
    pub fit: (u32, u32, u32, u32),
}

/// 按单元格尺寸把源图放进 `max_cols × `[`MAX_IMAGE_ROWS`] 的框；不放大。
///
/// 单元格太小（画不出一张可读的图）或源图尺寸为 0 时返回 `None`，
/// 调用方据此回落文本表示。
pub fn layout(src: (u32, u32), cell: CellPixels, max_cols: u16) -> Option<ImageLayout> {
    if !crate::render::graphics::cell_supported(cell) || src.0 == 0 || src.1 == 0 {
        return None;
    }
    // 720px 上限换算成列数：宽终端上图片不该被拉满整行。
    let cols_cap = u16::try_from(MAX_IMAGE_PX / cell.w)
        .unwrap_or(u16::MAX)
        .max(1);
    let max_cols = max_cols.min(cols_cap);
    if max_cols == 0 {
        return None;
    }
    let box_px = (
        u32::from(max_cols) * cell.w,
        u32::from(MAX_IMAGE_ROWS) * cell.h,
    );
    let (w, h) = scale_into(src, box_px);
    let cols = w.div_ceil(cell.w).min(u32::from(max_cols));
    let rows = h.div_ceil(cell.h).min(u32::from(MAX_IMAGE_ROWS));
    let px = (cols * cell.w, rows * cell.h);
    Some(ImageLayout {
        cols: u16::try_from(cols).unwrap_or(u16::MAX),
        rows: u16::try_from(rows).unwrap_or(u16::MAX),
        px,
        fit: fit(src, px),
    })
}

/// 源图在像素框内的居中落位 `(x, y, w, h)`：等比缩放到放得下，不放大。
pub fn fit(src: (u32, u32), px: (u32, u32)) -> (u32, u32, u32, u32) {
    if src.0 == 0 || src.1 == 0 || px.0 == 0 || px.1 == 0 {
        return (0, 0, 0, 0);
    }
    let (w, h) = scale_into(src, px);
    ((px.0 - w) / 2, (px.1 - h) / 2, w, h)
}

/// 等比缩放到 `box_px` 内，两者都不放大。
///
/// 用交叉相乘比较两个方向的收缩比例（`box.0/src.0` 与 `box.1/src.1`），
/// 避免浮点；上限乘积 8192 × 720 远小于 `u64` 上限。
fn scale_into(src: (u32, u32), box_px: (u32, u32)) -> (u32, u32) {
    if src.0 <= box_px.0 && src.1 <= box_px.1 {
        return src;
    }
    let width_limits =
        u64::from(box_px.0) * u64::from(src.1) <= u64::from(box_px.1) * u64::from(src.0);
    if width_limits {
        let h = u64::from(src.1) * u64::from(box_px.0) / u64::from(src.0);
        (box_px.0, u32::try_from(h).unwrap_or(1).max(1))
    } else {
        let w = u64::from(src.0) * u64::from(box_px.1) / u64::from(src.1);
        (u32::try_from(w).unwrap_or(1).max(1), box_px.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: CellPixels = CellPixels { w: 8, h: 18 };

    #[test]
    fn never_scales_up_small_images() {
        let layout = layout((16, 18), CELL, 80).expect("放得下");
        assert_eq!((layout.cols, layout.rows), (2, 1));
        assert_eq!(layout.px, (16, 18));
        assert_eq!(layout.fit, (0, 0, 16, 18), "原样居中，不放大");
    }

    #[test]
    fn caps_width_at_the_pixel_budget() {
        // 90 列 × 8px = 720px 正是上限；给 200 列也只用到 90 列
        let layout = layout((4000, 100), CELL, 200).expect("放得下");
        assert_eq!(layout.px.0, MAX_IMAGE_PX);
        assert_eq!(layout.cols, 90);
        assert_eq!(layout.rows, 1);
    }

    #[test]
    fn caps_height_at_the_row_budget() {
        let layout = layout((100, 4000), CELL, 80).expect("放得下");
        assert_eq!(layout.rows, MAX_IMAGE_ROWS);
        assert_eq!(
            layout.px,
            (
                u32::from(layout.cols) * CELL.w,
                u32::from(layout.rows) * CELL.h
            )
        );
    }

    #[test]
    fn rejects_degenerate_input() {
        assert!(layout((0, 10), CELL, 80).is_none());
        assert!(layout((10, 0), CELL, 80).is_none());
        assert!(layout((10, 10), CELL, 0).is_none(), "没有可用列");
        assert!(
            layout((10, 10), CellPixels { w: 1, h: 1 }, 80).is_none(),
            "单元格太小画不出可读的图"
        );
    }

    #[test]
    fn fit_centres_the_scaled_image() {
        // 源图 100×100 放进 40×30 框：按比例缩到 30×30，水平居中
        assert_eq!(fit((100, 100), (40, 30)), (5, 0, 30, 30));
        // 框比源图大：保持原尺寸并居中
        assert_eq!(fit((4, 2), (10, 10)), (3, 4, 4, 2));
        // 退化输入不产生除零
        assert_eq!(fit((0, 0), (10, 10)), (0, 0, 0, 0));
    }
}

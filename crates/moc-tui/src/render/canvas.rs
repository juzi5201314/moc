//! 像素画布：24 位 RGB 与调色板索引双平面。
//!
//! 图表的两种出口共用同一张画布：kitty / iTerm2 直接发 24 位 RGB 位图
//! （[`Canvas::rgb`]），sixel 只发调色板索引（[`Canvas::index`]）再由终端按
//! [`Canvas::palette`] 上色。两个平面只在 [`Canvas::set`]（及其内部的 `put`）
//! 里一起写：分开维护迟早会漂移。
//!
//! 颜色一律经 [`Canvas::color`] 登记，索引因此天然有界。上界推导：热力图
//! 每格文字都在该格混色上做 4 档 alpha 预混，最坏为
//! 8 个系列 × 5 档格色 ×（1 个格色 + 4 个文字混色）+ 背景/面板/文本/边框/
//! 强调 ≈ 210 色；取 256（sixel 调色板寄存器的常规上限）作为硬上界，超过
//! 即实现漂移，用 `debug_assert!` 在开发期暴露。
//!
//! 几何图元不做抗锯齿（[`Canvas::polyline`] 的折线尤其如此）：图片按终端格
//! 整数倍落位，整数格上的硬边比亚像素灰边更锐利。

/// 调色板容量上限：256，与 sixel 调色板寄存器数量一致。
///
/// 上界推导：热力图每格文字在其格色上做 4 档 AA 混色，最坏
/// 8 系列 × 5 档 ×（1 格色 + 4 混色）+ 基础色 ≈ 210，256 是这条推导的硬上界。
const MAX_COLORS: usize = 256;
/// 5-5-5 桶表槽位数：每通道高 5 位组成 15 位键。
const BUCKET_SLOTS: usize = 1 << 15;

/// 双平面像素画布。
pub struct Canvas {
    w: u32,
    h: u32,
    /// 24 位 RGB 平面，长度 `3 * w * h`，行优先。
    rgb: Vec<u8>,
    /// 调色板索引平面，长度 `w * h`，行优先；值即 [`Canvas::palette`] 下标。
    index: Vec<u8>,
    /// 出现过的颜色，索引即像素值；0 号槽位恒为底色（sixel 编码跳过 0 号槽位）。
    palette: Vec<[u8; 3]>,
    /// 5-5-5 量化桶 → 调色板索引；`None` 表示空桶。
    /// 索引 0..=255 全部合法，因此不能用 255 当空标记，用 `Option` 表达。
    bucket: Vec<Option<u8>>,
}

impl Canvas {
    /// 建一张 `w × h` 的画布，铺满底色 `bg`。
    pub fn new(w: u32, h: u32, bg: [u8; 3]) -> Self {
        // 画布尺寸来自终端格数 × 格像素（像素数学），u32 在 32/64 位 usize 上不截断。
        let pixels = w as usize * h as usize;
        let mut rgb = Vec::with_capacity(pixels * 3);
        for _ in 0..pixels {
            rgb.extend_from_slice(&bg);
        }
        let mut bucket = vec![None; BUCKET_SLOTS];
        bucket[bucket_index(bg)] = Some(0);
        Self {
            w,
            h,
            rgb,
            index: vec![0; pixels],
            palette: vec![bg],
            bucket,
        }
    }

    /// 画布宽（像素）。
    pub fn w(&self) -> u32 {
        self.w
    }

    /// 画布高（像素）。
    pub fn h(&self) -> u32 {
        self.h
    }

    /// 从已量化结果直接建画布：RGB 与索引平面一次写入。
    ///
    /// 通用内联图片走这里——NeuQuant 已经把整张图量化完，再逐个像素走
    /// [`Canvas::color`] 只会把同样的活干第二遍。`bucket` 留空：画布往后
    /// 只读（图片不再绘制新图元），需要时再懒回填。
    pub fn from_indexed(w: u32, h: u32, palette: Vec<[u8; 3]>, index: Vec<u8>) -> Self {
        let pixels = w as usize * h as usize;
        debug_assert!(
            !palette.is_empty() && palette.len() <= MAX_COLORS,
            "调色板项数越界：{}",
            palette.len()
        );
        debug_assert!(index.len() == pixels, "索引平面长度必须等于像素数");
        debug_assert!(
            index.iter().all(|slot| usize::from(*slot) < palette.len()),
            "索引越出调色板"
        );
        let mut rgb = Vec::with_capacity(pixels * 3);
        for slot in &index {
            let color = palette.get(usize::from(*slot)).copied().unwrap_or_default();
            rgb.extend_from_slice(&color);
        }
        Self {
            w,
            h,
            rgb,
            index,
            palette,
            bucket: vec![None; BUCKET_SLOTS],
        }
    }

    /// 登记颜色并返回调色板索引。
    ///
    /// 快路径是 5-5-5 桶：桶里存着同量化区间内最近一次使用的颜色，命中且
    /// 逐通道相等就直接返回。同桶不同色不视为命中，退化为对 `palette` 的
    /// 线性扫描（≤ 256 项）；线性命中后把桶回填成它，后续同色又是 O(1)。
    /// 仍未命中才追加新色。
    pub fn color(&mut self, rgb: [u8; 3]) -> u8 {
        let key = bucket_index(rgb);
        if let Some(slot) = self.bucket[key]
            && self.palette[usize::from(slot)] == rgb
        {
            return slot;
        }
        if let Some(i) = self.palette.iter().position(|c| *c == rgb) {
            self.bucket[key] = Some(i as u8); // 调色板最多 256 项，索引 < 256
            return i as u8; // 同上，不会截断
        }
        let idx = self.append(rgb);
        self.bucket[key] = Some(idx);
        idx
    }

    /// 追加新色；调色板已满时复用 0 号色。
    ///
    /// 满员只可能是绘制代码引入了预期之外的颜色（见模块文档的 ≈ 210 色上界），
    /// 属于实现 bug：开发期 `debug_assert!` 立刻失败，发布构建也绝不 panic。
    fn append(&mut self, rgb: [u8; 3]) -> u8 {
        debug_assert!(
            self.palette.len() < MAX_COLORS,
            "调色板超过 {MAX_COLORS} 色：图表配色方案已漂移"
        );
        if self.palette.len() >= MAX_COLORS {
            return 0;
        }
        self.palette.push(rgb);
        (self.palette.len() - 1) as u8 // 窄化：上界 255，不会截断
    }

    /// 写一个像素；越界坐标静默忽略。
    pub fn set(&mut self, x: u32, y: u32, color: u8) {
        if x >= self.w || y >= self.h {
            return;
        }
        self.put(x, y, color);
    }

    /// 无边界检查的写像素入口（调用方保证坐标在界内）。
    ///
    /// 所有批量图元都先把坐标夹紧再走这里，避免热循环里重复判定。
    fn put(&mut self, x: u32, y: u32, color: u8) {
        let i = self.offset(x, y);
        self.index[i] = color;
        let c = self.palette[usize::from(color)];
        self.rgb[i * 3..i * 3 + 3].copy_from_slice(&c);
    }

    /// 像素线性下标。
    fn offset(&self, x: u32, y: u32) -> usize {
        // 像素数学：u32 坐标在 32/64 位 usize 上不会截断。
        y as usize * self.w as usize + x as usize
    }

    /// 填矩形：`w`/`h` 是像素宽高（不是右下角坐标），越界部分按几何裁剪。
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u8) {
        if w == 0 || h == 0 || self.w == 0 || self.h == 0 {
            return;
        }
        let x0 = x.min(self.w);
        let y0 = y.min(self.h);
        let x1 = x.saturating_add(w).min(self.w);
        let y1 = y.saturating_add(h).min(self.h);
        for yy in y0..y1 {
            for xx in x0..x1 {
                self.put(xx, yy, color);
            }
        }
    }

    /// 画水平线（两端都含）；`x0`/`x1` 次序任意，越界部分按几何裁剪。
    pub fn hline(&mut self, x0: u32, x1: u32, y: u32, color: u8) {
        if y >= self.h || self.w == 0 {
            return;
        }
        let lo = x0.min(x1);
        if lo >= self.w {
            return;
        }
        let hi = x0.max(x1).min(self.w - 1);
        for x in lo..=hi {
            self.put(x, y, color);
        }
    }

    /// 画垂直线（两端都含）；`y0`/`y1` 次序任意，越界部分按几何裁剪。
    pub fn vline(&mut self, x: u32, y0: u32, y1: u32, color: u8) {
        if x >= self.w || self.h == 0 {
            return;
        }
        let lo = y0.min(y1);
        if lo >= self.h {
            return;
        }
        let hi = y0.max(y1).min(self.h - 1);
        for y in lo..=hi {
            self.put(x, y, color);
        }
    }

    /// 画矩形边框（`w`/`h` 含边框本身，越界按几何裁剪）。
    pub fn frame(&mut self, x: u32, y: u32, w: u32, h: u32, color: u8) {
        if w == 0 || h == 0 {
            return;
        }
        let x1 = x.saturating_add(w - 1);
        let y1 = y.saturating_add(h - 1);
        self.hline(x, x1, y, color);
        self.hline(x, x1, y1, color);
        self.vline(x, y, y1, color);
        self.vline(x1, y, y1, color);
    }

    /// 画实心圆点；半径不足 1 时落单个像素。
    ///
    /// 边缘不做全幅抗锯齿，只按覆盖程度分两档与已有底色预混（2/3、1/3），
    /// 折线与圆点交叠处因此不会出现黑白毛刺。
    pub fn disc(&mut self, cx: f64, cy: f64, r: f64, color: u8) {
        if r < 1.0 {
            self.plot_rounded(cx, cy, color);
            return;
        }
        let inner = r - 0.5;
        let outer = r + 0.5;
        // 外接方框外的浮点值先夹到非负，再饱和转 `u32`（负数→0，超界→上限），
        // 随后与画布尺寸取 min：裁剪语义，不是兜底。
        let x0 = (cx - outer).floor().max(0.0) as u32;
        let y0 = (cy - outer).floor().max(0.0) as u32;
        let x1 = ((cx + outer).ceil().max(0.0) as u32).min(self.w);
        let y1 = ((cy + outer).ceil().max(0.0) as u32).min(self.h);
        for y in y0..y1 {
            for x in x0..x1 {
                // 以像素中心参与距离判定，圆心落在像素边界时左右对称。
                let dx = f64::from(x) + 0.5 - cx;
                let dy = f64::from(y) + 0.5 - cy;
                let d2 = dx * dx + dy * dy;
                if d2 <= inner * inner {
                    self.put(x, y, color);
                } else if d2 <= r * r {
                    let blended = self.blend_at(x, y, color, 2, 3);
                    self.put(x, y, blended);
                } else if d2 <= outer * outer {
                    let blended = self.blend_at(x, y, color, 1, 3);
                    self.put(x, y, blended);
                }
            }
        }
    }

    /// 按四舍五入的整点落一个像素。
    fn plot_rounded(&mut self, x: f64, y: f64, color: u8) {
        if x < 0.0 || y < 0.0 {
            return;
        }
        // 浮点转整点的像素数学：负值已在上面排除，超界值饱和后被 `set` 裁掉。
        self.set(x.round() as u32, y.round() as u32, color);
    }

    /// 取像素现有颜色，按 `num/den` 把 `over` 叠上去，返回登记后的索引。
    fn blend_at(&mut self, x: u32, y: u32, over: u8, num: u32, den: u32) -> u8 {
        let under = {
            let i = self.offset(x, y);
            self.palette[usize::from(self.index[i])]
        };
        let over = self.palette[usize::from(over)];
        self.color(mix(under, over, num, den))
    }

    /// 画折线：逐段 Bresenham，`width` 取 1 或 2。
    ///
    /// 宽 2 时在每个采样点沿主方向补一个像素（水平为主补下一行、垂直为主补
    /// 右一列），不做抗锯齿——整数格下直线与折线的边缘更锐利。顶点坐标先
    /// 四舍五入到整像素，相邻段共用同一落点，不会出现断缝。
    pub fn polyline(&mut self, pts: &[(f64, f64)], color: u8, width: u32) {
        let width = width.clamp(1, 2);
        if pts.len() == 1 {
            self.plot_rounded(pts[0].0, pts[0].1, color);
            return;
        }
        for seg in pts.windows(2) {
            let (x0, y0) = round_point(seg[0]);
            let (x1, y1) = round_point(seg[1]);
            self.segment(x0, y0, x1, y1, color, width);
        }
    }

    /// Bresenham 直线；坐标用 `i64` 以免极端浮点值在取整时溢出。
    fn segment(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, color: u8, width: u32) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let horizontal = dx >= -dy;
        let mut err = dx + dy;
        let (mut x, mut y) = (x0, y0);
        loop {
            self.plot_i(x, y, color);
            if width == 2 {
                if horizontal {
                    self.plot_i(x, y + 1, color);
                } else {
                    self.plot_i(x + 1, y, color);
                }
            }
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// 整点落像素；负坐标（线段伸出画布）静默跳过。
    fn plot_i(&mut self, x: i64, y: i64, color: u8) {
        if x < 0 || y < 0 {
            return;
        }
        // 像素数学：非负 `i64` 转 `u32`，超出画布的部分由 `set` 裁剪。
        self.set(x as u32, y as u32, color);
    }

    /// 读调色板索引；越界一律返回 0（背景索引）。
    pub fn index_at(&self, x: u32, y: u32) -> u8 {
        if x >= self.w || y >= self.h {
            return 0;
        }
        self.index[self.offset(x, y)]
    }

    /// 24 位 RGB 平面，长度 `3 * w * h`，行优先。
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    /// 调色板索引平面，长度 `w * h`，行优先。
    pub fn index(&self) -> &[u8] {
        &self.index
    }

    /// 调色板；0 号恒为底色。
    pub fn palette(&self) -> &[[u8; 3]] {
        &self.palette
    }
}

/// 5-5-5 量化键：每通道取高 5 位。
///
/// 桶只是"精确同色"的快速缓存：同桶不同色不算命中，仍走线性扫描，量化误差
/// 不会把两个颜色混为一谈。
fn bucket_index(rgb: [u8; 3]) -> usize {
    // 15 位掩码运算，移位与低位丢弃都是刻意的量化。
    (usize::from(rgb[0]) >> 3) << 10 | (usize::from(rgb[1]) >> 3) << 5 | usize::from(rgb[2]) >> 3
}

/// 按 `num/den` 把 `over` 叠到 `under` 上（整数运算，四舍五入）。
fn mix(under: [u8; 3], over: [u8; 3], num: u32, den: u32) -> [u8; 3] {
    [
        mix_channel(under[0], over[0], num, den),
        mix_channel(under[1], over[1], num, den),
        mix_channel(under[2], over[2], num, den),
    ]
}

/// 单通道混合：`under + (over - under) * num / den`。
fn mix_channel(under: u8, over: u8, num: u32, den: u32) -> u8 {
    let u = u32::from(under);
    let o = u32::from(over);
    // 分子最大 255 * den，除以 den 后 ≤ 255，窄化不会截断。
    ((u * (den - num) + o * num + den / 2) / den) as u8
}

/// 顶点取整：浮点坐标四舍五入到整像素（像素数学，饱和转换）。
fn round_point(p: (f64, f64)) -> (i64, i64) {
    (p.0.round() as i64, p.1.round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 背景色（与主题无关，测试只关心索引与平面一致性）。
    const BG: [u8; 3] = [9, 12, 18];

    /// 桶命中与回填：同色两次拿到同一索引；同桶异色拿到新索引，且回访老色
    /// 仍是原索引（线性扫描 + 回填都不会复制颜色）。
    #[test]
    fn palette_bucket_hits_and_backfills() {
        let mut c = Canvas::new(4, 4, BG);
        let a = c.color([10, 20, 30]);
        let b = c.color([10, 20, 30]);
        assert_eq!(a, b, "同色必须命中同一调色板索引");
        // 31 >> 3 == 30 >> 3，三个通道的量化键完全相同 → 同桶异色。
        let neighbor = c.color([10, 20, 31]);
        assert_ne!(a, neighbor, "同桶异色不能被当成命中");
        assert_eq!(c.color([10, 20, 30]), a, "回访老色仍返回原索引");
        assert_eq!(c.palette().len(), 3, "背景 + 两个新色");
    }

    /// 矩形越界按几何裁剪：右下溢出只画可见部分，左上不动。
    #[test]
    fn fill_rect_clips_at_edges() {
        let mut c = Canvas::new(5, 4, BG);
        let ink = c.color([250, 250, 250]);
        c.fill_rect(3, 2, 10, 10, ink);
        let mut painted = 0;
        for y in 0..4 {
            for x in 0..5 {
                if c.index_at(x, y) == ink {
                    painted += 1;
                }
            }
        }
        assert_eq!(painted, 2 * 2, "只应画出右下角 2×2 的可见部分");
        assert_eq!(c.index_at(2, 2), 0, "左边一列保持背景");
        assert_eq!(c.index_at(3, 1), 0, "上一行保持背景");
    }

    /// 折线端点精确落位，中间不越界。
    #[test]
    fn polyline_lands_endpoints() {
        let mut c = Canvas::new(12, 6, BG);
        let ink = c.color([200, 40, 40]);
        c.polyline(&[(1.0, 1.0), (8.0, 1.0)], ink, 1);
        assert_eq!(c.index_at(1, 1), ink, "起点落色");
        assert_eq!(c.index_at(8, 1), ink, "终点落色");
        assert_eq!(c.index_at(4, 1), ink, "水平段连续");
        assert_eq!(c.index_at(0, 1), 0, "起点左侧是背景");
        assert_eq!(c.index_at(9, 1), 0, "终点右侧是背景");
        assert_eq!(c.index_at(4, 2), 0, "1px 线不占相邻行");
    }

    /// 圆心落色、外接方框内的像素数在圆面积附近，圆外不落色。
    #[test]
    fn disc_fills_center_and_stays_in_bounds() {
        let mut c = Canvas::new(24, 24, BG);
        let ink = c.color([80, 180, 120]);
        c.disc(12.0, 12.0, 3.0, ink);
        assert_eq!(c.index_at(12, 12), ink, "圆心必须是纯色");
        let mut painted = 0;
        for y in 6..18 {
            for x in 6..18 {
                if c.index_at(x, y) != 0 {
                    painted += 1;
                }
            }
        }
        // 半径 3 的真实面积 ≈ 28px²，边缘两档混色会多覆盖一圈。
        assert!(
            (20..=60).contains(&painted),
            "圆点覆盖像素数应在 20–60 之间，实际 {painted}"
        );
        assert_eq!(c.index_at(0, 0), 0, "远处的像素保持背景");
    }

    /// 半径不足 1 时按单像素处理（散点图的最小点）。
    #[test]
    fn disc_below_one_pixel_plots_single_dot() {
        let mut c = Canvas::new(8, 8, BG);
        let ink = c.color([240, 240, 0]);
        c.disc(4.2, 4.4, 0.6, ink);
        assert_eq!(c.index_at(4, 4), ink);
        let painted = c.index().iter().filter(|i| **i != 0).count();
        assert_eq!(painted, 1, "半径 < 1 只落一个像素");
    }

    /// 两个平面逐像素一致：`rgb[3i..3i+3]` 必须等于 `palette[index[i]]`。
    #[test]
    fn rgb_and_index_planes_agree() {
        let mut c = Canvas::new(40, 30, BG);
        let colors: Vec<u8> = (0..20u8)
            .map(|i| {
                c.color([
                    i.wrapping_mul(11),
                    i.wrapping_mul(5) + 3,
                    i.wrapping_mul(17),
                ])
            })
            .collect();
        // 固定种子的 LCG：测试要的是可复现的坐标序列，不引入 rand 依赖。
        let mut state = 0x5EED_1BAD_F00D_1234u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u32 // 取高位 31 位作为随机数
        };
        for _ in 0..100 {
            let x = next() % c.w();
            let y = next() % c.h();
            let ci = next() as usize % colors.len(); // 像素数学：u32 在 usize 上不截断
            c.set(x, y, colors[ci]);
        }
        for (i, idx) in c.index().iter().enumerate() {
            let expected = c.palette()[usize::from(*idx)];
            assert_eq!(
                &c.rgb()[i * 3..i * 3 + 3],
                &expected[..],
                "像素 {i} 的两平面不一致"
            );
        }
        assert!(c.palette().len() <= MAX_COLORS);
    }

    /// 调色板覆盖完整 u8 索引空间：256 项时最后一项索引 255 仍能正常存取
    /// （桶表用 `Option` 而非 255 哨兵，正是为了这个边界）。
    #[test]
    fn palette_uses_full_u8_index_range() {
        let mut c = Canvas::new(4, 4, BG);
        for i in 0..255u8 {
            assert_eq!(c.color([i, 0, 0]), i + 1, "底色占 0 号槽位，新色顺序追加");
        }
        assert_eq!(c.palette().len(), MAX_COLORS);
        assert_eq!(c.color([254, 0, 0]), 255, "最大索引仍可命中");
        c.set(1, 1, 255);
        assert_eq!(c.index_at(1, 1), 255);
        assert_eq!(&c.rgb()[15..18], &[254, 0, 0], "索引 255 的 RGB 正常落盘");
    }

    /// 所有图元在负方向/超大坐标下都不 panic（越界即裁剪）。
    #[test]
    fn primitives_clip_without_panicking() {
        let mut c = Canvas::new(6, 5, BG);
        let ink = c.color([255, 255, 255]);
        c.fill_rect(u32::MAX - 1, u32::MAX - 1, 10, 10, ink);
        c.hline(100, 200, 2, ink);
        c.vline(3, 100, 200, ink);
        c.frame(10, 10, 100, 100, ink);
        c.polyline(&[(-60.0, -60.0), (-20.0, -20.0)], ink, 2);
        c.disc(-100.0, -100.0, 4.0, ink);
        // 所有绘制都落在画布外，背景保持原样。
        assert!(c.index().iter().all(|i| *i == 0));
    }
}

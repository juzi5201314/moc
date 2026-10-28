//! 主题：语义化颜色槽位 + 按终端能力一次性量化。
//!
//! 设计照搬 grok-build 的结论：每个槽位在主题里以完整 RGB 定义，启动时按
//! 终端色彩能力（truecolor / 256 / 16 / 无色）统一量化成 ratatui `Color`，
//! 运行时零分支；diff 底色专门挑选量化到 256 后依然可辨的 RGB。

use std::sync::LazyLock;

use ratatui::style::Color;

/// 终端色彩能力，启动时探测一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColorLevel {
    /// `NO_COLOR` 或无终端：全部按默认前景色渲染。
    None,
    /// 16 色 ANSI。
    Basic,
    /// xterm 256 色。
    Ansi256,
    /// 24-bit 真彩色。
    TrueColor,
}

/// 探测终端色彩能力。
///
/// 只依赖环境变量（与 supports-color 的启发式一致），不做 DA1/DA2 往返；
/// tmux/ssh 会剥掉 `COLORTERM`，此时按 256 色保守处理。
pub fn detect_color_level() -> ColorLevel {
    #[cfg(test)]
    {
        return ColorLevel::TrueColor;
    }
    #[cfg(not(test))]
    {
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return ColorLevel::None;
        }
        if !std::env::var_os("TERM").is_some_and(|term| !term.is_empty() && term != "dumb") {
            // 无 TERM 的场景（管道、CI）谈不上交互式 UI，但仍可能被强制运行
            return ColorLevel::None;
        }
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return ColorLevel::TrueColor;
        }
        // 主流终端即使不设 COLORTERM 也支持 256 色
        let term = std::env::var("TERM").unwrap_or_default();
        if colorterm == "yes" || term.contains("256color") {
            return ColorLevel::Ansi256;
        }
        ColorLevel::Basic
    }
}

type Rgb = (u8, u8, u8);

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    (r, g, b)
}

/// 语义化颜色槽位。字段按用途分组；所有 UI 代码只允许引用这些槽位，
/// 禁止就地硬编码颜色。
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // 背景
    pub bg_base: Color,
    pub bg_dark: Color,
    pub bg_panel: Color,
    pub bg_highlight: Color,
    pub bg_hover: Color,
    pub bg_visual: Color,

    // 文本与灰阶
    pub text_primary: Color,
    pub text_secondary: Color,
    pub gray_dim: Color,
    pub gray: Color,
    pub gray_bright: Color,

    // 强调色
    pub accent_user: Color,
    pub accent_assistant: Color,
    pub accent_thinking: Color,
    pub accent_tool: Color,
    pub accent_error: Color,
    pub accent_success: Color,
    pub accent_running: Color,
    pub accent_warning: Color,
    pub accent_model: Color,

    // 边框
    pub border: Color,
    pub border_subtle: Color,
    pub border_active: Color,
    // diff
    pub diff_delete_bg: Color,
    pub diff_delete_fg: Color,
    pub diff_insert_bg: Color,
    pub diff_insert_fg: Color,
    pub diff_gutter_fg: Color,

    // markdown / 内容
    pub code_bg: Color,
    pub link: Color,
    pub path: Color,
    pub command: Color,

    // 滚动条
    pub scrollbar_fg: Color,
    pub scrollbar_bg: Color,

    // 上下文分块调色板：按分块顺序取用，相邻分块颜色可辨
    pub chart_1: Color,
    pub chart_2: Color,
    pub chart_3: Color,
    pub chart_4: Color,
    pub chart_5: Color,
    pub chart_6: Color,
    pub chart_7: Color,
    pub chart_8: Color,
}

/// 图表位图用的原始 RGB 调色板。
///
/// 图片永远是 24 位真彩色（终端不对像素做调色板量化），因此它**不能**从
/// [`Theme`] 里量化后的 `Color` 反推——256 色/16 色终端下 RGB 已经丢了。
/// 两个视图来自同一份字面量（[`define_theme!`] 一次展开两遍），改主题只需
/// 改一处，不会漂移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawPalette {
    /// 图表系列配色，与 [`Theme::chart`] 同序。
    pub chart: [[u8; 3]; Theme::CHART_LEN],
    /// 画布底色（与内容区底色一致，图片边界隐形）。
    pub bg_base: [u8; 3],
    /// 面板底色（进度轨道、热力图空格）。
    pub bg_panel: [u8; 3],
    pub text_primary: [u8; 3],
    pub text_secondary: [u8; 3],
    pub gray_dim: [u8; 3],
    pub border_subtle: [u8; 3],
    pub accent_success: [u8; 3],
    pub accent_warning: [u8; 3],
    pub accent_error: [u8; 3],
}

impl RawPalette {
    /// 全零初始值：`define_theme!` 展开时按字段名逐项填充。
    const fn empty() -> Self {
        Self {
            chart: [[0; 3]; Theme::CHART_LEN],
            bg_base: [0; 3],
            bg_panel: [0; 3],
            text_primary: [0; 3],
            text_secondary: [0; 3],
            gray_dim: [0; 3],
            border_subtle: [0; 3],
            accent_success: [0; 3],
            accent_warning: [0; 3],
            accent_error: [0; 3],
        }
    }
}

macro_rules! define_theme {
    (
        $( $field:ident : ($r:expr, $g:expr, $b:expr) , )+
    ) => {
        impl Theme {
            /// GrokNight 风格深色主题的原始调色板，逐槽位按色彩能力量化。
            pub fn resolved(level: ColorLevel) -> Theme {
                Theme {
                    $( $field: rgb_to_color(rgb($r, $g, $b), level), )+
                }
            }

            /// 图表位图用的真彩色调色板（与 `resolved` 同源字面量）。
            pub fn raw(&self) -> &'static RawPalette {
                &RAW_PALETTE
            }
        }

        /// 真彩色调色板：`define_theme!` 的字段表展开成同名赋值。
        const RAW_PALETTE: RawPalette = {
            let mut raw = RawPalette::empty();
            $( raw_palette_field!(raw, $field, ($r, $g, $b)); )+
            raw
        };
    };
}

/// 把 `define_theme!` 里的一个槽位写进 [`RawPalette`]：图片用不到的槽位直接丢弃。
macro_rules! raw_palette_field {
    ($raw:ident, bg_base, ($r:expr, $g:expr, $b:expr)) => {
        $raw.bg_base = [$r, $g, $b];
    };
    ($raw:ident, bg_panel, ($r:expr, $g:expr, $b:expr)) => {
        $raw.bg_panel = [$r, $g, $b];
    };
    ($raw:ident, text_primary, ($r:expr, $g:expr, $b:expr)) => {
        $raw.text_primary = [$r, $g, $b];
    };
    ($raw:ident, text_secondary, ($r:expr, $g:expr, $b:expr)) => {
        $raw.text_secondary = [$r, $g, $b];
    };
    ($raw:ident, gray_dim, ($r:expr, $g:expr, $b:expr)) => {
        $raw.gray_dim = [$r, $g, $b];
    };
    ($raw:ident, border_subtle, ($r:expr, $g:expr, $b:expr)) => {
        $raw.border_subtle = [$r, $g, $b];
    };
    ($raw:ident, accent_success, ($r:expr, $g:expr, $b:expr)) => {
        $raw.accent_success = [$r, $g, $b];
    };
    ($raw:ident, accent_warning, ($r:expr, $g:expr, $b:expr)) => {
        $raw.accent_warning = [$r, $g, $b];
    };
    ($raw:ident, accent_error, ($r:expr, $g:expr, $b:expr)) => {
        $raw.accent_error = [$r, $g, $b];
    };
    ($raw:ident, chart_1, ($r:expr, $g:expr, $b:expr)) => {
        $raw.chart[0] = [$r, $g, $b];
    };
    ($raw:ident, chart_2, ($r:expr, $g:expr, $b:expr)) => {
        $raw.chart[1] = [$r, $g, $b];
    };
    ($raw:ident, chart_3, ($r:expr, $g:expr, $b:expr)) => {
        $raw.chart[2] = [$r, $g, $b];
    };
    ($raw:ident, chart_4, ($r:expr, $g:expr, $b:expr)) => {
        $raw.chart[3] = [$r, $g, $b];
    };
    ($raw:ident, chart_5, ($r:expr, $g:expr, $b:expr)) => {
        $raw.chart[4] = [$r, $g, $b];
    };
    ($raw:ident, chart_6, ($r:expr, $g:expr, $b:expr)) => {
        $raw.chart[5] = [$r, $g, $b];
    };
    ($raw:ident, chart_7, ($r:expr, $g:expr, $b:expr)) => {
        $raw.chart[6] = [$r, $g, $b];
    };
    ($raw:ident, chart_8, ($r:expr, $g:expr, $b:expr)) => {
        $raw.chart[7] = [$r, $g, $b];
    };
    ($raw:ident, $other:ident, ($r:expr, $g:expr, $b:expr)) => {
        let _ = ($r, $g, $b);
    };
}

define_theme! {
    // 背景阶梯：终端最深 → 面板 → 高亮
    bg_base:            (0x14, 0x14, 0x14),
    bg_dark:            (0x0c, 0x0c, 0x0c),
    bg_panel:           (0x11, 0x11, 0x11),
    bg_highlight:       (0x24, 0x24, 0x24),
    bg_hover:           (0x1c, 0x1c, 0x1c),
    bg_visual:          (0x2a, 0x2a, 0x30),
    // 文本与灰阶
    text_primary:       (0xe1, 0xe1, 0xe1),
    text_secondary:     (0xc8, 0xc8, 0xc8),
    gray_dim:           (0x54, 0x54, 0x5a),
    gray:               (0x78, 0x78, 0x82),
    gray_bright:        (0x9e, 0x9e, 0xaa),
    // TokyoNight 系强调色
    accent_user:        (0x7a, 0xa2, 0xf7),
    accent_assistant:   (0xbb, 0x9a, 0xf7),
    accent_thinking:    (0x73, 0x7d, 0x94),
    accent_tool:        (0x7d, 0xcf, 0xff),
    accent_error:       (0xf7, 0x76, 0x8e),
    accent_success:     (0x9e, 0xce, 0x6a),
    accent_running:     (0xe0, 0xaf, 0x68),
    accent_warning:     (0xff, 0x9e, 0x64),
    accent_model:       (0x7a, 0xa2, 0xf7),
    // 边框：内容边框更克制，聚焦态明显更亮。
    border:             (0x32, 0x32, 0x37),
    border_subtle:      (0x2a, 0x2a, 0x2e),
    border_active:      (0x50, 0x50, 0x58),
    // diff 底色：量化到 256 后依然红是红、绿是绿
    diff_delete_bg:     (0x42, 0x0e, 0x14),
    diff_delete_fg:     (0xe0, 0x8c, 0x99),
    diff_insert_bg:     (0x06, 0x38, 0x06),
    diff_insert_fg:     (0x9e, 0xce, 0x6a),
    diff_gutter_fg:     (0x58, 0x58, 0x58),
    // 内容
    code_bg:            (0x18, 0x18, 0x1b),
    link:               (0x7a, 0xa6, 0xda),
    path:               (0x7d, 0xcf, 0xff),
    command:            (0x9e, 0xce, 0x6a),
    // 滚动条
    scrollbar_fg:       (0x3a, 0x3a, 0x3f),
    scrollbar_bg:       (0x11, 0x11, 0x11),
    // 上下文分块：按顺序取色，量化到 256 色后各分块仍互不相同
    chart_1:            (0x7a, 0xa2, 0xf7),
    chart_2:            (0xbb, 0x9a, 0xf7),
    chart_3:            (0x7d, 0xcf, 0xff),
    chart_4:            (0x9e, 0xce, 0x6a),
    chart_5:            (0xe0, 0xaf, 0x68),
    chart_6:            (0xf7, 0x76, 0x8e),
    chart_7:            (0x73, 0xda, 0xca),
    chart_8:            (0xff, 0x9e, 0x64),
}

impl Theme {
    /// 上下文分块调色板长度；分块数超过它时循环取色。
    pub const CHART_LEN: usize = 8;

    /// 按终端能力量化后的内置主题（进程内单例，惰性初始化）。
    pub fn global() -> &'static Theme {
        static THEME: LazyLock<Theme> = LazyLock::new(|| Theme::resolved(detect_color_level()));
        &THEME
    }

    /// 上下文分块的配色：第 `index` 块的颜色，超出调色板长度则循环。
    ///
    /// 256 色终端下各分块颜色互不相同（16 色终端会退化到近似色，
    /// 此时靠分块列表的顺序与色条位置区分）。
    pub fn chart(&self, index: usize) -> Color {
        match index % Self::CHART_LEN {
            0 => self.chart_1,
            1 => self.chart_2,
            2 => self.chart_3,
            3 => self.chart_4,
            4 => self.chart_5,
            5 => self.chart_6,
            6 => self.chart_7,
            _ => self.chart_8,
        }
    }
}

/// RGB → ratatui Color：按能力直接放行或最近色量化。
pub fn rgb_to_color(rgb: Rgb, level: ColorLevel) -> Color {
    match level {
        ColorLevel::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
        ColorLevel::Ansi256 => Color::Indexed(nearest_indexed(rgb)),
        ColorLevel::Basic => Color::Indexed(nearest_basic(rgb)),
        ColorLevel::None => Color::Reset,
    }
}

/// RGB → 最近 xterm-256 索引：6×6×6 立方与 24 级灰阶中取加权距离最小者。
fn nearest_indexed(rgb: Rgb) -> u8 {
    let (r, g, b) = (rgb.0 as i32, rgb.1 as i32, rgb.2 as i32);
    let cube_level = |c: i32| -> i32 {
        if c < 48 {
            0
        } else if c < 115 {
            1
        } else {
            ((c - 35) / 40).min(5)
        }
    };
    let cube_value = |level: i32| -> i32 { if level == 0 { 0 } else { 55 + level * 40 } };
    let (lr, lg, lb) = (cube_level(r), cube_level(g), cube_level(b));
    let (qr, qg, qb) = (cube_value(lr), cube_value(lg), cube_value(lb));
    let cube_distance = {
        let (dr, dg, db) = (r - qr, g - qg, b - qb);
        2 * dr * dr + 4 * dg * dg + 3 * db * db
    };
    let gray_level = (((r + g + b) / 3 - 8) / 10).clamp(0, 23);
    let gray_value = if gray_level == 0 {
        0
    } else {
        8 + 10 * gray_level
    };
    let gray_distance = {
        let (dr, dg, db) = (r - gray_value, g - gray_value, b - gray_value);
        2 * dr * dr + 4 * dg * dg + 3 * db * db
    };
    if cube_distance <= gray_distance {
        (16 + 36 * lr + 6 * lg + lb) as u8
    } else {
        (232 + gray_level) as u8
    }
}

/// RGB → 最近 ANSI-16 索引（xterm 标准基础色的近邻）。
fn nearest_basic(rgb: Rgb) -> u8 {
    const BASIC: [Rgb; 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let distance = |a: Rgb, b: Rgb| -> i32 {
        let (dr, dg, db) = (
            a.0 as i32 - b.0 as i32,
            a.1 as i32 - b.1 as i32,
            a.2 as i32 - b.2 as i32,
        );
        dr * dr + dg * dg + db * db
    };
    BASIC
        .iter()
        .enumerate()
        .min_by_key(|&(_, candidate)| distance(rgb, *candidate))
        .map(|(index, _)| index as u8)
        .unwrap_or(7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_keeps_diff_colors_distinct_in_256() {
        let delete = nearest_indexed(rgb(0x42, 0x0e, 0x14));
        let insert = nearest_indexed(rgb(0x06, 0x38, 0x06));
        assert_ne!(delete, insert, "diff 底色量化后必须可辨");
        assert!(delete >= 16, "红底不应量化为基础色");
    }

    /// 上下文分块调色板：量化到 256 色后各分块仍互不相同，且超出长度循环取色。
    #[test]
    fn chart_palette_stays_distinct_after_quantization() {
        let theme = Theme::resolved(ColorLevel::Ansi256);
        // 索引色最大 255，用位图去重：每个 u64 覆盖 64 个索引
        let mut seen = [0u64; 4];
        for index in 0..Theme::CHART_LEN {
            let Color::Indexed(code) = theme.chart(index) else {
                panic!("256 色终端应量化为索引色");
            };
            let code = usize::from(code);
            let bit = 1u64 << (code & 63);
            let slot = &mut seen[code >> 6];
            assert_eq!(*slot & bit, 0, "第 {index} 个分块配色与前面重复");
            *slot |= bit;
        }
        assert_eq!(
            theme.chart(Theme::CHART_LEN),
            theme.chart(0),
            "超出调色板循环"
        );
    }

    #[test]
    fn resolved_theme_has_no_reset_colors_unless_disabled() {
        let theme = Theme::resolved(ColorLevel::Ansi256);
        let debug = format!("{theme:?}");
        assert!(!debug.contains("Reset"), "256 色终端不应回落到默认色");
        let disabled = Theme::resolved(ColorLevel::None);
        assert_eq!(disabled.accent_user, Color::Reset);
    }

    #[test]
    fn grayscale_stepping_is_strictly_monotonic_in_truecolor_and_256() {
        let tc = Theme::resolved(ColorLevel::TrueColor);
        let extract_rgb = |color: Color| -> (u8, u8, u8) {
            match color {
                Color::Rgb(r, g, b) => (r, g, b),
                other => panic!("expected Color::Rgb, got {other:?}"),
            }
        };

        let dim = extract_rgb(tc.gray_dim);
        let gray = extract_rgb(tc.gray);
        let bright = extract_rgb(tc.gray_bright);
        let sec = extract_rgb(tc.text_secondary);
        let pri = extract_rgb(tc.text_primary);

        // 1. Channel-wise strict monotonicity: gray_dim < gray < gray_bright < text_secondary < text_primary
        assert!(
            dim.0 < gray.0 && gray.0 < bright.0 && bright.0 < sec.0 && sec.0 < pri.0,
            "R channel monotonic"
        );
        assert!(
            dim.1 < gray.1 && gray.1 < bright.1 && bright.1 < sec.1 && sec.1 < pri.1,
            "G channel monotonic"
        );
        assert!(
            dim.2 < gray.2 && gray.2 < bright.2 && bright.2 < sec.2 && sec.2 < pri.2,
            "B channel monotonic"
        );

        // 2. Perceptual luminance monotonicity (BT.709)
        let lum = |(r, g, b): (u8, u8, u8)| -> f64 {
            0.2126 * (r as f64) + 0.7152 * (g as f64) + 0.0722 * (b as f64)
        };
        assert!(lum(dim) < lum(gray), "gray_dim < gray luminance");
        assert!(lum(gray) < lum(bright), "gray < gray_bright luminance");
        assert!(
            lum(bright) < lum(sec),
            "gray_bright < text_secondary luminance"
        );
        assert!(
            lum(sec) < lum(pri),
            "text_secondary < text_primary luminance"
        );

        // 3. Ansi256 quantized stepping: strictly monotonic indexed colors
        let a256 = Theme::resolved(ColorLevel::Ansi256);
        let extract_idx = |color: Color| -> u8 {
            match color {
                Color::Indexed(idx) => idx,
                other => panic!("expected Color::Indexed, got {other:?}"),
            }
        };

        let idx_dim = extract_idx(a256.gray_dim);
        let idx_gray = extract_idx(a256.gray);
        let idx_bright = extract_idx(a256.gray_bright);
        let idx_sec = extract_idx(a256.text_secondary);
        let idx_pri = extract_idx(a256.text_primary);

        assert!(
            idx_dim < idx_gray
                && idx_gray < idx_bright
                && idx_bright < idx_sec
                && idx_sec < idx_pri,
            "Ansi256 grayscale levels must be strictly monotonic: {idx_dim} < {idx_gray} < {idx_bright} < {idx_sec} < {idx_pri}"
        );
        assert_eq!(idx_dim, 239);
        assert_eq!(idx_gray, 243);
        assert_eq!(idx_bright, 247);
        assert_eq!(idx_sec, 251);
        assert_eq!(idx_pri, 253);
    }

    #[test]
    fn accent_thinking_is_distinct_slate_hue_and_quantizes_distinctly() {
        let tc = Theme::resolved(ColorLevel::TrueColor);
        let Color::Rgb(r, g, b) = tc.accent_thinking else {
            panic!("expected Color::Rgb for accent_thinking in TrueColor");
        };

        // Slate tone: cool hue with prominent blue channel (b > g > r)
        assert_eq!((r, g, b), (0x73, 0x7d, 0x94));
        assert!(
            b > g && g > r,
            "accent_thinking should have slate/blue characteristic"
        );
        assert!(
            b.saturating_sub(r) >= 25,
            "accent_thinking should have noticeable chromatic divergence from flat gray"
        );

        // Distinct from grayscale and other accents
        assert_ne!(tc.accent_thinking, tc.gray_dim);
        assert_ne!(tc.accent_thinking, tc.gray);
        assert_ne!(tc.accent_thinking, tc.gray_bright);
        assert_ne!(tc.accent_thinking, tc.accent_user);
        assert_ne!(tc.accent_thinking, tc.accent_assistant);

        // In 256 colors: quantizes to 244 and remains distinct from surrounding gray slots
        let a256 = Theme::resolved(ColorLevel::Ansi256);
        let Color::Indexed(idx_thinking) = a256.accent_thinking else {
            panic!("expected Color::Indexed for accent_thinking in Ansi256");
        };
        assert_eq!(idx_thinking, 244);
        assert_ne!(idx_thinking, 239, "thinking must not collide with gray_dim");
        assert_ne!(idx_thinking, 243, "thinking must not collide with gray");
        assert_ne!(
            idx_thinking, 247,
            "thinking must not collide with gray_bright"
        );
    }
}

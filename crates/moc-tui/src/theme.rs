//! 主题：语义化颜色槽位 + 按终端能力一次性量化。
//!
//! 设计照搬 grok-build 的结论：每个槽位在主题里以完整 RGB 定义，启动时按
//! 终端色彩能力（truecolor / 256 / 16 / 无色）统一量化成 ratatui `Color`，
//! 运行时零分支；diff 底色专门挑选量化到 256 后依然可辨的 RGB。

use std::sync::OnceLock;

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
        }
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
    gray_dim:           (0x58, 0x58, 0x58),
    gray:               (0x6c, 0x6c, 0x6c),
    gray_bright:        (0x78, 0x78, 0x78),
    // TokyoNight 系强调色
    accent_user:        (0x7a, 0xa2, 0xf7),
    accent_assistant:   (0xbb, 0x9a, 0xf7),
    accent_thinking:    (0x58, 0x58, 0x58),
    accent_tool:        (0x7d, 0xcf, 0xff),
    accent_error:       (0xf7, 0x76, 0x8e),
    accent_success:     (0x9e, 0xce, 0x6a),
    accent_running:     (0xe0, 0xaf, 0x68),
    accent_warning:     (0xff, 0x9e, 0x64),
    accent_model:       (0x7a, 0xa2, 0xf7),
    // 边框：聚焦态明显更亮，让焦点位置一眼可辨
    border:             (0x32, 0x32, 0x37),
    border_active:      (0x50, 0x50, 0x58),
    // diff 底色：量化到 256 后依然红是红、绿是绿
    diff_delete_bg:     (0x42, 0x0e, 0x14),
    diff_delete_fg:     (0xe0, 0x8c, 0x99),
    diff_insert_bg:     (0x06, 0x38, 0x06),
    diff_insert_fg:     (0x9e, 0xce, 0x6a),
    diff_gutter_fg:     (0x58, 0x58, 0x58),
    // 内容
    code_bg:            (0x1c, 0x1c, 0x1c),
    link:               (0x7a, 0xa6, 0xda),
    path:               (0x7d, 0xcf, 0xff),
    command:            (0x9e, 0xce, 0x6a),
    // 滚动条
    scrollbar_fg:       (0x3a, 0x3a, 0x3f),
    scrollbar_bg:       (0x11, 0x11, 0x11),
}

impl Theme {
    /// 按终端能力量化后的内置主题（进程内单例，惰性初始化）。
    pub fn global() -> &'static Theme {
        static THEME: OnceLock<Theme> = OnceLock::new();
        THEME.get_or_init(|| Theme::resolved(detect_color_level()))
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
    let cube_value = |level: i32| -> i32 {
        if level == 0 {
            0
        } else {
            55 + level * 40
        }
    };
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
    fn truecolor_passthrough() {
        assert_eq!(
            rgb_to_color((0x7a, 0xa2, 0xf7), ColorLevel::TrueColor),
            Color::Rgb(0x7a, 0xa2, 0xf7)
        );
    }

    #[test]
    fn quantize_keeps_diff_colors_distinct_in_256() {
        let delete = nearest_indexed(rgb(0x42, 0x0e, 0x14));
        let insert = nearest_indexed(rgb(0x06, 0x38, 0x06));
        assert_ne!(delete, insert, "diff 底色量化后必须可辨");
        assert!(delete >= 16, "红底不应量化为基础色");
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
    fn gray_is_quantized_to_grayscale_ramp() {
        let gray = nearest_indexed(rgb(0x6c, 0x6c, 0x6c));
        assert!((232..=255).contains(&gray), "中性灰应落在 24 级灰阶段");
    }
}

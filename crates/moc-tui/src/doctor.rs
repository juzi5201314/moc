//! `/doctor`：把终端能力探测结果汇总成一条转写提示，附带修复建议。
//!
//! 探测全部在启动时一次性完成（图形协议、DEC 2026、键盘协议、色彩能力），
//! 这里只做展示，不再向终端发查询。

use crate::app::App;
use crate::render::graphics::GraphicsProto;
use crate::theme::ColorLevel;

/// 汇总当前终端能力：每行一项，可行的修复建议随附在对应行内。
pub fn report(app: &App) -> String {
    let mut lines = vec!["终端能力诊断".to_owned()];

    let under_tmux = std::env::var_os("TMUX").is_some();
    let env = |key: &str| std::env::var(key).unwrap_or_else(|_| "—".into());
    lines.push(format!(
        "环境：TERM={} TERM_PROGRAM={} COLORTERM={}{}",
        env("TERM"),
        env("TERM_PROGRAM"),
        env("COLORTERM"),
        if under_tmux { " tmux" } else { "" },
    ));

    lines.push(format!(
        "色彩：{}",
        match crate::theme::detect_color_level() {
            ColorLevel::TrueColor => "truecolor（24-bit）",
            ColorLevel::Ansi256 => "256 色",
            ColorLevel::Basic => "16 色（COLORTERM=truecolor 可启用真彩色）",
            ColorLevel::None => "无色彩支持（NO_COLOR 或 TERM 缺失）",
        }
    ));

    lines.push(if app.keyboard_enhanced {
        "键盘：kitty 消歧义协议已协商".to_owned()
    } else {
        "键盘：未协商增强协议（Ctrl+M / Ctrl+I 等组合会退化为 Enter / Tab）".to_owned()
    });

    lines.push(if app.sync_output {
        "同步输出：开启（DEC 2026，帧原子更新；仍撕裂可 MOC_FULL_REPAINT=1 排障）".to_owned()
    } else if under_tmux {
        "同步输出：关闭（tmux<3.7 / tmux-CC 不透传 2026，已降级；外层支持可 MOC_SYNC_OUTPUT=1 强制）"
            .to_owned()
    } else {
        "同步输出：关闭（终端未应答 DECRQM；慢连接下可能帧撕裂，确认支持可 MOC_SYNC_OUTPUT=1）"
            .to_owned()
    });

    match app.graphics {
        Some(support) => {
            lines.push(format!(
                "图形：{}（单元格 {}×{}px）",
                proto_name(support.proto),
                support.cell.w,
                support.cell.h
            ));
            if !crate::render::graphics::cell_supported(support.cell) {
                lines.push("  建议：单元格像素过小，图表与图片入口不启用".to_owned());
            }
        }
        None if under_tmux => {
            lines.push(
                "图形：无（tmux 吞应答已跳过探测；可 MOC_CHART_GRAPHICS=kitty|sixel|iterm2 强制）"
                    .to_owned(),
            );
        }
        None => {
            lines.push("图形：无（未探测到 kitty/sixel/iTerm2；图表按字符绘制）".to_owned());
        }
    }

    match crate::clipboard::local_channel() {
        Some(channel) => lines.push(format!("剪贴板：{channel} + OSC52（双通道并写）")),
        None => lines.push(
            "剪贴板：仅 OSC52（未找到 powershell.exe / wl-copy / xclip / pbcopy；建议安装 wl-clipboard 或 xclip）"
                .to_owned(),
        ),
    }

    lines.push("通知：OSC 9（终端不支持时静默忽略）".to_owned());

    let modes = app
        .sandbox_modes
        .iter()
        .map(|mode| mode.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!("沙盒：{}", if modes.is_empty() { "—" } else { &modes }));

    lines.join("\n")
}

fn proto_name(proto: GraphicsProto) -> &'static str {
    match proto {
        GraphicsProto::Kitty => "kitty",
        GraphicsProto::Sixel => "sixel",
        GraphicsProto::Iterm2 => "iTerm2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 报告覆盖全部诊断分区；具体取值随环境变，不锁死。
    #[test]
    fn report_covers_every_section() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(crate::theme::Theme::global(), events);
        let text = report(&app);
        for section in ["环境", "色彩", "键盘", "同步输出", "图形", "剪贴板", "通知", "沙盒"] {
            assert!(text.contains(section), "缺少分区「{section}」：{text}");
        }
    }
}

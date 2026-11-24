//! `/doctor`：把终端能力探测结果汇总成一条转写提示，附带修复建议。
//!
//! 探测全部在启动时一次性完成（图形协议、DEC 2026、键盘协议、色彩能力），
//! 这里只做展示，不再向终端发查询。

use crate::app::App;
use crate::render::graphics::GraphicsProto;
use crate::theme::ColorLevel;
use moc_i18n::t;

/// 汇总当前终端能力：每行一项，可行的修复建议随附在对应行内。
pub fn report(app: &App) -> String {
    let mut lines = vec![t!("tui.doctor.title").into_owned()];

    let under_tmux = std::env::var_os("TMUX").is_some();
    let env = |key: &str| std::env::var(key).unwrap_or_else(|_| "—".into());
    lines.push(
        t!(
            "tui.doctor.env",
            term = env("TERM"),
            term_program = env("TERM_PROGRAM"),
            colorterm = env("COLORTERM"),
            tmux = if under_tmux { " tmux" } else { "" }
        )
        .into_owned(),
    );

    lines.push(
        t!(
            "tui.doctor.color",
            level = match crate::theme::detect_color_level() {
                ColorLevel::TrueColor => t!("tui.doctor.color_true"),
                ColorLevel::Ansi256 => t!("tui.doctor.color_256"),
                ColorLevel::Basic => t!("tui.doctor.color_basic"),
                ColorLevel::None => t!("tui.doctor.color_none"),
            }
            .as_ref()
        )
        .into_owned(),
    );

    lines.push(
        if app.keyboard_enhanced {
            t!("tui.doctor.kbd_kitty")
        } else {
            t!("tui.doctor.kbd_plain")
        }
        .into_owned(),
    );

    lines.push(
        if app.sync_output {
            t!("tui.doctor.sync_on")
        } else if under_tmux {
            t!("tui.doctor.sync_tmux")
        } else {
            t!("tui.doctor.sync_off")
        }
        .into_owned(),
    );

    match app.graphics {
        Some(support) => {
            lines.push(
                t!(
                    "tui.doctor.graphics_fmt",
                    proto = proto_name(support.proto),
                    w = support.cell.w,
                    h = support.cell.h
                )
                .into_owned(),
            );
            if !crate::render::graphics::cell_supported(support.cell) {
                lines.push(t!("tui.doctor.graphics_small").into_owned());
            }
        }
        None if under_tmux => {
            lines.push(t!("tui.doctor.graphics_tmux").into_owned());
        }
        None => {
            lines.push(t!("tui.doctor.graphics_none").into_owned());
        }
    }

    match crate::clipboard::local_channel() {
        Some(channel) => lines.push(t!("tui.doctor.clipboard_fmt", channel = channel).into_owned()),
        None => lines.push(t!("tui.doctor.clipboard_osc52").into_owned()),
    }

    lines.push(t!("tui.doctor.notify").into_owned());

    let modes = app
        .sandbox_modes
        .iter()
        .map(|mode| mode.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(
        t!(
            "tui.doctor.sandbox_fmt",
            modes = if modes.is_empty() { "—" } else { &modes }
        )
        .into_owned(),
    );

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
        moc_i18n::set_locale("zh");
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(crate::theme::Theme::global(), events);
        let text = report(&app);
        for section in [
            "Environment",
            "Color",
            "Keyboard",
            "Sync output",
            "Graphics",
            "Clipboard",
            "Notifications",
            "Sandbox",
        ] {
            assert!(text.contains(section), "缺少分区「{section}」：{text}");
        }
    }
}

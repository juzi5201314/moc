//! 终端生命周期：raw mode、备用屏、键盘协议协商、鼠标捕获与恢复。
//!
//! 全部能力集中在一个 guard 里，任何退出路径（正常、错误、panic）都走
//! 同一份恢复序列；顺序与启用相反，保证终端状态无残留。

use anyhow::Context;
use crossterm::cursor::MoveTo;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::Write;

/// 只申请「消歧义」这一位：它让 Ctrl+M、Ctrl+I 这类与旧式控制码撞车的
/// 组合键按 `CSI u` 上报，不再退化成 Enter、Tab。更激进的位（全部按键转义、
/// 上报事件类型、关联文本）经 ConPTY／WSL 中继时会连消歧义一起失效，
/// kitty 规范的 Quickstart 同样只建议这一位。
const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;

/// 已进入交互模式的终端句柄；drop 时自动恢复。
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    restored: bool,
    /// kitty 键盘消歧义协议协商结果（`/doctor` 展示用）。
    keyboard_enhanced: bool,
    /// DEC 2026 同步输出开关；启动探测后由 `set_sync_output` 写入。
    sync_output: bool,
    /// 已写出的窗口标题：内容未变时不重发 OSC 序列。
    title: Option<String>,
}

impl TerminalGuard {
    /// 进入交互模式：raw mode + 备用屏 + 鼠标 + bracketed paste + 键盘协议。
    pub fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("启用 raw mode 失败")?;
        // 协商必须在输入线程启动前完成：crossterm 写 `CSI ? u` + `CSI c`
        // 并读取应答（终端不回话时最多等 2 秒），期间键入的按键仍留在
        // 事件队列里，不会被吞掉。
        let enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
        tracing::info!(enhanced, "键盘协议协商完成");
        let mut stdout = std::io::stdout();
        if let Err(error) = crossterm::execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange,
            PushKeyboardEnhancementFlags(KEYBOARD_FLAGS)
        ) {
            let _ = disable_raw_mode();
            return Err(error).context("切换备用屏/鼠标捕获失败");
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = std::io::stdout();
                let _ = crossterm::execute!(
                    stdout,
                    PopKeyboardEnhancementFlags,
                    DisableFocusChange,
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    LeaveAlternateScreen
                );
                let _ = disable_raw_mode();
                return Err(error).context("初始化终端缓冲失败");
            }
        };
        Ok(Self {
            terminal,
            restored: false,
            keyboard_enhanced: enhanced,
            sync_output: false,
            title: None,
        })
    }

    /// kitty 键盘协议协商结果：真 = 消歧义已生效（Ctrl+M/I 不再退化为 Enter/Tab）。
    pub fn keyboard_enhanced(&self) -> bool {
        self.keyboard_enhanced
    }

    /// 按启动探测结果开关 DEC 2026 同步输出包裹。
    pub fn set_sync_output(&mut self, enabled: bool) {
        self.sync_output = enabled;
    }

    /// 绘制一帧。启用同步输出时用 `?2026h`/`?2026l` 包住整帧文本更新：
    /// 终端在结束序列到达前不显示中间态，慢连接或大块 diff 下不再逐行撕裂。
    pub fn draw(
        &mut self,
        render: impl FnOnce(&mut ratatui::Frame),
    ) -> std::io::Result<ratatui::CompletedFrame<'_>> {
        // 序列写 stdout 的另一把句柄：`CompletedFrame` 借用 terminal 到返回之后，
        // 结束序列若走 backend_mut() 会与这次借用撞车。
        if self.sync_output {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(b"\x1b[?2026h");
            let _ = stdout.flush();
        }
        let result = self.terminal.draw(render);
        if self.sync_output {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(b"\x1b[?2026l");
            let _ = stdout.flush();
        }
        result
    }

    /// 写窗口标题（OSC 0）；与上次相同则跳过，控制字符剥掉。
    pub fn set_title(&mut self, title: &str) {
        let title: String = title
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        if self.title.as_deref() == Some(title.as_str()) {
            return;
        }
        let _ = write!(self.terminal.backend_mut(), "\x1b]0;{title}\x1b\\");
        let _ = self.terminal.backend_mut().flush();
        self.title = Some(title);
    }

    /// 强制整屏重绘：清屏并重建 ratatui 双缓冲。
    ///
    /// 不直接用 `Terminal::clear()`：它会先发 DSR 查询并同步读 stdin 来保存
    /// 光标位置，而输入线程也在读 stdin，可能吞键或阻塞 2 秒。这里直接发
    /// clear 序列，再换一个双缓冲全空的新 Terminal，下一帧必然写满全屏。
    pub fn reset_screen(&mut self) -> anyhow::Result<()> {
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, Clear(ClearType::All), MoveTo(0, 0)).context("清屏失败")?;
        self.terminal =
            Terminal::new(CrosstermBackend::new(stdout)).context("重建终端双缓冲失败")?;
        Ok(())
    }

    /// 恢复终端状态；幂等，供 Drop 与显式退出共用。
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        // 贴过的位图独立于文本层，必须在离开备用屏之前显式删掉
        crate::render::graphics::cleanup(&mut std::io::stdout());
        // 退出路径可能落在同步输出区域未关闭的帧中间：先补结束序列，
        // 再清掉窗口标题，不把状态残留进正常屏。
        let _ = self
            .terminal
            .backend_mut()
            .write_all(b"\x1b[?2026l\x1b]0;\x1b\\");
        let _ = self.terminal.backend_mut().flush();
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
            PopKeyboardEnhancementFlags,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

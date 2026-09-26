//! 终端生命周期：raw mode、备用屏、鼠标捕获、bracketed paste 与恢复。
//!
//! 全部能力集中在一个 guard 里，任何退出路径（正常、错误、panic）都走
//! 同一份恢复序列；顺序与启用相反，保证终端状态无残留。

use anyhow::Context;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen,disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// 已进入交互模式的终端句柄；drop 时自动恢复。
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TerminalGuard {
    /// 进入交互模式：raw mode + 备用屏 + 鼠标 + bracketed paste。
    pub fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("启用 raw mode 失败")?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .context("切换备用屏/鼠标捕获失败")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("初始化终端缓冲失败")?;
        Ok(Self { terminal })
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<std::io::Stdout>> {
        &mut self.terminal
    }

    /// 恢复终端状态；幂等，供 Drop 与显式退出共用。
    pub fn restore(&mut self) {
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
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

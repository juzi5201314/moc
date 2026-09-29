//! 终端生命周期：raw mode、备用屏、键盘协议协商、鼠标捕获与恢复。
//!
//! 全部能力集中在一个 guard 里，任何退出路径（正常、错误、panic）都走
//! 同一份恢复序列；顺序与启用相反，保证终端状态无残留。

use anyhow::Context;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// 只申请「消歧义」这一位：它让 Ctrl+M、Ctrl+I 这类与旧式控制码撞车的
/// 组合键按 `CSI u` 上报，不再退化成 Enter、Tab。更激进的位（全部按键转义、
/// 上报事件类型、关联文本）经 ConPTY／WSL 中继时会连消歧义一起失效，
/// kitty 规范的 Quickstart 同样只建议这一位。
const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;

/// 键盘协议协商结果：决定帮助页的提示，也决定退出时还原什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocol {
    /// 终端接受了渐进键盘增强，组合键可区分。
    Enhanced,
    /// 终端不支持：Ctrl+M、Ctrl+Enter 等会退化成 Enter。
    Legacy,
}

impl KeyboardProtocol {
    /// 帮助页脚注：让用户知道组合键在当前终端是否真的可用。
    pub fn description(self) -> &'static str {
        match self {
            Self::Enhanced => "键盘协议：已启用，Ctrl+M／Ctrl+Enter 等组合键可区分",
            Self::Legacy => "键盘协议：不可用，Ctrl+M 等同 Enter，切换模式请按 Alt+M",
        }
    }
}

/// 已进入交互模式的终端句柄；drop 时自动恢复。
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    keyboard_protocol: KeyboardProtocol,
    restored: bool,
}

impl TerminalGuard {
    /// 进入交互模式：raw mode + 备用屏 + 鼠标 + bracketed paste + 键盘协议。
    pub fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("启用 raw mode 失败")?;
        // 协商必须在输入线程启动前完成：crossterm 写 `CSI ? u` + `CSI c`
        // 并读取应答（终端不回话时最多等 2 秒），期间键入的按键仍留在
        // 事件队列里，不会被吞掉。
        let keyboard_protocol = match supports_keyboard_enhancement() {
            Ok(true) => KeyboardProtocol::Enhanced,
            Ok(false) | Err(_) => KeyboardProtocol::Legacy,
        };
        tracing::info!(
            protocol = ?keyboard_protocol,
            "键盘协议协商完成"
        );
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
            keyboard_protocol,
            restored: false,
        })
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<std::io::Stdout>> {
        &mut self.terminal
    }

    /// 本次会话的键盘协议协商结果。
    pub fn keyboard_protocol(&self) -> KeyboardProtocol {
        self.keyboard_protocol
    }

    /// 恢复终端状态；幂等，供 Drop 与显式退出共用。
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
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

//! Windows Terminal / ConPTY 的 win32-input-mode（`CSI ? 9001 h`）解码。
//!
//! WSL 经 ConPTY 运行且没有 kitty 键盘协议时，Shift+Enter 与 Enter 同为 `\r`，
//! Ctrl+Enter 退化成 `\n`。开启 win32-input-mode 后每次按键以
//! `CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _` 上报，修饰键状态完整保留。
//! 鼠标、焦点、粘贴与终端应答仍是普通 VT 序列：可能原样到达，也可能被
//! ConPTY 拆成逐字符的注入记录（`Vk = 0`），两种形态都在这里还原成 crossterm 事件。
//! 语义对照 oh-my-pi 的 `windows-input-mode.ts`（作者在 WT 1.24 实机验证过）。

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

const RIGHT_ALT: u32 = 0x01;
const LEFT_ALT: u32 = 0x02;
const RIGHT_CTRL: u32 = 0x04;
const LEFT_CTRL: u32 = 0x08;
const SHIFT: u32 = 0x10;

const VK_MENU: u32 = 18;
/// `SendInput` 注入的 Unicode 字符（IME 上屏走这条）。
const VK_PACKET: u32 = 231;
/// 单独按下不产生输入的修饰/锁定键。
const MODIFIER_VKS: [u32; 14] = [
    16, 17, 18, 20, 91, 92, 144, 145, 160, 161, 162, 163, 164, 165,
];

/// 被拆成注入记录的粘贴标记 `ESC [ 2 0 0 ~`（第 5 位为 `1` 时是结束标记）。
const PASTE_MARKER: [u32; 6] = [
    0x1b,
    b'[' as u32,
    b'2' as u32,
    b'0' as u32,
    b'0' as u32,
    b'~' as u32,
];
const PASTE_END: &str = "\x1b[201~";
/// 等待记录后续字节的上限：超过仍不成形就按原始字节处理。
const MAX_PENDING_RECORD: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    vk: u32,
    uc: u32,
    down: bool,
    state: u32,
    repeat: u32,
}

impl Record {
    fn parse(fields: &str) -> Option<Self> {
        let mut values = [0u32; 6];
        let mut count = 0;
        for field in fields.split(';') {
            if count == values.len() {
                return None;
            }
            values[count] = if field.is_empty() {
                0
            } else {
                field.parse().ok()?
            };
            count += 1;
        }
        Some(Self {
            vk: values[0],
            uc: values[2],
            down: values[3] == 1,
            state: values[4],
            repeat: if count < 6 { 1 } else { values[5].max(1) },
        })
    }

    /// 无修饰、不重复的按下：注入字符记录的形态。
    fn plain_press(&self) -> bool {
        self.down && self.state == 0 && self.repeat == 1
    }
}

/// 字节流 → crossterm 事件的增量解码器；跨 `read` 的半截输入留在内部缓冲。
#[derive(Default)]
pub struct Decoder {
    /// 尚未成形的原始字节：半条记录、半个 UTF-8 字符。
    bytes: Vec<u8>,
    /// 正在拼装的 VT 转义序列（来自原始字节或注入记录）。
    sequence: String,
    /// 括号粘贴进行中时累积的正文。
    paste: Option<String>,
    /// 疑似粘贴标记的注入记录；凑不齐时按普通按键重放。
    marker: Vec<Record>,
    high_surrogate: Option<u32>,
    events: Vec<Event>,
}

impl Decoder {
    /// 是否有等待后续输入的半截内容；读超时后应调用 [`Decoder::flush`]。
    pub fn pending(&self) -> bool {
        !self.bytes.is_empty() || !self.sequence.is_empty() || !self.marker.is_empty()
    }

    pub fn feed(&mut self, input: &[u8]) -> Vec<Event> {
        self.bytes.extend_from_slice(input);
        let bytes = std::mem::take(&mut self.bytes);
        let mut index = 0;
        while index < bytes.len() {
            let rest = &bytes[index..];
            if rest[0] == 0x1b {
                match record_len(rest) {
                    RecordScan::Complete(len) => {
                        // 只含数字与分号，必为 ASCII
                        let fields = std::str::from_utf8(&rest[2..len - 1]).unwrap_or_default();
                        if let Some(record) = Record::parse(fields) {
                            self.record(record);
                        }
                        index += len;
                        continue;
                    }
                    RecordScan::Partial => break,
                    RecordScan::NotRecord => {}
                }
            }
            match decode_char(rest) {
                CharScan::Char(character, len) => {
                    self.flush_marker();
                    self.feed_char(character);
                    index += len;
                }
                CharScan::Partial => break,
                CharScan::Invalid => index += 1,
            }
        }
        self.bytes = bytes[index..].to_vec();
        std::mem::take(&mut self.events)
    }

    /// 读超时：把半截内容按已到达的样子交出去（单独的 ESC 就是 Esc 键）。
    pub fn flush(&mut self) -> Vec<Event> {
        self.flush_marker();
        for byte in std::mem::take(&mut self.bytes) {
            self.feed_char(char::from(byte));
        }
        if self.sequence == "\x1b" {
            self.events.push(key(KeyCode::Esc, KeyModifiers::NONE));
        }
        self.sequence.clear();
        std::mem::take(&mut self.events)
    }

    fn record(&mut self, record: Record) {
        if !self.marker.is_empty() && !record.down {
            return;
        }
        if record.plain_press() {
            let position = self.marker.len();
            if record.uc == PASTE_MARKER[position]
                || (position == 4 && record.uc == u32::from(b'1'))
            {
                self.marker.push(record);
                if self.marker.len() == PASTE_MARKER.len() {
                    for held in std::mem::take(&mut self.marker) {
                        self.text_unit(held.uc);
                    }
                }
                return;
            }
        }
        self.flush_marker();
        if record.plain_press() && record.uc == PASTE_MARKER[0] {
            self.marker.push(record);
            return;
        }
        self.dispatch(record);
    }

    fn flush_marker(&mut self) {
        for held in std::mem::take(&mut self.marker) {
            self.dispatch(held);
        }
    }

    fn dispatch(&mut self, record: Record) {
        if !record.down {
            // Alt+小键盘输入的字符随 Alt 松开的记录送达
            if record.vk == VK_MENU && record.uc != 0 {
                self.text_unit(record.uc);
            }
            return;
        }
        if MODIFIER_VKS.contains(&record.vk) {
            return;
        }
        if record.vk == 0
            || record.vk == VK_PACKET
            || self.paste.is_some()
            || (0xD800..=0xDFFF).contains(&record.uc)
        {
            for _ in 0..record.repeat {
                self.text_unit(record.uc);
            }
            return;
        }
        if let Some(event) = key_event(record) {
            for _ in 0..record.repeat {
                self.events.push(Event::Key(event));
            }
        }
    }

    /// 记录里的 UTF-16 码元 → 字符（拼合代理对）。
    fn text_unit(&mut self, unit: u32) {
        let character = match unit {
            0 => return,
            0xD800..=0xDBFF => {
                self.high_surrogate = Some(unit);
                return;
            }
            0xDC00..=0xDFFF => {
                let Some(high) = self.high_surrogate.take() else {
                    return;
                };
                char::from_u32(0x10000 + ((high - 0xD800) << 10) + (unit - 0xDC00))
            }
            _ => char::from_u32(unit),
        };
        self.high_surrogate = None;
        if let Some(character) = character {
            self.feed_char(character);
        }
    }

    /// 文本层：识别粘贴、鼠标、焦点序列，其余字符按键入处理。
    fn feed_char(&mut self, character: char) {
        if let Some(paste) = &mut self.paste {
            paste.push(character);
            if paste.ends_with(PASTE_END) {
                paste.truncate(paste.len() - PASTE_END.len());
                let text = std::mem::take(paste);
                self.paste = None;
                self.events.push(Event::Paste(text));
            }
            return;
        }
        if self.sequence.is_empty() {
            if character == '\x1b' {
                self.sequence.push(character);
            } else {
                self.events.push(char_key(character, KeyModifiers::NONE));
            }
            return;
        }
        self.sequence.push(character);
        let mut chars = self.sequence.chars().skip(1);
        let introducer = chars.next().unwrap_or_default();
        match introducer {
            '[' => {
                let body = &self.sequence[2..];
                let Some(last) = body.chars().last() else {
                    return;
                };
                match u32::from(last) {
                    0x40..=0x7e => {
                        let body = body.to_owned();
                        self.sequence.clear();
                        self.csi(&body);
                    }
                    0x20..=0x3f if body.len() < MAX_PENDING_RECORD => {}
                    _ => self.sequence.clear(),
                }
            }
            // OSC / DCS / APC 等字符串序列（终端应答）：读到终止符后丢弃
            ']' | 'P' | '_' | '^' | 'X' => {
                if self.sequence.len() > 2
                    && (self.sequence.ends_with('\x07') || self.sequence.ends_with("\x1b\\"))
                {
                    self.sequence.clear();
                }
            }
            '\x1b' => {
                self.events.push(key(KeyCode::Esc, KeyModifiers::NONE));
                self.sequence.truncate(1);
            }
            other => {
                self.sequence.clear();
                self.events.push(char_key(other, KeyModifiers::ALT));
            }
        }
    }

    fn csi(&mut self, body: &str) {
        match body {
            "200~" => self.paste = Some(String::new()),
            "I" => self.events.push(Event::FocusGained),
            "O" => self.events.push(Event::FocusLost),
            _ => {
                if let Some(mouse) = body.strip_prefix('<').and_then(sgr_mouse) {
                    self.events.push(Event::Mouse(mouse));
                }
            }
        }
    }
}

enum RecordScan {
    Complete(usize),
    Partial,
    NotRecord,
}

/// `rest` 以 ESC 开头：判断是否是一条 `CSI 数字;… _` 记录。
fn record_len(rest: &[u8]) -> RecordScan {
    match rest.get(1) {
        None => return RecordScan::Partial,
        Some(b'[') => {}
        Some(_) => return RecordScan::NotRecord,
    }
    for (offset, byte) in rest.iter().enumerate().skip(2) {
        match byte {
            b'0'..=b'9' | b';' => {}
            b'_' => return RecordScan::Complete(offset + 1),
            _ => return RecordScan::NotRecord,
        }
    }
    if rest.len() <= MAX_PENDING_RECORD {
        RecordScan::Partial
    } else {
        RecordScan::NotRecord
    }
}

enum CharScan {
    Char(char, usize),
    Partial,
    Invalid,
}

fn decode_char(rest: &[u8]) -> CharScan {
    let len = match rest[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return CharScan::Invalid,
    };
    if rest.len() < len {
        return CharScan::Partial;
    }
    match std::str::from_utf8(&rest[..len]) {
        Ok(text) => CharScan::Char(text.chars().next().unwrap_or_default(), len),
        Err(_) => CharScan::Invalid,
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

/// 单个字符的按键语义，与 crossterm 在 raw mode 下解析传统字节的结果一致。
fn char_key(character: char, mut modifiers: KeyModifiers) -> Event {
    let code = match character {
        '\r' => KeyCode::Enter,
        '\t' => KeyCode::Tab,
        '\x7f' => KeyCode::Backspace,
        '\x1b' => KeyCode::Esc,
        '\0' => {
            modifiers |= KeyModifiers::CONTROL;
            KeyCode::Char(' ')
        }
        '\x01'..='\x1a' => {
            modifiers |= KeyModifiers::CONTROL;
            KeyCode::Char(char::from(character as u8 - 1 + b'a'))
        }
        '\x1c'..='\x1f' => {
            modifiers |= KeyModifiers::CONTROL;
            KeyCode::Char(char::from(character as u8 - 0x1c + b'4'))
        }
        _ => {
            if character.is_uppercase() {
                modifiers |= KeyModifiers::SHIFT;
            }
            KeyCode::Char(character)
        }
    };
    key(code, modifiers)
}

/// 真实按键记录 → 按键事件；不产生输入的组合返回 `None`。
fn key_event(record: Record) -> Option<KeyEvent> {
    let state = record.state;
    let shift = state & SHIFT != 0;
    let alt = state & (LEFT_ALT | RIGHT_ALT) != 0;
    let ctrl = state & (LEFT_CTRL | RIGHT_CTRL) != 0;
    let text = char::from_u32(record.uc).filter(|character| !character.is_control());
    // AltGr = 右 Alt + 左 Ctrl：它组合出的字符（如德语键盘的 @）是普通文本
    if state & RIGHT_ALT != 0
        && state & LEFT_CTRL != 0
        && let Some(character) = text
    {
        return event_key(char_key(character, KeyModifiers::NONE));
    }
    let mut modifiers = KeyModifiers::NONE;
    modifiers.set(KeyModifiers::SHIFT, shift);
    modifiers.set(KeyModifiers::ALT, alt);
    modifiers.set(KeyModifiers::CONTROL, ctrl);
    let code = match record.vk {
        13 => KeyCode::Enter,
        9 if shift && !ctrl && !alt => KeyCode::BackTab,
        9 => KeyCode::Tab,
        8 => KeyCode::Backspace,
        27 => KeyCode::Esc,
        33 => KeyCode::PageUp,
        34 => KeyCode::PageDown,
        35 => KeyCode::End,
        36 => KeyCode::Home,
        37 => KeyCode::Left,
        38 => KeyCode::Up,
        39 => KeyCode::Right,
        40 => KeyCode::Down,
        45 => KeyCode::Insert,
        46 => KeyCode::Delete,
        112..=123 => KeyCode::F((record.vk - 111) as u8),
        // Ctrl 下 Uc 是控制码：字母、数字、空格按虚拟键取字符
        65..=90 if ctrl || alt => KeyCode::Char(char::from(record.vk as u8 + 32)),
        48..=57 | 32 if ctrl || alt => KeyCode::Char(char::from(record.vk as u8)),
        _ if ctrl || alt => KeyCode::Char(text?),
        _ => return event_key(char_key(text?, KeyModifiers::NONE)),
    };
    Some(KeyEvent::new(code, modifiers))
}

fn event_key(event: Event) -> Option<KeyEvent> {
    match event {
        Event::Key(key) => Some(key),
        _ => None,
    }
}

/// SGR 鼠标 `CSI < Cb ; Cx ; Cy M|m`（`body` 已去掉 `<`）。
fn sgr_mouse(body: &str) -> Option<MouseEvent> {
    let pressed = body.ends_with('M');
    if !pressed && !body.ends_with('m') {
        return None;
    }
    let mut fields = body[..body.len() - 1].split(';').map(str::parse::<u16>);
    let (Some(Ok(code)), Some(Ok(column)), Some(Ok(row)), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return None;
    };
    let mut modifiers = KeyModifiers::NONE;
    modifiers.set(KeyModifiers::SHIFT, code & 4 != 0);
    modifiers.set(KeyModifiers::ALT, code & 8 != 0);
    modifiers.set(KeyModifiers::CONTROL, code & 16 != 0);
    let button = match code & 3 {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        _ => None,
    };
    let kind = if code & 64 != 0 {
        match code & 3 {
            0 => MouseEventKind::ScrollUp,
            1 => MouseEventKind::ScrollDown,
            2 => MouseEventKind::ScrollLeft,
            _ => MouseEventKind::ScrollRight,
        }
    } else if code & 32 != 0 {
        button.map_or(MouseEventKind::Moved, MouseEventKind::Drag)
    } else if pressed {
        MouseEventKind::Down(button?)
    } else {
        MouseEventKind::Up(button?)
    };
    Some(MouseEvent {
        kind,
        column: column.saturating_sub(1),
        row: row.saturating_sub(1),
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(vk: u32, uc: u32, state: u32) -> String {
        format!("\x1b[{vk};0;{uc};1;{state};1_\x1b[{vk};0;{uc};0;{state};1_")
    }

    fn keys(events: &[Event]) -> Vec<(KeyCode, KeyModifiers)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Key(key) => Some((key.code, key.modifiers)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn modified_enter_and_letters_keep_their_modifiers() {
        let mut decoder = Decoder::default();
        let input = [
            record(13, 13, 0),
            record(13, 13, SHIFT),
            record(13, 10, LEFT_CTRL),
            record(65, 1, LEFT_CTRL),
            record(65, u32::from('A'), SHIFT),
            record(9, 9, SHIFT),
            // 单独的 Shift 按下不产生输入
            "\x1b[16;42;0;1;16;1_".into(),
        ]
        .concat();
        assert_eq!(
            keys(&decoder.feed(input.as_bytes())),
            [
                (KeyCode::Enter, KeyModifiers::NONE),
                (KeyCode::Enter, KeyModifiers::SHIFT),
                (KeyCode::Enter, KeyModifiers::CONTROL),
                (KeyCode::Char('a'), KeyModifiers::CONTROL),
                (KeyCode::Char('A'), KeyModifiers::SHIFT),
                (KeyCode::BackTab, KeyModifiers::SHIFT),
            ]
        );
    }

    #[test]
    fn split_records_and_injected_text_reassemble() {
        let mut decoder = Decoder::default();
        let full = [record(13, 13, LEFT_CTRL), record(0, 0x4e2d, 0)].concat();
        let (head, tail) = full.as_bytes().split_at(5);
        assert!(decoder.feed(head).is_empty());
        assert_eq!(
            keys(&decoder.feed(tail)),
            [
                (KeyCode::Enter, KeyModifiers::CONTROL),
                (KeyCode::Char('中'), KeyModifiers::NONE),
            ]
        );
        // 代理对拆在两条注入记录里
        let emoji = format!("{}{}", record(0, 0xD83D, 0), record(0, 0xDE00, 0));
        assert_eq!(
            keys(&decoder.feed(emoji.as_bytes())),
            [(KeyCode::Char('😀'), KeyModifiers::NONE)]
        );
    }

    #[test]
    fn raw_and_injected_vt_sequences_become_mouse_focus_and_paste() {
        let mut decoder = Decoder::default();
        let events = decoder.feed(b"\x1b[<0;5;3M\x1b[I\x1b[200~a\r\nb\x1b[201~");
        assert_eq!(
            events,
            [
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 4,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                }),
                Event::FocusGained,
                Event::Paste("a\r\nb".into()),
            ]
        );
        // 鼠标序列逐字符注入、粘贴标记与正文拆成按键记录
        let injected: String = "\x1b[<64;1;1M"
            .chars()
            .map(|character| record(0, u32::from(character), 0))
            .collect();
        let paste: String = "\x1b[200~x\x1b[201~"
            .chars()
            .map(|character| record(65, u32::from(character), 0))
            .collect();
        let events = decoder.feed(format!("{injected}{paste}").as_bytes());
        assert_eq!(
            events,
            [
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                }),
                Event::Paste("x".into()),
            ]
        );
    }

    #[test]
    fn lone_escape_waits_for_flush() {
        let mut decoder = Decoder::default();
        assert!(decoder.feed(record(27, 27, 0).as_bytes()).is_empty());
        assert!(decoder.pending());
        assert_eq!(keys(&decoder.flush()), [(KeyCode::Esc, KeyModifiers::NONE)]);
        // Esc 后紧跟普通按键：不是粘贴标记，按序交出
        let input = [record(27, 27, 0), record(81, u32::from('q'), 0)].concat();
        assert_eq!(
            keys(&decoder.feed(input.as_bytes())),
            [
                (KeyCode::Esc, KeyModifiers::NONE),
                (KeyCode::Char('q'), KeyModifiers::NONE),
            ]
        );
    }
}

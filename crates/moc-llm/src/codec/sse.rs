//! SSE（Server-Sent Events）增量解码器。
//!
//! 这是流式的唯一手写字节解析层：三种协议共享它，协议层只解释 `data` 载荷。
//! 解析严格按帧边界切分，UTF-8 只在行完整时才解码，因此任意 TCP 分片位置都安全。

use std::borrow::Cow;

/// 一个完整的 SSE 帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// `event:` 字段。
    pub event: Option<String>,
    /// `data:` 字段拼接结果；多行以 `\n` 连接。
    pub data: String,
    /// `id:` 字段。
    pub id: Option<String>,
}

/// 增量解码器。
///
/// 缓冲区用「读游标 + 下一次追加时回收前缀」而不是每帧 `split_off`：流式热路径
/// 上每帧都要追加与消费一次，逐帧重建缓冲区会让分配与搬移随帧数线性放大。回收
/// 只在 `push` 时发生；已解析的行保存在字段状态中，未完成行保留字节与扫描进度。
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
    /// 已消费前缀的长度，也是当前行起点；完整行解析后推进，`push` 时整体丢弃。
    pos: usize,
    /// 当前行已扫描到的位置；分片未包含行尾时，下次只扫描新增字节。
    scan: usize,
    /// CR 已结束上一行；若下一字节为 LF，则只作为同一行尾的一部分跳过。
    skip_lf: bool,
    /// 只剥离流开头的一个 UTF-8 BOM；其三个字节可以分属不同网络块。
    bom_checked: bool,
    /// 当前帧的字段状态，跨分片保留，避免重复解析完整行及拼接数据。
    event: Option<String>,
    id: Option<String>,
    data: String,
    has_data: bool,
}

impl SseDecoder {
    /// 新建空解码器。
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            pos: 0,
            scan: 0,
            skip_lf: false,
            bom_checked: false,
            event: None,
            id: None,
            data: String::new(),
            has_data: false,
        }
    }

    /// 追加一段字节；分片位置任意。
    pub fn push(&mut self, chunk: &[u8]) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.scan -= self.pos;
            self.pos = 0;
        }
        self.buf.extend_from_slice(chunk);
        if !self.bom_checked {
            const BOM: &[u8] = b"\xef\xbb\xbf";
            if self.buf.len() >= BOM.len() {
                if self.buf.starts_with(BOM) {
                    self.buf.drain(..BOM.len());
                }
                self.bom_checked = true;
            } else if !BOM.starts_with(&self.buf) {
                self.bom_checked = true;
            }
        }
    }

    /// 取出下一个完整帧；不完整时返回 `None`，保留字段状态与未完成行。
    pub fn next_event(&mut self) -> Option<SseEvent> {
        // BOM 尚未收齐时不能推进扫描位置，否则剥离 BOM 后偏移会失效。
        if !self.bom_checked {
            return None;
        }

        loop {
            if self.skip_lf {
                let &byte = self.buf.get(self.pos)?;
                if byte == b'\n' {
                    self.pos += 1;
                }
                self.scan = self.pos;
                self.skip_lf = false;
            }
            let Some(offset) = self.buf[self.scan..]
                .iter()
                .position(|byte| matches!(byte, b'\r' | b'\n'))
            else {
                self.scan = self.buf.len();
                return None;
            };
            let newline = self.scan + offset;
            let line = &self.buf[self.pos..newline];
            self.pos = newline + 1;
            self.scan = self.pos;
            // 单独 CR 立即结束一行，不等待后续 LF；跨块的 CRLF 也只算一次行尾。
            self.skip_lf = self.buf[newline] == b'\r';

            if line.is_empty() {
                if !self.has_data {
                    self.event = None;
                    self.id = None;
                    continue;
                }
                self.has_data = false;
                return Some(SseEvent {
                    event: self.event.take(),
                    data: std::mem::take(&mut self.data),
                    id: self.id.take(),
                });
            }
            // 注释行（`:` 开头）不构成事件字段，心跳与代理注入的注释都靠它识别。
            if line[0] == b':' {
                continue;
            }

            match split_field(line) {
                (b"event", value) => self.event = Some(value.into()),
                (b"id", value) if !value.contains('\0') => self.id = Some(value.into()),
                (b"data", value) => {
                    if self.has_data {
                        self.data.push('\n');
                    }
                    self.has_data = true;
                    self.data.push_str(&value);
                }
                // `retry:`、注释行（`:` 开头）等一律忽略。
                _ => {}
            }
        }
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// 与 `str::split_once(':')` 等价的字节版本，并按规范剥掉紧随冒号的单个空格。
///
/// 非 UTF-8 字节按损失转换处理：上游偶尔会混入二进制噪声，不值得为此中断整条流。
fn split_field(line: &[u8]) -> (&[u8], Cow<'_, str>) {
    match line.iter().position(|&byte| byte == b':') {
        Some(colon) => {
            let mut value = &line[colon + 1..];
            if value.first() == Some(&b' ') {
                value = &value[1..];
            }
            (&line[..colon], String::from_utf8_lossy(value))
        }
        None => (line, Cow::Borrowed("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(decoder: &mut SseDecoder) -> Vec<SseEvent> {
        let mut events = Vec::new();
        while let Some(event) = decoder.next_event() {
            events.push(event);
        }
        events
    }

    fn data_event(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.into(),
            id: None,
        }
    }

    #[test]
    fn single_frame() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: {\"a\":1}\n\n");
        assert_eq!(drain(&mut decoder), vec![data_event("{\"a\":1}")]);
        // 消费后再取仍为空。
        assert_eq!(decoder.next_event(), None);
    }

    #[test]
    fn multiline_data_is_joined_with_newline() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: first\ndata: second\ndata: third\n\n");
        assert_eq!(
            drain(&mut decoder),
            vec![data_event("first\nsecond\nthird")]
        );
    }

    #[test]
    fn event_and_id_fields_are_captured() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"event: message_start\nid: 42\ndata: payload\n\n");
        assert_eq!(
            drain(&mut decoder),
            vec![SseEvent {
                event: Some("message_start".into()),
                data: "payload".into(),
                id: Some("42".into()),
            }]
        );
    }

    #[test]
    fn accepts_bom_and_every_standard_line_ending_at_any_split() {
        for ending in ["\n", "\r", "\r\n"] {
            let wire = format!("\u{feff}data: 北京{ending}{ending}data: next{ending}{ending}");
            for split in 0..=wire.len() {
                let mut decoder = SseDecoder::new();
                decoder.push(&wire.as_bytes()[..split]);
                let mut events = drain(&mut decoder);
                decoder.push(&wire.as_bytes()[split..]);
                events.extend(drain(&mut decoder));
                assert_eq!(
                    events,
                    vec![data_event("北京"), data_event("next")],
                    "{ending:?}, split={split}"
                );
            }
        }
    }

    #[test]
    fn empty_data_lines_preserve_leading_and_trailing_newlines() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data:\ndata: middle\ndata:\n\n");
        assert_eq!(drain(&mut decoder), vec![data_event("\nmiddle\n")]);
    }

    #[test]
    fn trailing_incomplete_frame_is_dropped() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: complete\n\ndata: partial");
        assert_eq!(drain(&mut decoder), vec![data_event("complete")]);
        // EOF 之后未以空行结束的帧永远不产出。
        assert_eq!(decoder.next_event(), None);

        // 补上结尾后照常产出，证明未消费字节被保留。
        decoder.push(b"\n\n");
        assert_eq!(drain(&mut decoder), vec![data_event("partial")]);
    }

    #[test]
    fn large_frame_in_small_chunks_preserves_partial_lines_and_fields() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"\xef\xbb\xbf: heartbeat\r\nevent: message\r\nid: 42\r\n");
        let line = "北京".repeat(512 * 1024);

        // 两条长行共 6 MiB，同时覆盖未完成行扫描和已完成字段的跨块保留。
        for _ in 0..2 {
            decoder.push(b"data: ");
            for chunk in line.as_bytes().chunks(1024) {
                decoder.push(chunk);
                assert_eq!(decoder.next_event(), None);
            }
            decoder.push(b"\r");
            assert_eq!(decoder.next_event(), None);
            decoder.push(b"\n");
            assert_eq!(decoder.next_event(), None);
        }

        // 空行的 CR 立即产出帧；稍后到达的 LF 不应影响下一帧。
        decoder.push(b"\r");
        assert_eq!(
            decoder.next_event(),
            Some(SseEvent {
                event: Some("message".into()),
                data: format!("{line}\n{line}"),
                id: Some("42".into()),
            })
        );
        assert_eq!(decoder.next_event(), None);
        decoder.push(b"\ndata: next\r\n\r\n");
        assert_eq!(drain(&mut decoder), vec![data_event("next")]);
    }
}

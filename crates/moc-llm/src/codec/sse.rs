//! SSE（Server-Sent Events）增量解码器。
//!
//! 这是流式的唯一手写字节解析层：三种协议共享它，协议层只解释 `data` 载荷。
//! 解析严格按帧边界切分，UTF-8 只在帧完整时才解码，因此任意 TCP 分片位置都安全。

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
/// 只在 `push` 时发生，此时未消费部分至多是一个未完成帧，搬移代价摊还到帧大小。
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
    /// 已消费前缀的长度；帧完成时推进，`push` 时整体丢弃。
    pos: usize,
}

impl SseDecoder {
    /// 新建空解码器。
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// 追加一段字节；分片位置任意。
    pub fn push(&mut self, chunk: &[u8]) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// 取出下一个完整帧；不完整时返回 `None` 且不消费任何字节。
    pub fn next_event(&mut self) -> Option<SseEvent> {
        let mut cursor = 0;
        // 无字段的空白行可以立即消费（例如心跳产生的空行），但它们之后的部分
        // 仍可能属于未完成帧，因此单独记录可消费边界。
        let mut consume_upto = 0;
        let mut event = None;
        let mut id = None;
        let mut data = String::new();
        let mut has_field = false;

        loop {
            let Some((line, next)) = self.line_at(cursor) else {
                self.pos += consume_upto;
                return None;
            };
            cursor = next;

            if line.is_empty() {
                if !has_field {
                    consume_upto = cursor;
                    continue;
                }
                self.pos += cursor;
                return Some(SseEvent { event, data, id });
            }
            // 注释行（`:` 开头）不构成事件字段，心跳与代理注入的注释都靠它识别。
            if line[0] == b':' {
                continue;
            }
            has_field = true;

            match split_field(line) {
                (b"event", value) => event = Some(value.into()),
                (b"id", value) => id = Some(value.into()),
                (b"data", value) => {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(&value);
                }
                // `retry:`、注释行（`:` 开头）等一律忽略。
                _ => {}
            }
        }
    }

    /// 读取从 `cursor` 开始的一整行；返回 `(行内容, 下一行起点)`。
    ///
    /// 行尾 `\r` 被剥除，因此 `\n` 与 `\r\n` 等价；没有行尾时返回 `None`。
    fn line_at(&self, cursor: usize) -> Option<(&[u8], usize)> {
        let rest = &self.buf[self.pos..];
        let newline = find_byte(rest, cursor, b'\n')?;
        let next = newline + 1;
        let mut line = &rest[cursor..newline];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        Some((line, next))
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

/// 在 `haystack[start..]` 中查找字节 `needle`，返回绝对下标。
fn find_byte(haystack: &[u8], start: usize, needle: u8) -> Option<usize> {
    haystack[start..]
        .iter()
        .position(|&byte| byte == needle)
        .map(|offset| start + offset)
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
}

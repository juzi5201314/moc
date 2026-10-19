//! 分帧层：把字节流切成一条条报文。
//!
//! 两种分帧，只影响字节边界，不影响报文内容：
//!
//! | 分帧 | 线上形状 | 用途 |
//! |---|---|---|
//! | [`Framing::ContentLength`] | `Content-Length: N\r\n\r\n<body>`（默认） | stdio / socket，与 LSP 一致的成熟形状 |
//! | [`Framing::Ndjson`] | 一行一条 JSON，以 `\n` 结束 | 手工调试、日志管道 |
//!
//! 分帧**不参与握手**：第一帧就必须两种编码一致，因此它是连接配置（
//! [`crate::ServerOptions::framing`] / [`crate::ClientBuilder::framing`]）而不是协商结果。
//!
//! 解码器与 `moc-llm` 的 `SseDecoder` 同构：缓冲区用「读游标 + `push` 时回收已消费
//! 前缀」，而不是逐帧重建缓冲区。长连接下每帧都要追加与消费一次，逐帧
//! `split_off` / `drain` 会让分配与搬移随帧数线性放大；回收只在 `push` 发生，此时
//! 未消费部分至多是一个未完成帧，代价摊还到帧大小，缓冲容量因此有界。

use crate::error::FrameError;

/// 分帧方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// `Content-Length` 头 + 空行（默认）。
    #[default]
    ContentLength,
    /// 换行分隔的 JSON（NDJSON）。
    Ndjson,
}

/// 协议层默认的报文上限。
pub const DEFAULT_MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// 头部块上限；超过即视为分帧错位。
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// 增量解码器。
pub struct FrameDecoder {
    framing: Framing,
    max_frame_bytes: usize,
    /// 已读字节；`pos` 之前是已消费前缀。
    buf: Vec<u8>,
    /// 读游标：未消费数据的起点。
    pos: usize,
    /// `Content-Length` 下已经解析出头部的报文体长度。
    ///
    /// 头部一旦解析完成就推进 `pos` 到报文体起点，因此这里只记长度，不记下标，
    /// `push` 回收前缀时无需任何修正。
    body_len: Option<usize>,
}

impl FrameDecoder {
    /// 新建空解码器。
    pub fn new(framing: Framing, max_frame_bytes: usize) -> Self {
        Self {
            framing,
            max_frame_bytes,
            buf: Vec::new(),
            pos: 0,
            body_len: None,
        }
    }

    /// 追加一段字节；分片位置任意。
    pub fn push(&mut self, chunk: &[u8]) {
        if self.pos > 0 {
            if self.pos == self.buf.len() {
                self.buf.clear();
            } else {
                self.buf.copy_within(self.pos.., 0);
                let remaining = self.buf.len() - self.pos;
                self.buf.truncate(remaining);
            }
            self.pos = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// 取出下一帧的报文体。
    ///
    /// 返回 `Ok(None)` 表示字节还不够，需要继续 `push`；返回 `Err` 表示分帧错位，
    /// 调用方必须关闭连接（此状态不可恢复）。
    pub fn next_frame(&mut self) -> Result<Option<&[u8]>, FrameError> {
        match self.framing {
            Framing::ContentLength => self.next_content_length_frame(),
            Framing::Ndjson => self.next_ndjson_frame(),
        }
    }

    /// 未消费字节数（含头部与半截报文体）。
    #[cfg(test)]
    fn buffered_len(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn next_content_length_frame(&mut self) -> Result<Option<&[u8]>, FrameError> {
        if self.body_len.is_none() {
            match self.parse_headers()? {
                Some((body_start, len)) => {
                    self.pos = body_start;
                    self.body_len = Some(len);
                }
                None => return Ok(None),
            }
        }
        let Some(len) = self.body_len else {
            return Ok(None);
        };
        if self.buf.len() - self.pos < len {
            return Ok(None);
        }
        let start = self.pos;
        self.pos = start + len;
        self.body_len = None;
        Ok(Some(&self.buf[start..start + len]))
    }

    /// 解析头部块，成功返回 `(报文体起点, 报文体长度)`。
    ///
    /// 头部尚未收全时返回 `Ok(None)`；头部结构非法时返回 `Err`（致命）。
    /// 行尾 `\r` 被剥除，因此 `\r\n` 与 `\n` 等价；头名 ASCII 大小写不敏感；
    /// 只解释 `Content-Length`，其余头跳过。
    fn parse_headers(&self) -> Result<Option<(usize, usize)>, FrameError> {
        let block_start = self.pos;
        let mut cursor = block_start;
        let mut content_length: Option<usize> = None;
        loop {
            let Some((line_end, next)) = line_at(&self.buf, cursor) else {
                if self.buf.len() - block_start > MAX_HEADER_BYTES {
                    return Err(FrameError::HeaderTooLarge);
                }
                return Ok(None);
            };
            if line_end - block_start > MAX_HEADER_BYTES {
                return Err(FrameError::HeaderTooLarge);
            }
            let line = &self.buf[cursor..line_end];
            if line.is_empty() {
                let Some(len) = content_length else {
                    return Err(FrameError::InvalidContentLength(
                        "缺少 Content-Length 头".to_owned(),
                    ));
                };
                if len > self.max_frame_bytes {
                    return Err(FrameError::TooLarge {
                        len,
                        max: self.max_frame_bytes,
                    });
                }
                return Ok(Some((next, len)));
            }
            let Some(colon) = find_byte(line, 0, b':') else {
                return Err(FrameError::UnexpectedPrefix);
            };
            let name = trim_ascii(&line[..colon]);
            if !name.iter().all(|byte| is_token_byte(*byte)) {
                return Err(FrameError::UnexpectedPrefix);
            }
            let value = trim_ascii(&line[colon + 1..]);
            if name.eq_ignore_ascii_case(b"content-length") {
                content_length = Some(parse_decimal(value)?);
            }
            cursor = next;
        }
    }

    fn next_ndjson_frame(&mut self) -> Result<Option<&[u8]>, FrameError> {
        loop {
            let Some(newline) = find_byte(&self.buf, self.pos, b'\n') else {
                let pending = self.buf.len() - self.pos;
                if pending > self.max_frame_bytes {
                    return Err(FrameError::TooLarge {
                        len: pending,
                        max: self.max_frame_bytes,
                    });
                }
                return Ok(None);
            };
            let start = self.pos;
            self.pos = newline + 1;
            let end = if newline > start && self.buf[newline - 1] == b'\r' {
                newline - 1
            } else {
                newline
            };
            let len = end - start;
            if len == 0 {
                // 空行（含裸 `\r\n`）跳过，方便手工调试
                continue;
            }
            if len > self.max_frame_bytes {
                return Err(FrameError::TooLarge {
                    len,
                    max: self.max_frame_bytes,
                });
            }
            return Ok(Some(&self.buf[start..end]));
        }
    }
}

/// 增量编码器。
pub struct FrameEncoder {
    framing: Framing,
    /// 头部文本缓冲：整条连接复用，避免每帧因头部格式化再分配一次。
    scratch: Vec<u8>,
}

impl FrameEncoder {
    /// 新建编码器。
    pub fn new(framing: Framing) -> Self {
        Self {
            framing,
            scratch: Vec::with_capacity(32),
        }
    }

    /// 把 `payload` 追加为一帧到 `out`。
    ///
    /// 返回 `false` 表示载荷超过 `max_frame_bytes`，此时 `out` 不变（绝不写半个头）。
    pub fn encode(&mut self, payload: &[u8], max_frame_bytes: usize, out: &mut Vec<u8>) -> bool {
        if payload.len() > max_frame_bytes {
            return false;
        }
        match self.framing {
            Framing::ContentLength => {
                self.scratch.clear();
                self.scratch.extend_from_slice(b"Content-Length: ");
                push_decimal(&mut self.scratch, payload.len());
                self.scratch.extend_from_slice(b"\r\n\r\n");
                out.reserve(self.scratch.len() + payload.len());
                out.extend_from_slice(&self.scratch);
                out.extend_from_slice(payload);
            }
            Framing::Ndjson => {
                out.reserve(payload.len() + 1);
                out.extend_from_slice(payload);
                out.push(b'\n');
            }
        }
        true
    }
}

/// 读取 `buf[cursor..]` 里的一整行；返回 `(行尾下标（已剥 `\r`）, 下一行起点)`。
fn line_at(buf: &[u8], cursor: usize) -> Option<(usize, usize)> {
    let newline = find_byte(buf, cursor, b'\n')?;
    let end = if newline > cursor && buf[newline - 1] == b'\r' {
        newline - 1
    } else {
        newline
    };
    Some((end, newline + 1))
}

/// 在 `haystack[start..]` 中查找 `needle`，返回绝对下标。
fn find_byte(haystack: &[u8], start: usize, needle: u8) -> Option<usize> {
    haystack[start..]
        .iter()
        .position(|byte| *byte == needle)
        .map(|offset| start + offset)
}

/// 剥掉首尾的 ASCII 空白。
fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

/// HTTP 头名允许的字节（token：可见 ASCII 去掉分隔符）。
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// 解析十进制长度；空串、非数字、溢出都是分帧失败。
fn parse_decimal(bytes: &[u8]) -> Result<usize, FrameError> {
    if bytes.is_empty() {
        return Err(FrameError::NonNumericLength);
    }
    let mut value: usize = 0;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return Err(FrameError::NonNumericLength);
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(byte - b'0')))
            .ok_or_else(|| FrameError::InvalidContentLength("长度超出 usize 范围".to_owned()))?;
    }
    Ok(value)
}

/// 追加十进制整数，零分配。
fn push_decimal(out: &mut Vec<u8>, mut value: usize) {
    if value == 0 {
        out.push(b'0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut cursor = digits.len();
    while value > 0 {
        cursor -= 1;
        digits[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    out.extend_from_slice(&digits[cursor..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用同一帧反复喂解码器，断言内部缓冲容量不会随帧数增长。
    #[test]
    fn buffer_capacity_stays_bounded() {
        for framing in [Framing::ContentLength, Framing::Ndjson] {
            let mut decoder = FrameDecoder::new(framing, DEFAULT_MAX_FRAME_BYTES);
            let payload = vec![b'x'; 4 * 1024];
            let mut encoder = FrameEncoder::new(framing);
            let mut frame = Vec::new();
            assert!(encoder.encode(&payload, DEFAULT_MAX_FRAME_BYTES, &mut frame));
            for _ in 0..10_000 {
                decoder.push(&frame);
                let decoded = decoder.next_frame().expect("分帧合法").expect("完整帧");
                assert_eq!(decoded.len(), payload.len());
            }
            assert_eq!(decoder.buffered_len(), 0);
            assert!(
                decoder.buf.capacity() <= 2 * frame.len(),
                "缓冲容量随帧数增长：{}",
                decoder.buf.capacity()
            );
        }
    }

    #[test]
    fn encoder_rejects_oversized_payload_without_writing() {
        let mut encoder = FrameEncoder::new(Framing::ContentLength);
        let mut out = Vec::new();
        assert!(!encoder.encode(b"12345", 4, &mut out));
        assert!(out.is_empty());
        assert!(encoder.encode(b"1234", 4, &mut out));
        assert_eq!(out, b"Content-Length: 4\r\n\r\n1234");
    }

    #[test]
    fn parse_decimal_rejects_bad_input() {
        assert_eq!(parse_decimal(b"0"), Ok(0));
        assert_eq!(parse_decimal(b"12345"), Ok(12345));
        assert_eq!(parse_decimal(b""), Err(FrameError::NonNumericLength));
        assert_eq!(parse_decimal(b"-1"), Err(FrameError::NonNumericLength));
        assert_eq!(parse_decimal(b"1e3"), Err(FrameError::NonNumericLength));
        assert!(matches!(
            parse_decimal(b"99999999999999999999999999"),
            Err(FrameError::InvalidContentLength(_))
        ));
    }
}

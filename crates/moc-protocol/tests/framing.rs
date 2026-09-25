//! 分帧层边界测试：任意切分位置、两种分帧、超限与错位。

use moc_protocol::error::FrameError;
use moc_protocol::frame::{DEFAULT_MAX_FRAME_BYTES, FrameDecoder, FrameEncoder, Framing};
use moc_protocol::{Id, Request, Response, RpcMessage};

const MAX: usize = DEFAULT_MAX_FRAME_BYTES;

/// 抽出解码器里现有的全部帧。
fn drain(decoder: &mut FrameDecoder) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame().expect("分帧合法") {
        frames.push(frame.to_vec());
    }
    frames
}

/// 两条报文的字节流：`Content-Length` 帧可以粘在一起，也必须能在任意位置被切开。
#[test]
fn content_length_split_at_every_offset() {
    let first = RpcMessage::Request(Request::new(Id::Number(1), "ping", None))
        .to_vec()
        .expect("可序列化");
    let second = RpcMessage::Response(Response::result(
        Id::Number(1),
        serde_json::json!({ "server_time_ms": 1 }),
    ))
    .to_vec()
    .expect("可序列化");
    let mut encoder = FrameEncoder::new(Framing::ContentLength);
    let mut blob = Vec::new();
    assert!(encoder.encode(&first, MAX, &mut blob));
    assert!(encoder.encode(&second, MAX, &mut blob));
    assert!(blob.len() > first.len() + second.len(), "两帧是独立的");

    for split in 0..=blob.len() {
        let mut decoder = FrameDecoder::new(Framing::ContentLength, MAX);
        decoder.push(&blob[..split]);
        let mut frames = drain(&mut decoder);
        decoder.push(&blob[split..]);
        frames.extend(drain(&mut decoder));
        assert_eq!(
            frames,
            vec![first.clone(), second.clone()],
            "在偏移 {split} 处切分后解码结果必须一致"
        );
    }
}

/// `Content-Type` 之类的头跳过，`\n\n` 与 `\r\n\r\n` 等价，大小写不敏感。
#[test]
fn content_length_accepts_lf_and_extra_headers() {
    let mut decoder = FrameDecoder::new(Framing::ContentLength, MAX);
    decoder.push(b"Content-Type: application/json\ncontent-length: 5\n\nhello");
    assert_eq!(decoder.next_frame().expect("分帧合法"), Some(&b"hello"[..]));
    assert_eq!(decoder.next_frame().expect("分帧合法"), None);
}

/// 畸形头部一律致命：错误会重复出现（分帧无法重新同步），调用方必须关闭连接。
#[test]
fn content_length_rejects_malformed_headers() {
    assert_fatal_header(b"Content-Length: abc\r\n\r\n", |error| {
        matches!(error, FrameError::NonNumericLength)
    });
    assert_fatal_header(b"Content-Length: -5\r\n\r\n", |error| {
        matches!(error, FrameError::NonNumericLength)
    });
    assert_fatal_header(b"X-Foo: 1\r\n\r\n", |error| {
        matches!(error, FrameError::InvalidContentLength(_))
    });
    assert_fatal_header(b"not-a-header\r\n\r\n", |error| {
        matches!(error, FrameError::UnexpectedPrefix)
    });
}

/// 断言该头部块致命：两次调用都失败。
fn assert_fatal_header(bytes: &[u8], expected: impl Fn(&FrameError) -> bool) {
    let mut decoder = FrameDecoder::new(Framing::ContentLength, MAX);
    decoder.push(bytes);
    let first = decoder.next_frame().expect_err("畸形头部必须失败");
    assert!(expected(&first), "错误类型不符：{first:?}");
    // 致命：再次调用仍然失败，不会「跳过坏头」继续
    assert!(decoder.next_frame().is_err(), "分帧错位不可恢复");
}

/// 头部块超过 8 KiB 仍未结束即判定错位。
#[test]
fn oversized_header_block_is_rejected() {
    let mut decoder = FrameDecoder::new(Framing::ContentLength, MAX);
    let mut header = Vec::new();
    while header.len() <= 9 * 1024 {
        header.extend_from_slice(b"X-Padding: 0123456789\r\n");
    }
    decoder.push(&header);
    assert!(matches!(
        decoder.next_frame().expect_err("头部超限"),
        FrameError::HeaderTooLarge
    ));
}

/// NDJSON：`\r\n` 与 `\n` 都接受，空行跳过，任意切分位置安全。
#[test]
fn ndjson_handles_crlf_and_blank_lines() {
    let blob = b"{\"a\":1}\r\n\n{\"b\":2}\n";
    for split in 0..=blob.len() {
        let mut decoder = FrameDecoder::new(Framing::Ndjson, MAX);
        decoder.push(&blob[..split]);
        let mut frames = drain(&mut decoder);
        decoder.push(&blob[split..]);
        frames.extend(drain(&mut decoder));
        assert_eq!(
            frames,
            vec![b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()],
            "在偏移 {split} 处切分后解码结果必须一致"
        );
    }
}

/// 声明或实际长度超过上限即拒绝，且不占用缓冲。
#[test]
fn frame_over_limit_is_rejected() {
    let mut decoder = FrameDecoder::new(Framing::ContentLength, 10);
    decoder.push(b"Content-Length: 100\r\n\r\n");
    assert_eq!(
        decoder.next_frame().expect_err("超限"),
        FrameError::TooLarge { len: 100, max: 10 }
    );

    // NDJSON：行已完整但超限
    let mut decoder = FrameDecoder::new(Framing::Ndjson, 10);
    decoder.push(b"aaaaaaaaaaaa\n");
    assert_eq!(
        decoder.next_frame().expect_err("超限"),
        FrameError::TooLarge { len: 12, max: 10 }
    );

    // NDJSON：行还没结束就已经超限，不能无限攒缓冲
    let mut decoder = FrameDecoder::new(Framing::Ndjson, 10);
    decoder.push(&[b'a'; 11]);
    assert!(matches!(
        decoder.next_frame().expect_err("超限"),
        FrameError::TooLarge { .. }
    ));
}

/// 2 MiB 载荷在两种分帧下逐字节往返。
///
/// NDJSON 的报文体不能含裸换行，因此载荷取可打印 ASCII——真实报文体是 JSON，
/// 序列化不会产生裸换行，这条约束对实际使用是免费的。
#[test]
fn large_message_round_trip_both_framings() {
    // 确定性伪随机：避免全零载荷掩盖长度计算错误
    let mut payload = Vec::with_capacity(2 * 1024 * 1024);
    let mut state: u32 = 0x1234_5678;
    for _ in 0..2 * 1024 * 1024 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        payload.push(b' ' + (state >> 25) as u8 % 95);
    }

    for framing in [Framing::ContentLength, Framing::Ndjson] {
        let mut encoder = FrameEncoder::new(framing);
        let mut frame = Vec::new();
        assert!(encoder.encode(&payload, MAX, &mut frame));
        let mut decoder = FrameDecoder::new(framing, MAX);
        // 分三次喂入，包含跨包边界
        let chunk = frame.len() / 3;
        for part in frame.chunks(chunk.max(1)) {
            decoder.push(part);
        }
        let decoded = decoder.next_frame().expect("分帧合法").expect("完整帧");
        assert_eq!(decoded.len(), payload.len(), "分帧 {framing:?} 长度不符");
        assert!(decoded == payload.as_slice(), "分帧 {framing:?} 内容不符");
        assert_eq!(decoder.next_frame().expect("分帧合法"), None);
    }
}

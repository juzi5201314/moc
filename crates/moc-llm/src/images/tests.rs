//! 图片请求策略：覆盖历史/当前消息、工具图片及真实线上预算。
use super::*;
use crate::{Message, Role, ToolCall};

fn png(width: u32, height: u32, alpha: bool) -> ImageSource {
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([
            (x % 251) as u8,
            (y % 251) as u8,
            ((x + y) % 251) as u8,
            if alpha { 100 } else { 255 },
        ])
    });
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    ImageSource::Base64 {
        media_type: "image/png".into(),
        data: STANDARD.encode(bytes.into_inner()),
    }
}

fn request(images: Vec<ImageSource>) -> ChatRequest {
    ChatRequest {
        messages: images
            .into_iter()
            .map(|source| Message {
                role: Role::User,
                content: vec![ContentPart::Image {
                    source,
                    detail: None,
                }]
                .into(),
            })
            .collect(),
        max_output_tokens: Some(100),
        ..Default::default()
    }
}

#[test]
fn fifteen_images_keep_every_occurrence_and_original_history() {
    let cache = ImageCache::default();
    let image = png(360, 460, false);
    let original = request(vec![image; 15]);
    let snapshot = original.clone();
    for protocol in [
        crate::Protocol::OpenAiChat,
        crate::Protocol::OpenAiResponses,
        crate::Protocol::AnthropicMessages,
    ] {
        let cfg = LlmConfig {
            protocol,
            ..Default::default()
        };
        let prepared = prepare(&cache, &cfg, &original, true).unwrap();
        assert_eq!(prepared.diagnostics.images, 15);
        assert_eq!(prepared.diagnostics.history_images, 14);
        assert!(prepared.diagnostics.image_bytes <= MAX_REQUEST_IMAGE_BYTES);
        assert!(prepared.encoded.body.len() <= DEFAULT_MAX_REQUEST_BYTES);
        assert_eq!(original, snapshot);
    }
}

#[test]
fn budget_counts_duplicates_and_reserves_text_tools_and_envelope() {
    let cache = ImageCache::default();
    let cfg = LlmConfig::default();
    let image = png(80, 120, true);
    let mut req = request(vec![image.clone(); 5]);
    req.messages
        .insert(0, Message::system("长指令".repeat(100)));
    let normal = prepare(&cache, &cfg, &req, true).unwrap();
    assert_eq!(normal.diagnostics.images, 5);
    req.options.max_request_bytes = Some(100);
    let error = prepare(&cache, &cfg, &req, true).unwrap_err();
    assert!(error.to_string().contains("请求体"));
    assert!(error.to_string().contains("未发送"));
    assert_eq!(req.messages.len(), 6);
}

#[test]
fn small_gateway_does_not_silently_drop_even_the_latest_image() {
    let cache = ImageCache::default();
    let mut req = request(vec![png(180, 240, true)]);
    req.options.max_request_bytes = Some(400);
    let error = prepare(&cache, &LlmConfig::default(), &req, false).unwrap_err();
    assert!(error.to_string().contains("未发送"));
    assert!(matches!(
        req.messages[0].content[0],
        ContentPart::Image { .. }
    ));
}

#[test]
fn transparent_image_keeps_alpha_and_tall_image_keeps_aspect_ratio() {
    let source = png(40, 400, true);
    let result = normalize(&source, 500 * 1024).unwrap();
    let ImageSource::Base64 { data, .. } = result else {
        panic!()
    };
    let decoded = image::load_from_memory(&STANDARD.decode(data).unwrap()).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (40, 400));
    assert_eq!(decoded.to_rgba8().get_pixel(0, 0).0[3], 100);
    let (width, height) = dimensions(1000, 8000);
    assert!(width * height <= MAX_PIXELS);
    assert!((height as f64 / width as f64 - 8.0).abs() < 0.02);
    assert!(width > 500, "长截图不能按 1568px 长边缩到不可辨认");
}

#[test]
fn malformed_image_fails_locally_instead_of_sending_original() {
    let req = request(vec![ImageSource::Base64 {
        media_type: "image/png".into(),
        data: STANDARD.encode(b"broken"),
    }]);
    let error = prepare(&ImageCache::default(), &LlmConfig::default(), &req, true).unwrap_err();
    assert!(error.to_string().contains("图片"));
}

#[test]
fn allocation_spends_small_images_first_and_counts_every_occurrence() {
    let lengths = [400, 400_000, 400_000, 400_000, 400_000];
    let caps = allocate(&lengths, 800_400);
    assert_eq!(caps, [300, 150_000, 150_000, 150_000, 150_000]);
    assert!(caps.iter().map(|n| n.div_ceil(3) * 4).sum::<usize>() <= 800_400);
    let fifteen = allocate(&[700_000; 15], MAX_REQUEST_IMAGE_BYTES);
    assert_eq!(fifteen.len(), 15);
    assert!(fifteen.iter().all(|&cap| cap > 400 * 1024));
    assert!(fifteen.iter().map(|n| n.div_ceil(3) * 4).sum::<usize>() <= MAX_REQUEST_IMAGE_BYTES);
}

#[test]
fn tight_budget_reencodes_five_images_without_dropping_or_shrinking() {
    let mut state = 12345u32;
    let image = image::RgbImage::from_fn(96, 96, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
    });
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    let source = ImageSource::Base64 {
        media_type: "image/png".into(),
        data: STANDARD.encode(bytes.into_inner()),
    };
    let mut req = request(Vec::new());
    req.messages.push(Message {
        role: Role::User,
        content: (0..5)
            .map(|_| ContentPart::Image {
                source: source.clone(),
                detail: None,
            })
            .collect::<Vec<_>>()
            .into(),
    });
    req.options.max_request_bytes = Some(100_000);
    let prepared = prepare(&ImageCache::default(), &LlmConfig::default(), &req, true).unwrap();
    assert_eq!(prepared.diagnostics.images, 5);
    assert_eq!(prepared.diagnostics.history_images, 0);
    assert!(prepared.encoded.body.len() <= 100_000);
    assert!(prepared.diagnostics.original_image_bytes > 100_000);
    for source in prepared.request.messages[0]
        .content
        .iter()
        .flat_map(sources)
    {
        let (_, data) = inline(source).unwrap().unwrap();
        let decoded = image::load_from_memory(&STANDARD.decode(data).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (96, 96));
    }
}

#[test]
fn inspection_matches_the_prepared_wire_image() {
    let req = request(vec![png(400, 500, false)]);
    let cfg = LlmConfig::default();
    let wire = prepare(&ImageCache::default(), &cfg, &req, true).unwrap();
    let inspection = crate::Client::new()
        .inspect_request(&cfg, &req, true)
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&wire.encoded.body).unwrap(),
        inspection.body
    );
}

#[test]
fn tool_images_and_data_urls_use_the_same_preprocessor() {
    let source = png(80, 100, false);
    let ImageSource::Base64 { media_type, data } = &source else {
        panic!()
    };
    let mut req = request(vec![ImageSource::Url(format!(
        "data:{media_type};base64,{data}"
    ))]);
    req.messages.push(Message {
        role: Role::Assistant,
        content: vec![ContentPart::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "screenshot".into(),
            arguments: "{}".into(),
        })]
        .into(),
    });
    req.messages.push(Message::tool_result_with_images(
        "call_1",
        "截图",
        false,
        vec![source],
    ));
    let prepared = prepare(&ImageCache::default(), &LlmConfig::default(), &req, true).unwrap();
    assert_eq!(prepared.diagnostics.images, 2);
    assert_eq!(prepared.diagnostics.remote_images, 0);
    assert!(prepared.diagnostics.image_bytes > 0);
}

//! 所有入口共用的发送前图片预处理；仅改请求副本，原始会话保留高清图片。
use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::Mutex;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use sha2::{Digest, Sha256};

use crate::codec::{self, EncodedRequest};
use crate::{ChatRequest, ContentPart, ImageSource, LlmConfig, LlmError, Role};

#[cfg(test)]
mod tests;

pub(crate) const DEFAULT_MAX_REQUEST_BYTES: usize = 50 * 1024 * 1024;
const MAX_REQUEST_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 500 * 1024;
// 总像素限制保留长截图的短边；不能只把长边一律缩到 1568px。
const MAX_PIXELS: u32 = 1568 * 1568;
const MAX_DIMENSION: u32 = 8192;
const MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;
const MAX_INPUT_PIXELS: u64 = 64_000_000;

/// 只存体积与计数，不包含图片、提示词或鉴权信息。
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestDiagnostics {
    pub body_bytes: usize,
    pub images: usize,
    pub history_images: usize,
    pub remote_images: usize,
    /// 内联图片在处理前/后的 Base64 字节数，按出现次数统计。
    pub original_image_bytes: usize,
    pub image_bytes: usize,
    pub max_request_bytes: usize,
    pub elapsed_ms: u64,
}

impl std::fmt::Display for RequestDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "请求体 {} 字节（{:.1} KiB），图片 {} 张（历史 {} 张），图片 Base64 {} → {} 字节，非图片部分 {} 字节，本地请求上限 {} 字节，等待 {:.1} 秒",
            self.body_bytes,
            self.body_bytes as f64 / 1024.0,
            self.images,
            self.history_images,
            self.original_image_bytes,
            self.image_bytes,
            self.body_bytes.saturating_sub(self.image_bytes),
            self.max_request_bytes,
            self.elapsed_ms as f64 / 1000.0
        )?;
        if self.remote_images > 0 {
            write!(
                f,
                "；其中 {} 张是远端 URL，其下载体积无法从请求体判断",
                self.remote_images
            )?;
        }
        Ok(())
    }
}

/// 最多 32 个、合计 24 MiB 的压缩结果；固定小上界，用顺序队列实现 FIFO 淘汰。
#[derive(Debug, Default)]
pub(crate) struct ImageCache(Mutex<VecDeque<([u8; 32], usize, ImageSource)>>);

impl ImageCache {
    fn normalize(&self, source: &ImageSource, budget: usize) -> Result<ImageSource, LlmError> {
        let Some((_, data)) = inline(source)? else {
            return Ok(source.clone());
        };
        let hash: [u8; 32] = Sha256::digest(data.as_bytes()).into();
        {
            let cache = self.0.lock().expect("图片缓存");
            if let Some((_, _, value)) = cache
                .iter()
                .find(|(key, cap, _)| *key == hash && *cap == budget)
            {
                return Ok(value.clone());
            }
        }
        let value = normalize(source, budget)?;
        let mut cache = self.0.lock().expect("图片缓存");
        while cache.len() >= 32
            || cache
                .iter()
                .map(|(_, _, source)| inline_len(source))
                .sum::<usize>()
                + inline_len(&value)
                > 24 * 1024 * 1024
        {
            if cache.pop_front().is_none() {
                break;
            }
        }
        cache.push_back((hash, budget, value.clone()));
        debug_assert!(cache.len() <= 32);
        Ok(value)
    }
}

#[derive(Debug)]
pub(crate) struct PreparedRequest {
    pub encoded: EncodedRequest,
    pub diagnostics: RequestDiagnostics,
    pub request: ChatRequest,
}

fn sources(part: &ContentPart) -> &[ImageSource] {
    match part {
        ContentPart::Image { source, .. } => std::slice::from_ref(source),
        ContentPart::ToolResult(result) => &result.images,
        _ => &[],
    }
}

fn map_images(
    req: &mut ChatRequest,
    message_indexes: &[usize],
    mut map: impl FnMut(&ImageSource) -> Result<ImageSource, LlmError>,
) -> Result<(), LlmError> {
    for &index in message_indexes {
        let message = &mut req.messages[index];
        for part in &mut message.content {
            let images = match part {
                ContentPart::Image { source, .. } => std::slice::from_mut(source),
                ContentPart::ToolResult(result) => result.images.as_mut_slice(),
                _ => &mut [],
            };
            for source in images {
                *source = map(source)?;
            }
        }
    }
    Ok(())
}

/// 整个上下文共同分配预算，小图片先满足，剩余额度均分给较大的图片。
fn allocate(lengths: &[usize], budget: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..lengths.len()).collect();
    order.sort_by_key(|&i| lengths[i]);
    let mut remaining = budget;
    let mut caps = vec![0; lengths.len()];
    for (rank, &i) in order.iter().enumerate() {
        let cap = lengths[i].min(remaining / (lengths.len() - rank));
        // Base64 每 4 字节编码至多 3 个原始字节，向下取整保证 padding 也在预算内。
        caps[i] = cap / 4 * 3;
        remaining -= cap;
    }
    caps
}

pub(crate) fn prepare(
    cache: &ImageCache,
    cfg: &LlmConfig,
    original: &ChatRequest,
    stream: bool,
) -> Result<PreparedRequest, LlmError> {
    let max_bytes = original
        .options
        .max_request_bytes
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
        .unwrap_or(DEFAULT_MAX_REQUEST_BYTES);
    let latest_user = original
        .messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .unwrap_or(0);
    let mut diagnostics = RequestDiagnostics {
        max_request_bytes: max_bytes,
        ..Default::default()
    };
    let mut image_messages = Vec::new();
    for (index, message) in original.messages.iter().enumerate() {
        let before = diagnostics.images;
        message.content.with_parts(|parts| {
            for source in parts.iter().flat_map(sources) {
                diagnostics.images += 1;
                diagnostics.history_images += usize::from(index < latest_user);
                diagnostics.original_image_bytes += inline_len(source);
                diagnostics.remote_images += usize::from(
                    matches!(source, ImageSource::Url(url) if !url.starts_with("data:")),
                );
            }
        });
        if diagnostics.images > before {
            image_messages.push(index);
        }
    }
    let mut req = original.clone();
    map_images(&mut req, &image_messages, |source| {
        cache.normalize(source, MAX_IMAGE_BYTES)
    })?;
    let mut encoded = codec::encode(cfg, &req, stream)?;
    let mut lengths = Vec::new();
    for &index in &image_messages {
        let message = &req.messages[index];
        message.content.with_parts(|parts| {
            lengths.extend(
                parts
                    .iter()
                    .flat_map(sources)
                    .filter_map(|source| inline(source).ok().flatten().map(|(_, data)| data.len())),
            );
        });
    }
    let image_bytes: usize = lengths.iter().sum();
    // 图片 MIME 变化最多多几个字节；预留逐图封套差额，最终再按精确编码长度验收。
    let overhead = encoded
        .body
        .len()
        .saturating_sub(image_bytes)
        .saturating_add(lengths.len() * 32);
    let budget = MAX_REQUEST_IMAGE_BYTES.min(max_bytes.saturating_sub(overhead));
    if image_bytes > budget {
        let caps = allocate(&lengths, budget);
        let mut index = 0;
        req = original.clone();
        map_images(&mut req, &image_messages, |source| {
            if inline(source)?.is_none() {
                return Ok(source.clone());
            }
            let cap = caps[index].min(MAX_IMAGE_BYTES);
            index += 1;
            cache.normalize(source, cap).map_err(|error| LlmError::Encode(format!(
                "图片请求体预算不足，未发送：{} 张图片（历史 {} 张）共享 {} 字节 Base64 预算，文本/工具/封套约 {} 字节；第 {} 张无法在保留细节的策略下压入预算。请分批处理、裁剪关注区域、压缩历史，或按网关实际限制调整模型 max_request_bytes。{error}",
                diagnostics.images, diagnostics.history_images, budget, overhead, index)))
        })?;
        encoded = codec::encode(cfg, &req, stream)?;
    }
    diagnostics.body_bytes = encoded.body.len();
    for &index in &image_messages {
        let message = &req.messages[index];
        diagnostics.image_bytes += message.content.with_parts(|parts| {
            parts
                .iter()
                .flat_map(sources)
                .map(inline_len)
                .sum::<usize>()
        });
    }
    if encoded.body.len() > max_bytes || diagnostics.image_bytes > MAX_REQUEST_IMAGE_BYTES {
        return Err(LlmError::Encode(format!(
            "请求体超限，未发送：{diagnostics}；图片 Base64 总预算 {} 字节。请分批处理图片、压缩历史或按网关限制调整模型 max_request_bytes。",
            MAX_REQUEST_IMAGE_BYTES
        )));
    }
    Ok(PreparedRequest {
        encoded,
        diagnostics,
        request: req,
    })
}

fn inline(source: &ImageSource) -> Result<Option<(&str, &str)>, LlmError> {
    match source {
        ImageSource::Base64 { media_type, data } => Ok(Some((media_type, data))),
        ImageSource::Url(url) if url.starts_with("data:") => {
            let (media, data) = url[5..]
                .split_once(";base64,")
                .ok_or_else(|| image_error("data URL 必须使用 Base64"))?;
            Ok(Some((media, data)))
        }
        ImageSource::Url(_) => Ok(None),
    }
}

fn inline_len(source: &ImageSource) -> usize {
    inline(source)
        .ok()
        .flatten()
        .map_or(0, |(_, data)| data.len())
}

fn image_error(message: impl std::fmt::Display) -> LlmError {
    LlmError::Encode(format!("图片预处理失败，未发送：{message}"))
}

fn dimensions(width: u32, height: u32) -> (u32, u32) {
    let scale = (MAX_PIXELS as f64 / (width as f64 * height as f64))
        .sqrt()
        .min(MAX_DIMENSION as f64 / width.max(height) as f64)
        .min(1.0);
    (
        (width as f64 * scale).floor().max(1.0) as u32,
        (height as f64 * scale).floor().max(1.0) as u32,
    )
}

fn normalize(source: &ImageSource, budget: usize) -> Result<ImageSource, LlmError> {
    let Some((_, data)) = inline(source)? else {
        return Ok(source.clone());
    };
    if data.len() > MAX_INPUT_BYTES.div_ceil(3) * 4 {
        return Err(image_error("单张输入超过 20 MiB，请先裁剪或缩放"));
    }
    let bytes = STANDARD.decode(data).map_err(image_error)?;
    let mut reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(image_error)?;
    let format = reader
        .format()
        .ok_or_else(|| image_error("无法识别图片格式"))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(image_error)?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_INPUT_PIXELS {
        return Err(image_error("解码像素超过 6400 万，请先裁剪或缩放"));
    }
    let orientation = decoder.orientation().map_err(image_error)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(image_error)?;
    image.apply_orientation(orientation);
    let (width, height) = dimensions(image.width(), image.height());
    let resized = (width, height) != (image.width(), image.height());
    let original_ok = !resized
        && orientation == image::metadata::Orientation::NoTransforms
        && matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
        );
    if original_ok && bytes.len() <= budget.min(MAX_IMAGE_BYTES / 4) {
        return Ok(ImageSource::Base64 {
            media_type: format.to_mime_type().into(),
            data: data.into(),
        });
    }
    if resized {
        image = image.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
    }
    let original = if original_ok {
        Some((bytes, format.to_mime_type()))
    } else {
        None
    };
    let mut best = encode_candidates(&image, original, budget)?;
    let mut final_dimensions = (width, height);
    // 保留至少初始预览的 70% 边长，质量不低于 60；不为塞满上下文无限缩成小图。
    for scale in [0.85, 0.70] {
        if best.0.len() <= budget {
            break;
        }
        let width = (width as f64 * scale).round().max(1.0) as u32;
        let height = (height as f64 * scale).round().max(1.0) as u32;
        let smaller = image.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
        let candidate = encode_candidates(&smaller, None, budget)?;
        if candidate.0.len() < best.0.len() {
            best = candidate;
            final_dimensions = (width, height);
        }
    }
    let (bytes, media) = best;
    if bytes.len() > budget {
        return Err(image_error(format!(
            "{}×{} 图片需要 {} 字节，分配到 {} 字节；已达到预览尺寸/质量下限，请裁剪关注区域或分批处理",
            final_dimensions.0,
            final_dimensions.1,
            bytes.len(),
            budget
        )));
    }
    Ok(ImageSource::Base64 {
        media_type: media.into(),
        data: STANDARD.encode(bytes),
    })
}

fn encode_candidates(
    image: &DynamicImage,
    mut best: Option<(Vec<u8>, &'static str)>,
    budget: usize,
) -> Result<(Vec<u8>, &'static str), LlmError> {
    // 透明通道可能全为 255；这样的截图可用 JPEG，实际透明像素必须保留。
    let rgba = image.to_rgba8();
    let alpha = rgba.pixels().any(|pixel| pixel.0[3] != 255);
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(rgba)
        .write_to(&mut png, ImageFormat::Png)
        .map_err(image_error)?;
    keep_smallest(&mut best, png.into_inner(), "image/png");
    if alpha {
        let mut webp = Cursor::new(Vec::new());
        image
            .write_to(&mut webp, ImageFormat::WebP)
            .map_err(image_error)?;
        keep_smallest(&mut best, webp.into_inner(), "image/webp");
    } else {
        let rgb = image.to_rgb8();
        for quality in [85, 75, 60] {
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality)
                .encode_image(&rgb)
                .map_err(image_error)?;
            keep_smallest(&mut best, jpeg, "image/jpeg");
            if best
                .as_ref()
                .is_some_and(|(bytes, _)| bytes.len() <= budget)
            {
                break;
            }
        }
    }
    Ok(best.expect("PNG 编码成功"))
}

fn keep_smallest(best: &mut Option<(Vec<u8>, &'static str)>, bytes: Vec<u8>, media: &'static str) {
    if best
        .as_ref()
        .is_none_or(|(current, _)| bytes.len() < current.len())
    {
        *best = Some((bytes, media));
    }
}

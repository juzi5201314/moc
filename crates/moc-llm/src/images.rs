//! 所有入口共用的发送前图片预处理；仅改请求副本，原始会话保留高清图片。
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Mutex;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use rust_i18n::t;
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
            "{}",
            t!(
                "llm.img.diagnostics",
                bytes = self.body_bytes,
                kib = format!("{:.1}", self.body_bytes as f64 / 1024.0),
                images = self.images,
                history = self.history_images,
                b64_before = self.original_image_bytes,
                b64_after = self.image_bytes,
                other = self.body_bytes.saturating_sub(self.image_bytes),
                limit = self.max_request_bytes,
                wait = format!("{:.1}", self.elapsed_ms as f64 / 1000.0)
            )
        )?;
        if self.remote_images > 0 {
            write!(
                f,
                "{}",
                t!("llm.img.remote_note", count = self.remote_images)
            )?;
        }
        Ok(())
    }
}

/// 缓存 key = (采样指纹, 全量校验和)。
/// 指纹只采样首/中/尾各 4 KiB 加上总长度，负责快速分桶；
/// 每次请求都会对历史里全部内联图片算 key，对完整 base64 做 SHA-256
/// 意味着每轮重扫几十 MB。但同长度图片只改未采样区域时指纹不变，
/// 因此再加一层覆盖全部字节的折叠校验和（加法折叠，速度接近内存带宽），
/// 保证真实编辑必然 miss，不会把旧图发给模型。
const FINGERPRINT_SAMPLE: usize = 4 * 1024;
const MAX_CACHE_BYTES: usize = 24 * 1024 * 1024;

fn checksum(data: &str) -> u64 {
    let (words, remainder) = data.as_bytes().as_chunks::<8>();
    let mut sum = 0u64;
    for word in words {
        sum = sum.wrapping_add(u64::from_le_bytes(*word));
    }
    let mut tail = [0u8; 8];
    tail[..remainder.len()].copy_from_slice(remainder);
    sum.wrapping_add(u64::from_le_bytes(tail))
}

fn fingerprint(data: &str) -> [u8; 32] {
    let bytes = data.as_bytes();
    let mut hasher = Sha256::new();
    hasher.update(bytes.len().to_le_bytes());
    if bytes.len() <= FINGERPRINT_SAMPLE * 3 {
        hasher.update(bytes);
    } else {
        let mid = bytes.len() / 2;
        hasher.update(&bytes[..FINGERPRINT_SAMPLE]);
        hasher.update(&bytes[mid - FINGERPRINT_SAMPLE / 2..mid + FINGERPRINT_SAMPLE / 2]);
        hasher.update(&bytes[bytes.len() - FINGERPRINT_SAMPLE..]);
    }
    hasher.finalize().into()
}

/// 一个压缩变体覆盖 `[decoded, limit]` 的请求预算区间：
/// 产出预算不小于当前预算、且解码体积不大于当前预算时可直接复用。
/// 预算随会话增长逐轮微缩，旧变体的区间仍能命中，避免每轮全量重压缩。
#[derive(Debug)]
struct Variant {
    /// 产出时的解码字节预算。
    limit: usize,
    /// 压缩结果的解码字节数；`budget`/`cap` 均为解码字节口径。
    decoded: usize,
    source: ImageSource,
}

/// 每条指纹至多保留的变体数，超出时淘汰预算区间最低（质量最差）的。
const MAX_VARIANTS: usize = 8;

#[derive(Debug, Default)]
struct CacheEntry {
    variants: Vec<Variant>,
    used: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<([u8; 32], u64), CacheEntry>,
    bytes: usize,
    clock: u64,
}

/// 按内容指纹缓存压缩变体，合计 24 MiB 上界，淘汰最久未用的整项。
#[derive(Debug, Default)]
pub(crate) struct ImageCache(Mutex<CacheState>);

/// 缓存命中判定与体积上界统一用解码字节估算（base64 长度 × 3/4）。
fn decoded_len(source: &ImageSource) -> usize {
    inline_len(source) / 4 * 3
}

impl ImageCache {
    fn normalize(&self, source: &ImageSource, budget: usize) -> Result<ImageSource, LlmError> {
        let Some((_, data)) = inline(source)? else {
            return Ok(source.clone());
        };
        let key = (fingerprint(data), checksum(data));
        {
            let mut cache = self.0.lock().expect("图片缓存");
            cache.clock += 1;
            let clock = cache.clock;
            if let Some(entry) = cache.entries.get_mut(&key) {
                entry.used = clock;
                if let Some(variant) = entry
                    .variants
                    .iter()
                    .filter(|variant| variant.decoded <= budget && budget <= variant.limit)
                    .max_by_key(|variant| variant.decoded)
                {
                    return Ok(variant.source.clone());
                }
            }
        }
        let value = normalize(source, budget)?;
        let decoded = decoded_len(&value);
        let mut cache = self.0.lock().expect("图片缓存");
        cache.clock += 1;
        let clock = cache.clock;
        let mut removed_len = None;
        let mut inserted = false;
        {
            let entry = cache.entries.entry(key).or_default();
            entry.used = clock;
            if let Some(variant) = entry.variants.iter_mut().find(|v| v.decoded == decoded) {
                // 同体积变体合并预算区间，覆盖更大范围的后续请求。
                variant.limit = variant.limit.max(budget);
            } else {
                if entry.variants.len() >= MAX_VARIANTS
                    && let Some(worst) = entry
                        .variants
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, v)| v.limit)
                        .map(|(i, _)| i)
                {
                    removed_len = Some(inline_len(&entry.variants.remove(worst).source));
                }
                entry.variants.push(Variant {
                    limit: budget,
                    decoded,
                    source: value.clone(),
                });
                inserted = true;
            }
        }
        if let Some(removed) = removed_len {
            cache.bytes -= removed;
        }
        if inserted {
            cache.bytes += inline_len(&value);
        }
        while cache.bytes > MAX_CACHE_BYTES {
            let victim = cache
                .entries
                .iter()
                .filter(|(other, _)| *other != &key)
                .min_by_key(|(_, entry)| entry.used)
                .map(|(other, _)| *other);
            let Some(victim) = victim else {
                break;
            };
            let removed = cache.entries.remove(&victim).expect("淘汰目标存在");
            cache.bytes -= removed
                .variants
                .iter()
                .map(|v| inline_len(&v.source))
                .sum::<usize>();
        }
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
            // 预算随历史增长逐轮微缩；向下量化到 1/8 档位后，
            // 同档预算直接命中缓存变体，不至于每张图每轮都重压缩。
            let quantum = (cap / 8).max(4 * 1024);
            let cap = (cap / quantum * quantum).max(1);
            cache.normalize(source, cap).map_err(|error| LlmError::Encode(
                t!(
                    "llm.img.budget_short",
                    count = diagnostics.images,
                    history = diagnostics.history_images,
                    budget = budget,
                    other = overhead,
                    index = index,
                    error = error
                )
                .into_owned(),
            ))
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
        return Err(LlmError::Encode(
            t!(
                "llm.img.over_limit",
                diagnostics = diagnostics.to_string(),
                budget = MAX_REQUEST_IMAGE_BYTES
            )
            .into_owned(),
        ));
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
                .ok_or_else(|| image_error(t!("llm.img.data_url_base64")))?;
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
    LlmError::Encode(t!("llm.img.preprocess", message = message).into_owned())
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
        return Err(image_error(t!("llm.img.image_too_large")));
    }
    let bytes = STANDARD.decode(data).map_err(image_error)?;
    let mut reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(image_error)?;
    let format = reader
        .format()
        .ok_or_else(|| image_error(t!("llm.img.unrecognized")))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(image_error)?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_INPUT_PIXELS {
        return Err(image_error(t!("llm.img.pixels_cap")));
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
        return Err(image_error(t!(
            "llm.img.fit_note",
            w = final_dimensions.0,
            h = final_dimensions.1,
            need = bytes.len(),
            have = budget
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

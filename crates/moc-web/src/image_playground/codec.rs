//! Image Playground 图像探测、上传参考图保护性预处理与生成结果本地自动转码。
//!
//! - 静态图支持原样、WebP（有损/无损，经 `webpx`）、AVIF（纯 Rust `ravif`）、JPEG、PNG。
//! - 动图（GIF / 动图 WebP / APNG）走独立策略：原样保留、动图 WebP（`webpx::AnimationEncoder`）
//!   或真彩无损 APNG（`png` crate），绝不会误压成静态单帧。

use std::io::Cursor;
use std::num::NonZeroU64;

use anyhow::Context as _;
use image::codecs::avif::AvifEncoder;
use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngDecoder, PngEncoder};
use image::codecs::webp::WebPDecoder;
use image::{
    AnimationDecoder as _, DynamicImage, Frames, ImageDecoder, ImageEncoder as _, ImageFormat,
    ImageReader, RgbaImage,
};
use moc_i18n::t;
use webpx::{AnimationEncoder, Encoder as WebpEncoder, Unstoppable};

use super::config::{
    AnimatedOutputFormat, InputCompressionConfig, OutputCompressionConfig, OutputCompressionFormat,
};

/// 防止恶意畸形图片耗尽内存的解码分配上限。
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
const MAX_INPUT_PIXELS: u64 = 100_000_000;
/// 动图解码资源上限，防止高帧数或超大画布动图耗尽进程内存。
const MAX_ANIMATION_FRAMES: usize = 600;
const MAX_ANIMATION_TOTAL_PIXELS: u64 = 64_000_000;
const MAX_ANIMATION_TOTAL_BYTES: usize = 256 * 1024 * 1024;

/// 探测到的原始图片元数据。
#[derive(Debug, Clone)]
pub struct ImageMeta {
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub extension: String,
    pub mime_type: String,
    pub is_animated: bool,
}

/// 转码结果。
#[derive(Debug, Clone)]
pub struct EncodedImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub extension: String,
    pub mime_type: String,
    pub is_animated: bool,
    pub original_size_bytes: u64,
    pub original_format: String,
}

/// 预处理后的 Edit 参考图。
#[derive(Debug, Clone)]
pub struct PreparedInput {
    pub bytes: Vec<u8>,
    pub mime_type: String,
    pub extension: String,
}

/// 探测图片格式、尺寸与是否为多帧动图。
pub fn inspect_image(bytes: &[u8], _mime_hint: Option<&str>) -> anyhow::Result<ImageMeta> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .with_context(|| t!("web.playground.codec.header_read").into_owned())?;
    let detected = reader.format();
    if !matches!(
        detected,
        Some(
            ImageFormat::Gif
                | ImageFormat::Png
                | ImageFormat::Jpeg
                | ImageFormat::WebP
                | ImageFormat::Bmp
        )
    ) {
        anyhow::bail!("{}", t!("web.playground.codec.unsupported_format"));
    }

    let mut dim_reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .with_context(|| t!("web.playground.codec.image_read").into_owned())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    dim_reader.limits(limits);

    let (width, height) = dim_reader
        .into_dimensions()
        .with_context(|| t!("web.playground.codec.dimensions_parse").into_owned())?;
    if width == 0 || height == 0 {
        anyhow::bail!(
            "{}",
            t!(
                "web.playground.codec.invalid_dimensions",
                width = width,
                height = height
            )
        );
    }
    if u64::from(width) * u64::from(height) > MAX_INPUT_PIXELS {
        anyhow::bail!(
            "{}",
            t!(
                "web.playground.codec.pixels_exceeded",
                width = width,
                height = height
            )
        );
    }

    let is_animated = detect_animated(bytes, detected);

    let (format, extension, mime_type) = match detected {
        Some(ImageFormat::Gif) => ("gif", "gif", "image/gif"),
        Some(ImageFormat::Png) if is_animated => ("apng", "png", "image/png"),
        Some(ImageFormat::Png) => ("png", "png", "image/png"),
        Some(ImageFormat::Jpeg) => ("jpeg", "jpg", "image/jpeg"),
        Some(ImageFormat::WebP) => ("webp", "webp", "image/webp"),
        Some(ImageFormat::Bmp) => ("bmp", "bmp", "image/bmp"),
        _ => unreachable!("已在上方校验支持的图片格式"),
    };

    Ok(ImageMeta {
        width,
        height,
        format: format.to_owned(),
        extension: extension.to_owned(),
        mime_type: mime_type.to_owned(),
        is_animated,
    })
}

/// 上传参考图（Edit）温和保护性预处理。
pub fn prepare_edit_input(
    bytes: &[u8],
    mime_hint: Option<&str>,
    cfg: &InputCompressionConfig,
) -> anyhow::Result<PreparedInput> {
    let meta = inspect_image(bytes, mime_hint)?;
    let max_bytes = (cfg.max_bytes_mb.max(0.1) * 1024.0 * 1024.0) as usize;
    let standard_format = matches!(meta.format.as_str(), "png" | "jpeg" | "webp");

    if !cfg.enabled {
        return Ok(PreparedInput {
            bytes: bytes.to_vec(),
            mime_type: meta.mime_type,
            extension: meta.extension,
        });
    }

    let dim_ok = cfg.max_dimension == 0
        || (meta.width > 0 && meta.height > 0 && meta.width.max(meta.height) <= cfg.max_dimension);

    // 尺寸、体积、格式全部达标且非动图：零损耗直传。
    if standard_format && !meta.is_animated && dim_ok && bytes.len() <= max_bytes {
        return Ok(PreparedInput {
            bytes: bytes.to_vec(),
            mime_type: meta.mime_type,
            extension: meta.extension,
        });
    }

    let mut img = decode_static_image(bytes)?;
    if cfg.max_dimension > 0 && img.width().max(img.height()) > cfg.max_dimension {
        let scale = cfg.max_dimension as f64 / img.width().max(img.height()) as f64;
        let w = ((img.width() as f64) * scale).round().max(1.0) as u32;
        let h = ((img.height() as f64) * scale).round().max(1.0) as u32;
        img = img.resize_exact(w, h, image::imageops::FilterType::Lanczos3);
    }

    let has_alpha = image_has_alpha(&img);
    if has_alpha {
        // 含透明通道优先尝试无损 PNG，若仍超体积则使用高质有损 WebP。
        let png_bytes = encode_png(&img)?;
        if png_bytes.len() <= max_bytes {
            return Ok(PreparedInput {
                bytes: png_bytes,
                mime_type: "image/png".into(),
                extension: "png".into(),
            });
        }
        for q in [92u8, 84, 75] {
            let webp_bytes = encode_webp_static(&img, q, false)?;
            if webp_bytes.len() <= max_bytes || q == 75 {
                return Ok(PreparedInput {
                    bytes: webp_bytes,
                    mime_type: "image/webp".into(),
                    extension: "webp".into(),
                });
            }
        }
    }

    for q in [92u8, 85, 75] {
        let jpeg_bytes = encode_jpeg(&img, q)?;
        if jpeg_bytes.len() <= max_bytes || q == 75 {
            return Ok(PreparedInput {
                bytes: jpeg_bytes,
                mime_type: "image/jpeg".into(),
                extension: "jpg".into(),
            });
        }
    }

    unreachable!("JPEG 循环必定在 q=75 返回")
}

/// 收到上游生成图后，按 Playground 自动压缩配置转码落盘。
pub fn transcode_output(
    bytes: Vec<u8>,
    mime_hint: Option<&str>,
    cfg: &OutputCompressionConfig,
) -> anyhow::Result<EncodedImage> {
    let original_size_bytes = bytes.len() as u64;
    let meta = inspect_image(&bytes, mime_hint)?;
    let original_format = meta.format.clone();

    if meta.is_animated {
        return transcode_animated(bytes, meta, cfg, original_size_bytes);
    }

    match cfg.format {
        OutputCompressionFormat::Original => Ok(EncodedImage {
            bytes,
            width: meta.width,
            height: meta.height,
            format: meta.format,
            extension: meta.extension,
            mime_type: meta.mime_type,
            is_animated: false,
            original_size_bytes,
            original_format,
        }),
        OutputCompressionFormat::Webp => {
            let img = decode_static_image(&bytes)?;
            let out = encode_webp_static(&img, cfg.quality, false)?;
            Ok(EncodedImage {
                bytes: out,
                width: img.width(),
                height: img.height(),
                format: "webp".into(),
                extension: "webp".into(),
                mime_type: "image/webp".into(),
                is_animated: false,
                original_size_bytes,
                original_format,
            })
        }
        OutputCompressionFormat::WebpLossless => {
            let img = decode_static_image(&bytes)?;
            let out = encode_webp_static(&img, 100, true)?;
            Ok(EncodedImage {
                bytes: out,
                width: img.width(),
                height: img.height(),
                format: "webp".into(),
                extension: "webp".into(),
                mime_type: "image/webp".into(),
                is_animated: false,
                original_size_bytes,
                original_format,
            })
        }
        OutputCompressionFormat::Avif => {
            let img = decode_static_image(&bytes)?;
            let out = encode_avif(&img, cfg.quality)?;
            Ok(EncodedImage {
                bytes: out,
                width: img.width(),
                height: img.height(),
                format: "avif".into(),
                extension: "avif".into(),
                mime_type: "image/avif".into(),
                is_animated: false,
                original_size_bytes,
                original_format,
            })
        }
        OutputCompressionFormat::Jpeg => {
            let img = decode_static_image(&bytes)?;
            let out = encode_jpeg(&img, cfg.quality)?;
            Ok(EncodedImage {
                bytes: out,
                width: img.width(),
                height: img.height(),
                format: "jpeg".into(),
                extension: "jpg".into(),
                mime_type: "image/jpeg".into(),
                is_animated: false,
                original_size_bytes,
                original_format,
            })
        }
        OutputCompressionFormat::Png | OutputCompressionFormat::Apng => {
            let img = decode_static_image(&bytes)?;
            let out = encode_png(&img)?;
            Ok(EncodedImage {
                bytes: out,
                width: img.width(),
                height: img.height(),
                format: "png".into(),
                extension: "png".into(),
                mime_type: "image/png".into(),
                is_animated: false,
                original_size_bytes,
                original_format,
            })
        }
    }
}

fn transcode_animated(
    bytes: Vec<u8>,
    meta: ImageMeta,
    cfg: &OutputCompressionConfig,
    original_size_bytes: u64,
) -> anyhow::Result<EncodedImage> {
    let original_format = meta.format.clone();
    match cfg.animated_format {
        AnimatedOutputFormat::Original => Ok(EncodedImage {
            bytes,
            width: meta.width,
            height: meta.height,
            format: meta.format,
            extension: meta.extension,
            mime_type: meta.mime_type,
            is_animated: true,
            original_size_bytes,
            original_format,
        }),
        AnimatedOutputFormat::WebpAnimated => {
            let (width, height, frames) = decode_animation_frames(&bytes)?;
            let out = encode_animated_webp(width, height, &frames, cfg.animated_quality)?;
            Ok(EncodedImage {
                bytes: out,
                width,
                height,
                format: "webp".into(),
                extension: "webp".into(),
                mime_type: "image/webp".into(),
                is_animated: true,
                original_size_bytes,
                original_format,
            })
        }
        AnimatedOutputFormat::Apng => {
            let (width, height, frames) = decode_animation_frames(&bytes)?;
            let out = encode_apng(width, height, &frames)?;
            Ok(EncodedImage {
                bytes: out,
                width,
                height,
                format: "apng".into(),
                extension: "png".into(),
                mime_type: "image/png".into(),
                is_animated: true,
                original_size_bytes,
                original_format,
            })
        }
    }
}

struct AnimFrame {
    rgba: Vec<u8>,
    delay_ms: u32,
}

fn detect_animated(bytes: &[u8], format: Option<ImageFormat>) -> bool {
    match format {
        Some(ImageFormat::Gif) => {
            let mut opts = gif::DecodeOptions::new();
            if let Some(limit) = NonZeroU64::new(MAX_DECODE_ALLOC) {
                opts.set_memory_limit(gif::MemoryLimit::Bytes(limit));
            }
            let Ok(mut decoder) = opts.read_info(Cursor::new(bytes)) else {
                return false;
            };
            if u64::from(decoder.width()) * u64::from(decoder.height()) > MAX_INPUT_PIXELS {
                return false;
            }
            let mut count = 0usize;
            while let Ok(Some(_)) = decoder.read_next_frame() {
                count += 1;
                if count > 1 {
                    return true;
                }
            }
            false
        }
        Some(ImageFormat::Png) => PngDecoder::new(Cursor::new(bytes))
            .map(|decoder| decoder.is_apng().unwrap_or(false))
            .unwrap_or(false),
        Some(ImageFormat::WebP) => WebPDecoder::new(Cursor::new(bytes))
            .map(|decoder| decoder.has_animation())
            .unwrap_or(false),
        _ => false,
    }
}

fn decode_static_image(bytes: &[u8]) -> anyhow::Result<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .with_context(|| t!("web.playground.codec.format_unknown").into_owned())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .with_context(|| t!("web.playground.codec.decoder_create").into_owned())?;
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > MAX_INPUT_PIXELS {
        anyhow::bail!(
            "{}",
            t!(
                "web.playground.codec.pixels_exceeded",
                width = w,
                height = h
            )
        );
    }
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)
        .with_context(|| t!("web.playground.codec.decode_pixels").into_owned())?;
    img.apply_orientation(orientation);
    Ok(img)
}

fn validate_animation_dimensions(
    width: u32,
    height: u32,
    max_total_bytes: usize,
) -> anyhow::Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!(
            "{}",
            t!(
                "web.playground.codec.anim_invalid_dimensions",
                width = width,
                height = height
            )
        );
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_INPUT_PIXELS {
        anyhow::bail!(
            "{}",
            t!(
                "web.playground.codec.anim_frame_pixels_exceeded",
                width = width,
                height = height
            )
        );
    }
    let frame_bytes = pixels.saturating_mul(4);
    if frame_bytes > max_total_bytes as u64 {
        anyhow::bail!(
            "{}",
            t!(
                "web.playground.codec.anim_frame_memory_exceeded",
                width = width,
                height = height
            )
        );
    }
    Ok(())
}

fn collect_bounded_frames(
    frames: Frames<'_>,
    max_frames: usize,
    max_total_pixels: u64,
    max_total_bytes: usize,
) -> anyhow::Result<(u32, u32, Vec<AnimFrame>)> {
    let mut out = Vec::new();
    let mut width = 0u32;
    let mut height = 0u32;
    let mut total_pixels = 0u64;
    let mut total_bytes = 0usize;

    for frame_result in frames {
        if out.len() >= max_frames {
            anyhow::bail!(
                "{}",
                t!(
                    "web.playground.codec.anim_frames_exceeded",
                    max = max_frames
                )
            );
        }
        let frame = frame_result
            .with_context(|| t!("web.playground.codec.anim_frame_read").into_owned())?;
        let (num, den) = frame.delay().numer_denom_ms();
        let delay_ms = if den == 0 { 100 } else { (num / den).max(20) };
        let buf: RgbaImage = frame.into_buffer();
        let (fw, fh) = (buf.width(), buf.height());
        if out.is_empty() {
            width = fw;
            height = fh;
        }
        let frame_pixels = u64::from(fw) * u64::from(fh);
        total_pixels = total_pixels.saturating_add(frame_pixels);
        if total_pixels > max_total_pixels {
            anyhow::bail!(
                "{}",
                t!(
                    "web.playground.codec.anim_total_pixels_exceeded",
                    max = max_total_pixels
                )
            );
        }
        let rgba = buf.into_raw();
        total_bytes = total_bytes.saturating_add(rgba.len());
        if total_bytes > max_total_bytes {
            anyhow::bail!(
                "{}",
                t!(
                    "web.playground.codec.anim_total_memory_exceeded",
                    max_mib = max_total_bytes / (1024 * 1024)
                )
            );
        }
        out.push(AnimFrame { rgba, delay_ms });
    }

    if out.is_empty() {
        anyhow::bail!("{}", t!("web.playground.codec.anim_no_frames"));
    }
    Ok((width, height, out))
}

fn decode_animation_frames(bytes: &[u8]) -> anyhow::Result<(u32, u32, Vec<AnimFrame>)> {
    decode_animation_frames_with_limits(
        bytes,
        MAX_ANIMATION_FRAMES,
        MAX_ANIMATION_TOTAL_PIXELS,
        MAX_ANIMATION_TOTAL_BYTES,
    )
}

fn decode_animation_frames_with_limits(
    bytes: &[u8],
    max_frames: usize,
    max_total_pixels: u64,
    max_total_bytes: usize,
) -> anyhow::Result<(u32, u32, Vec<AnimFrame>)> {
    let format = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .format();
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);

    match format {
        Some(ImageFormat::Gif) => {
            let mut decoder = GifDecoder::new(Cursor::new(bytes))
                .with_context(|| t!("web.playground.codec.gif_decode").into_owned())?;
            let (w, h) = decoder.dimensions();
            validate_animation_dimensions(w, h, max_total_bytes)?;
            decoder
                .set_limits(limits)
                .with_context(|| t!("web.playground.codec.gif_limits").into_owned())?;
            collect_bounded_frames(
                decoder.into_frames(),
                max_frames,
                max_total_pixels,
                max_total_bytes,
            )
        }
        Some(ImageFormat::Png) => {
            let mut decoder = PngDecoder::new(Cursor::new(bytes))
                .with_context(|| t!("web.playground.codec.apng_decode").into_owned())?;
            let (w, h) = decoder.dimensions();
            validate_animation_dimensions(w, h, max_total_bytes)?;
            decoder
                .set_limits(limits)
                .with_context(|| t!("web.playground.codec.apng_limits").into_owned())?;
            let apng = decoder
                .apng()
                .with_context(|| t!("web.playground.codec.apng_invalid").into_owned())?;
            collect_bounded_frames(
                apng.into_frames(),
                max_frames,
                max_total_pixels,
                max_total_bytes,
            )
        }
        Some(ImageFormat::WebP) => {
            let decoder = WebPDecoder::new(Cursor::new(bytes))
                .with_context(|| t!("web.playground.codec.webp_anim_decode").into_owned())?;
            let (w, h) = decoder.dimensions();
            validate_animation_dimensions(w, h, max_total_bytes)?;
            collect_bounded_frames(
                decoder.into_frames(),
                max_frames,
                max_total_pixels,
                max_total_bytes,
            )
        }
        _ => anyhow::bail!("{}", t!("web.playground.codec.anim_unsupported_format")),
    }
}

fn image_has_alpha(img: &DynamicImage) -> bool {
    img.to_rgba8().pixels().any(|p| p.0[3] != 255)
}

fn encode_webp_static(img: &DynamicImage, quality: u8, lossless: bool) -> anyhow::Result<Vec<u8>> {
    let rgba = img.to_rgba8();
    let encoder = WebpEncoder::new_rgba(rgba.as_raw(), rgba.width(), rgba.height())
        .lossless(lossless)
        .quality(f32::from(quality.clamp(1, 100)));
    let bytes = encoder.encode(Unstoppable).map_err(|error| {
        anyhow::anyhow!("{}", t!("web.playground.codec.webp_encode", error = error))
    })?;
    Ok(bytes)
}

fn encode_animated_webp(
    width: u32,
    height: u32,
    frames: &[AnimFrame],
    quality: u8,
) -> anyhow::Result<Vec<u8>> {
    let mut encoder = AnimationEncoder::new(width, height).map_err(|error| {
        anyhow::anyhow!(
            "{}",
            t!("web.playground.codec.webp_anim_init", error = error)
        )
    })?;
    encoder.set_quality(f32::from(quality.clamp(1, 100)));
    let mut timestamp_ms: i32 = 0;
    for frame in frames {
        encoder
            .add_frame_rgba(&frame.rgba, timestamp_ms)
            .map_err(|error| {
                anyhow::anyhow!(
                    "{}",
                    t!("web.playground.codec.webp_anim_frame", error = error)
                )
            })?;
        timestamp_ms = timestamp_ms.saturating_add(frame.delay_ms.min(60_000) as i32);
    }
    encoder.finish(timestamp_ms).map_err(|error| {
        anyhow::anyhow!(
            "{}",
            t!("web.playground.codec.webp_anim_finish", error = error)
        )
    })
}

fn encode_apng(width: u32, height: u32, frames: &[AnimFrame]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Balanced);
        encoder
            .set_animated(frames.len() as u32, 0)
            .with_context(|| t!("web.playground.codec.apng_anim_header").into_owned())?;
        let mut writer = encoder
            .write_header()
            .with_context(|| t!("web.playground.codec.apng_write_header").into_owned())?;
        for frame in frames {
            let delay = frame.delay_ms.min(u32::from(u16::MAX)) as u16;
            writer
                .set_frame_delay(delay, 1000)
                .with_context(|| t!("web.playground.codec.apng_frame_delay").into_owned())?;
            writer
                .write_image_data(&frame.rgba)
                .with_context(|| t!("web.playground.codec.apng_frame_data").into_owned())?;
        }
        writer
            .finish()
            .with_context(|| t!("web.playground.codec.apng_finish").into_owned())?;
    }
    Ok(out)
}

fn encode_avif(img: &DynamicImage, quality: u8) -> anyhow::Result<Vec<u8>> {
    let rgba = img.to_rgba8();
    let mut out = Vec::new();
    // speed=6 在单图体积与 CPU 耗时（约 1-2s）之间取实用平衡。
    let encoder = AvifEncoder::new_with_speed_quality(&mut out, 6, quality.clamp(1, 100));
    encoder
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .with_context(|| t!("web.playground.codec.avif_encode").into_owned())?;
    Ok(out)
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> anyhow::Result<Vec<u8>> {
    // 若存在透明通道则先铺白底合成，避免透明像素变黑。
    let rgba = img.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = u16::from(px.0[3]);
        let inv = 255 - a;
        let r = ((u16::from(px.0[0]) * a + 255 * inv) / 255) as u8;
        let g = ((u16::from(px.0[1]) * a + 255 * inv) / 255) as u8;
        let b = ((u16::from(px.0[2]) * a + 255 * inv) / 255) as u8;
        rgb.put_pixel(x, y, image::Rgb([r, g, b]));
    }
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
        .encode_image(&rgb)
        .with_context(|| t!("web.playground.codec.jpeg_encode").into_owned())?;
    Ok(out)
}

fn encode_png(img: &DynamicImage) -> anyhow::Result<Vec<u8>> {
    let rgba = img.to_rgba8();
    let mut out = Vec::new();
    let encoder =
        PngEncoder::new_with_quality(&mut out, CompressionType::Best, FilterType::Adaptive);
    encoder
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .with_context(|| t!("web.playground.codec.png_encode").into_owned())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png() -> Vec<u8> {
        let mut img = RgbaImage::new(32, 24);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x * 8) as u8, (y * 10) as u8, 128, 255]);
        }
        encode_png(&DynamicImage::ImageRgba8(img)).unwrap()
    }

    fn sample_animated_gif() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = gif::Encoder::new(&mut out, 8, 8, &[]).unwrap();
            enc.set_repeat(gif::Repeat::Infinite).unwrap();
            for c in [0u8, 200u8] {
                let mut pixels = vec![c; 8 * 8 * 4];
                let mut frame = gif::Frame::from_rgba_speed(8, 8, &mut pixels, 10);
                frame.delay = 10;
                enc.write_frame(&frame).unwrap();
            }
        }
        out
    }

    #[test]
    fn static_transcode_supports_webp_avif_jpeg_and_png() {
        let png = sample_png();
        for (format, expected_ext) in [
            (OutputCompressionFormat::Webp, "webp"),
            (OutputCompressionFormat::WebpLossless, "webp"),
            (OutputCompressionFormat::Avif, "avif"),
            (OutputCompressionFormat::Jpeg, "jpg"),
            (OutputCompressionFormat::Png, "png"),
        ] {
            let res = transcode_output(
                png.clone(),
                Some("image/png"),
                &OutputCompressionConfig {
                    format,
                    quality: 80,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(res.extension, expected_ext);
            assert_eq!((res.width, res.height), (32, 24));
            assert!(!res.is_animated);
            assert!(!res.bytes.is_empty());
        }
    }

    #[test]
    fn animated_gif_preserves_animation_across_modes() {
        let gif_bytes = sample_animated_gif();
        let meta = inspect_image(&gif_bytes, Some("image/gif")).unwrap();
        assert!(meta.is_animated);
        assert_eq!(meta.format, "gif");

        // 即使静态格式为 jpeg，动图也不应被压成静态图。
        let keep = transcode_output(
            gif_bytes.clone(),
            Some("image/gif"),
            &OutputCompressionConfig {
                format: OutputCompressionFormat::Jpeg,
                animated_format: AnimatedOutputFormat::Original,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(keep.is_animated);
        assert_eq!(keep.format, "gif");

        let webp_anim = transcode_output(
            gif_bytes.clone(),
            Some("image/gif"),
            &OutputCompressionConfig {
                animated_format: AnimatedOutputFormat::WebpAnimated,
                animated_quality: 75,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(webp_anim.is_animated);
        assert_eq!(webp_anim.extension, "webp");

        let apng = transcode_output(
            gif_bytes,
            Some("image/gif"),
            &OutputCompressionConfig {
                animated_format: AnimatedOutputFormat::Apng,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(apng.is_animated);
        assert_eq!(apng.format, "apng");
        assert!(inspect_image(&apng.bytes, None).unwrap().is_animated);
    }

    #[test]
    fn inspect_image_rejects_unrecognized_or_svg_bytes() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"></svg>"#;
        assert!(inspect_image(svg, Some("image/svg+xml")).is_err());
        assert!(inspect_image(svg, Some("image/png")).is_err());
        assert!(inspect_image(b"not an image", None).is_err());
    }

    #[test]
    fn decode_animation_frames_enforces_frame_and_pixel_limits() {
        let gif_bytes = sample_animated_gif();
        // 2 帧、每帧 8x8 = 64 像素（共 128 像素）
        assert!(decode_animation_frames_with_limits(&gif_bytes, 1, 1_000, 1024 * 1024).is_err());
        assert!(decode_animation_frames_with_limits(&gif_bytes, 10, 100, 1024 * 1024).is_err());
        assert!(decode_animation_frames_with_limits(&gif_bytes, 10, 1_000, 300).is_err());
        let (w, h, frames) =
            decode_animation_frames_with_limits(&gif_bytes, 10, 1_000, 1024 * 1024).unwrap();
        assert_eq!((w, h, frames.len()), (8, 8, 2));
    }
}

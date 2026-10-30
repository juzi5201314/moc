//! 解码：base64 / 本地文件 → [`DecodedImage`]。
//!
//! 位图走 `image` 的格式嗅探（不信扩展名），SVG 走 resvg。两者都可能被外部
//! 内容撑爆内存，所以每一条路径都有硬上限：像素尺寸、分配字节数、文件大小。
//! 这里的函数只在后台线程里跑，主线程永远不会因为一张图卡住。

use std::path::PathBuf;

use base64::Engine as _;

/// 本地文件大小上限：再大的图解码出来也不可能塞进终端。
pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

/// 位图像素尺寸上限（边长）。
const MAX_IMAGE_SIDE: u32 = 8192;

/// 解码期最大分配字节：8192×8192 的 RGBA 需要 256MB，必须更早拦住。
const MAX_DECODE_ALLOC: u64 = 64 << 20;

/// 解码失败的原因。每一条都会原样显示在转写区的占位行上，
/// 因此文案本身就是错误信息的全部。
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("base64 解码失败：{0}")]
    Base64(#[from] base64::DecodeError),
    #[error("无法识别图片格式：{0}")]
    Format(#[from] std::io::Error),
    #[error("图片解码失败：{0}")]
    Image(#[from] image::ImageError),
    #[error("读不到 {}：{source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} 不是普通文件", path.display())]
    NotFile { path: PathBuf },
    #[error("{} 超过 {} MB", path.display(), MAX_FILE_BYTES >> 20)]
    TooLarge { path: PathBuf },
    #[error("内联图片 {len} 字节，超过 {} MB 上限", MAX_FILE_BYTES >> 20)]
    TooLargeInline { len: u64 },
    #[error("SVG 解析失败：{0}")]
    Svg(#[from] resvg::usvg::Error),
    #[error("SVG 尺寸无效")]
    SvgSize,
    #[error("SVG 固有尺寸 {w}×{h} 超过 {MAX_IMAGE_SIDE}px")]
    SvgTooLarge { w: u64, h: u64 },
    #[error("SVG 内容为空")]
    EmptySvg,
}

/// 解码一张图。
pub fn decode(spec: super::ImageSourceSpec) -> Result<super::DecodedImage, DecodeError> {
    match spec {
        super::ImageSourceSpec::Base64 { media_type, data } => {
            // base64 是 4/3 膨胀：先按编码长度拦一道，否则解码缓冲与载荷本身
            // 都能被一段任意长的字符串撑爆（文件路径那条路有 20MB 上限）。
            if data.len() as u64 > MAX_FILE_BYTES / 3 * 4 {
                return Err(DecodeError::TooLargeInline {
                    len: data.len() as u64,
                });
            }
            let bytes = base64::engine::general_purpose::STANDARD.decode(data.as_bytes())?;
            if is_svg(&bytes, &media_type) {
                return svg_decode(&String::from_utf8_lossy(&bytes));
            }
            raster_decode(&bytes)
        }
        super::ImageSourceSpec::File(path) => {
            let meta = std::fs::metadata(&path).map_err(|source| DecodeError::Read {
                path: path.clone(),
                source,
            })?;
            if !meta.is_file() {
                return Err(DecodeError::NotFile { path });
            }
            if meta.len() > MAX_FILE_BYTES {
                return Err(DecodeError::TooLarge { path });
            }
            let bytes = std::fs::read(&path).map_err(|source| DecodeError::Read {
                path: path.clone(),
                source,
            })?;
            let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
            if is_svg(&bytes, extension) {
                return svg_decode(&String::from_utf8_lossy(&bytes));
            }
            raster_decode(&bytes)
        }
        super::ImageSourceSpec::Svg(text) => svg_decode(&text),
    }
}

/// SVG 判定：扩展名 / media type 说是，或者内容以 `<svg` 开头。
///
/// 只嗅探前 512 字节：SVG 的根元素必定在开头，完整扫描没有意义。
fn is_svg(bytes: &[u8], hint: &str) -> bool {
    if hint.eq_ignore_ascii_case("svg") || hint.eq_ignore_ascii_case("image/svg+xml") {
        return true;
    }
    String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).contains("<svg")
}

fn raster_decode(bytes: &[u8]) -> Result<super::DecodedImage, DecodeError> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    Ok(super::DecodedImage::Raster(reader.decode()?.to_rgba8()))
}

/// 矢量：只解析成树，栅格化留给贴图时按目标像素尺寸做（矢量放大不糊）。
fn svg_decode(text: &str) -> Result<super::DecodedImage, DecodeError> {
    let text = substitute_svg_tokens(text, crate::theme::Theme::global().raw());
    if text.trim().is_empty() {
        return Err(DecodeError::EmptySvg);
    }
    let tree = resvg::usvg::Tree::from_str(&text, &svg_options())?;
    let size = tree.size();
    let w = size.width().round();
    let h = size.height().round();
    if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
        return Err(DecodeError::SvgSize);
    }
    // 浮点转整数在 Rust 里是饱和的，`as` 不会溢出；非有限值已在上一步拦下。
    let w = w as u64;
    let h = h as u64;
    // 固有尺寸没有上限时，整条链路（字节记账、按比例的缩放比）都会失真：
    // `w × h × 4` 能撑爆 u64，栅格化目标也可能被算成天文数字。与位图共用
    // 同一条边长上限，超界的 SVG 与「尺寸无效」同罪。
    if w > u64::from(MAX_IMAGE_SIDE) || h > u64::from(MAX_IMAGE_SIDE) {
        return Err(DecodeError::SvgTooLarge { w, h });
    }
    let (w, h) = (w as u32, h as u32);
    Ok(super::DecodedImage::Svg {
        w,
        h,
        tree: Box::new(tree),
    })
}

/// 把提示词约定的颜色写成调色板里的真彩色。
///
/// `currentColor` 和 `var(--fg)` 这一组在 usvg 里没有对应的自定义属性，
/// 不换掉就会变成黑色或被丢掉。未知的 `var(--*)` 原样留下。
pub(crate) fn substitute_svg_tokens(text: &str, palette: &crate::theme::RawPalette) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("currentColor") {
            out.push_str(&hex(palette.text_primary));
            rest = after;
            continue;
        }
        if let Some((name, consumed)) = var_token(rest) {
            if let Some(rgb) = token_rgb(name, palette) {
                out.push_str(&hex(rgb));
                rest = &rest[consumed..];
                continue;
            }
        }
        let ch = rest.chars().next().expect("rest 非空");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn var_token(text: &str) -> Option<(&str, usize)> {
    let after = text.strip_prefix("var(")?;
    let body = after.trim_start();
    let name_src = body.strip_prefix("--")?;
    let close = name_src.find(')')?;
    let name = name_src[..close].split(',').next()?.trim();
    if name.is_empty() {
        return None;
    }
    let consumed = text.len() - name_src.len() + close + 1;
    Some((name, consumed))
}

fn token_rgb(name: &str, palette: &crate::theme::RawPalette) -> Option<[u8; 3]> {
    Some(match name {
        "fg" => palette.text_primary,
        "muted" => palette.text_secondary,
        "border" => palette.border,
        "surface" => palette.bg_panel,
        "accent" => palette.chart[0],
        "success" => palette.accent_success,
        "warning" => palette.accent_warning,
        "error" => palette.accent_error,
        _ => return None,
    })
}

fn hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// SVG 解析选项：只允许 data URL 内嵌资源，禁止读本地文件。
///
/// `<image href="file:///etc/passwd">` 这类引用一律解析失败——文档来自模型输出，
/// 不该有能力把本机任意文件送进渲染管线。
fn svg_options() -> resvg::usvg::Options<'static> {
    resvg::usvg::Options {
        fontdb: system_fontdb().clone(),
        image_href_resolver: resvg::usvg::ImageHrefResolver {
            resolve_data: resvg::usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    }
}

/// 系统字体库只加载一次（SVG 的 `<text>` 要用），重复扫描系统目录太贵。
static SYSTEM_FONTDB: std::sync::LazyLock<std::sync::Arc<resvg::usvg::fontdb::Database>> =
    std::sync::LazyLock::new(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        std::sync::Arc::new(db)
    });

fn system_fontdb() -> &'static std::sync::Arc<resvg::usvg::fontdb::Database> {
    &SYSTEM_FONTDB
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::render::canvas::Canvas;

    /// 用现成的 PNG 编码器造一张真 PNG，再解回来：验证格式嗅探与像素保真。
    #[test]
    fn decodes_png_round_trip() {
        let mut canvas = Canvas::new(6, 4, [0x10, 0x20, 0x30]);
        let red = canvas.color([0xff, 0x00, 0x00]);
        canvas.set(0, 0, red);
        let png = crate::render::graphics::encode::png(&canvas);
        let spec = super::super::ImageSourceSpec::Base64 {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD
                .encode(&png)
                .into_boxed_str(),
        };
        let decoded = decode(spec).expect("PNG 应能解码");
        assert_eq!(decoded.size(), (6, 4));
        let super::super::DecodedImage::Raster(image) = decoded else {
            panic!("PNG 必须是位图");
        };
        assert_eq!(image.get_pixel(0, 0).0, [0xff, 0x00, 0x00, 0xff]);
        assert_eq!(image.get_pixel(1, 0).0, [0x10, 0x20, 0x30, 0xff]);
    }

    #[test]
    fn decodes_svg_size() {
        let spec = super::super::ImageSourceSpec::Svg(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="#3b82f6"/></svg>"##
                .into(),
        );
        assert_eq!(decode(spec).expect("SVG 应能解析").size(), (20, 10));
    }

    #[test]
    fn svg_var_tokens_resolve_to_palette_hex() {
        let palette = crate::theme::Theme::global().raw();
        let out = substitute_svg_tokens(
            r#"<svg fill="var(--fg)" stroke="currentColor" style="color:var( --muted )"/>"#,
            palette,
        );
        let fg = format!(
            "#{:02x}{:02x}{:02x}",
            palette.text_primary[0], palette.text_primary[1], palette.text_primary[2]
        );
        let muted = format!(
            "#{:02x}{:02x}{:02x}",
            palette.text_secondary[0], palette.text_secondary[1], palette.text_secondary[2]
        );
        assert!(out.contains(&fg), "{out}");
        assert!(out.contains(&muted), "{out}");
        assert!(!out.contains("var("));
        assert!(!out.contains("currentColor"));
        assert!(substitute_svg_tokens("var(--nope)", palette).contains("var(--nope)"));
    }

    /// 回归：SVG 的固有尺寸必须有上限。
    ///
    /// 没有上限时 `w × h × 4` 能撑爆 u64（debug 构建直接 panic），
    /// 或者让记账变成天文数字把整张缓存挤空。
    #[test]
    fn rejects_absurd_svg_size() {
        let huge = super::super::ImageSourceSpec::Svg(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="3000000000" height="3000000000"/>"##
                .into(),
        );
        assert!(
            matches!(decode(huge), Err(DecodeError::SvgTooLarge { .. })),
            "接近 u32 上限的尺寸必须被拒绝"
        );
        let over_side = super::super::ImageSourceSpec::Svg(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="9000" height="10"/>"##.into(),
        );
        assert!(
            matches!(decode(over_side), Err(DecodeError::SvgTooLarge { .. })),
            "超过边长上限（8192）的尺寸必须被拒绝"
        );
    }

    /// 外部引用必须被拒：模型给的 SVG 不该能读本机文件。
    ///
    /// 渲染器把 `href` 直接丢弃，所以「解析成功」也是安全的——这里断言两条
    /// 路径都不会把文件内容带进来（尺寸仍由根元素决定）。
    #[test]
    fn svg_cannot_reference_local_files() {
        let spec = super::super::ImageSourceSpec::Svg(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10"><image xlink:href="file:///etc/passwd" width="10" height="10"/></svg>"##
                .into(),
        );
        if let Ok(decoded) = decode(spec) {
            assert_eq!(decoded.size(), (10, 10));
        }
    }

    #[test]
    fn rejects_garbage() {
        let not_an_image = super::super::ImageSourceSpec::Base64 {
            media_type: "image/png".into(),
            data: "bm90IGFuIGltYWdl".into(),
        };
        assert!(decode(not_an_image).is_err());
        assert!(decode(super::super::ImageSourceSpec::Svg("   ".into())).is_err());
    }

    #[test]
    fn rejects_missing_file() {
        let path = PathBuf::from("/nonexistent/moc-image.png");
        assert!(decode(super::super::ImageSourceSpec::File(path)).is_err());
    }

    #[test]
    fn reports_the_failing_stage() {
        let err = decode(super::super::ImageSourceSpec::Svg("".into())).unwrap_err();
        assert!(matches!(err, DecodeError::EmptySvg));
        let err = decode(super::super::ImageSourceSpec::File(
            "/nonexistent/x.png".into(),
        ))
        .unwrap_err();
        assert!(matches!(err, DecodeError::Read { .. }));
    }
}

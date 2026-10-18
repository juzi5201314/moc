//! 终端图形协议的像素载荷编码：kitty / sixel / iTerm2，外加 PNG 编码。
//!
//! 三个协议都直接吃 [`Canvas`] 的调色板索引平面（sixel 原生就是索引色），
//! 不经过二次量化；kitty 要真彩色 RGB，用 `f=24,o=z`（原始像素 + zlib）。
//!
//! 画布尺寸恒等于「格数 × 单元格像素尺寸」，终端拿到 `c=`/`r=` 后按整数格
//! 落位，不做缩放——这是文字不虚化的前提。

use std::io::Write;

use base64::Engine as _;

use super::canvas::Canvas;
use crate::render::graphics::GraphicsProto;

/// 一次图形载荷：三种协议各自的线上表示。
///
/// 传输体积大（sixel 是逐像素字节流、kitty 是压缩后的 base64），因此生成一次
/// 后按 `Arc` 缓存，重发时不重新编码。
pub enum Payload {
    /// kitty 的传输控制串（含全部分块），只发一次；放置另走
    /// [`kitty_place`]，删除走 [`kitty_delete`]。
    Kitty(String),
    /// sixel 完整字节流（含调色板与全部色带），重发即整串重发。
    Sixel(Vec<u8>),
    /// iTerm2 内联图片串（PNG + base64），重发即整串重发。
    Iterm2(String),
}

/// 载荷可能上百 KB：调试输出只报类型与字节数，不把像素数据灌进日志。
impl std::fmt::Debug for Payload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Payload::{:?}({} bytes)",
            self.proto(),
            self.as_bytes().len()
        )
    }
}

impl Payload {
    /// 该载荷对应的协议。
    pub fn proto(&self) -> GraphicsProto {
        match self {
            Self::Kitty(_) => GraphicsProto::Kitty,
            Self::Sixel(_) => GraphicsProto::Sixel,
            Self::Iterm2(_) => GraphicsProto::Iterm2,
        }
    }

    /// 线上字节（写 stdout 用）。
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Kitty(text) | Self::Iterm2(text) => text.as_bytes(),
            Self::Sixel(bytes) => bytes,
        }
    }
}

/// 按协议编码：`id` 是 kitty 的图像 ID（sixel/iTerm2 直接内联，不需要 ID），
/// `cols`/`rows` 是终端格数（iTerm2 用它们声明显示尺寸）。
pub fn encode(canvas: &Canvas, proto: GraphicsProto, id: u32, cols: u16, rows: u16) -> Payload {
    match proto {
        GraphicsProto::Kitty => Payload::Kitty(kitty_transmit(canvas, id)),
        GraphicsProto::Sixel => Payload::Sixel(sixel(canvas)),
        GraphicsProto::Iterm2 => Payload::Iterm2(iterm2(canvas, cols, rows)),
    }
}

/// kitty 单个分块的 payload 上限（规范建议 4096 字节 base64）。
const KITTY_CHUNK: usize = 4096;

/// kitty 传输串：`a=t` 只传输不显示，`f=24` 原始 RGB，`o=z` zlib 压缩。
///
/// 分块规则按规范：首块带全部键，后续块只带 `m`，最后一块 `m=0`。
/// `q=2` 关掉应答，避免终端把 OK 写回输入流。
fn kitty_transmit(canvas: &Canvas, id: u32) -> String {
    let compressed = zlib(canvas.rgb());
    let payload = base64::engine::general_purpose::STANDARD.encode(&compressed);
    let chunks: Vec<&str> = payload
        .as_bytes()
        .chunks(KITTY_CHUNK)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64 是 ASCII"))
        .collect();
    let last = chunks.len().saturating_sub(1);
    let mut out = String::with_capacity(payload.len() + chunks.len() * 16);
    for (index, chunk) in chunks.iter().enumerate() {
        out.push_str("\x1b_G");
        if index == 0 {
            out.push_str(&format!(
                "f=24,o=z,s={},v={},a=t,i={id},q=2,",
                canvas.w(),
                canvas.h()
            ));
        }
        out.push_str(if index == last { "m=0;" } else { "m=1;" });
        out.push_str(chunk);
        out.push_str("\x1b\\");
    }
    out
}

/// kitty 放置：把已传输的图像按整数格贴到当前光标位置。
pub fn kitty_place(id: u32, cols: u16, rows: u16) -> String {
    format!("\x1b_Ga=p,i={id},p={id},c={cols},r={rows},q=2\x1b\\")
}

/// kitty 删除：按图像 ID 删掉全部放置（离开备用屏前与图像变回表格时用）。
pub fn kitty_delete(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={id},q=2\x1b\\")
}

/// kitty 隐藏：只删放置、保留已传输的图像数据。
///
/// 图像换位与暂时离开视口都走它——重放时不必重新传输几十 KB 的像素数据。
/// 同时带 `i` 与 `p`：部分终端只按 placement id 删除时会漏掉没有 `i` 的请求，
/// 两个键一起给才能稳定命中同一张图。
pub fn kitty_hide(id: u32) -> String {
    format!("\x1b_Ga=d,d=p,i={id},p={id},q=2\x1b\\")
}

/// sixel 字节流。
///
/// 结构：`ESC P q` + 栅格属性 + 调色板定义 + 逐 6 行色带（同色 RLE，`$` 回车、
/// `-` 换带）+ `ESC \`。画布索引平面直接映射成 6 位掩码，无需量化。
fn sixel(canvas: &Canvas) -> Vec<u8> {
    let (w, h) = (canvas.w(), canvas.h());
    let palette = canvas.palette();
    let index = canvas.index();
    let mut out = Vec::with_capacity(palette.len() * 24 + index.len() / 4 + 64);
    out.extend_from_slice(b"\x1bPq");
    out.extend_from_slice(format!("\"1;1;{w};{h}").as_bytes());
    for (slot, rgb) in palette.iter().enumerate() {
        let percent = |value: u8| u32::from(value) * 100 / 255;
        out.extend_from_slice(
            format!(
                "#{slot};2;{};{};{}",
                percent(rgb[0]),
                percent(rgb[1]),
                percent(rgb[2])
            )
            .as_bytes(),
        );
    }
    // 每色带一张「槽位 → 每列 6 位掩码」的平面：一次扫描压完 6 行，
    // 之后每个槽位只扫自己那一行 w 字节，不重复读像素。
    let width = w as usize;
    let slots = palette.len();
    let mut plane = vec![0u8; slots * width];
    let mut used = vec![0u64; slots.div_ceil(64)];
    let mut y = 0;
    while y < h {
        plane.fill(0);
        used.fill(0);
        let rows = (h - y).min(6);
        for bit in 0..rows {
            let row = ((y + bit) * w) as usize;
            for x in 0..width {
                let slot = index[row + x] as usize;
                plane[slot * width + x] |= 1 << bit;
                used[slot >> 6] |= 1 << (slot & 63);
            }
        }
        let mut first = true;
        // 0 号槽位是画布底色，与终端单元格背景同色：不画就是它，省掉一半带宽。
        for slot in 1..slots {
            if used[slot >> 6] & (1 << (slot & 63)) == 0 {
                continue;
            }
            let band = &plane[slot * width..(slot + 1) * width];
            if !first {
                out.push(b'$');
            }
            out.extend_from_slice(format!("#{slot}").as_bytes());
            push_runs(&mut out, band);
            first = false;
        }
        out.push(b'-');
        y += rows;
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// 把一行 6 位掩码按 RLE 写进输出：`!<count><char>`，短段直接重复字符。
///
/// 行尾的空掩码（背景）全部丢弃——sixel 语义是「只画有像素的列」，
/// 省略即保持底色。
fn push_runs(out: &mut Vec<u8>, band: &[u8]) {
    let end = band
        .iter()
        .rposition(|&value| value != 0)
        .map_or(0, |at| at + 1);
    let mut index = 0;
    while index < end {
        let value = band[index];
        let mut run = 1;
        while index + run < end && band[index + run] == value {
            run += 1;
        }
        let ch = 0x3f + value;
        if run < 4 {
            out.extend(std::iter::repeat_n(ch, run));
        } else {
            out.extend_from_slice(format!("!{run}").as_bytes());
            out.push(ch);
        }
        index += run;
    }
}

/// iTerm2 内联图片：`OSC 1337;File=...` + base64(PNG) + BEL。
fn iterm2(canvas: &Canvas, cols: u16, rows: u16) -> String {
    let png = png(canvas);
    let payload = base64::engine::general_purpose::STANDARD.encode(&png);
    format!(
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=0:{payload}\x07",
        png.len()
    )
}

/// PNG（RGB 8 位、无滤波、单 IDAT）：iTerm2 与预览落盘共用。
pub fn png(canvas: &Canvas) -> Vec<u8> {
    let (w, h) = (canvas.w(), canvas.h());
    let stride = 3 * w as usize;
    let rgb = canvas.rgb();
    let mut raw = Vec::with_capacity((stride + 1) * h as usize);
    for row in 0..h as usize {
        raw.push(0); // filter type 0（None）：图表是纯色块，滤波省不下多少
        raw.extend_from_slice(&rgb[row * stride..(row + 1) * stride]);
    }
    let compressed = zlib(&raw);
    let mut out = Vec::with_capacity(compressed.len() + 64);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    push_chunk(&mut out, b"IHDR", &ihdr);
    push_chunk(&mut out, b"IDAT", &compressed);
    push_chunk(&mut out, b"IEND", &[]);
    out
}

/// PNG 块：长度 + 类型 + 数据 + CRC32（类型与数据）。
fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// zlib 压缩（PNG 的 IDAT 与 kitty 的 `o=z` 同一种格式）。
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).expect("写内存缓冲不会失败");
    encoder.finish().expect("收尾内存缓冲不会失败")
}

/// 标准 CRC-32（IEEE 802.3）：PNG 每个块都要带。
struct Crc32 {
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        Self { value: !0 }
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            let index = ((self.value ^ u32::from(byte)) & 0xFF) as usize;
            self.value = CRC_TABLE[index] ^ (self.value >> 8);
        }
    }

    fn finish(self) -> u32 {
        !self.value
    }
}

/// 256 项查表（编译期算好）：逐位算法每字节 8 次迭代，表格版 1 次。
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 {
                0xEDB8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
};

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试画布：三条不同颜色的竖条，覆盖「同带多色」与「末带不足 6 行」。
    fn canvas() -> Canvas {
        let mut canvas = Canvas::new(7, 8, [0x10, 0x10, 0x10]);
        let red = canvas.color([0xff, 0, 0]);
        let green = canvas.color([0, 0xff, 0]);
        canvas.fill_rect(0, 0, 3, 8, red);
        canvas.fill_rect(3, 0, 2, 5, green);
        canvas
    }

    /// sixel 的最小解码器：解回索引平面，用于逐像素比对（编码器的唯一可信证明）。
    fn decode_sixel(bytes: &[u8]) -> (u32, u32, Vec<[u8; 3]>, Vec<u8>) {
        let text = std::str::from_utf8(bytes).expect("sixel 是 ASCII");
        let body = text
            .strip_prefix("\x1bPq")
            .and_then(|rest| rest.strip_suffix("\x1b\\"))
            .expect("sixel 头尾固定");
        let mut palette: Vec<[u8; 3]> = Vec::new();
        let bytes = body.as_bytes();
        assert_eq!(bytes[0], b'"', "sixel 必须先给栅格属性");
        // 栅格属性 `"Pan;Pad;Ph;Pv`：图片宽高在最后两项
        let attr_end = body[1..]
            .find(|ch: char| !ch.is_ascii_digit() && ch != ';')
            .map_or_else(|| body.len(), |at| at + 1);
        let attrs = &body[1..attr_end];
        let fields: Vec<u32> = attrs
            .split(';')
            .map(|field| field.parse().expect("栅格属性是十进制"))
            .collect();
        let width = fields[2];
        let height = fields[3];
        let mut index = attr_end;
        let mut pixels = vec![0u8; (width * height) as usize];
        let mut color = 0usize;
        let mut x = 0u32;
        let mut band = 0u32;
        let mut repeat = 0usize;
        while index < bytes.len() {
            match bytes[index] {
                b'#' => {
                    index += 1;
                    let mut value = 0usize;
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        value = value * 10 + usize::from(bytes[index] - b'0');
                        index += 1;
                    }
                    if bytes.get(index) == Some(&b';') {
                        index += 1;
                        let start = index;
                        while index < bytes.len()
                            && (bytes[index].is_ascii_digit() || bytes[index] == b';')
                        {
                            index += 1;
                        }
                        let params: Vec<u8> = std::str::from_utf8(&bytes[start..index])
                            .expect("调色板参数是 ASCII")
                            .split(';')
                            .map(|field| field.parse().expect("调色板参数是十进制"))
                            .collect();
                        assert_eq!(params[0], 2, "只支持 RGB 色彩模式");
                        let convert = |percent: u8| (u32::from(percent) * 255 / 100) as u8;
                        let rgb = [convert(params[1]), convert(params[2]), convert(params[3])];
                        if palette.len() <= value {
                            palette.resize(value + 1, [0, 0, 0]);
                        }
                        palette[value] = rgb;
                    }
                    color = value;
                }
                b'!' => {
                    index += 1;
                    let mut value = 0usize;
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        value = value * 10 + usize::from(bytes[index] - b'0');
                        index += 1;
                    }
                    repeat = value;
                }
                b'$' => {
                    x = 0;
                    index += 1;
                }
                b'-' => {
                    band += 1;
                    x = 0;
                    index += 1;
                }
                byte if (0x3f..=0x7e).contains(&byte) => {
                    index += 1;
                    let bits = byte - 0x3f;
                    let count = repeat.max(1);
                    assert!(count <= 256, "段长 {count} 不可能超过图宽；sixel：{body:?}");
                    for _ in 0..count {
                        for bit in 0..6u32 {
                            let row = band * 6 + bit;
                            if bits & (1 << bit) != 0 && row < height && x < width {
                                pixels[(row * width + x) as usize] = color as u8;
                            }
                        }
                        x += 1;
                    }
                    repeat = 0;
                }
                other => panic!("未预期的 sixel 字节 {other:#x}"),
            }
        }
        (width, height, palette, pixels)
    }

    /// sixel 往返：解码出的索引平面必须与画布逐像素一致（含调色板槽位）。
    #[test]
    fn sixel_round_trips_pixel_exactly() {
        let canvas = canvas();
        let bytes = sixel(&canvas);
        let (width, height, palette, pixels) = decode_sixel(&bytes);
        assert_eq!((width, height), (7, 8), "栅格属性里的尺寸");
        // sixel 的调色板是 0–100 百分比，颜色按该粒度量化（像素索引不受影响）
        let quantize = |value: u8| {
            let percent = u32::from(value) * 100 / 255;
            (percent * 255 / 100) as u8
        };
        for (slot, rgb) in canvas.palette().iter().enumerate() {
            let expected = [quantize(rgb[0]), quantize(rgb[1]), quantize(rgb[2])];
            assert_eq!(palette[slot], expected, "第 {slot} 号调色板颜色");
        }
        assert_eq!(pixels, canvas.index(), "解码像素必须与原画布一致");
    }

    /// 空带也要推进：末带只有 2 行时不能把画布高度算错。
    #[test]
    fn sixel_keeps_trailing_partial_band() {
        let mut canvas = Canvas::new(5, 8, [0, 0, 0]);
        let color = canvas.color([0x20, 0x80, 0xf0]);
        canvas.fill_rect(0, 7, 5, 1, color);
        let (_, height, _, pixels) = decode_sixel(&sixel(&canvas));
        assert_eq!(height, 8);
        assert_eq!(pixels[7 * 5..8 * 5], [1u8; 5], "最后一行必须染色");
        assert_eq!(pixels[0..6 * 5], [0u8; 30], "其余行保持背景");
    }

    /// 噪声画布：8 色伪随机铺满，zlib 压不小，足以把 kitty 载荷顶到多个分块。
    fn noisy_canvas(w: u32, h: u32) -> Canvas {
        let mut canvas = Canvas::new(w, h, [0x10, 0x10, 0x10]);
        let slots: Vec<u8> = (0..8)
            .map(|step| {
                let step = step as u8;
                canvas.color([
                    step.wrapping_mul(31),
                    step.wrapping_mul(67),
                    step.wrapping_mul(97),
                ])
            })
            .collect();
        let mut state = 0x1234_5678u32;
        for y in 0..h {
            for x in 0..w {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                canvas.set(x, y, slots[(state >> 16) as usize & 7]);
            }
        }
        canvas
    }

    /// kitty：分块不超过 4096、`m` 标志正确、解出的 zlib 数据长度 = 3*w*h。
    #[test]
    fn kitty_chunks_carry_zlib_rgb() {
        let canvas = noisy_canvas(256, 160);
        let payload = kitty_transmit(&canvas, 7);
        let frames: Vec<&str> = payload.split("\x1b\\").filter(|f| !f.is_empty()).collect();
        assert!(frames.len() > 1, "应当分块：{} 块", frames.len());
        let mut joined = String::new();
        for (index, frame) in frames.iter().enumerate() {
            let body = frame.strip_prefix("\x1b_G").expect("控制串前缀");
            let (keys, chunk) = body.split_once(';').expect("键与载荷以分号分隔");
            if index == 0 {
                assert!(keys.contains("f=24,o=z"), "首块要声明格式：{keys}");
                assert!(keys.contains("s=256,v=160"), "首块要声明像素尺寸：{keys}");
                assert!(keys.contains("a=t,i=7,q=2"), "首块要声明动作与 ID：{keys}");
            } else if index + 1 == frames.len() {
                assert_eq!(keys, "m=0", "末块只带 m=0");
            } else {
                assert_eq!(keys, "m=1", "中间块只带 m=1");
            }
            assert!(chunk.len() <= KITTY_CHUNK, "分块不得超过 {KITTY_CHUNK}");
            joined.push_str(chunk);
        }
        let compressed = base64::engine::general_purpose::STANDARD
            .decode(&joined)
            .expect("base64 可解");
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::ZlibDecoder::new(compressed.as_slice()),
            &mut decoded,
        )
        .expect("zlib 可解");
        assert_eq!(
            decoded,
            canvas.rgb(),
            "解压后必须与原画布的 RGB 平面逐字节一致"
        );
    }

    /// kitty 放置、隐藏与删除：`a=p` 带格数与 ID，`d=p` 只删放置，`d=i` 连数据删。
    #[test]
    fn kitty_placement_and_delete_carry_image_id() {
        assert_eq!(
            kitty_place(9, 40, 12),
            "\x1b_Ga=p,i=9,p=9,c=40,r=12,q=2\x1b\\"
        );
        assert_eq!(kitty_hide(9), "\x1b_Ga=d,d=p,i=9,p=9,q=2\x1b\\");
        assert_eq!(kitty_delete(9), "\x1b_Ga=d,d=i,i=9,q=2\x1b\\");
    }

    /// PNG：头与 IHDR 尺寸正确，且能被 zlib 解出「每行 1+3w」的原始扫描线。
    #[test]
    fn png_has_valid_header_and_scanlines() {
        let canvas = canvas();
        let bytes = png(&canvas);
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&bytes[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 7);
        assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 8);
        let start = bytes
            .windows(4)
            .position(|window| window == b"IDAT")
            .expect("IDAT");
        let length =
            u32::from_be_bytes(bytes[start - 4..start].try_into().expect("IDAT 长度前缀")) as usize;
        let data = &bytes[start + 4..start + 4 + length];
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(data), &mut raw)
            .expect("IDAT 可解");
        assert_eq!(raw.len(), (1 + 3 * 7) * 8, "每行一个滤波字节 + 3*w");
        assert!(raw[0] == 0, "滤波类型 None");
        assert_eq!(&raw[1..4], &[0xff, 0, 0], "首行首像素是红色");
    }

    /// iTerm2：`OSC 1337;File` 头 + base64 的 PNG，尺寸与格数都写进键值。
    #[test]
    fn iterm2_inlines_png_with_cell_size() {
        let canvas = canvas();
        let text = iterm2(&canvas, 7, 4);
        let head = text
            .strip_prefix("\x1b]1337;File=inline=1;size=")
            .expect("OSC 头");
        let (size, rest) = head.split_once(';').expect("size 键");
        assert_eq!(size.parse::<usize>().unwrap(), png(&canvas).len());
        assert!(
            rest.contains("width=7;height=4;preserveAspectRatio=0:"),
            "{rest}"
        );
        let payload = rest
            .split_once(':')
            .expect("冒号分隔载荷")
            .1
            .strip_suffix('\x07')
            .expect("BEL 结尾");
        let png = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("base64 可解");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}

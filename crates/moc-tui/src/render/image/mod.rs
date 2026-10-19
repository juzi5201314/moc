//! 通用内联图片：内容寻址的解码缓存 + 编码载荷缓存。
//!
//! 一张图从字节到屏幕上的像素要过三关：解码（PNG/SVG → RGBA 或矢量树）、
//! 缩放（源图 → 格数 × 单元格像素的画布）、编码（画布 → 终端协议载荷）。
//! 三关都只跟「内容 + 目标像素尺寸」有关，因此全部可以按 [`ImageKey`] 缓存：
//! 滚动、重绘、同一张图贴多处都不会重复劳动。
//!
//! 解码在后台线程里做，主线程只查表；拿不到就先用占位行，好了由
//! [`crate::protocol::AppEvent::ImagePrepared`] 通知重绘。缓存按字节数 LRU
//! 淘汰，每帧被 `payload()` 触达的可见图永远不会被淘汰。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use xxhash_rust::xxh3::Xxh3;

use crate::protocol::AppEvent;
use crate::render::canvas::Canvas;
use crate::render::graphics::{GraphicsProto, encode};
use crate::theme::Theme;

pub mod decode;
pub mod layout;
pub mod quantize;

pub use layout::{ImageLayout, layout};

/// 缓存上限（字节）：解码后的 RGBA 与编码后的载荷都算在内。
///
/// 32MB 大约够 20 张 640×480 的图；超过就按 LRU 淘汰，可见图每帧被触达，
/// 因此淘汰的永远是刚翻过去的历史。
///
/// 单张图允许突破预算：淘汰只发生在**新增**条目时，且绝不挑刚落地的那个，
/// 否则「解码完成 → 立刻被自己挤掉 → 下一帧重新解码」会变成自持死循环。
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// SVG 树在缓存里的记账下限。
///
/// 矢量树的实际占用是几十 KB 量级，与它的固有像素尺寸无关；但也不能记成 0
/// ——那样它会被 `evict` 的 `bytes > 0` 过滤跳过，永远不参与淘汰。
const SVG_ENTRY_BYTES: usize = 4 * 1024;

/// 图片的稳定标识：键变即内容变。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageKey {
    /// 工具结果 / 协议内联：media type + base64 的 xxh3 摘要。
    Bytes { digest: u64 },
    /// 本地文件：规范化路径 + 修改时间纳秒 + 长度。
    File {
        path: Arc<PathBuf>,
        mtime_ns: i128,
        len: u64,
    },
    /// SVG 代码块：原文摘要。
    Svg { digest: u64 },
}

impl ImageKey {
    /// 槽位指纹的一部分：内容不同就不能复用上一帧的放置。
    pub fn digest(&self) -> u64 {
        match self {
            Self::Bytes { digest } | Self::Svg { digest } => *digest,
            Self::File {
                path,
                mtime_ns,
                len,
            } => {
                let mut hasher = Xxh3::new();
                hasher.update(path.as_os_str().as_encoded_bytes());
                hasher.update(&mtime_ns.to_le_bytes());
                hasher.update(&len.to_le_bytes());
                hasher.digest()
            }
        }
    }

    /// 由 base64 载荷算键：媒体类型参与摘要，避免同字节不同解释的串串门。
    pub fn from_bytes(media_type: &str, data: &str) -> Self {
        let mut hasher = Xxh3::new();
        hasher.update(media_type.as_bytes());
        hasher.update(b"\0");
        hasher.update(data.as_bytes());
        Self::Bytes {
            digest: hasher.digest(),
        }
    }

    /// 由 SVG 原文算键。
    pub fn from_svg(text: &str) -> Self {
        let mut hasher = Xxh3::new();
        hasher.update(text.as_bytes());
        Self::Svg {
            digest: hasher.digest(),
        }
    }
}

/// 交给后台线程的解码输入。
pub enum ImageSourceSpec {
    Base64 {
        media_type: Box<str>,
        data: Box<str>,
    },
    File(PathBuf),
    Svg(Box<str>),
}

/// 解码结果。位图保留 RGBA，矢量保留 usvg 树（按目标尺寸重栅格化）。
pub enum DecodedImage {
    Raster(image::RgbaImage),
    /// 矢量树装箱：usvg 的树结构比位图句柄大一个数量级，不装箱会把每个
    /// `DecodedImage` 都撑成几百字节。
    Svg {
        tree: Box<resvg::usvg::Tree>,
        w: u32,
        h: u32,
    },
}

impl DecodedImage {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Raster(image) => image.dimensions(),
            Self::Svg { w, h, .. } => (*w, *h),
        }
    }

    /// 在缓存里按多少字节记账。
    ///
    /// 位图就是 RGBA 平面的真实大小；矢量树记一个保守下限——它的实际占用与
    /// 固有像素尺寸无关，按 `w×h×4` 记账会把一张 3000×3000 的 SVG 记成 36MB，
    /// 而它真正占的只有几十 KB 的树结构（栅格化缓冲是贴图时临时的）。
    pub fn cache_bytes(&self) -> usize {
        match self {
            Self::Raster(image) => image.as_raw().len(),
            Self::Svg { .. } => SVG_ENTRY_BYTES,
        }
    }
}

/// 只报尺寸与类型：矢量树的 `Debug` 输出动辄几千行，日志里没有价值。
impl std::fmt::Debug for DecodedImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (w, h) = self.size();
        let kind = match self {
            Self::Raster(_) => "Raster",
            Self::Svg { .. } => "Svg",
        };
        write!(formatter, "DecodedImage::{kind}({w}×{h})")
    }
}

/// 已被主线程接管的解码结果：包一层好按 `Arc` 共享。
pub struct PreparedImage {
    decoded: DecodedImage,
}

impl PreparedImage {
    pub fn new(decoded: DecodedImage) -> Self {
        Self { decoded }
    }

    pub fn size(&self) -> (u32, u32) {
        self.decoded.size()
    }

    pub fn cache_bytes(&self) -> usize {
        self.decoded.cache_bytes()
    }
}

/// 一张图在缓存里的状态。
#[derive(Clone)]
pub enum ImageState {
    /// 已丢给后台线程，等 `ImagePrepared`。
    Pending,
    Ready(Arc<PreparedImage>),
    /// 解码失败（消息原样显示）。
    Failed(Arc<str>),
}

/// 一次渲染结果里的图片落位（行号是块内局部行）。
#[derive(Debug, Clone)]
pub struct ImagePlacement {
    pub row: usize,
    /// 相对转写区左缘的列。
    pub x: u16,
    pub cols: u16,
    pub rows: u16,
    pub px: (u32, u32),
    pub key: ImageKey,
}

/// 编码好的载荷：直接写 stdout 的字节 + 是否还需要传输像素数据。
pub struct EncodedImage {
    pub payload: Arc<encode::Payload>,
    pub image_id: u32,
    /// `true` = 终端里还没有这份像素数据（首次或像素尺寸变了），必须先写
    /// `payload`；否则只需重新放置。
    pub transmit: bool,
}

/// 一次编码缓存的键：协议 + 格数 + 像素尺寸。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct EncodedKey {
    proto: GraphicsProto,
    cols: u16,
    rows: u16,
    px: (u32, u32),
}

/// 同一张图最多保留几份不同尺寸的编码载荷。
///
/// 同一张图可能同时出现在两个宽度不同的位置（批次工具内 vs 独立工具），
/// 只留一份会每帧互相顶掉、重复缩放编码并重传像素；留几份就能各自命中。
/// 终端的格尺寸不会在多份之间反复横跳，超过这个数只可能是旧尺寸的残渣。
const MAX_ENCODED_VARIANTS: usize = 3;

struct Entry {
    state: ImageState,
    /// 编码载荷按 [`EncodedKey`] 缓存。
    encoded: HashMap<EncodedKey, Arc<encode::Payload>>,
    /// 终端里确实存在这份像素数据（`(协议, 像素尺寸)`）。
    ///
    /// 终端的图像数据只会因为按图像 ID 删除或整屏清空而消失；那两种情况下
    /// 必须调用 [`ImageStore::forget_transmission`]，否则图滚回来时只会重新
    /// 放置一个终端里并不存在的图像，屏幕上留下空白。
    transmitted: std::collections::HashSet<(GraphicsProto, (u32, u32))>,
    image_id: u32,
    last_used: u64,
    bytes: usize,
}

struct Inner {
    entries: HashMap<ImageKey, Entry>,
    /// 正在后台编码的 `(图, 尺寸)`：挡住同一份编码被连续几帧重复提交。
    encoding: std::collections::HashSet<(ImageKey, EncodedKey)>,
    next_id: u32,
    tick: u64,
}

impl Inner {
    /// 当前占用的字节数。条目数是个位到百位，现算比维护一个增量计数器更不容易
    /// 漂移；淘汰本身也只在新图落地时发生。
    fn bytes(&self) -> usize {
        self.entries.values().map(|entry| entry.bytes).sum()
    }

    /// 按 `last_used` 从旧到新淘汰，直到回到预算内。
    ///
    /// 每帧被 `payload()` 触达的可见图 `last_used` 最新，永远不会先被淘汰。
    /// `protect` 是刚刚落地/刚编码的那一张：它必须免于这一轮淘汰，否则
    /// 「解码完成 → 立刻被自己挤掉 → 下一帧重新解码」会变成自持死循环。
    fn evict(&mut self, protect: &ImageKey) {
        while self.bytes() > MAX_CACHE_BYTES {
            let victim = self
                .entries
                .iter()
                .filter(|(key, entry)| entry.bytes > 0 && *key != protect)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            // 只剩受保护的那张、或账面超预算却没有条目占字节：就此收手，
            // 允许单张超大图超出预算（它至少能显示出来）
            let Some(victim) = victim else {
                return;
            };
            self.entries.remove(&victim);
        }
    }

    /// 取一个进程内唯一的 kitty 图像 ID。
    fn take_image_id(&mut self) -> u32 {
        let id = self.next_id;
        // 0 在部分终端里是「无 ID」的哨兵值，回绕时也不能落回 0
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

/// 图片缓存。线程安全，克隆开销为零（内部全是 `Arc`）。
#[derive(Clone)]
pub struct ImageStore {
    inner: Arc<Mutex<Inner>>,
    events: tokio::sync::mpsc::UnboundedSender<AppEvent>,
}

impl ImageStore {
    pub fn new(events: tokio::sync::mpsc::UnboundedSender<AppEvent>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: HashMap::new(),
                encoding: std::collections::HashSet::new(),
                // 从 1 开始：0 在部分终端里是「无 ID」的哨兵值
                next_id: 1,
                tick: 0,
            })),
            events,
        }
    }

    /// 取一个进程内唯一的 kitty 图像 ID。
    ///
    /// 表格图表与通用图片共用同一个编号空间：kitty 里用已存在的 `i=` 重传会
    /// 释放旧图像并清掉它的全部放置，两套自增器各自从 1 起编号必然互相覆盖。
    pub fn allocate_image_id(&self) -> u32 {
        self.lock().take_image_id()
    }

    /// 查一张图的状态；未缓存则插入 `Pending` 并起后台线程解码。
    ///
    /// `make` 只在需要解码时调用一次，并且是在后台线程里调用——构造
    /// [`ImageSourceSpec`] 可能要拷贝整份 base64，不能让主线程承担。
    pub fn state(
        &self,
        key: &ImageKey,
        make: impl FnOnce() -> ImageSourceSpec + Send + 'static,
    ) -> ImageState {
        let mut spawn = false;
        let state = {
            let mut inner = self.lock();
            inner.tick += 1;
            let tick = inner.tick;
            if let Some(entry) = inner.entries.get_mut(key) {
                entry.last_used = tick;
                entry.state.clone()
            } else {
                let image_id = inner.take_image_id();
                inner.entries.insert(
                    key.clone(),
                    Entry {
                        state: ImageState::Pending,
                        encoded: HashMap::new(),
                        transmitted: std::collections::HashSet::new(),
                        image_id,
                        last_used: tick,
                        bytes: 0,
                    },
                );
                spawn = true;
                ImageState::Pending
            }
        };
        if spawn {
            self.spawn_decode(key.clone(), make);
        }
        state
    }

    /// 只读查询：估高路径用，绝不触发解码。
    pub fn peek(&self, key: &ImageKey) -> Option<ImageState> {
        let inner = self.lock();
        inner.entries.get(key).map(|entry| entry.state.clone())
    }

    /// 后台线程完成后由事件循环调用（主线程）。
    pub fn complete(&self, key: ImageKey, result: Result<DecodedImage, String>) {
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        let bytes = match &result {
            Ok(decoded) => decoded.cache_bytes(),
            Err(reason) => reason.len(),
        };
        let state = match result {
            Ok(decoded) => ImageState::Ready(Arc::new(PreparedImage::new(decoded))),
            Err(reason) => ImageState::Failed(reason.into()),
        };
        match inner.entries.get_mut(&key) {
            Some(entry) => {
                entry.bytes = bytes;
                entry.state = state;
                entry.last_used = tick;
            }
            // 解码期间被淘汰过：结果仍然有用，重新插入
            None => {
                let image_id = inner.take_image_id();
                inner.entries.insert(
                    key.clone(),
                    Entry {
                        state,
                        encoded: HashMap::new(),
                        transmitted: std::collections::HashSet::new(),
                        image_id,
                        last_used: tick,
                        bytes,
                    },
                );
            }
        }
        // 刚完成的那张免于本轮淘汰：它就是「超预算 → 删自己 → 再解码」的受害者
        inner.evict(&key);
    }

    /// 终端侧的像素数据已经没了（按图像 ID 删除、或整屏清空）。
    ///
    /// 不清掉这份记录的话，下一次放置只会重发 `a=p` 而不重传 `a=t`，
    /// 贴上去的是一张终端里并不存在的图——屏幕上是空白。
    pub fn forget_transmission(&self, image_id: u32) {
        for entry in self.lock().entries.values_mut() {
            if entry.image_id == image_id {
                entry.transmitted.clear();
            }
        }
    }

    /// 整屏清空后一次作废所有「已传输」记录。
    pub fn forget_all_transmissions(&self) {
        for entry in self.lock().entries.values_mut() {
            entry.transmitted.clear();
        }
    }

    /// 取某个协议与格尺寸下的编码载荷；未就绪或不可编码时返回 `None`。
    ///
    /// 编码结果按 `(proto, cols, rows, px)` 缓存：终端尺寸变化会重新编码，
    /// 同一尺寸的重复帧直接命中。
    ///
    /// 缩放 + 量化 + 编码是最重的一段（Lanczos3 缩放叠 NeuQuant，几十毫秒），
    /// 因此**不在主线程做**：首次遇到某个尺寸时起一个编码线程并返回 `None`，
    /// 完成后经 [`AppEvent::ImageEncoded`] 通知主线程重绘。代价是首帧看不到图，
    /// 换来的是滚动与输入不会因为一张大图卡住。
    pub fn payload(
        &self,
        key: &ImageKey,
        proto: GraphicsProto,
        cols: u16,
        rows: u16,
        px: (u32, u32),
        theme: &'static Theme,
    ) -> Option<EncodedImage> {
        let key_wanted = EncodedKey {
            proto,
            cols,
            rows,
            px,
        };
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        let mut pending = None;
        let encoded = {
            let entry = inner.entries.get_mut(key)?;
            entry.last_used = tick;
            let ImageState::Ready(prepared) = &entry.state else {
                return None;
            };
            match entry.encoded.get(&key_wanted) {
                Some(payload) => {
                    let transmit = !entry.transmitted.contains(&(proto, px));
                    entry.transmitted.insert((proto, px));
                    Some(EncodedImage {
                        payload: Arc::clone(payload),
                        image_id: entry.image_id,
                        transmit,
                    })
                }
                // 这个尺寸还没编码过：丢给后台线程，本帧先空着
                None => {
                    pending = Some((Arc::clone(prepared), entry.image_id));
                    None
                }
            }
        };
        inner.evict(key);
        drop(inner);
        if let Some((prepared, image_id)) = pending {
            self.spawn_encode(key.clone(), key_wanted, prepared, image_id, theme);
        }
        encoded
    }

    /// 起后台编码线程：缩放 + 量化 + 协议编码，完成后回填缓存并通知主线程。
    ///
    /// 同一 `(key, 尺寸)` 可能被连续几帧请求：编码期间缓存里仍然没有条目，
    /// 会重复起线程。用 `encoding` 集合挡住重复提交。
    fn spawn_encode(
        &self,
        key: ImageKey,
        wanted: EncodedKey,
        prepared: Arc<PreparedImage>,
        image_id: u32,
        theme: &'static Theme,
    ) {
        {
            let mut inner = self.lock();
            if !inner.encoding.insert((key.clone(), wanted)) {
                return;
            }
        }
        let store = self.clone();
        std::thread::Builder::new()
            .name("moc-image-encode".into())
            .spawn(move || {
                let canvas = prepare_canvas(&prepared.decoded, wanted.px, theme);
                let payload = Arc::new(encode::encode(
                    &canvas,
                    wanted.proto,
                    image_id,
                    wanted.cols,
                    wanted.rows,
                ));
                let mut inner = store.lock();
                inner.encoding.remove(&(key.clone(), wanted));
                if let Some(entry) = inner.entries.get_mut(&key) {
                    entry.encoded.insert(wanted, payload);
                    if entry.encoded.len() > MAX_ENCODED_VARIANTS {
                        // 超出上限时只留当前尺寸：格尺寸来回变化不值得为旧尺寸占内存
                        entry.encoded.retain(|cached, _| *cached == wanted);
                    }
                    entry.bytes = prepared.cache_bytes()
                        + entry
                            .encoded
                            .values()
                            .map(|p| p.as_bytes().len())
                            .sum::<usize>();
                }
                drop(inner);
                // 接收端已关闭（TUI 正在退出）就静默丢弃
                let _ = store.events.send(AppEvent::ImageEncoded { key });
            })
            .ok();
    }

    /// 提前把某个尺寸的编码排进后台线程。
    ///
    /// 与 [`ImageStore::payload`] 的区别是不改「已传输」记录、不返回载荷：
    /// 只负责把重活丢出去。生产路径由 `payload()` 自己触发，这个入口给测试用
    /// ——测试不能调 `emit`（它会消费放置记录），但仍需要等后台编码落地。
    #[cfg(test)]
    pub fn request_payload(
        &self,
        key: &ImageKey,
        proto: GraphicsProto,
        cols: u16,
        rows: u16,
        px: (u32, u32),
        theme: &'static Theme,
    ) {
        let wanted = EncodedKey {
            proto,
            cols,
            rows,
            px,
        };
        let pending = {
            let mut inner = self.lock();
            let Some(entry) = inner.entries.get_mut(key) else {
                return;
            };
            if entry.encoded.contains_key(&wanted) {
                return;
            }
            let ImageState::Ready(prepared) = &entry.state else {
                return;
            };
            Some((Arc::clone(prepared), entry.image_id))
        };
        if let Some((prepared, image_id)) = pending {
            self.spawn_encode(key.clone(), wanted, prepared, image_id, theme);
        }
    }

    /// 该尺寸的载荷是否已经编码好（测试用：等后台编码线程落地）。
    #[cfg(test)]
    pub fn has_payload(
        &self,
        key: &ImageKey,
        proto: GraphicsProto,
        cols: u16,
        rows: u16,
        px: (u32, u32),
    ) -> bool {
        let inner = self.lock();
        inner.entries.get(key).is_some_and(|entry| {
            entry.encoded.contains_key(&EncodedKey {
                proto,
                cols,
                rows,
                px,
            })
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // 锁内只做查表与算术，没有会 panic 的操作，毒化就当作致命状态恢复
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// 起后台解码线程；结果经事件通道回到主线程。
    ///
    /// `make` 在**后台线程里**才执行：工具图片的 `spec()` 要克隆整份 base64，
    /// 那可能是几 MB 的拷贝，放在主线程上做就等于把「不卡顿」的承诺还回去。
    fn spawn_decode(&self, key: ImageKey, make: impl FnOnce() -> ImageSourceSpec + Send + 'static) {
        let events = self.events.clone();
        std::thread::Builder::new()
            .name("moc-image".into())
            .spawn(move || {
                let result = decode::decode(make()).map_err(|error| error.to_string());
                // 接收端已关闭（TUI 正在退出）就静默丢弃，没必要报错
                let _ = events.send(AppEvent::ImagePrepared { key, result });
            })
            .ok();
    }
}

/// 解码结果 + 目标像素框 → 画布。
///
/// 图片在框内居中、不放大；空白部分铺主题底色，与终端背景同色，
/// 这样 sixel 的「跳过 0 号槽位」正好省掉留白。
fn prepare_canvas(decoded: &DecodedImage, px: (u32, u32), theme: &Theme) -> Canvas {
    let bg = theme.raw().bg_base;
    let fit = layout::fit(decoded.size(), px);
    if fit.2 == 0 || fit.3 == 0 {
        return Canvas::new(px.0, px.1, bg);
    }
    let rgba = match decoded {
        DecodedImage::Raster(source) => {
            let mut base =
                image::RgbaImage::from_pixel(px.0, px.1, image::Rgba([bg[0], bg[1], bg[2], 0xff]));
            let scaled = image::imageops::resize(
                source,
                fit.2,
                fit.3,
                image::imageops::FilterType::Lanczos3,
            );
            image::imageops::overlay(&mut base, &scaled, i64::from(fit.0), i64::from(fit.1));
            base
        }
        DecodedImage::Svg { tree, .. } => {
            let Some(mut pixmap) = resvg::tiny_skia::Pixmap::new(px.0, px.1) else {
                return Canvas::new(px.0, px.1, bg);
            };
            pixmap.fill(tiny_skia_color(bg));
            let size = tree.size();
            let scale_x = fit.2 as f32 / size.width();
            let scale_y = fit.3 as f32 / size.height();
            let transform = resvg::tiny_skia::Transform::from_scale(scale_x, scale_y)
                .post_translate(fit.0 as f32, fit.1 as f32);
            resvg::render(tree, transform, &mut pixmap.as_mut());
            // 底色已铺满不透明：预乘值与直通值相等
            match image::RgbaImage::from_raw(px.0, px.1, pixmap.take()) {
                Some(image) => image,
                None => return Canvas::new(px.0, px.1, bg),
            }
        }
    };
    let (palette, index) = quantize::quantize(&rgba, bg);
    Canvas::from_indexed(px.0, px.1, palette, index)
}

fn tiny_skia_color(rgb: [u8; 3]) -> resvg::tiny_skia::Color {
    resvg::tiny_skia::Color::from_rgba8(rgb[0], rgb[1], rgb[2], 0xff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_image_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<DecodedImage>();
        assert_send::<ImageSourceSpec>();
        assert_send::<PreparedImage>();
    }

    #[test]
    fn digest_tracks_content() {
        let a = ImageKey::from_bytes("image/png", "aaa");
        let b = ImageKey::from_bytes("image/png", "aaa");
        let c = ImageKey::from_bytes("image/png", "bbb");
        let d = ImageKey::from_bytes("image/jpeg", "aaa");
        assert_eq!(a.digest(), b.digest());
        assert_ne!(a.digest(), c.digest());
        assert_ne!(a.digest(), d.digest(), "媒体类型参与指纹");
    }

    #[test]
    fn file_keys_react_to_mtime_and_length() {
        let path = Arc::new(PathBuf::from("/tmp/a.png"));
        let a = ImageKey::File {
            path: Arc::clone(&path),
            mtime_ns: 1,
            len: 10,
        };
        let b = ImageKey::File {
            path: Arc::clone(&path),
            mtime_ns: 2,
            len: 10,
        };
        let c = ImageKey::File {
            path,
            mtime_ns: 1,
            len: 11,
        };
        assert_ne!(a.digest(), b.digest(), "mtime 变了要重解码");
        assert_ne!(a.digest(), c.digest(), "长度变了要重解码");
    }

    fn store() -> ImageStore {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        ImageStore::new(events)
    }

    /// 回归：解码结果超过缓存预算时，不能在同一轮 `evict` 里把自己删掉。
    ///
    /// 那会形成「解码完成 → 立刻被淘汰 → 下一帧重新解码」的自持循环，
    /// 图片永远显示不出来，而且每帧都在起新线程。
    #[test]
    fn oversized_image_survives_its_own_arrival() {
        let store = store();
        let key = ImageKey::from_bytes("image/png", "huge");
        // 3000×3000 RGBA = 36MB > 32MB 预算
        store.complete(
            key.clone(),
            Ok(DecodedImage::Raster(image::RgbaImage::new(3000, 3000))),
        );
        assert!(
            matches!(store.peek(&key), Some(ImageState::Ready(_))),
            "刚解码完的条目被自己挤掉了"
        );
    }

    /// 回归：图表与内联图片共用同一个 kitty 图像 ID 命名空间。
    ///
    /// 两套自增器各自从 1 起编号时，用同一 `i=` 重传会释放对方图像并清掉它的
    /// 全部放置——图表位置会显示成内联图片的像素。
    #[test]
    fn image_ids_come_from_one_allocator() {
        let store = store();
        store.state(&ImageKey::from_bytes("image/png", "a"), || {
            ImageSourceSpec::Svg("<svg/>".into())
        });
        let chart_id = store.allocate_image_id();
        let second = store.allocate_image_id();
        assert_ne!(chart_id, second);
        assert_ne!(chart_id, 0, "0 在部分终端里是「无 ID」哨兵");
        assert_ne!(second, 0);
    }

    /// 回归：终端里删掉图像数据后必须重新传输，否则滚回来是一张空白。
    #[test]
    fn forgetting_transmission_forces_a_retransmit() {
        const PROTO: GraphicsProto = GraphicsProto::Kitty;
        let store = store();
        let key = ImageKey::from_bytes("image/png", "retransmit");
        store.complete(
            key.clone(),
            Ok(DecodedImage::Raster(image::RgbaImage::from_pixel(
                8,
                8,
                image::Rgba([1, 2, 3, 255]),
            ))),
        );
        let theme = crate::theme::Theme::global();
        let px = (16, 16);
        for _ in 0..200 {
            store.request_payload(&key, PROTO, 2, 1, px, theme);
            if store.has_payload(&key, PROTO, 2, 1, px) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(store.has_payload(&key, PROTO, 2, 1, px), "编码未完成");

        let first = store.payload(&key, PROTO, 2, 1, px, theme).expect("载荷");
        assert!(first.transmit, "首次放置必须传像素数据");
        let again = store.payload(&key, PROTO, 2, 1, px, theme).expect("载荷");
        assert!(!again.transmit, "数据已在终端里，不重复传");

        store.forget_transmission(first.image_id);
        let after = store.payload(&key, PROTO, 2, 1, px, theme).expect("载荷");
        assert!(after.transmit, "终端侧数据没了就必须重传");
    }

    /// 同一张图放在两个尺寸不同的位置：两份编码各自命中，不互相顶掉。
    #[test]
    fn two_sizes_keep_separate_payloads() {
        const PROTO: GraphicsProto = GraphicsProto::Sixel;
        let store = store();
        let key = ImageKey::from_bytes("image/png", "two-sizes");
        store.complete(
            key.clone(),
            Ok(DecodedImage::Raster(image::RgbaImage::from_pixel(
                32,
                32,
                image::Rgba([9, 9, 9, 255]),
            ))),
        );
        let theme = crate::theme::Theme::global();
        for px in [(16, 16), (32, 32)] {
            for _ in 0..200 {
                store.request_payload(&key, PROTO, 2, 1, px, theme);
                if store.has_payload(&key, PROTO, 2, 1, px) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert!(store.has_payload(&key, PROTO, 2, 1, (16, 16)));
        assert!(
            store.has_payload(&key, PROTO, 2, 1, (32, 32)),
            "第二种尺寸不该把第一种顶掉"
        );
    }
}

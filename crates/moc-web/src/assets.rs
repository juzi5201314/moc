//! 前端资源：pnpm 构建产物由 `build.rs` 压成单个 zstd 帧嵌进二进制，部署只需要一个可执行文件。
//!
//! 首次请求时解压并切分成 `Asset`（约 15MB 常驻），之后按 URL 二分查找；
//! 浏览器声明支持 `Accept-Encoding: zstd` 时，响应体在进程内只压一次。

use std::sync::{LazyLock, OnceLock};

/// 归档魔数，与 `build.rs` 的 `MAGIC` 一致。
const MAGIC: &[u8; 8] = b"MOCWEB\x01\n";
/// 小于这个长度的响应不压缩：省下的字节抵不过 zstd 帧头的开销。
const MIN_ENCODABLE: usize = 1024;
/// 传输层压缩级别：要的是快，体积由构建期的 19 级拿下。
const LEVEL: i32 = 3;

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// 一个嵌入的静态文件。
pub struct Asset {
    path: Box<str>,
    mime: &'static str,
    body: Box<[u8]>,
    encoded: OnceLock<Box<[u8]>>,
}

impl Asset {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn mime(&self) -> &'static str {
        self.mime
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// zstd 压缩后的响应体；内容太小或本就不该压缩时为 `None`。
    pub fn encoded(&self) -> Option<&[u8]> {
        if self.body.len() < MIN_ENCODABLE {
            return None;
        }
        Some(self.encoded.get_or_init(|| {
            zstd::bulk::compress(&self.body, LEVEL)
                .expect("压缩嵌入的资源不会失败")
                .into_boxed_slice()
        }))
    }
}

struct Assets {
    entries: Vec<Asset>,
}

static ASSETS: LazyLock<Assets> = LazyLock::new(|| Assets::decode(ARCHIVE));

/// 按 URL 路径精确匹配；`/` 映射到 `/index.html`。
pub fn lookup(path: &str) -> Option<&'static Asset> {
    let path = if path == "/" { "/index.html" } else { path };
    let assets = &*ASSETS;
    assets
        .entries
        .binary_search_by(|entry| entry.path.as_ref().cmp(path))
        .ok()
        .map(|index| &assets.entries[index])
}

/// SPA 入口；`build.rs` 无论 dist 是否存在都会生成它。
pub fn index() -> &'static Asset {
    lookup("/index.html").expect("build.rs 总会生成 /index.html")
}

impl Assets {
    fn decode(archive: &[u8]) -> Self {
        let raw = zstd::stream::decode_all(archive).expect("嵌入的 web 资源包解压失败");
        let mut rest = raw.as_slice();
        assert_eq!(
            take(&mut rest, MAGIC.len()),
            MAGIC,
            "嵌入的 web 资源包魔数不符"
        );
        let count = take_u32(&mut rest);
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let path_len = take_u16(&mut rest);
            let path =
                std::str::from_utf8(take(&mut rest, path_len)).expect("嵌入的资源路径不是 UTF-8");
            let body_len = take_u32(&mut rest);
            entries.push(Asset {
                mime: mime_for(path),
                path: path.into(),
                body: take(&mut rest, body_len).to_vec().into_boxed_slice(),
                encoded: OnceLock::new(),
            });
        }
        // 二分查找依赖 build.rs 按 URL 排序写出条目。
        debug_assert!(
            entries.windows(2).all(|pair| pair[0].path < pair[1].path),
            "web 资源包条目未按 URL 排序"
        );
        debug_assert!(rest.is_empty(), "web 资源包尾部有多余字节");
        Self { entries }
    }
}

fn take<'a>(rest: &mut &'a [u8], len: usize) -> &'a [u8] {
    let (head, tail) = rest.split_at_checked(len).expect("嵌入的 web 资源包被截断");
    *rest = tail;
    head
}

fn take_u16(rest: &mut &[u8]) -> usize {
    u16::from_le_bytes(take(rest, 2).try_into().expect("2 字节")).into()
}

fn take_u32(rest: &mut &[u8]) -> usize {
    u32::from_le_bytes(take(rest, 4).try_into().expect("4 字节"))
        .try_into()
        .expect("usize 至少 32 位")
}

fn mime_for(path: &str) -> &'static str {
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_index_resolve_to_the_same_asset() {
        let root = lookup("/").expect("/ 映射 index");
        assert_eq!(root.path(), "/index.html");
        assert!(std::ptr::eq(root, index()));
        assert!(index().mime().starts_with("text/html"));
        assert!(index().body().starts_with(b"<!doctype html"));
    }

    #[test]
    fn unknown_paths_miss() {
        assert!(lookup("/nope").is_none());
    }

    #[test]
    fn sourcemaps_never_reach_the_binary() {
        assert!(
            ASSETS
                .entries
                .iter()
                .all(|entry| !entry.path().ends_with(".map"))
        );
    }

    #[test]
    fn encoded_bodies_round_trip() {
        let asset = ASSETS
            .entries
            .iter()
            .find(|entry| entry.encoded().is_some())
            .expect("dist 里总有超过 1KiB 的资源");
        let decoded = zstd::stream::decode_all(asset.encoded().unwrap()).unwrap();
        assert_eq!(decoded, asset.body());
    }
}

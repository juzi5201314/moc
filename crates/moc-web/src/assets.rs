//! 每个资源是独立的预压缩帧，静态索引与数据均由构建脚本生成。
//! 支持 zstd 的客户端直接引用嵌入字节；普通响应只临时解压当前文件。
use std::borrow::Cow;

pub struct Asset {
    path: &'static str,
    offset: usize,
    stored_len: usize,
    raw_len: usize,
    compressed: bool,
}

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

impl Asset {
    pub fn path(&self) -> &'static str {
        self.path
    }

    pub fn mime(&self) -> &'static str {
        mime_for(self.path)
    }

    fn stored(&self) -> &'static [u8] {
        &ARCHIVE[self.offset..self.offset + self.stored_len]
    }

    pub fn body(&self) -> Cow<'static, [u8]> {
        if self.compressed {
            Cow::Owned(
                zstd::bulk::decompress(self.stored(), self.raw_len)
                    .expect("构建期验证的资源帧可解压"),
            )
        } else {
            Cow::Borrowed(self.stored())
        }
    }

    pub fn encoded(&self) -> Option<&'static [u8]> {
        self.compressed.then(|| self.stored())
    }
}

pub fn lookup(path: &str) -> Option<&'static Asset> {
    let path = if path == "/" { "/index.html" } else { path };
    ASSETS
        .binary_search_by(|entry| entry.path.cmp(path))
        .ok()
        .map(|index| &ASSETS[index])
}

pub fn index() -> &'static Asset {
    lookup("/index.html").expect("前端产物总有 index.html")
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
        let root = lookup("/").unwrap();
        assert!(std::ptr::eq(root, index()));
        assert!(root.mime().starts_with("text/html"));
        assert!(root.body().starts_with(b"<!doctype html"));
        assert!(lookup("/nope").is_none());
    }

    #[test]
    fn all_embedded_resources_match_the_build_output() {
        assert!(ASSETS.windows(2).all(|pair| pair[0].path < pair[1].path));
        for asset in ASSETS {
            assert!(!asset.path.ends_with(".map"));
            let path = std::path::Path::new(env!("OUT_DIR"))
                .join("dist")
                .join(&asset.path[1..]);
            let original = std::fs::read(path).unwrap();
            assert_eq!(asset.body().as_ref(), original, "{}", asset.path);
            if let Some(encoded) = asset.encoded() {
                assert_eq!(zstd::stream::decode_all(encoded).unwrap(), original);
                // 压缩响应直接借用静态归档，不分配正文缓存。
                assert!(std::ptr::eq(encoded.as_ptr(), asset.stored().as_ptr()));
            }
        }
    }
}

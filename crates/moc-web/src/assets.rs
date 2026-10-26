//! 前端资源：pnpm 构建产物由 `build.rs` 在编译期嵌进二进制，部署只需要一个可执行文件。

/// 一个嵌入的静态文件。
pub struct Asset {
    pub path: &'static str,
    pub mime: &'static str,
    pub body: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// 按 URL 路径精确匹配；`/` 映射到 `/index.html`。
pub fn lookup(path: &str) -> Option<&'static Asset> {
    let path = if path == "/" { "/index.html" } else { path };
    ASSETS.iter().find(|asset| asset.path == path)
}

/// SPA 入口；`build.rs` 无论 dist 是否存在都会生成它。
pub fn index() -> &'static Asset {
    lookup("/index.html").expect("build.rs 总会生成 /index.html")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_index_resolve_to_the_same_asset() {
        let root = lookup("/").expect("/ 映射 index");
        assert_eq!(root.path, "/index.html");
        assert!(std::ptr::eq(root, index()));
        assert!(index().mime.starts_with("text/html"));
    }

    #[test]
    fn unknown_paths_miss() {
        assert!(lookup("/nope").is_none());
    }
}

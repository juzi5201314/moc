//! 把 `web/dist`（pnpm 构建产物）编译期嵌进二进制。
//!
//! dist 缺失时默认嵌一页占位说明，让没装 Node 的 `cargo build` / `cargo test`
//! 照样通过；发布构建设 `MOC_WEB_REQUIRE_DIST=1`，缺失直接失败。

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const BUILD_HINT: &str =
    "pnpm --dir crates/moc-web/web install && pnpm --dir crates/moc-web/web build";

fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-env-changed=MOC_WEB_REQUIRE_DIST");

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("cargo 总会设置 CARGO_MANIFEST_DIR"),
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo 总会设置 OUT_DIR"));
    let dist = manifest_dir.join("web").join("dist");

    let mut generated = String::from("pub static ASSETS: &[Asset] = &[\n");
    if dist.join("index.html").is_file() {
        let mut files = Vec::new();
        collect_files(&dist, &mut files);
        files.sort();
        for file in files {
            // 让 cargo 在任意产物文件变化时重跑。
            println!("cargo:rerun-if-changed={}", file.display());
            let relative = file
                .strip_prefix(&dist)
                .expect("collect_files 只返回 dist 内的路径");
            let url = format!(
                "/{}",
                relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/")
            );
            let absolute = file.to_string_lossy().replace('\\', "\\\\");
            writeln!(
                generated,
                "    Asset {{ path: {url:?}, mime: {:?}, body: include_bytes!(\"{absolute}\") }},",
                mime_for(&file)
            )
            .expect("写入 String 不会失败");
        }
    } else {
        if env::var("MOC_WEB_REQUIRE_DIST").as_deref() == Ok("1") {
            panic!("crates/moc-web/web/dist 缺失：先运行 {BUILD_HINT}");
        }
        let placeholder = format!(
            "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><title>moc</title></head>\
             <body style=\"font-family:system-ui,sans-serif;padding:3rem;line-height:1.6\">\
             <h1>Web 前端未构建</h1><p>在仓库根目录运行：</p><pre>{BUILD_HINT}</pre>\
             <p>然后重新 <code>cargo build</code>。</p></body></html>"
        );
        writeln!(
            generated,
            "    Asset {{ path: \"/index.html\", mime: \"text/html; charset=utf-8\", body: {:?}.as_bytes() }},",
            placeholder
        )
        .expect("写入 String 不会失败");
    }
    generated.push_str("];\n");

    fs::write(out_dir.join("web_assets.rs"), generated).expect("OUT_DIR 可写");
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|error| panic!("读取 {} 失败：{error}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|error| panic!("读取 {} 目录项失败：{error}", dir.display()))
            .path();
        if path.is_dir() {
            collect_files(&path, files);
        } else if path.is_file() {
            files.push(path);
        }
    }
}

fn mime_for(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
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

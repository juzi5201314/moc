//! 把 `web/dist`（pnpm 构建产物）编译期嵌进二进制。
//!
//! 单个文件各自独立无助于体积：shiki 的语言定义、mermaid 的 diagram chunk
//! 之间大量重复，逐文件压缩无法跨文件复用匹配。所以这里把所有资源打成一条
//! 记录序列，整体压成一个 zstd 帧（19 级）再交付给 `include_bytes!`；体积
//! 约为逐文件压缩的八成。运行时的解析见 `src/assets.rs`，两处的格式常量
//! （魔数、字段宽度）必须一致。
//!
//! sourcemap（`*.map`）不进二进制：它占了 dist 的三分之二，是给开发时的
//! `pnpm dev` 用的，生产前端靠 `vite.config.ts` 关掉生成。
//!
//! dist 缺失时默认嵌一页占位说明，让没装 Node 的 `cargo build` / `cargo test`
//! 照样通过；发布构建设 `MOC_WEB_REQUIRE_DIST=1`，缺失直接失败。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// 归档魔数，与 `src/assets.rs` 的 `MAGIC` 一致。
const MAGIC: &[u8; 8] = b"MOCWEB\x01\n";
/// 只影响构建期耗时：一次编译，换最小的嵌入体积。
const LEVEL: i32 = 19;

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

    // (URL 路径, 文件内容)；按 URL 排序，运行时二分查找。
    let mut assets: Vec<(String, Vec<u8>)> = Vec::new();
    if dist.join("index.html").is_file() {
        let mut files = Vec::new();
        collect_files(&dist, &mut files);
        for file in files {
            if has_extension(&file, "map") {
                continue;
            }
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
            let body = fs::read(&file)
                .unwrap_or_else(|error| panic!("读取 {} 失败：{error}", file.display()));
            assets.push((url, body));
        }
        assets.sort();
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
        assets.push(("/index.html".to_owned(), placeholder.into_bytes()));
    }

    let raw = encode(&assets);
    let compressed = zstd::bulk::compress(&raw, LEVEL).expect("zstd 压缩 web 资源失败");
    let archive = out_dir.join("web_assets.zst");
    fs::write(&archive, &compressed).expect("OUT_DIR 可写");

    let path = archive.to_string_lossy().replace('\\', "\\\\");
    fs::write(
        out_dir.join("web_assets.rs"),
        format!("pub static ARCHIVE: &[u8] = include_bytes!(\"{path}\");\n"),
    )
    .expect("OUT_DIR 可写");
}

/// 序列化归档：魔数、条目数、每条（路径长度、路径、内容长度、内容）。
fn encode(assets: &[(String, Vec<u8>)]) -> Vec<u8> {
    let capacity: usize = assets
        .iter()
        .map(|(url, body)| url.len() + body.len() + 6)
        .sum();
    let mut raw = Vec::with_capacity(capacity + MAGIC.len() + 4);
    raw.extend_from_slice(MAGIC);
    raw.extend_from_slice(
        &u32::try_from(assets.len())
            .expect("条目数超 u32")
            .to_le_bytes(),
    );
    for (url, body) in assets {
        assert!(url.len() <= u16::MAX as usize, "路径过长：{url}");
        raw.extend_from_slice(&(url.len() as u16).to_le_bytes());
        raw.extend_from_slice(url.as_bytes());
        raw.extend_from_slice(
            &u32::try_from(body.len())
                .expect("单文件超 4GiB")
                .to_le_bytes(),
        );
        raw.extend_from_slice(body);
    }
    raw
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

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
}

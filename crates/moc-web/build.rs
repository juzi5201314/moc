//! 编译期构建前端，并把产物嵌进二进制。
//!
//! 前端输入（`INPUTS`）变化时 cargo 才重跑本脚本：先 `pnpm install --frozen-lockfile`，
//! 再 `pnpm build` 输出到 `OUT_DIR/dist`（经 `MOC_WEB_OUT_DIR` 传给 `vite.config.ts`）。
//! 产物不落在源码树里：被监视的路径里不会出现本脚本自己写的文件，否则每次编译都会判定过期。
//!
//! 包管理器以 `pnpm-lock.yaml` 为唯一锁文件：PATH 上有 pnpm 就直接用；只有 npm 时
//! 经 `npm exec` 运行 `package.json` 的 `packageManager` 钉住的 pnpm 版本，依赖解析结果不变。
//!
//! 每个资源单独压成标准 zstd 帧，构建期生成静态索引；支持 zstd 的浏览器
//! 直接读取嵌入帧，不支持的请求只解压当前资源，响应结束后释放。
//! 相比整包压缩，二进制稍大，但避免约 16MiB 资源正文和压缩缓存常驻。
//!
//! sourcemap（`*.map`）不进二进制：它是给开发时的 `pnpm dev` 用的，
//! 生产前端靠 `vite.config.ts` 关掉生成。

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 只影响构建期耗时与嵌入体积：release 用 19 级换最小体积；
/// 其余 profile 前端一改就要重压，用 3 级（17MB 产物 19 级约 5s，3 级约 0.04s，体积多约四分之一）。
const RELEASE_LEVEL: i32 = 19;
const DEV_LEVEL: i32 = 3;

/// 影响前端产物的输入，相对 `web/`。只列源码与配置：`node_modules`、`dist` 不在内。
const INPUTS: &[&str] = &[
    "src",
    "public",
    "index.html",
    "package.json",
    "pnpm-lock.yaml",
    "vite.config.ts",
    "tsconfig.json",
    "tsconfig.app.json",
    "tsconfig.node.json",
];

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("cargo 总会设置 CARGO_MANIFEST_DIR"),
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo 总会设置 OUT_DIR"));
    let web = manifest_dir.join("web");
    for input in INPUTS {
        println!("cargo:rerun-if-changed=web/{input}");
    }

    let dist = out_dir.join("dist");
    let pnpm = pnpm(&web);
    run(&pnpm, &web, &dist, &["install", "--frozen-lockfile"]);
    run(&pnpm, &web, &dist, &["run", "build"]);

    // (URL 路径, 文件内容)；按 URL 排序，运行时二分查找。
    let mut assets: Vec<(String, Vec<u8>)> = Vec::new();
    let mut files = Vec::new();
    collect_files(&dist, &mut files);
    for file in files {
        if has_extension(&file, "map") {
            continue;
        }
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
        let body =
            fs::read(&file).unwrap_or_else(|error| panic!("读取 {} 失败：{error}", file.display()));
        assets.push((url, body));
    }
    assets.sort();

    let level = if env::var("PROFILE").as_deref() == Ok("release") {
        RELEASE_LEVEL
    } else {
        DEV_LEVEL
    };
    let mut packed = Vec::new();
    let mut source = String::from("static ASSETS: &[Asset] = &[\n");
    for (url, body) in assets {
        let compressed = zstd::bulk::compress(&body, level).expect("zstd 压缩 web 资源失败");
        let (stored, encoded) = if body.len() >= 1024 && compressed.len() < body.len() {
            (compressed.as_slice(), true)
        } else {
            (body.as_slice(), false)
        };
        let offset = packed.len();
        packed.extend_from_slice(stored);
        source.push_str(&format!(
            "Asset {{ path: {url:?}, offset: {offset}, stored_len: {}, raw_len: {}, compressed: {encoded} }},\n",
            stored.len(), body.len(),
        ));
    }
    source.push_str("] ;\nstatic ARCHIVE: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/web_assets.bin\"));\n");
    fs::write(out_dir.join("web_assets.bin"), packed).expect("OUT_DIR 可写");
    fs::write(out_dir.join("web_assets.rs"), source).expect("OUT_DIR 可写");
}

/// 调用 pnpm 的方式：可执行文件与前置参数。
struct Pnpm {
    program: String,
    prefix: Vec<String>,
}

/// PATH 上有 pnpm 直接用；否则借 npm 运行 `packageManager` 钉住的 pnpm。
fn pnpm(web: &Path) -> Pnpm {
    if let Some(program) = find_program("pnpm") {
        return Pnpm {
            program,
            prefix: Vec::new(),
        };
    }
    let program = find_program("npm")
        .unwrap_or_else(|| panic!("构建 Web 前端需要 Node.js，并在 PATH 上提供 pnpm 或 npm"));
    let prefix = vec![
        "exec".to_owned(),
        "--yes".to_owned(),
        format!("--package={}", package_manager(web)),
        "--".to_owned(),
        "pnpm".to_owned(),
    ];
    Pnpm { program, prefix }
}

/// Windows 上 pnpm/npm 多是 `.cmd` 包装，`Command` 只会自动补 `.exe`，所以两种名字都试。
fn find_program(name: &str) -> Option<String> {
    [name.to_owned(), format!("{name}.cmd")]
        .into_iter()
        .find(|program| {
            Command::new(program)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
}

/// `packageManager` 形如 `pnpm@12.15.0+sha512.…`；npm exec 不认哈希后缀，只取版本部分。
fn package_manager(web: &Path) -> String {
    let path = web.join("package.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 {} 失败：{error}", path.display()));
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("解析 {} 失败：{error}", path.display()));
    let spec = manifest["packageManager"]
        .as_str()
        .filter(|spec| spec.starts_with("pnpm@"))
        .unwrap_or_else(|| panic!("{} 的 packageManager 必须钉住 pnpm 版本", path.display()));
    spec.split_once('+')
        .map_or(spec, |(version, _)| version)
        .to_owned()
}

fn run(pnpm: &Pnpm, web: &Path, dist: &Path, args: &[&str]) {
    let status = Command::new(&pnpm.program)
        .args(&pnpm.prefix)
        .args(args)
        .current_dir(web)
        .env("MOC_WEB_OUT_DIR", dist)
        // cargo 把构建脚本的 stdout 当作 `cargo:` 指令解析，子进程输出一律转到 stderr。
        .stdout(io::stderr())
        .status()
        .unwrap_or_else(|error| panic!("启动 {} 失败：{error}", pnpm.program));
    assert!(status.success(), "`pnpm {}` 失败：{status}", args.join(" "));
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

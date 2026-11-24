//! 线性解析工作区 `locales/*.toml`，生成单份按 key 字典序排列的静态翻译表。
//!
//! 相比 `rust-i18n-support` 的递归 merge（深层嵌套表触发指数级重复遍历，
//! 且 10 个 crate 各自重复解析与内嵌），本脚本以 $O(N)$ 单趟扫描整个 `locales/`
//! 目录，只在 `moc-i18n` 构建一次，运行时通过静态数组二分查找零堆分配读取。

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("cargo 总会设置 CARGO_MANIFEST_DIR"),
    );
    let locales_dir = manifest_dir
        .join("../../locales")
        .canonicalize()
        .expect("工作区根 locales/ 目录必须存在");

    println!("cargo:rerun-if-changed={}", locales_dir.display());

    let mut files: Vec<PathBuf> = fs::read_dir(&locales_dir)
        .expect("读取 locales/ 目录失败")
        .filter_map(|entry| {
            let path = entry.expect("读取 locales/ 目录项失败").path();
            (path.extension().and_then(|ext| ext.to_str()) == Some("toml")).then_some(path)
        })
        .collect();
    files.sort();

    let mut catalog: BTreeMap<String, (String, String)> = BTreeMap::new();
    for path in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("读取 {} 失败：{error}", path.display()));
        let table: toml::Table = toml::from_str(&text)
            .unwrap_or_else(|error| panic!("解析 {} 失败：{error}", path.display()));
        collect_table(path, &table, String::new(), &mut catalog);
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo 总会设置 OUT_DIR"));
    let out_path = out_dir.join("catalog.rs");
    let file = File::create(&out_path)
        .unwrap_or_else(|error| panic!("创建 {} 失败：{error}", out_path.display()));
    let mut writer = BufWriter::new(file);

    writeln!(
        writer,
        "// 由 crates/moc-i18n/build.rs 自动生成，切勿手改。\n\
         // 严格按 key 字典序排列，供运行时零分配二分查找。\n\
         pub(crate) static CATALOG: &[(&str, &str, &str)] = &["
    )
    .expect("写入 catalog 头部失败");

    for (key, (zh, en)) in &catalog {
        writeln!(writer, "    ({key:?}, {zh:?}, {en:?}),").expect("写入 catalog 条目失败");
    }

    writeln!(writer, "];").expect("写入 catalog 尾部失败");
    writer.flush().expect("刷新 catalog.rs 失败");
}

fn collect_table(
    path: &Path,
    table: &toml::Table,
    prefix: String,
    catalog: &mut BTreeMap<String, (String, String)>,
) {
    for (name, value) in table {
        if prefix.is_empty() && name == "_version" {
            continue;
        }
        let full_key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let toml::Value::Table(child) = value else {
            panic!(
                "{}: 键 `{full_key}` 期望表结构（嵌套分组或 {{ zh, en }} 叶子），实际为 {:?}",
                path.display(),
                value.type_str()
            );
        };
        if !child.is_empty() && child.values().all(|item| item.is_str()) {
            let mut zh: Option<&str> = None;
            let mut en: Option<&str> = None;
            for (locale, text) in child {
                let text = text.as_str().expect("已检查均为字符串");
                match locale.as_str() {
                    "zh" => zh = Some(text),
                    "en" => en = Some(text),
                    other => panic!(
                        "{}: 键 `{full_key}` 包含未支持的语言 `{other}`（仅允许 zh/en）",
                        path.display()
                    ),
                }
            }
            let zh =
                zh.unwrap_or_else(|| panic!("{}: 键 `{full_key}` 缺少源语言 `zh`", path.display()));
            let en = en.unwrap_or("");
            if catalog
                .insert(full_key.clone(), (zh.to_owned(), en.to_owned()))
                .is_some()
            {
                panic!("{}: 重复的本地化键 `{full_key}`", path.display());
            }
        } else {
            collect_table(path, child, full_key, catalog);
        }
    }
}

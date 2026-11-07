//! 协议清单对账：前端 `protocol/types.ts` 是手抄的 serde DTO，Rust 侧改枚举
//! variant 或方法名时只能靠运行时兜底。本测试扫本 crate 的 `src/` 提取
//! 两件事实，与 checked-in 的 `protocol-manifest.json` 对比：
//! 1. 全部 `impl Method` / `*_method!` 宏的方法名与方向（ClientRequest /
//!    ServerRequest / ServerNotification）。
//! 2. 全部 serde `pub enum` 的线上值（内部 tag 名 + variant 的 serde 名）。
//!
//! 前端 vitest（`moc-web/web/src/protocol/manifest.test.ts`）用同一份清单对
//! 账 TS 侧。重生成：`MOC_UPDATE_MANIFEST=1 cargo test -p moc-protocol`。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

const MANIFEST: &str = "protocol-manifest.json";

// ---------- 源码预处理 ----------

/// 剥注释并截断 `#[cfg(test)]` 之后的内容：doc 注释里的方法名、测试里的伪枚举
/// 都不该进清单。字符串与字符字面量原样保留（方法名/serde 名都在字面量里）。
fn clean(src: &str) -> String {
    let src = src.split("#[cfg(test)]").next().unwrap_or("");
    let bytes = src.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' => {
                // 含闭引号整体拷贝；否则下一个迭代会把同一引号当成新字符串起点。
                out.push(b'"');
                i += 1;
                while i < bytes.len() {
                    let ch = bytes[i];
                    out.push(ch);
                    i += if ch == b'\\' { 2 } else { 1 };
                    if ch == b'"' {
                        break;
                    }
                }
            }
            // 字符字面量 `'x'` / `'\n'` 原样跳过；生命周期 `'a` 不当字符串。
            b'\'' if bytes.get(i + 2) == Some(&b'\'')
                || bytes.get(i + 1) == Some(&b'\\') =>
            {
                out.push(b'\'');
                i += 1;
                while i < bytes.len() {
                    let ch = bytes[i];
                    out.push(ch);
                    i += if ch == b'\\' { 2 } else { 1 };
                    if ch == b'\'' {
                        break;
                    }
                }
            }
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).expect("源码是 UTF-8")
}

fn rust_sources() -> Vec<String> {
    let mut files = Vec::new();
    collect(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    files.sort();
    files
        .iter()
        .map(|file| clean(&fs::read_to_string(file).expect("源码可读")))
        .collect()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("src 可读") {
        let path = entry.expect("目录项").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

// ---------- 轻量语法工具（都作用于剥掉注释的源码） ----------

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `needle` 之后读到第一个 `"..."` 字面量；找不到返回 None。
fn next_literal(src: &[u8], from: usize) -> Option<(String, usize)> {
    let mut i = from;
    while i < src.len() && src[i] != b'"' {
        i += 1;
    }
    if i == src.len() {
        return None;
    }
    i += 1;
    let start = i;
    while i < src.len() && src[i] != b'"' {
        i += 1;
    }
    Some((String::from_utf8_lossy(&src[start..i]).into_owned(), i + 1))
}

/// `src[open]`（`(`/`{`/`[`）的配对闭括号位置。
fn matching(src: &[u8], open: usize) -> usize {
    let (l, r) = match src[open] {
        b'(' => (b'(', b')'),
        b'{' => (b'{', b'}'),
        b'[' => (b'[', b']'),
        _ => unreachable!(),
    };
    let mut depth = 0usize;
    let mut i = open;
    while i < src.len() {
        if src[i] == b'"' {
            i = next_literal(src, i).map(|(_, end)| end).unwrap_or(src.len());
            continue;
        }
        if src[i] == l {
            depth += 1;
        } else if src[i] == r {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
        i += 1;
    }
    panic!("括号不配对 at byte {open}");
}

/// `src[open]` 闭括号体内的顶层逗号切片（忽略嵌套 `(){}[]` 与字符串内的逗号）。
fn top_level_split<'a>(src: &'a [u8], open: usize, close: usize) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = open + 1;
    let mut i = open + 1;
    while i < close {
        match src[i] {
            b'"' => i = next_literal(src, i).map(|(_, end)| end).unwrap_or(close),
            b'(' | b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b')' | b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b',' if depth == 0 => {
                parts.push(std::str::from_utf8(&src[start..i]).unwrap());
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    let tail = std::str::from_utf8(&src[start..close]).unwrap();
    if !tail.trim().is_empty() {
        parts.push(tail);
    }
    parts
}

/// 切片开头的 `#[...]` 属性块；返回（属性文本, 剩余体）。
fn take_attrs(mut text: &str) -> (String, &str) {
    let mut attrs = String::new();
    loop {
        let trimmed = text.trim_start();
        if !trimmed.starts_with("#[") {
            return (attrs, trimmed);
        }
        let open = 1; // `#[` 的 `[`
        let bytes = trimmed.as_bytes();
        let close = matching(bytes, open);
        attrs.push_str(&trimmed[..=close]);
        text = &trimmed[close + 1..];
    }
}

// ---------- 方法表 ----------

fn methods(sources: &[String]) -> BTreeMap<String, &'static str> {
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut directions: BTreeMap<String, &'static str> = BTreeMap::new();
    for src in sources {
        let bytes = src.as_bytes();
        for (pattern, ty_name) in [
            ("impl Method for ", None),
            ("impl ClientRequest for ", Some("client")),
            ("impl ServerRequest for ", Some("server")),
            ("impl ServerNotification for ", Some("server")),
        ] {
            let mut at = 0;
            while let Some(found) = src[at..].find(pattern) {
                let start = at + found + pattern.len();
                let end = start + src[start..].find(|c: char| !is_ident(c as u8)).unwrap_or(0);
                let ty = src[start..end].to_owned();
                at = end;
                // 宏定义里的 `impl Method for $ty`/`for $name` 不是真实类型。
                if ty.is_empty() || ty.starts_with('$') {
                    continue;
                }
                match ty_name {
                    Some(direction) => {
                        directions.insert(ty, direction);
                    }
                    None => {
                        let body_open = src[end..].find('{').map(|o| end + o).unwrap_or_else(|| {
                            panic!("`impl Method for {ty}` 没有方法体");
                        });
                        let body_close = matching(bytes, body_open);
                        let body = &src[body_open..=body_close];
                        let name_at = body
                            .find("const NAME")
                            .unwrap_or_else(|| panic!("`impl Method for {ty}` 缺少 const NAME"));
                        let (name, _) = next_literal(&body.as_bytes()[name_at..], 0)
                            .expect("const NAME 字面量");
                        names.insert(ty, name);
                    }
                }
            }
        }
        // `client_method!(Type, "x/y", Params, Result)`：方向由宏名给出。
        let mut at = 0;
        while let Some(found) = src[at..].find("_method!(") {
            let bang = at + found + "_method!".len();
            let mut prefix_end = at + found;
            while prefix_end > at && is_ident(bytes[prefix_end - 1]) {
                prefix_end -= 1;
            }
            let prefix = &src[prefix_end..at + found];
            let open = src[bang..].find('(').map(|o| bang + o).expect("宏调用括号");
            let close = matching(bytes, open);
            let args = top_level_split(bytes, open, close);
            let ty = args
                .first()
                .map(|arg| take_attrs(arg).1.trim().to_owned())
                .unwrap_or_default();
            let (name, _) = args
                .get(1)
                .and_then(|arg| next_literal(arg.as_bytes(), 0))
                .unwrap_or_else(|| panic!("`{prefix}_method!` 第二参数应为方法名字面量"));
            let direction = match prefix {
                "client" => "client",
                other => panic!("未知的 `{other}_method!` 宏：先在清单生成器登记其方向"),
            };
            names.insert(ty.clone(), name);
            directions.insert(ty, direction);
            at = close;
        }
    }
    names
        .into_iter()
        .map(|(ty, name)| {
            let direction = directions
                .get(&ty)
                .unwrap_or_else(|| panic!("`{ty}` 有 Method 实现但没有方向标记 impl"));
            (name, *direction)
        })
        .collect()
}

// ---------- serde 枚举 ----------

/// `PascalCase`/`camelCase` 等按 serde rename_all 规则改名（只覆盖本仓实际用的）。
fn rename(ident: &str, rule: Option<&str>) -> String {
    let words: Vec<String> = {
        let mut words = Vec::new();
        let mut word = String::new();
        for ch in ident.chars() {
            if ch == '_' {
                words.push(std::mem::take(&mut word));
            } else if ch.is_ascii_uppercase() && !word.is_empty() {
                words.push(std::mem::take(&mut word));
                word.push(ch.to_ascii_lowercase());
            } else {
                word.push(ch.to_ascii_lowercase());
            }
        }
        if !word.is_empty() {
            words.push(word);
        }
        words
    };
    match rule {
        Some("snake_case") => words.join("_"),
        Some("SCREAMING_SNAKE_CASE") => words.join("_").to_uppercase(),
        Some("kebab-case") => words.join("-"),
        Some("camelCase") => {
            let (first, rest) = words.split_first().expect("至少一个词");
            let mut out = first.clone();
            for word in rest {
                let (head, tail) = word.split_at(1);
                out.push_str(&head.to_uppercase());
                out.push_str(tail);
            }
            out
        }
        // serde 默认：variant 名原样。
        _ => ident.to_owned(),
    }
}

fn attr_value<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("{key} = \"");
    let start = attrs.find(&marker)? + marker.len();
    let end = attrs[start..].find('"')? + start;
    Some(&attrs[start..end])
}

fn enums(sources: &[String]) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    for src in sources {
        let bytes = src.as_bytes();
        let mut at = 0;
        while let Some(found) = src[at..].find("pub enum ") {
            let decl = at + found;
            let name_start = decl + "pub enum ".len();
            let name_end = name_start
                + src[name_start..]
                    .find(|c: char| !is_ident(c as u8))
                    .expect("枚举名");
            let name = src[name_start..name_end].to_owned();
            at = name_end;

            // 枚举前的属性栈：往回跳过空白，逐块吃 `#[...]`。
            let mut attrs = String::new();
            let mut back = decl;
            loop {
                while back > 0 && bytes[back - 1].is_ascii_whitespace() {
                    back -= 1;
                }
                if back == 0 || bytes[back - 1] != b']' {
                    break;
                }
                // 属性块的 `[` 前应是 `#`。
                let mut depth = 0usize;
                let mut open = back;
                while open > 0 {
                    match bytes[open - 1] {
                        b']' => depth += 1,
                        b'[' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    open -= 1;
                }
                if open < 2 || bytes[open - 2] != b'#' {
                    break;
                }
                attrs.insert_str(0, &src[open - 2..back]);
                back = open - 2;
            }

            let body_open = match src[name_end..].find('{').map(|o| name_end + o) {
                Some(open) => open,
                None => break, // `pub enum` 出现在别处（use 语句等）
            };
            at = matching(bytes, body_open);
            let is_serde = attrs.contains("derive(")
                && (attrs.contains("Serialize") || attrs.contains("Deserialize"));
            let untagged = attrs.contains("untagged");
            if !is_serde || untagged {
                continue;
            }
            let rule = attr_value(&attrs, "rename_all");
            let tag = attr_value(&attrs, "tag").map(str::to_owned);
            let mut values: Vec<String> = Vec::new();
            for part in top_level_split(bytes, body_open, at) {
                let (variant_attrs, body) = take_attrs(part);
                if variant_attrs.contains("other") || variant_attrs.contains("skip") {
                    continue;
                }
                let ident: String = body
                    .trim_start()
                    .chars()
                    .take_while(|ch| is_ident(*ch as u8))
                    .collect();
                if ident.is_empty() {
                    continue;
                }
                values.push(attr_value(&variant_attrs, "rename").map_or_else(
                    || rename(&ident, rule),
                    str::to_owned,
                ));
            }
            values.sort();
            values.dedup();
            out.insert(
                name,
                json!({ "tag": tag, "values": values }),
            );
        }
    }
    out
}

// ---------- 清单对账 ----------

#[test]
fn protocol_manifest_is_fresh() {
    let sources = rust_sources();
    let mut client: Vec<_> = Vec::new();
    let mut server: Vec<_> = Vec::new();
    for (name, direction) in methods(&sources) {
        match direction {
            "client" => client.push(name),
            _ => server.push(name),
        }
    }
    client.sort();
    client.dedup();
    server.sort();
    server.dedup();
    let manifest = serde_json::json!({
        "protocolVersion": moc_protocol::PROTOCOL_VERSION,
        "methods": { "client": client, "server": server },
        "enums": enums(&sources),
    });
    let generated = serde_json::to_string_pretty(&manifest).expect("JSON") + "\n";
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(MANIFEST);
    if std::env::var_os("MOC_UPDATE_MANIFEST").is_some() {
        fs::write(&path, &generated).expect("清单可写");
        return;
    }
    let checked_in = fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("缺少 {MANIFEST}；运行 MOC_UPDATE_MANIFEST=1 cargo test -p moc-protocol 生成")
    });
    assert_eq!(
        checked_in, generated,
        "协议清单过期：检查上方 diff，确认后运行 \
         MOC_UPDATE_MANIFEST=1 cargo test -p moc-protocol 重新生成，\
         并同步更新 moc-web 前端类型"
    );
}

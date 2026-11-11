//! 协议 schema 生成与对账：`protocol.schema.json` 是协议的单一事实来源，
//! 供协议文档、未来非 TS 客户端与前端 `types.gen.ts` 代码生成共用。
//!
//! 生成方式：[`register_all`] 用 80 个 Method 的 `Params`/`Result` 做根，
//! schemars 传递闭包出全部线上 DTO 进 `$defs`；方法表本身记录方向与
//! params/result 的 `$ref`，即方法索引。
//!
//! 一致性保障：本测试同时用源码扫描（`clean`/`methods`/`enums`）独立枚举
//! 方法类型与 serde 枚举，和注册表、`$defs` 双向核对——新增方法忘了在
//! [`register_all`] 登记、新增枚举到不了线上、方向改错，测试都会失败。
//!
//! 重生成：`MOC_UPDATE_SCHEMA=1 cargo test -p moc-protocol`。

use std::any::type_name;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use moc_protocol::method::{ClientRequest, Method, ServerNotification, ServerRequest};
use moc_protocol::methods::*;
use schemars::{JsonSchema, SchemaGenerator};
use serde_json::{Value, json};

const SCHEMA: &str = "protocol.schema.json";

// ---------- 注册表：方法类型为 schema 根 ----------

fn schema_json<T: JsonSchema + ?Sized>(generator: &mut SchemaGenerator) -> Value {
    serde_json::to_value(generator.subschema_for::<T>()).expect("schema 可序列化")
}

fn entry<M: Method>(generator: &mut SchemaGenerator) -> Value
where
    M::Params: JsonSchema,
    M::Result: JsonSchema,
{
    json!({
        "type": type_name::<M>().rsplit("::").next().unwrap(),
        "params": schema_json::<M::Params>(generator),
        "result": schema_json::<M::Result>(generator),
    })
}

fn client<M>(generator: &mut SchemaGenerator, out: &mut BTreeMap<String, Value>)
where
    M: Method + ClientRequest,
    M::Params: JsonSchema,
    M::Result: JsonSchema,
{
    out.insert(M::NAME.into(), entry::<M>(generator));
}

fn server_request<M>(generator: &mut SchemaGenerator, out: &mut BTreeMap<String, Value>)
where
    M: Method + ServerRequest,
    M::Params: JsonSchema,
    M::Result: JsonSchema,
{
    out.insert(M::NAME.into(), entry::<M>(generator));
}

fn server_notification<M>(generator: &mut SchemaGenerator, out: &mut BTreeMap<String, Value>)
where
    M: Method + ServerNotification,
    M::Params: JsonSchema,
    M::Result: JsonSchema,
{
    out.insert(M::NAME.into(), entry::<M>(generator));
}

/// 全部线上方法；遗漏由下方 `methods()` 扫描器对照发现。
fn register_all(
    generator: &mut SchemaGenerator,
    client_map: &mut BTreeMap<String, Value>,
    server_req: &mut BTreeMap<String, Value>,
    server_ntf: &mut BTreeMap<String, Value>,
) {
    // agent.rs
    client::<AgentSpawn>(generator, client_map);
    client::<AgentList>(generator, client_map);
    client::<AgentMessage>(generator, client_map);
    client::<AgentStop>(generator, client_map);
    client::<AgentWait>(generator, client_map);
    // ask.rs
    server_request::<SessionRequestAsk>(generator, server_req);
    // bash.rs
    client::<SessionBash>(generator, client_map);
    // btw.rs
    client::<SessionBtw>(generator, client_map);
    client::<SessionBtwCancel>(generator, client_map);
    server_notification::<BtwUpdateMethod>(generator, server_ntf);
    // chart.rs
    client::<ChartAnalyze>(generator, client_map);
    // config.rs
    client::<ConfigGet>(generator, client_map);
    // fs.rs
    client::<FsListDir>(generator, client_map);
    client::<FsCreateDir>(generator, client_map);
    client::<FsReadFile>(generator, client_map);
    client::<FsWriteFile>(generator, client_map);
    client::<FsCreateFile>(generator, client_map);
    client::<FsRename>(generator, client_map);
    client::<FsDelete>(generator, client_map);
    // history.rs
    client::<SessionHistory>(generator, client_map);
    // initialize.rs
    client::<Initialize>(generator, client_map);
    client::<Ping>(generator, client_map);
    // job.rs
    client::<JobList>(generator, client_map);
    client::<JobReadOutput>(generator, client_map);
    client::<JobBackground>(generator, client_map);
    client::<JobKill>(generator, client_map);
    // management.rs
    client::<ConfigManage>(generator, client_map);
    // permission.rs
    server_request::<SessionRequestPermission>(generator, server_req);
    // plan.rs
    client::<SessionPlanMode>(generator, client_map);
    client::<SessionPlanApprove>(generator, client_map);
    client::<SessionPlanRevise>(generator, client_map);
    client::<SessionPlanDiff>(generator, client_map);
    // plugin.rs
    client::<PluginList>(generator, client_map);
    client::<PluginReload>(generator, client_map);
    client::<PluginRunCommand>(generator, client_map);
    // prompt.rs
    client::<SessionPrompt>(generator, client_map);
    client::<SessionCancel>(generator, client_map);
    client::<SessionRetry>(generator, client_map);
    client::<SessionRedo>(generator, client_map);
    client::<SessionSteer>(generator, client_map);
    client::<SessionUnsteer>(generator, client_map);
    client::<SessionReask>(generator, client_map);
    // resources.rs
    client::<ResourceComplete>(generator, client_map);
    client::<ResourceList>(generator, client_map);
    client::<ResourceRead>(generator, client_map);
    // skills.rs
    client::<SkillsCompact>(generator, client_map);
    client::<SkillsCancel>(generator, client_map);
    server_notification::<SkillsProgress>(generator, server_ntf);
    // session.rs
    client::<SessionNew>(generator, client_map);
    client::<SessionList>(generator, client_map);
    client::<SessionSearch>(generator, client_map);
    client::<SessionLoad>(generator, client_map);
    client::<SessionLoadPage>(generator, client_map);
    client::<SessionUnload>(generator, client_map);
    client::<SessionDelete>(generator, client_map);
    client::<SessionRename>(generator, client_map);
    client::<SessionRewind>(generator, client_map);
    client::<SessionCompact>(generator, client_map);
    client::<SessionGoal>(generator, client_map);
    client::<SessionSetModel>(generator, client_map);
    client::<SessionSetEngine>(generator, client_map);
    client::<SessionSetPermission>(generator, client_map);
    client::<SessionSetAutonomy>(generator, client_map);
    client::<SessionSetFastMode>(generator, client_map);
    client::<SessionUnlock>(generator, client_map);
    client::<SessionUsage>(generator, client_map);
    client::<SessionContext>(generator, client_map);
    client::<SessionContextRead>(generator, client_map);
    client::<SessionContextContent>(generator, client_map);
    client::<SessionContextClose>(generator, client_map);
    client::<SessionTools>(generator, client_map);
    server_notification::<SessionUpdateMethod>(generator, server_ntf);
    // web.rs
    client::<WebStateGet>(generator, client_map);
    client::<WebStateSave>(generator, client_map);
    server_notification::<WebStateUpdated>(generator, server_ntf);
    // workflow.rs
    client::<SessionWorkflowRun>(generator, client_map);
    client::<SessionWorkflowResume>(generator, client_map);
    client::<SessionWorkflowRuns>(generator, client_map);
    client::<SessionWorkflowStop>(generator, client_map);
    client::<SessionWorkflowActivate>(generator, client_map);
    client::<SessionWorkflowApprove>(generator, client_map);
    client::<SessionWorkflowSetRoleModel>(generator, client_map);
    client::<SessionWorkflowArtifact>(generator, client_map);
    // workspace.rs
    client::<WorkspaceGitBranch>(generator, client_map);
}

// ---------- 源码预处理（独立扫描器，与注册表/schema 双向核对） ----------

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
            b'\'' if bytes.get(i + 2) == Some(&b'\'') || bytes.get(i + 1) == Some(&b'\\') => {
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
    collect(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
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
            i = next_literal(src, i)
                .map(|(_, end)| end)
                .unwrap_or(src.len());
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

// ---------- 方法表扫描（类型名 → (线上名, 方向)） ----------

fn scanned_methods(sources: &[String]) -> BTreeMap<String, (String, &'static str)> {
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut directions: BTreeMap<String, &'static str> = BTreeMap::new();
    for src in sources {
        let bytes = src.as_bytes();
        for (pattern, direction) in [
            ("impl Method for ", None),
            ("impl ClientRequest for ", Some("client")),
            ("impl ServerRequest for ", Some("server_request")),
            ("impl ServerNotification for ", Some("server_notification")),
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
                match direction {
                    Some(direction) => {
                        directions.insert(ty, direction);
                    }
                    None => {
                        let body_open =
                            src[end..].find('{').map(|o| end + o).unwrap_or_else(|| {
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
            (ty, (name, *direction))
        })
        .collect()
}

// ---------- serde 枚举名扫描（只用于 `$defs` 覆盖核对，wire 值由 schemars 保证） ----------

fn serde_type_names(sources: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for src in sources {
        let bytes = src.as_bytes();
        for kw in ["pub enum ", "pub struct "] {
            let mut at = 0;
            while let Some(found) = src[at..].find(kw) {
                let decl = at + found;
                let name_start = decl + kw.len();
                let Some(name_len) = src[name_start..].find(|c: char| !is_ident(c as u8)) else {
                    break;
                };
                let name = src[name_start..name_start + name_len].to_owned();
                at = name_start + name_len;

                // 类型前的属性栈：往回跳过空白，逐块吃 `#[...]`。
                let mut attrs = String::new();
                let mut back = decl;
                loop {
                    while back > 0 && bytes[back - 1].is_ascii_whitespace() {
                        back -= 1;
                    }
                    if back == 0 || bytes[back - 1] != b']' {
                        break;
                    }
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
                let is_serde = attrs.contains("derive(")
                    && (attrs.contains("Serialize") || attrs.contains("Deserialize"));
                if is_serde {
                    out.push(name);
                }
            }
        }
    }
    out
}

// ---------- schema 生成与对账 ----------

fn build_schema() -> (Value, BTreeMap<String, (String, &'static str)>) {
    let sources = rust_sources();
    let scanned = scanned_methods(&sources);

    let mut generator = SchemaGenerator::default();
    let (mut client_map, mut server_req, mut server_ntf) =
        (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    register_all(
        &mut generator,
        &mut client_map,
        &mut server_req,
        &mut server_ntf,
    );

    // 覆盖核对一：扫到的每个方法类型都在注册表里，且 wire 名、方向一致。
    let registered: BTreeMap<String, (String, &str)> = [
        ("client", &client_map),
        ("server_request", &server_req),
        ("server_notification", &server_ntf),
    ]
    .into_iter()
    .flat_map(|(direction, map)| {
        map.iter().map(move |(name, entry)| {
            (
                entry["type"].as_str().unwrap().to_owned(),
                (name.clone(), direction),
            )
        })
    })
    .collect();
    let mut problems: Vec<String> = Vec::new();
    for (ty, (name, direction)) in &scanned {
        match registered.get(ty) {
            None => problems.push(format!("方法 `{name}`（{ty}）未在 register_all 登记")),
            Some((reg_name, reg_dir)) => {
                if reg_name != name {
                    problems.push(format!("{ty}：注册 wire 名 `{reg_name}` ≠ 扫描 `{name}`"));
                }
                if reg_dir != direction {
                    problems.push(format!("{ty}：注册方向 `{reg_dir}` ≠ 扫描 `{direction}`"));
                }
            }
        }
    }
    let scanned_tys: std::collections::BTreeSet<_> = scanned.keys().collect();
    for ty in registered.keys() {
        if !scanned_tys.contains(ty) {
            problems.push(format!("register_all 里的 {ty} 不是源码中的 Method 类型"));
        }
    }
    assert!(
        problems.is_empty(),
        "方法注册表与源码不一致：\n{}",
        problems.join("\n")
    );

    // 覆盖核对二：每个 serde pub 类型都能从方法根到达 $defs（即在线上）。
    // 豁免的是「有 serde 形状但不走 RPC 方法面」的类型：CommandMeta 走
    // moc-web 的 /commands.json HTTP 端点，RpcError 是 JSON-RPC 信封层。
    const NOT_ON_WIRE: &[&str] = &["CommandMeta", "RpcError"];
    let defs = generator.definitions();
    let mut unreachable: Vec<String> = serde_type_names(&sources)
        .into_iter()
        .filter(|name| !defs.contains_key(name.as_str()) && !NOT_ON_WIRE.contains(&name.as_str()))
        .collect();
    unreachable.sort();
    assert!(
        unreachable.is_empty(),
        "serde 类型不在任何方法的 Params/Result 传递闭包里（非线上类型或漏注册）：\n{}",
        unreachable.join("\n")
    );

    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "moc protocol",
        "protocolVersion": moc_protocol::PROTOCOL_VERSION,
        "methods": {
            "client": client_map,
            "server_request": server_req,
            "server_notification": server_ntf,
        },
        "$defs": defs,
    });
    (schema, scanned)
}

#[test]
fn protocol_schema_is_fresh() {
    let (schema, _) = build_schema();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(SCHEMA);
    if std::env::var_os("MOC_UPDATE_SCHEMA").is_some() {
        let generated = serde_json::to_string_pretty(&schema).expect("JSON") + "\n";
        fs::write(&path, &generated).expect("schema 可写");
        return;
    }
    let checked_in = fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("缺少 {SCHEMA}；运行 MOC_UPDATE_SCHEMA=1 cargo test -p moc-protocol 生成")
    });
    // serde_json 的 preserve_order 可随工作区 feature 合并变化；对象键顺序不是线协议。
    let checked_in: Value =
        serde_json::from_str(&checked_in).expect("已提交的 schema 必须是合法 JSON");
    assert_eq!(
        checked_in, schema,
        "协议 schema 过期：检查上方 diff，确认后运行 \
         MOC_UPDATE_SCHEMA=1 cargo test -p moc-protocol 重新生成，\
         并同步更新 moc-web 前端类型（pnpm gen:protocol）"
    );
}

//! docs.rs rustdoc JSON 的结构化抓取：item 页与 module 页渲染。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use reqwest::Url;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::super::ExtractResult;
use super::super::http::{HttpFetcher, WebFetchFailure};
use super::{SiteEndpoints, markdown_result, status_error};

const MAX_RUSTDOC_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
struct Target {
    crate_name: String,
    version: String,
    module_path: Vec<String>,
    item_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RustdocCrate {
    root: u64,
    #[serde(default)]
    crate_version: Option<String>,
    #[serde(default)]
    index: BTreeMap<String, RustdocItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct RustdocItem {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    docs: Option<String>,
    #[serde(default)]
    inner: Map<String, Value>,
    #[serde(default)]
    visibility: Value,
    #[serde(default)]
    deprecation: Option<Value>,
}

pub(super) async fn extract(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    cache_dir: &Path,
) -> Option<Result<ExtractResult, WebFetchFailure>> {
    let target = parse_target(url)?;
    Some(fetch_and_render(url, http, endpoints, cache_dir, target).await)
}

async fn fetch_and_render(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    cache_dir: &Path,
    target: Target,
) -> Result<ExtractResult, WebFetchFailure> {
    let cache = cache_path(cache_dir, &target.crate_name, &target.version);
    let (crate_, notes) = if let Some(crate_) = read_cache(&cache).await {
        (crate_, vec!["docs.rs rustdoc JSON cache".to_owned()])
    } else {
        let json_url = format!(
            "{}/crate/{}/{}/json.gz",
            endpoints.docs_rs.trim_end_matches('/'),
            target.crate_name,
            target.version
        );
        let response = http
            .get(&json_url, &{
                let mut headers = super::headers("application/gzip");
                headers.insert(
                    reqwest::header::ACCEPT,
                    HeaderValue::from_static("application/gzip, application/json"),
                );
                headers
            })
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(status_error("docs.rs", response.status));
        }
        let bytes = response.bytes;
        let json = tokio::task::spawn_blocking(move || decompress(&bytes))
            .await
            .map_err(|error| {
                WebFetchFailure::message(format!("docs.rs decompress task: {error}"))
            })??;
        let parsed: RustdocCrate = serde_json::from_str(&json)
            .map_err(|error| WebFetchFailure::message(format!("docs.rs rustdoc JSON: {error}")))?;
        write_cache(&cache, &json).await;
        (parsed, vec!["docs.rs rustdoc JSON".to_owned()])
    };

    let content = render(&crate_, &target)?;
    Ok(markdown_result(
        url,
        None,
        Some(200),
        "docs.rs",
        notes,
        content,
    ))
}

fn parse_target(url: &Url) -> Option<Target> {
    let segments: Vec<String> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    if segments.len() < 3 || segments[0] == "crate" {
        return None;
    }
    let crate_name = segments[0].clone();
    let version = segments[1].clone();
    let mut rest = segments[2..].to_vec();
    let mut item_name = None;
    if let Some(last) = rest.last().cloned() {
        if last == "index.html" {
            rest.pop();
        } else if let Some(stem) = last.strip_suffix(".html")
            && let Some((kind, name)) = stem.split_once('.')
            && is_item_kind(kind)
        {
            item_name = Some(name.to_owned());
            rest.pop();
        }
    }
    Some(Target {
        crate_name,
        version,
        module_path: rest,
        item_name,
    })
}

fn is_item_kind(kind: &str) -> bool {
    matches!(
        kind,
        "struct"
            | "trait"
            | "fn"
            | "enum"
            | "macro"
            | "type"
            | "constant"
            | "static"
            | "attr"
            | "derive"
            | "union"
            | "primitive"
    )
}

fn decompress(bytes: &[u8]) -> Result<String, WebFetchFailure> {
    let mut output = Vec::new();
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = flate2::read::GzDecoder::new(bytes).take(MAX_RUSTDOC_BYTES);
        decoder
            .read_to_end(&mut output)
            .map_err(|error| WebFetchFailure::message(format!("docs.rs gzip: {error}")))?;
    } else {
        output.extend_from_slice(bytes);
    }
    String::from_utf8(output)
        .map_err(|error| WebFetchFailure::message(format!("docs.rs JSON is not UTF-8: {error}")))
}

fn render(crate_: &RustdocCrate, target: &Target) -> Result<String, WebFetchFailure> {
    let root = crate_
        .index
        .get(&crate_.root.to_string())
        .ok_or_else(|| WebFetchFailure::message("docs.rs root item missing"))?;
    let mut current = root;
    for segment in target.module_path.iter().skip(1) {
        current = find_child_module(current, segment, &crate_.index).ok_or_else(|| {
            WebFetchFailure::message(format!("docs.rs module not found: {segment}"))
        })?;
    }

    let version = crate_.crate_version.as_deref().unwrap_or(&target.version);
    if let Some(name) = &target.item_name {
        let item = find_item(current, name, &crate_.index)
            .ok_or_else(|| WebFetchFailure::message(format!("docs.rs item not found: {name}")))?;
        return Ok(render_item(item, &crate_.index, version));
    }
    Ok(render_module(current, &crate_.index, target, version))
}

fn find_child_module<'a>(
    module: &RustdocItem,
    name: &str,
    index: &'a BTreeMap<String, RustdocItem>,
) -> Option<&'a RustdocItem> {
    for id in module_items(module) {
        let item = index.get(&id.to_string())?;
        if item.name.as_deref() == Some(name) && item.inner.contains_key("module") {
            return Some(item);
        }
    }
    None
}

fn find_item<'a>(
    module: &RustdocItem,
    name: &str,
    index: &'a BTreeMap<String, RustdocItem>,
) -> Option<&'a RustdocItem> {
    for id in module_items(module) {
        let item = index.get(&id.to_string())?;
        if item.name.as_deref() == Some(name) {
            return Some(item);
        }
        if let Some(use_) = item.inner.get("use")
            && use_.get("name").and_then(Value::as_str) == Some(name)
            && let Some(target) = use_.get("id").and_then(Value::as_u64)
        {
            return index.get(&target.to_string());
        }
    }
    None
}

fn module_items(module: &RustdocItem) -> Vec<u64> {
    module
        .inner
        .get("module")
        .and_then(|module| module.get("items"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

fn render_item(item: &RustdocItem, index: &BTreeMap<String, RustdocItem>, version: &str) -> String {
    let kind = item
        .inner
        .keys()
        .next()
        .map(String::as_str)
        .unwrap_or("item");
    let name = item.name.as_deref().unwrap_or("?");
    let mut markdown = format!("# {kind} {name}\n\n");
    if let Some(deprecation) = &item.deprecation {
        let note = deprecation
            .get("note")
            .and_then(Value::as_str)
            .map(|note| format!(": {note}"))
            .unwrap_or_default();
        markdown.push_str(&format!("> **Deprecated**{note}\n\n"));
    }
    if let Some(decl) = render_decl(item) {
        markdown.push_str(&format!("```rust\n{decl}\n```\n\n"));
    }
    if let Some(docs) = &item.docs
        && !docs.trim().is_empty()
    {
        markdown.push_str(docs.trim());
        markdown.push_str("\n\n");
    }

    if matches!(kind, "struct" | "enum" | "trait" | "union") {
        let data = item.inner.get(kind);
        let impls = data
            .and_then(|data| data.get("impls"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let trait_items = data
            .and_then(|data| data.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        if !trait_items.is_empty() {
            let mut required = Vec::new();
            let mut provided = Vec::new();
            for id in trait_items.iter().filter_map(Value::as_u64) {
                let Some(child) = index.get(&id.to_string()) else {
                    continue;
                };
                if let Some(function) = child.inner.get("function") {
                    let line = format!(
                        "- `{}`{}",
                        render_function_sig(child.name.as_deref().unwrap_or("?"), function),
                        first_line_suffix(child.docs.as_deref())
                    );
                    if function
                        .get("has_body")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        provided.push(line);
                    } else {
                        required.push(line);
                    }
                } else if child.inner.contains_key("assoc_type") {
                    required.push(format!(
                        "- `type {}`{}",
                        child.name.as_deref().unwrap_or("?"),
                        first_line_suffix(child.docs.as_deref())
                    ));
                }
            }
            if !required.is_empty() {
                markdown.push_str(&format!(
                    "## Required Methods\n\n{}\n\n",
                    required.join("\n")
                ));
            }
            if !provided.is_empty() {
                markdown.push_str(&format!(
                    "## Provided Methods\n\n{}\n\n",
                    provided.join("\n")
                ));
            }
        }

        let methods = collect_inherent_methods(&impls, index);
        if !methods.is_empty() {
            markdown.push_str(&format!("## Methods\n\n{}\n\n", methods.join("\n")));
        }
        let trait_impls = collect_trait_impls(&impls, index);
        if !trait_impls.is_empty() {
            markdown.push_str(&format!(
                "## Trait Implementations\n\n{}\n\n",
                trait_impls
                    .iter()
                    .map(|name| format!("- {name}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }

    if kind == "enum"
        && let Some(variants) = item
            .inner
            .get("enum")
            .and_then(|enum_| enum_.get("variants"))
            .and_then(Value::as_array)
    {
        let mut lines = Vec::new();
        for id in variants.iter().filter_map(Value::as_u64) {
            let Some(variant) = index.get(&id.to_string()) else {
                continue;
            };
            lines.push(format!(
                "- `{}`{}",
                variant.name.as_deref().unwrap_or("?"),
                first_line_suffix(variant.docs.as_deref())
            ));
        }
        if !lines.is_empty() {
            markdown.push_str(&format!("## Variants\n\n{}\n\n", lines.join("\n")));
        }
    }

    markdown.push_str(&format!("---\n*{version}*\n"));
    markdown
}

fn render_module(
    module: &RustdocItem,
    index: &BTreeMap<String, RustdocItem>,
    target: &Target,
    version: &str,
) -> String {
    let title = if target.module_path.is_empty() {
        target.crate_name.clone()
    } else {
        target.module_path.join("::")
    };
    let mut markdown = format!("# {title}\n\n");
    if let Some(docs) = &module.docs
        && !docs.trim().is_empty()
    {
        markdown.push_str(docs.trim());
        markdown.push_str("\n\n");
    }

    let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for id in module_items(module) {
        let Some(mut item) = index.get(&id.to_string()) else {
            continue;
        };
        let mut display_name = item.name.clone();
        if let Some(use_) = item.inner.get("use") {
            display_name = use_.get("name").and_then(Value::as_str).map(str::to_owned);
            if let Some(target) = use_.get("id").and_then(Value::as_u64) {
                match index.get(&target.to_string()) {
                    Some(resolved) => item = resolved,
                    None => continue,
                }
            } else {
                continue;
            }
        }
        let Some(display_name) = display_name else {
            continue;
        };
        if is_restricted(&item.visibility) {
            continue;
        }
        let kind = item
            .inner
            .keys()
            .next()
            .map(String::as_str)
            .unwrap_or("other");
        let line = if kind == "function"
            && let Some(decl) = render_decl(item)
        {
            format!("- `{decl}`{}", first_line_suffix(item.docs.as_deref()))
        } else {
            format!(
                "- **{display_name}**{}",
                first_line_suffix(item.docs.as_deref())
            )
        };
        groups.entry(kind).or_default().push(line);
    }

    for (kind, label) in [
        ("module", "Modules"),
        ("macro_def", "Macros"),
        ("struct", "Structs"),
        ("enum", "Enums"),
        ("trait", "Traits"),
        ("function", "Functions"),
        ("type_alias", "Type Aliases"),
        ("constant", "Constants"),
        ("static", "Statics"),
        ("union", "Unions"),
    ] {
        if let Some(items) = groups.get(kind)
            && !items.is_empty()
        {
            markdown.push_str(&format!("## {label}\n\n{}\n\n", items.join("\n")));
        }
    }
    markdown.push_str(&format!("---\n*{version}*\n"));
    markdown
}

fn is_restricted(visibility: &Value) -> bool {
    visibility.as_str() == Some("crate") || visibility.get("restricted").is_some()
}

fn render_decl(item: &RustdocItem) -> Option<String> {
    let name = item.name.as_deref().unwrap_or("?");
    if let Some(function) = item.inner.get("function") {
        return Some(render_function_sig(name, function));
    }
    if let Some(struct_) = item.inner.get("struct") {
        return Some(format!(
            "struct {name}{}",
            render_generics(struct_.get("generics"))
        ));
    }
    if let Some(enum_) = item.inner.get("enum") {
        return Some(format!(
            "enum {name}{}",
            render_generics(enum_.get("generics"))
        ));
    }
    if let Some(trait_) = item.inner.get("trait") {
        let prefix = if trait_
            .get("is_unsafe")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "unsafe "
        } else {
            ""
        };
        return Some(format!(
            "{prefix}trait {name}{}",
            render_generics(trait_.get("generics"))
        ));
    }
    if let Some(alias) = item.inner.get("type_alias") {
        let value = alias
            .get("type")
            .map(|ty| format!(" = {}", render_type(ty, 0)))
            .unwrap_or_default();
        return Some(format!(
            "type {name}{}{value}",
            render_generics(alias.get("generics"))
        ));
    }
    if item.inner.contains_key("macro_def") {
        return Some(format!("macro {name}!(...)"));
    }
    if let Some(constant) = item.inner.get("constant") {
        let value = constant
            .get("value")
            .and_then(Value::as_str)
            .map(|value| format!(" = {value}"))
            .unwrap_or_default();
        return Some(format!(
            "const {name}: {}{value}",
            constant
                .get("type")
                .map(|ty| render_type(ty, 0))
                .unwrap_or_else(|| "_".into())
        ));
    }
    None
}

fn render_function_sig(name: &str, function: &Value) -> String {
    let mut parts = Vec::new();
    for (key, label) in [
        ("is_const", "const"),
        ("is_async", "async"),
        ("is_unsafe", "unsafe"),
    ] {
        if function.get(key).and_then(Value::as_bool).unwrap_or(false) {
            parts.push(label);
        }
    }
    parts.push("fn");
    let generics = render_generics(function.get("generics"));
    let inputs = function
        .get("sig")
        .and_then(|sig| sig.get("inputs"))
        .and_then(Value::as_array)
        .map(|inputs| {
            inputs
                .iter()
                .map(|input| {
                    let name = input.get(0).and_then(Value::as_str).unwrap_or("_");
                    let ty = input
                        .get(1)
                        .map(|ty| render_type(ty, 0))
                        .unwrap_or_else(|| "_".into());
                    if name == "self" {
                        ty
                    } else {
                        format!("{name}: {ty}")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let output = function
        .get("sig")
        .and_then(|sig| sig.get("output"))
        .map(|ty| format!(" -> {}", render_type(ty, 0)))
        .unwrap_or_default();
    format!("{} {name}{generics}({inputs}){output}", parts.join(" "))
}

fn render_generics(generics: Option<&Value>) -> String {
    let Some(params) = generics
        .and_then(|generics| generics.get("params"))
        .and_then(Value::as_array)
    else {
        return String::new();
    };
    let names: Vec<&str> = params
        .iter()
        .filter(|param| {
            !param
                .get("kind")
                .and_then(|kind| kind.get("lifetime"))
                .is_some()
        })
        .filter_map(|param| param.get("name").and_then(Value::as_str))
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!("<{}>", names.join(", "))
    }
}

fn render_type(ty: &Value, depth: usize) -> String {
    if depth > 10 {
        return "_".into();
    }
    if let Some(text) = ty.as_str() {
        return text.to_owned();
    }
    if let Some(value) = ty.get("generic").and_then(Value::as_str) {
        return value.to_owned();
    }
    if let Some(value) = ty.get("primitive").and_then(Value::as_str) {
        return value.to_owned();
    }
    if ty.get("infer").is_some() {
        return "_".into();
    }
    if let Some(resolved) = ty.get("resolved_path") {
        let path = resolved.get("path").and_then(Value::as_str).unwrap_or("_");
        let args = resolved
            .get("args")
            .and_then(|args| args.get("angle_bracketed"))
            .and_then(|args| args.get("args"))
            .and_then(Value::as_array);
        let Some(args) = args else {
            return path.to_owned();
        };
        let rendered: Vec<String> = args
            .iter()
            .map(|arg| {
                if let Some(ty) = arg.get("type") {
                    render_type(ty, depth + 1)
                } else if let Some(lifetime) = arg.get("lifetime").and_then(Value::as_str) {
                    format!("'{lifetime}")
                } else {
                    "_".into()
                }
            })
            .collect();
        return format!("{path}<{}>", rendered.join(", "));
    }
    if let Some(borrowed) = ty.get("borrowed_ref") {
        let lifetime = borrowed
            .get("lifetime")
            .and_then(Value::as_str)
            .map(|lifetime| format!("'{lifetime} "))
            .unwrap_or_default();
        let mutable = if borrowed
            .get("is_mutable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "mut "
        } else {
            ""
        };
        return format!(
            "&{lifetime}{mutable}{}",
            borrowed
                .get("type")
                .map(|ty| render_type(ty, depth + 1))
                .unwrap_or_else(|| "_".into())
        );
    }
    if let Some(items) = ty.get("tuple").and_then(Value::as_array) {
        let rendered: Vec<String> = items
            .iter()
            .map(|item| render_type(item, depth + 1))
            .collect();
        return format!("({})", rendered.join(", "));
    }
    if let Some(item) = ty.get("slice") {
        return format!("[{}]", render_type(item, depth + 1));
    }
    if let Some(array) = ty.get("array") {
        let item = array
            .get("type")
            .map(|ty| render_type(ty, depth + 1))
            .unwrap_or_else(|| "_".into());
        let len = array.get("len").and_then(Value::as_str).unwrap_or("_");
        return format!("[{item}; {len}]");
    }
    if let Some(pointer) = ty.get("raw_pointer") {
        let mutable = if pointer
            .get("is_mutable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "mut "
        } else {
            "const "
        };
        return format!(
            "*{mutable}{}",
            pointer
                .get("type")
                .map(|ty| render_type(ty, depth + 1))
                .unwrap_or_else(|| "_".into())
        );
    }
    if let Some(qualified) = ty.get("qualified_path") {
        let name = qualified.get("name").and_then(Value::as_str).unwrap_or("_");
        let self_type = qualified
            .get("self_type")
            .map(|ty| render_type(ty, depth + 1))
            .unwrap_or_else(|| "_".into());
        return format!("<{self_type}>::{name}");
    }
    if let Some(bounds) = ty.get("impl_trait").and_then(Value::as_array) {
        let parts: Vec<String> = bounds
            .iter()
            .filter_map(|bound| {
                bound
                    .get("trait_bound")
                    .and_then(|bound| bound.get("trait"))
            })
            .map(|bound| render_type(bound, depth + 1))
            .collect();
        return format!("impl {}", parts.join(" + "));
    }
    if let Some(dyn_trait) = ty.get("dyn_trait") {
        let traits = dyn_trait
            .get("traits")
            .and_then(Value::as_array)
            .map(|traits| {
                traits
                    .iter()
                    .filter_map(|trait_| trait_.get("trait"))
                    .map(|trait_| render_type(trait_, depth + 1))
                    .collect::<Vec<_>>()
                    .join(" + ")
            })
            .unwrap_or_default();
        let lifetime = dyn_trait
            .get("lifetime")
            .and_then(Value::as_str)
            .map(|lifetime| format!(" + '{lifetime}"))
            .unwrap_or_default();
        return format!("dyn {traits}{lifetime}");
    }
    "_".into()
}

fn collect_inherent_methods(impls: &[Value], index: &BTreeMap<String, RustdocItem>) -> Vec<String> {
    let mut methods = Vec::new();
    for impl_id in impls.iter().filter_map(Value::as_u64) {
        let Some(impl_) = index.get(&impl_id.to_string()) else {
            continue;
        };
        let Some(data) = impl_.inner.get("impl") else {
            continue;
        };
        if data
            .get("is_synthetic")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || data.get("trait").is_some()
            || data.get("blanket_impl").is_some()
        {
            continue;
        }
        for item_id in data
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
        {
            let Some(method) = index.get(&item_id.to_string()) else {
                continue;
            };
            if let Some(function) = method.inner.get("function") {
                methods.push(format!(
                    "- `{}`{}",
                    render_function_sig(method.name.as_deref().unwrap_or("?"), function),
                    first_line_suffix(method.docs.as_deref())
                ));
            }
        }
    }
    methods
}

fn collect_trait_impls(impls: &[Value], index: &BTreeMap<String, RustdocItem>) -> Vec<String> {
    let mut names = Vec::new();
    for impl_id in impls.iter().filter_map(Value::as_u64) {
        let Some(impl_) = index.get(&impl_id.to_string()) else {
            continue;
        };
        let Some(data) = impl_.inner.get("impl") else {
            continue;
        };
        if data
            .get("is_synthetic")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || data.get("blanket_impl").is_some()
        {
            continue;
        }
        if let Some(trait_) = data.get("trait") {
            let name = render_type(
                &serde_json::json!({
                    "resolved_path": {
                        "path": trait_.get("path").and_then(Value::as_str).unwrap_or("_"),
                        "args": trait_.get("args").cloned().unwrap_or(Value::Null),
                    }
                }),
                0,
            );
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

fn first_line_suffix(docs: Option<&str>) -> String {
    let Some(docs) = docs else {
        return String::new();
    };
    let line = docs.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        String::new()
    } else if line.chars().count() > 200 {
        format!(" — {}...", line.chars().take(197).collect::<String>())
    } else {
        format!(" — {line}")
    }
}

fn cache_path(cache_dir: &Path, crate_name: &str, version: &str) -> PathBuf {
    cache_dir.join("cache").join("docs-rs").join(format!(
        "{}-{}.json",
        sanitize(crate_name),
        sanitize(version)
    ))
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

async fn read_cache(path: &Path) -> Option<RustdocCrate> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&text).ok()
}

async fn write_cache(path: &Path, json: &str) {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(path, json).await;
}

#[cfg(test)]
mod tests {
    use super::super::super::http::BatchBudget;
    use super::super::test_support::{endpoints, fetcher, gzip};
    use super::*;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const RUSTDOC: &str = r#"{
        "root": 0,
        "crate_version": "1.0.0",
        "index": {
            "0": {"name": "serde", "docs": "Serde crate.", "visibility": "public", "inner": {"module": {"items": [1]}}},
            "1": {"name": "Serialize", "docs": "A trait.", "visibility": "public", "inner": {"trait": {"generics": {"params": []}, "is_auto": false, "is_unsafe": false, "items": [2], "impls": []}}},
            "2": {"name": "serialize", "docs": "Serialize this.", "visibility": "public", "inner": {"function": {"sig": {"inputs": [["self", {"generic": "Self"}]], "output": null}, "generics": {"params": []}, "has_body": true, "is_async": false, "is_unsafe": false, "is_const": false}}}
        }
    }"#;

    #[tokio::test]
    async fn renders_item_and_reuses_session_cache() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/crate/serde/latest/json.gz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(gzip(RUSTDOC.as_bytes()))
                    .insert_header("content-type", "application/gzip"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let endpoints = endpoints(&server);
        let cache = tempfile::tempdir().expect("cache");
        let batch = BatchBudget::new(1024 * 1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let url = Url::parse("https://docs.rs/serde/latest/serde/trait.Serialize.html").unwrap();
        let first = extract(&url, &http, &endpoints, cache.path())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.method, "docs.rs");
        assert!(first.raw_content.contains("# trait Serialize"));
        assert!(first.raw_content.contains("A trait."));
        assert!(first.raw_content.contains("## Provided Methods"));
        assert!(first.raw_content.contains("fn serialize(Self)"));
        let second = extract(&url, &http, &endpoints, cache.path())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.raw_content, first.raw_content);
        server.verify().await;
    }

    #[tokio::test]
    async fn bad_rustdoc_json_is_returned_for_fallback() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/crate/serde/latest/json.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"not gzip", "application/gzip"))
            .mount(&server)
            .await;
        let endpoints = endpoints(&server);
        let cache = tempfile::tempdir().expect("cache");
        let batch = BatchBudget::new(1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let error = extract(
            &Url::parse("https://docs.rs/serde/latest/serde/trait.Serialize.html").unwrap(),
            &http,
            &endpoints,
            cache.path(),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(matches!(error, WebFetchFailure::Message(_)));
    }
}

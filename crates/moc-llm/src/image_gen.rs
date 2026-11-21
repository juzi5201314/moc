//! OpenAI Images API 兼容客户端（`/v1/images/generations` 与 `/v1/images/edits`）。
//!
//! 本模块不内置任何模型名白名单：请求字段遵循「未指定即不发送」，
//! 以兼容 OpenAI GPT Image 系列与任意第三方兼容网关；上游返回非 2xx 时
//! 原样保留 HTTP 状态码与响应体文本。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::multipart::{Form, Part};
use reqwest::redirect::Policy;
use reqwest::{Client as ReqwestClient, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::client::Client;
use crate::config::{Auth, LlmConfig};
use crate::traffic::{record_rx, record_tx};

/// 响应体读取上限（允许单次返回多张高分辨率 base64 图片）。
const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
/// 从响应 `url` 拉取单张图片的上限。
const MAX_REMOTE_IMAGE_BYTES: usize = 100 * 1024 * 1024;
/// 从响应 `url` 拉取图片时允许的最大重定向跳数。
const MAX_REMOTE_IMAGE_REDIRECTS: usize = 5;
/// 未配置 `timeout_secs` 时，从响应 `url` 拉取图片的默认总超时（秒）。
const DEFAULT_REMOTE_IMAGE_TIMEOUT_SECS: f64 = 60.0;
/// 未配置 `connect_timeout_secs` 时，从响应 `url` 拉取图片的默认连接超时（秒）。
const DEFAULT_REMOTE_IMAGE_CONNECT_TIMEOUT_SECS: f64 = 15.0;
/// 上游错误响应体读取上限。
const MAX_ERROR_BODY_BYTES: usize = 256 * 1024;

/// `/v1/images/edits` 的请求编码方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditEncoding {
    /// 标准 multipart：单图用 `image`，多图用 `image[]`。
    #[default]
    Multipart,
    /// 兼容部分第三方网关：多图也重复使用 `image` 字段名。
    MultipartRepeatImage,
    /// OpenAI 2026 JSON 编辑体：`images: [{ image_url: "data:..." }]`。
    Json,
}

/// 图像生成 / 编辑可选参数；`None` 的字段不写入线上请求。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageGenParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_compression: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_fidelity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_encoding: Option<EditEncoding>,
    /// 透传到请求顶层的自定义扩展字段；与标准字段同名时以用户输入为准。
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra_params: Map<String, Value>,
}

/// 传入 `/v1/images/edits` 的参考图。
#[derive(Debug, Clone)]
pub struct ImageInputFile {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// 单张生成结果（原始字节 + 可选元信息）。
#[derive(Debug, Clone)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    /// 上游声明的 MIME / 格式提示（如 `image/png`、`image/gif`），实际以魔数探测为准。
    pub mime_hint: Option<String>,
    pub revised_prompt: Option<String>,
    pub source_url: Option<String>,
}

/// 一次图像生成 / 编辑调用的结果。
#[derive(Debug, Clone)]
pub struct ImageGenResponse {
    pub images: Vec<GeneratedImage>,
    pub output_format: Option<String>,
    pub usage: Option<Value>,
}

/// 图像生成错误。
#[derive(Debug, thiserror::Error)]
pub enum ImageGenError {
    #[error("HTTP {status}: {message}")]
    Upstream {
        status: u16,
        message: String,
        raw_body: String,
    },
    #[error("{0}")]
    Transport(String),
    #[error("{0}")]
    Decode(String),
}

impl ImageGenError {
    pub fn upstream_status(&self) -> Option<u16> {
        match self {
            Self::Upstream { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn raw_body(&self) -> Option<&str> {
        match self {
            Self::Upstream { raw_body, .. } => Some(raw_body.as_str()),
            _ => None,
        }
    }
}

impl Client {
    /// 调用 OpenAI 兼容图像生成或编辑接口：无输入图走 `/images/generations`，有输入图走 `/images/edits`。
    pub async fn generate_images(
        &self,
        cfg: &LlmConfig,
        prompt: &str,
        inputs: &[ImageInputFile],
        params: &ImageGenParams,
    ) -> Result<ImageGenResponse, ImageGenError> {
        let http = self
            .http_for(cfg)
            .map_err(|error| ImageGenError::Transport(error.to_string()))?;

        let response = if inputs.is_empty() {
            let url = cfg.images_generations_url();
            let body = build_json_body(cfg, prompt, None, params);
            let encoded = serde_json::to_vec(&body)
                .map_err(|error| ImageGenError::Decode(error.to_string()))?;
            record_tx(encoded.len() as u64);
            let mut req = http
                .post(&url)
                .header(CONTENT_TYPE, "application/json")
                .body(encoded);
            req = apply_common_headers(req, cfg);
            req.send()
                .await
                .map_err(|error| ImageGenError::Transport(error.to_string()))?
        } else {
            let url = cfg.images_edits_url();
            let encoding = params.edit_encoding.unwrap_or_default();
            match encoding {
                EditEncoding::Json => {
                    let body = build_json_body(cfg, prompt, Some(inputs), params);
                    let encoded = serde_json::to_vec(&body)
                        .map_err(|error| ImageGenError::Decode(error.to_string()))?;
                    record_tx(encoded.len() as u64);
                    let mut req = http
                        .post(&url)
                        .header(CONTENT_TYPE, "application/json")
                        .body(encoded);
                    req = apply_common_headers(req, cfg);
                    req.send()
                        .await
                        .map_err(|error| ImageGenError::Transport(error.to_string()))?
                }
                EditEncoding::Multipart | EditEncoding::MultipartRepeatImage => {
                    let repeat_plain = encoding == EditEncoding::MultipartRepeatImage;
                    let form = build_multipart_form(cfg, prompt, inputs, params, repeat_plain)?;
                    let approx_tx: u64 = inputs.iter().map(|item| item.bytes.len() as u64).sum();
                    record_tx(approx_tx);
                    let mut req = http.post(&url).multipart(form);
                    req = apply_common_headers(req, cfg);
                    req.send()
                        .await
                        .map_err(|error| ImageGenError::Transport(error.to_string()))?
                }
            }
        };

        let status = response.status();
        if !status.is_success() {
            let raw = read_limited(response, MAX_ERROR_BODY_BYTES).await;
            let raw_body = String::from_utf8_lossy(&raw).into_owned();
            let message = extract_upstream_message(&raw_body)
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("Upstream error").into());
            return Err(ImageGenError::Upstream {
                status: status.as_u16(),
                message,
                raw_body,
            });
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = read_limited_checked(response, MAX_RESPONSE_BYTES).await?;

        // 兜底：部分非标兼容服务直接返回二进制图片响应体。
        if content_type
            .as_deref()
            .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("image/"))
        {
            return Ok(ImageGenResponse {
                images: vec![GeneratedImage {
                    bytes,
                    mime_hint: content_type,
                    revised_prompt: None,
                    source_url: None,
                }],
                output_format: None,
                usage: None,
            });
        }

        parse_json_response(cfg, &bytes).await
    }
}

fn apply_common_headers(
    mut builder: reqwest::RequestBuilder,
    cfg: &LlmConfig,
) -> reqwest::RequestBuilder {
    match &cfg.auth {
        Auth::None => {}
        Auth::Bearer(token) => {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        Auth::Header { name, value } => {
            builder = builder.header(name.as_str(), value.as_str());
        }
    }
    for (name, value) in &cfg.headers {
        // multipart 请求的 Content-Type 必须由 reqwest 带 boundary 生成，跳过人工覆盖。
        if name.eq_ignore_ascii_case("content-type") {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    if let Some(timeout) = positive_duration(cfg.timeout_secs) {
        builder = builder.timeout(timeout);
    }
    builder
}

fn build_json_body(
    cfg: &LlmConfig,
    prompt: &str,
    inputs: Option<&[ImageInputFile]>,
    params: &ImageGenParams,
) -> Map<String, Value> {
    // 优先级：供应商/模型 compat.extra_body < 本次请求 extra_params < 显式表单参数 < model/prompt/images。
    let mut body = cfg.extra_body.clone();
    for (key, value) in &params.extra_params {
        body.insert(key.clone(), value.clone());
    }
    insert_standard_fields(&mut body, params);
    body.insert("model".into(), Value::String(cfg.model.clone()));
    body.insert("prompt".into(), Value::String(prompt.to_owned()));
    if let Some(files) = inputs {
        let images = files
            .iter()
            .map(|file| {
                let data_url = format!(
                    "data:{};base64,{}",
                    file.mime_type,
                    STANDARD.encode(&file.bytes)
                );
                serde_json::json!({ "image_url": data_url })
            })
            .collect::<Vec<_>>();
        body.insert("images".into(), Value::Array(images));
    }
    body
}

fn build_multipart_form(
    cfg: &LlmConfig,
    prompt: &str,
    inputs: &[ImageInputFile],
    params: &ImageGenParams,
    repeat_plain_image: bool,
) -> Result<Form, ImageGenError> {
    let mut fields = cfg.extra_body.clone();
    for (key, value) in &params.extra_params {
        fields.insert(key.clone(), value.clone());
    }
    insert_standard_fields(&mut fields, params);
    fields.insert("model".into(), Value::String(cfg.model.clone()));
    fields.insert("prompt".into(), Value::String(prompt.to_owned()));

    let mut form = Form::new();
    for (key, value) in fields {
        let text = match value {
            Value::Null => continue,
            Value::String(s) => s,
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            other => other.to_string(),
        };
        form = form.text(key, text);
    }

    let field_name = if inputs.len() > 1 && !repeat_plain_image {
        "image[]"
    } else {
        "image"
    };
    for file in inputs {
        let part = Part::bytes(file.bytes.clone())
            .file_name(file.filename.clone())
            .mime_str(&file.mime_type)
            .map_err(|error| ImageGenError::Decode(error.to_string()))?;
        form = form.part(field_name, part);
    }
    Ok(form)
}

fn insert_standard_fields(body: &mut Map<String, Value>, params: &ImageGenParams) {
    if let Some(n) = params.n {
        body.insert("n".into(), Value::Number(n.into()));
    }
    for (key, value) in [
        ("size", &params.size),
        ("aspect_ratio", &params.aspect_ratio),
        ("image_size", &params.image_size),
        ("quality", &params.quality),
        ("background", &params.background),
        ("output_format", &params.output_format),
        ("response_format", &params.response_format),
        ("input_fidelity", &params.input_fidelity),
        ("style", &params.style),
        ("moderation", &params.moderation),
    ] {
        if let Some(text) = value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            body.insert(key.into(), Value::String(text.to_owned()));
        }
    }
    if let Some(compression) = params.output_compression {
        body.insert("output_compression".into(), Value::Number(compression.into()));
    }
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    data: Vec<WireImageItem>,
    output_format: Option<String>,
    usage: Option<Value>,
}

#[derive(Deserialize)]
struct WireImageItem {
    b64_json: Option<String>,
    url: Option<String>,
    revised_prompt: Option<String>,
    media_type: Option<String>,
}

async fn parse_json_response(
    cfg: &LlmConfig,
    body: &[u8],
) -> Result<ImageGenResponse, ImageGenError> {
    let wire: WireResponse = serde_json::from_slice(body).map_err(|error| {
        let preview = String::from_utf8_lossy(&body[..body.len().min(512)]);
        ImageGenError::Decode(format!("无法解析上游 JSON 响应：{error}（响应片段：{preview}）"))
    })?;
    if wire.data.is_empty() {
        return Err(ImageGenError::Decode("上游返回了空的 data 数组".into()));
    }
    let default_mime = wire
        .output_format
        .as_deref()
        .and_then(format_to_mime)
        .map(str::to_owned);

    let mut images = Vec::with_capacity(wire.data.len());
    for item in wire.data {
        if let Some(raw_b64) = item.b64_json.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let (data_mime, b64) = strip_data_url(raw_b64);
            let bytes = STANDARD
                .decode(b64.trim())
                .map_err(|error| ImageGenError::Decode(format!("Base64 解码失败：{error}")))?;
            images.push(GeneratedImage {
                bytes,
                mime_hint: data_mime
                    .map(str::to_owned)
                    .or(item.media_type)
                    .or_else(|| default_mime.clone()),
                revised_prompt: item.revised_prompt,
                source_url: item.url,
            });
        } else if let Some(url) = item.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let (bytes, mime) = download_remote_image(cfg, url).await?;
            images.push(GeneratedImage {
                bytes,
                mime_hint: mime.or(item.media_type).or_else(|| default_mime.clone()),
                revised_prompt: item.revised_prompt,
                source_url: Some(url.to_owned()),
            });
        } else {
            return Err(ImageGenError::Decode(
                "上游响应条目既不包含 b64_json 也不包含 url".into(),
            ));
        }
    }

    Ok(ImageGenResponse {
        images,
        output_format: wire.output_format,
        usage: wire.usage,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SameOriginAllowance {
    LocalhostLoopbackOnly,
    LiteralNonPublicIp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AllowedUpstreamOrigin {
    scheme: String,
    host_key: String,
    port: u16,
    kind: SameOriginAllowance,
}

fn canonical_host_and_ip(url: &Url) -> Option<(String, Option<IpAddr>)> {
    let raw = url.host_str()?;
    let trimmed = raw
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw);
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        Some((ip.to_string(), Some(ip)))
    } else {
        Some((raw.trim_end_matches('.').to_ascii_lowercase(), None))
    }
}

fn allowed_upstream_origin(cfg: &LlmConfig) -> Option<AllowedUpstreamOrigin> {
    let url = Url::parse(&cfg.base_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let port = url.port_or_known_default()?;
    let (host_key, ip_opt) = canonical_host_and_ip(&url)?;
    let kind = match ip_opt {
        Some(ip) if !is_public_ip(ip) => SameOriginAllowance::LiteralNonPublicIp,
        Some(_) => return None,
        None if host_key == "localhost" => SameOriginAllowance::LocalhostLoopbackOnly,
        None => return None,
    };
    Some(AllowedUpstreamOrigin {
        scheme: url.scheme().to_owned(),
        host_key,
        port,
        kind,
    })
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_documentation()
        || ip.is_multicast()
    {
        return false;
    }
    // 0.0.0.0/8（当前网络）
    if a == 0 {
        return false;
    }
    // 100.64.0.0/10（CGNAT 共享地址空间，RFC 6598）
    if a == 100 && (b & 0b1100_0000) == 64 {
        return false;
    }
    // 192.0.0.0/24（IETF 协议保留段）
    if a == 192 && b == 0 && c == 0 {
        return false;
    }
    // 198.18.0.0/15（基准测试网络，RFC 2544）
    if a == 198 && (b == 18 || b == 19) {
        return false;
    }
    // 240.0.0.0/4（保留地址段，含受限广播）
    if a >= 240 {
        return false;
    }
    true
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(v4);
    }
    if let Some(v4) = ip.to_ipv4() {
        return is_public_ipv4(v4);
    }
    let segs = ip.segments();
    // NAT64 64:ff9b::/96：校验内嵌 IPv4 地址
    if segs[0] == 0x0064
        && segs[1] == 0xff9b
        && segs[2] == 0
        && segs[3] == 0
        && segs[4] == 0
        && segs[5] == 0
    {
        let v4 = Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            segs[6] as u8,
            (segs[7] >> 8) as u8,
            segs[7] as u8,
        );
        return is_public_ipv4(v4);
    }
    // NAT64 局部使用段 64:ff9b:1::/48
    if segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2] == 0x0001 {
        return false;
    }
    // 6to4 2002::/16：校验内嵌 IPv4 地址
    if segs[0] == 0x2002 {
        let v4 = Ipv4Addr::new(
            (segs[1] >> 8) as u8,
            segs[1] as u8,
            (segs[2] >> 8) as u8,
            segs[2] as u8,
        );
        return is_public_ipv4(v4);
    }
    // Teredo 2001::/32
    if segs[0] == 0x2001 && segs[1] == 0 {
        return false;
    }
    // ULA fc00::/7
    if (segs[0] & 0xfe00) == 0xfc00 {
        return false;
    }
    // Link-local fe80::/10
    if (segs[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    // Site-local fec0::/10
    if (segs[0] & 0xffc0) == 0xfec0 {
        return false;
    }
    // Documentation 2001:db8::/32
    if segs[0] == 0x2001 && segs[1] == 0x0db8 {
        return false;
    }
    // Discard 100::/64
    if segs[0] == 0x0100 && segs[1] == 0 && segs[2] == 0 && segs[3] == 0 {
        return false;
    }
    true
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn positive_duration(secs: Option<f64>) -> Option<Duration> {
    let secs = secs.filter(|s| s.is_finite() && *s > 0.0)?;
    Duration::try_from_secs_f64(secs).ok()
}

fn remote_image_timeout(cfg: &LlmConfig) -> Duration {
    positive_duration(cfg.timeout_secs)
        .unwrap_or_else(|| Duration::from_secs_f64(DEFAULT_REMOTE_IMAGE_TIMEOUT_SECS))
}

fn remote_image_connect_timeout(cfg: &LlmConfig) -> Duration {
    positive_duration(cfg.connect_timeout_secs)
        .unwrap_or_else(|| Duration::from_secs_f64(DEFAULT_REMOTE_IMAGE_CONNECT_TIMEOUT_SECS))
}

/// 校验图片下载目标 URL 的协议与目标网络，必要时解析域名并钉死 IP，防止 SSRF 与 DNS 重绑定。
async fn validate_and_pin_target(
    url: &Url,
    upstream: Option<&AllowedUpstreamOrigin>,
) -> Result<Option<(String, Vec<SocketAddr>)>, ImageGenError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ImageGenError::Transport(format!(
            "拒绝下载不受支持的图片协议：{}",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ImageGenError::Transport(
            "拒绝下载包含内嵌凭据的图片 URL".into(),
        ));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ImageGenError::Transport("图片 URL 缺少有效端口".into()))?;
    let (host_key, ip_opt) = canonical_host_and_ip(url)
        .ok_or_else(|| ImageGenError::Transport("图片 URL 缺少有效主机名".into()))?;
    let raw_host = url
        .host_str()
        .ok_or_else(|| ImageGenError::Transport("图片 URL 缺少有效主机名".into()))?;

    let is_same_upstream_origin = upstream.is_some_and(|u| {
        u.scheme == url.scheme() && u.host_key == host_key && u.port == port
    });

    if let Some(ip) = ip_opt {
        let allowed = is_public_ip(ip)
            || (is_same_upstream_origin
                && upstream
                    .is_some_and(|u| u.kind == SameOriginAllowance::LiteralNonPublicIp));
        if !allowed {
            return Err(ImageGenError::Transport(format!(
                "拒绝访问非公网图片地址：{ip}"
            )));
        }
        return Ok(None);
    }

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((raw_host, port))
        .await
        .map_err(|error| ImageGenError::Transport(format!("解析图片域名失败：{error}")))?
        .collect();
    if addrs.is_empty() {
        return Err(ImageGenError::Transport(format!(
            "图片域名未解析出任何地址：{raw_host}"
        )));
    }

    let allow_localhost_loopback = is_same_upstream_origin
        && upstream.is_some_and(|u| u.kind == SameOriginAllowance::LocalhostLoopbackOnly);

    for addr in &addrs {
        let ip = addr.ip();
        let ok = if allow_localhost_loopback {
            ip.is_loopback()
        } else {
            is_public_ip(ip)
        };
        if !ok {
            return Err(ImageGenError::Transport(format!(
                "拒绝访问解析到非公网地址的图片域名：{raw_host} -> {ip}"
            )));
        }
    }

    Ok(Some((raw_host.to_owned(), addrs)))
}

async fn download_remote_image(
    cfg: &LlmConfig,
    raw_url: &str,
) -> Result<(Vec<u8>, Option<String>), ImageGenError> {
    let timeout = remote_image_timeout(cfg);
    let deadline = Instant::now() + timeout;
    match tokio::time::timeout_at(deadline, download_remote_image_inner(cfg, raw_url, deadline))
        .await
    {
        Ok(result) => result,
        Err(_) => Err(ImageGenError::Transport(format!(
            "下载生成图片超时（超过 {:.1}s）",
            timeout.as_secs_f64()
        ))),
    }
}

async fn download_remote_image_inner(
    cfg: &LlmConfig,
    raw_url: &str,
    deadline: Instant,
) -> Result<(Vec<u8>, Option<String>), ImageGenError> {
    let mut current_url = Url::parse(raw_url)
        .map_err(|error| ImageGenError::Transport(format!("无效的生成图片 URL：{error}")))?;
    let upstream = allowed_upstream_origin(cfg);
    let base_connect_timeout = remote_image_connect_timeout(cfg);

    for redirects in 0..=MAX_REMOTE_IMAGE_REDIRECTS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ImageGenError::Transport("下载生成图片超时".into()));
        }

        let pinned = validate_and_pin_target(&current_url, upstream.as_ref()).await?;
        let mut builder = ReqwestClient::builder()
            .redirect(Policy::none())
            .connect_timeout(base_connect_timeout.min(remaining));
        if let Some((domain, addrs)) = pinned {
            builder = builder.resolve_to_addrs(&domain, &addrs);
        }
        let hop_client = builder
            .build()
            .map_err(|error| ImageGenError::Transport(format!("构建图片下载客户端失败：{error}")))?;

        let response = hop_client
            .get(current_url.as_str())
            .timeout(remaining)
            .send()
            .await
            .map_err(|error| ImageGenError::Transport(format!("下载生成图片失败：{error}")))?;
        let status = response.status();
        if status.is_redirection() {
            if redirects >= MAX_REMOTE_IMAGE_REDIRECTS {
                return Err(ImageGenError::Transport(format!(
                    "下载生成图片重定向次数超过上限（{MAX_REMOTE_IMAGE_REDIRECTS}）"
                )));
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    ImageGenError::Transport("下载生成图片收到重定向但缺少 Location 头".into())
                })?;
            current_url = current_url.join(location).map_err(|error| {
                ImageGenError::Transport(format!("下载生成图片重定向目标无效：{error}"))
            })?;
            continue;
        }
        if !status.is_success() {
            return Err(ImageGenError::Transport(format!(
                "下载生成图片失败：HTTP {status}"
            )));
        }
        let mime = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|ct| ct.split(';').next().unwrap_or(ct).trim().to_owned());
        let bytes = read_limited_checked(response, MAX_REMOTE_IMAGE_BYTES).await?;
        return Ok((bytes, mime));
    }

    unreachable!("重定向循环已在上限处返回")
}

fn strip_data_url(input: &str) -> (Option<&str>, &str) {
    if let Some(rest) = input.strip_prefix("data:")
        && let Some((meta, payload)) = rest.split_once(";base64,")
    {
        return (Some(meta), payload);
    }
    (None, input)
}

fn format_to_mime(format: &str) -> Option<&'static str> {
    match format.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpeg" | "jpg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "avif" => Some("image/avif"),
        _ => None,
    }
}

fn extract_upstream_message(raw: &str) -> Option<String> {
    let json: Value = serde_json::from_str(raw).ok()?;
    json.get("error")
        .and_then(|err| err.get("message").or(Some(err)))
        .and_then(Value::as_str)
        .or_else(|| json.get("message").and_then(Value::as_str))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        record_rx(chunk.len() as u64);
        let room = limit.saturating_sub(out.len());
        if room == 0 {
            break;
        }
        let take = chunk.len().min(room);
        out.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            break;
        }
    }
    out
}

async fn read_limited_checked(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ImageGenError> {
    let mut out = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ImageGenError::Transport(error.to_string()))?
    {
        record_rx(chunk.len() as u64);
        if out.len() + chunk.len() > limit {
            return Err(ImageGenError::Decode(format!(
                "图片响应体超过大小上限（{} MiB）",
                limit / (1024 * 1024)
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn generations_omits_unset_fields_and_parses_b64_and_url() {
        let server = MockServer::start().await;
        let pixel = b"\x89PNG\r\n\x1a\nfake";
        Mock::given(method("GET"))
            .and(path("/img/1.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(pixel.to_vec()),
            )
            .mount(&server)
            .await;

        let img_url = format!("{}/img/1.png", server.uri());
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output_format": "png",
                "data": [
                    { "b64_json": STANDARD.encode(pixel), "revised_prompt": "revised" },
                    { "url": img_url }
                ]
            })))
            .mount(&server)
            .await;

        let cfg = LlmConfig {
            base_url: format!("{}/v1", server.uri()),
            model: "custom-flux-model".into(),
            auth: Auth::Bearer("test-key".into()),
            ..Default::default()
        };
        let params = ImageGenParams {
            size: Some("1024x1024".into()),
            ..Default::default()
        };
        let client = Client::new();
        let resp = client
            .generate_images(&cfg, "a red apple", &[], &params)
            .await
            .expect("生成成功");

        assert_eq!(resp.images.len(), 2);
        assert_eq!(resp.images[0].bytes, pixel);
        assert_eq!(resp.images[0].revised_prompt.as_deref(), Some("revised"));
        assert_eq!(resp.images[1].bytes, pixel);

        let requests = server.received_requests().await.unwrap();
        let gen_req = requests
            .iter()
            .find(|r| r.url.path() == "/v1/images/generations")
            .unwrap();
        let sent: Value = serde_json::from_slice(&gen_req.body).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({
                "model": "custom-flux-model",
                "prompt": "a red apple",
                "size": "1024x1024"
            })
        );
    }

    #[tokio::test]
    async fn edits_multipart_uses_image_array_and_preserves_upstream_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/edits"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"error":{"message":"model does not support edits"}}"#),
            )
            .mount(&server)
            .await;

        let cfg = LlmConfig {
            base_url: server.uri(),
            model: "txt2img-only".into(),
            ..Default::default()
        };
        let inputs = vec![
            ImageInputFile {
                filename: "a.png".into(),
                mime_type: "image/png".into(),
                bytes: b"img1".to_vec(),
            },
            ImageInputFile {
                filename: "b.png".into(),
                mime_type: "image/png".into(),
                bytes: b"img2".to_vec(),
            },
        ];
        let err = Client::new()
            .generate_images(&cfg, "make it blue", &inputs, &ImageGenParams::default())
            .await
            .unwrap_err();
        assert_eq!(err.upstream_status(), Some(400));
        assert!(
            err.raw_body()
                .unwrap()
                .contains("model does not support edits")
        );

        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(body.contains("name=\"image[]\""), "多图应使用 image[] 字段");
        assert!(body.contains("filename=\"a.png\""));
        assert!(body.contains("filename=\"b.png\""));
    }

    #[tokio::test]
    async fn download_remote_image_rejects_disallowed_schemes_and_private_targets() {
        let upstream = MockServer::start().await;
        let other_local = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/secret.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"secret".to_vec()))
            .mount(&other_local)
            .await;

        let redirect_target = format!("{}/secret.png", other_local.uri());
        Mock::given(method("GET"))
            .and(path("/redirect-ssrf"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", redirect_target.as_str()),
            )
            .mount(&upstream)
            .await;

        let cfg = LlmConfig {
            base_url: format!("{}/v1", upstream.uri()),
            ..Default::default()
        };

        // 拒绝非 http/https 协议
        let err = download_remote_image(&cfg, "file:///etc/passwd")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("协议"), "{err}");

        // 拒绝私网与跨端口回环地址
        let err = download_remote_image(&cfg, "http://10.0.0.1/img.png")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("非公网"), "{err}");

        let err = download_remote_image(&cfg, &format!("{}/secret.png", other_local.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("非公网"), "{err}");

        // 拒绝同源接口重定向到另一本地端口
        let err = download_remote_image(&cfg, &format!("{}/redirect-ssrf", upstream.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("非公网"), "{err}");
        assert!(other_local.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn download_remote_image_honors_cfg_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(2))
                    .set_body_bytes(b"slow".to_vec()),
            )
            .mount(&server)
            .await;

        let cfg = LlmConfig {
            base_url: format!("{}/v1", server.uri()),
            timeout_secs: Some(0.1),
            ..Default::default()
        };

        let err = download_remote_image(&cfg, &format!("{}/slow.png", server.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("超时"), "{err}");
    }
}

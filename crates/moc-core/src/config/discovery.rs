//! 供应商连通性测试与模型目录发现：解析上游元数据并补全已知模型默认能力。

use std::time::{Duration, Instant};

use super::model::{ProviderDocument, ResolvedProvider};
use super::presets;
use crate::CoreError;
use moc_i18n::t;
use moc_llm::{Auth, EndpointStyle, LlmConfig, Protocol};
use moc_protocol::{CatalogModel, ModelPricing, ProviderTestResult};
use serde_json::Value;

fn resolve_for_probe(id: &str, value: Value) -> Result<LlmConfig, CoreError> {
    let mut document: ProviderDocument =
        serde_json::from_value(value).map_err(|error| CoreError::Config(error.to_string()))?;
    document.models.clear();
    // 即使供应商处于停用状态，仍允许先测试连接或拉取模型再启用。
    document.enabled = true;
    let resolved = ResolvedProvider::from_document(id, document)?;
    Ok(resolved.catalog_config())
}

fn build_client(config: &LlmConfig, timeout: Option<Duration>) -> Result<reqwest::Client, CoreError> {
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(seconds) = config.connect_timeout_secs {
        builder = builder.connect_timeout(Duration::from_secs_f64(seconds));
    }
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder.build().map_err(failure)
}

fn build_request(
    client: &reqwest::Client,
    config: &LlmConfig,
    url: reqwest::Url,
) -> reqwest::RequestBuilder {
    let mut request = client.get(url);
    for (key, value) in &config.headers {
        request = request.header(key, value);
    }
    if config.protocol == Protocol::AnthropicMessages {
        request = request.header("anthropic-version", "2023-06-01");
    }
    match &config.auth {
        Auth::None => request,
        Auth::Bearer(key) if matches!(config.endpoint, EndpointStyle::Azure { .. }) => {
            request.header("api-key", key)
        }
        Auth::Bearer(key) => request.bearer_auth(key),
        Auth::Header { name, value } => request.header(name, value),
    }
}

fn parse_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        .filter(|&n| n > 0)
}

fn parse_f64(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn round_price(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn parse_pricing(row: &Value) -> Option<ModelPricing> {
    let pricing = row.get("pricing")?.as_object()?;
    // OpenRouter 风格：`prompt` / `completion` 为每 token 美元单价，换算为每百万 token。
    if let (Some(prompt), Some(completion)) = (
        parse_f64(pricing.get("prompt")),
        parse_f64(pricing.get("completion")),
    ) {
        let scale = 1_000_000.0;
        let cache_read = parse_f64(pricing.get("input_cache_read")).map(|v| round_price(v * scale));
        let cache_write =
            parse_f64(pricing.get("input_cache_write")).map(|v| round_price(v * scale));
        return Some(ModelPricing {
            input: round_price(prompt * scale),
            output: round_price(completion * scale),
            cache_read,
            cache_write,
        });
    }
    // 直接以每百万 token 美元计价的格式（`input` / `output`）。
    let input = parse_f64(pricing.get("input"))?;
    let output = parse_f64(pricing.get("output"))?;
    Some(ModelPricing {
        input: round_price(input),
        output: round_price(output),
        cache_read: parse_f64(pricing.get("cache_read")).map(round_price),
        cache_write: parse_f64(pricing.get("cache_write")).map(round_price),
    })
}

fn parse_catalog_row(row: &Value) -> Result<CatalogModel, CoreError> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Config(t!("core.err.model_entry_no_id").into_owned()))?;
    let name = row
        .get("display_name")
        .or_else(|| row.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(id);
    let context_window = parse_u64(
        row.get("context_length")
            .or_else(|| row.get("context_window"))
            .or_else(|| row.get("max_context_length"))
            .or_else(|| row.get("input_token_limit")),
    );
    let max_output_tokens = parse_u64(
        row.pointer("/top_provider/max_completion_tokens")
            .or_else(|| row.get("max_output_tokens"))
            .or_else(|| row.get("max_completion_tokens"))
            .or_else(|| row.get("output_token_limit")),
    )
    .and_then(|v| u32::try_from(v).ok());
    let has_vision = row
        .pointer("/architecture/input_modalities")
        .and_then(Value::as_array)
        .map(|items| items.iter().any(|v| v.as_str() == Some("image")))
        .or_else(|| row.pointer("/capabilities/vision").and_then(Value::as_bool));
    let input = match has_vision {
        Some(true) => Some(vec!["text".into(), "image".into()]),
        Some(false) => Some(vec!["text".into()]),
        None => None,
    };
    let reasoning = row
        .pointer("/capabilities/reasoning")
        .and_then(Value::as_bool)
        .or_else(|| {
            row.get("supported_parameters")
                .and_then(Value::as_array)
                .map(|params| {
                    params.iter().any(|v| {
                        matches!(v.as_str(), Some("reasoning" | "include_reasoning"))
                    })
                })
        });
    let pricing = parse_pricing(row);
    Ok(presets::enrich_catalog_model(CatalogModel {
        id: id.into(),
        name: name.into(),
        context_window,
        max_output_tokens,
        reasoning,
        input,
        pricing,
        fast_mode: None,
        image_gen: None,
    }))
}

pub async fn models(id: &str, value: Value) -> Result<Vec<CatalogModel>, CoreError> {
    let config = resolve_for_probe(id, value)?;
    let client = build_client(&config, None)?;
    let url = reqwest::Url::parse(&config.models_endpoint_url())
        .map_err(|error| CoreError::Config(error.to_string()))?;
    let mut result = Vec::new();
    let mut cursor = None::<String>;
    loop {
        let mut page = url.clone();
        if let Some(cursor) = &cursor {
            page.query_pairs_mut().append_pair("after_id", cursor);
        }
        let response = build_request(&client, &config, page)
            .send()
            .await
            .map_err(failure)?
            .error_for_status()
            .map_err(failure)?;
        let body: Value = response.json().await.map_err(failure)?;
        let rows = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| CoreError::Config(t!("core.err.no_model_list").into_owned()))?;
        for row in rows {
            result.push(parse_catalog_row(row)?);
        }
        if body.get("has_more").and_then(Value::as_bool) != Some(true) {
            break;
        }
        let next = body
            .get("last_id")
            .and_then(Value::as_str)
            .or_else(|| rows.last()?.get("id")?.as_str())
            .ok_or_else(|| CoreError::Config(t!("core.err.pagination_incomplete").into_owned()))?;
        if cursor.as_deref() == Some(next) {
            return Err(CoreError::Config(t!("core.err.same_page").into_owned()));
        }
        cursor = Some(next.into());
    }
    result.sort_by(|left, right| left.id.cmp(&right.id));
    result.dedup_by(|left, right| left.id == right.id);
    Ok(result)
}

/// 探测供应商配置的有效性、鉴权与网络连通性；不修改配置文件。
pub async fn test_provider(id: &str, value: Value) -> Result<ProviderTestResult, CoreError> {
    let config = match resolve_for_probe(id, value) {
        Ok(config) => config,
        Err(error) => {
            return Ok(ProviderTestResult {
                ok: false,
                latency_ms: 0,
                endpoint: String::new(),
                status: None,
                models_count: None,
                message: error.detail(),
            });
        }
    };
    let endpoint = config.models_endpoint_url();
    let url = match reqwest::Url::parse(&endpoint) {
        Ok(url) => url,
        Err(error) => {
            return Ok(ProviderTestResult {
                ok: false,
                latency_ms: 0,
                endpoint,
                status: None,
                models_count: None,
                message: error.to_string(),
            });
        }
    };
    let client = build_client(&config, Some(Duration::from_secs(15)))?;
    let started = Instant::now();
    let response = match build_request(&client, &config, url).send().await {
        Ok(response) => response,
        Err(error) => {
            let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            return Ok(ProviderTestResult {
                ok: false,
                latency_ms,
                endpoint,
                status: error.status().map(|s| s.as_u16()),
                models_count: None,
                message: t!("core.provider_test.network_error", error = error).into_owned(),
            });
        }
    };
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let status = response.status();
    let code = status.as_u16();
    if status.is_success() {
        let models_count = response
            .json::<Value>()
            .await
            .ok()
            .and_then(|body| body.get("data").and_then(Value::as_array).map(Vec::len));
        let message = match models_count {
            Some(count) => t!(
                "core.provider_test.ok_models",
                count = count,
                ms = latency_ms
            )
            .into_owned(),
            None => t!(
                "core.provider_test.ok_reachable",
                status = code,
                ms = latency_ms
            )
            .into_owned(),
        };
        return Ok(ProviderTestResult {
            ok: true,
            latency_ms,
            endpoint,
            status: Some(code),
            models_count,
            message,
        });
    }
    if matches!(code, 401 | 403) {
        return Ok(ProviderTestResult {
            ok: false,
            latency_ms,
            endpoint,
            status: Some(code),
            models_count: None,
            message: t!("core.provider_test.auth_failed", status = code).into_owned(),
        });
    }
    if matches!(code, 404 | 405 | 501) {
        return Ok(ProviderTestResult {
            ok: true,
            latency_ms,
            endpoint,
            status: Some(code),
            models_count: None,
            message: t!(
                "core.provider_test.no_models_endpoint",
                status = code,
                ms = latency_ms
            )
            .into_owned(),
        });
    }
    Ok(ProviderTestResult {
        ok: false,
        latency_ms,
        endpoint,
        status: Some(code),
        models_count: None,
        message: t!("core.provider_test.http_error", status = code).into_owned(),
    })
}

fn failure(error: reqwest::Error) -> CoreError {
    CoreError::Config(t!("core.err.fetch_models_failed", error = error).into_owned())
}

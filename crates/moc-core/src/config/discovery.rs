//! 发现只返回候选模型，由前端勾选后再写入配置。

use super::model::{ProviderDocument, ResolvedProvider};
use crate::CoreError;
use moc_llm::{Auth, EndpointStyle, Protocol};
use moc_i18n::t;

pub async fn models(
    id: &str,
    value: serde_json::Value,
) -> Result<Vec<moc_protocol::CatalogModel>, CoreError> {
    let mut document: ProviderDocument =
        serde_json::from_value(value).map_err(|error| CoreError::Config(error.to_string()))?;
    document.models.clear();
    let defaults = ResolvedProvider::from_document(id, document)?;
    let config = defaults.catalog_config();
    let mut client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(seconds) = config.connect_timeout_secs {
        client = client.connect_timeout(std::time::Duration::from_secs_f64(seconds));
    }
    let client = client.build().map_err(failure)?;
    let url = reqwest::Url::parse(&config.models_endpoint_url())
        .map_err(|error| CoreError::Config(error.to_string()))?;
    let mut result = Vec::new();
    let mut cursor = None::<String>;
    loop {
        let mut page = url.clone();
        if let Some(cursor) = &cursor {
            page.query_pairs_mut().append_pair("after_id", cursor);
        }
        let mut request = client.get(page);
        for (key, value) in &config.headers {
            request = request.header(key, value);
        }
        if config.protocol == Protocol::AnthropicMessages {
            request = request.header("anthropic-version", "2023-06-01");
        }
        request = match &config.auth {
            Auth::None => request,
            Auth::Bearer(key) if matches!(config.endpoint, EndpointStyle::Azure { .. }) => {
                request.header("api-key", key)
            }
            Auth::Bearer(key) => request.bearer_auth(key),
            Auth::Header { name, value } => request.header(name, value),
        };
        let response = request
            .send()
            .await
            .map_err(failure)?
            .error_for_status()
            .map_err(failure)?;
        let body: serde_json::Value = response.json().await.map_err(failure)?;
        let rows = body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| CoreError::Config(t!("core.err.no_model_list").into_owned()))?;
        for row in rows {
            let id = row
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| CoreError::Config(t!("core.err.model_entry_no_id").into_owned()))?;
            result.push(moc_protocol::CatalogModel {
                id: id.into(),
                name: row
                    .get("display_name")
                    .or_else(|| row.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(id)
                    .into(),
            });
        }
        if body.get("has_more").and_then(serde_json::Value::as_bool) != Some(true) {
            break;
        }
        let next = body
            .get("last_id")
            .and_then(serde_json::Value::as_str)
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

fn failure(error: reqwest::Error) -> CoreError {
    CoreError::Config(t!("core.err.fetch_models_failed", error = error).into_owned())
}

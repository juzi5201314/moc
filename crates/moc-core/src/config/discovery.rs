//! 发现只返回候选模型，由前端勾选后再写入配置。

use super::model::{ProviderDocument, ResolvedProvider};
use crate::CoreError;
use moc_llm::{Auth, EndpointStyle, Protocol};

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
            .ok_or_else(|| CoreError::Config("模型服务没有返回模型列表".into()))?;
        for row in rows {
            let id = row
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| CoreError::Config("模型列表里有条目缺少模型 ID".into()))?;
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
            .ok_or_else(|| CoreError::Config("模型列表的分页数据不完整".into()))?;
        if cursor.as_deref() == Some(next) {
            return Err(CoreError::Config(
                "模型服务重复返回同一页，已停止获取".into(),
            ));
        }
        cursor = Some(next.into());
    }
    result.sort_by(|left, right| left.id.cmp(&right.id));
    result.dedup_by(|left, right| left.id == right.id);
    Ok(result)
}

fn failure(error: reqwest::Error) -> CoreError {
    CoreError::Config(format!("获取模型失败：{error}"))
}

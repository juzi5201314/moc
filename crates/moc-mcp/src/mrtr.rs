//! MRTR（SEP-2322）客户端侧的输入履行视图。
//!
//! server 对 `tools/call` 返回 `input_required` 时，moc 在 [`crate::manager`]
//! 的调用回路里逐键履行 `inputRequests`：本模块把 rmcp 的 `InputRequest`
//! 归一化为 [`PendingInput`]，调用方（moc-core）注入 [`InputFulfiller`]
//! 逐键产出结果 JSON（`ElicitResult`/`ListRootsResult` 等线形）。
//! `requestState` 的回显与轮数上限由 manager 负责，本模块不感知。

use std::sync::Arc;

use futures_util::future::BoxFuture;
use rmcp::model::{ElicitRequestParams, ElicitResult, ElicitationAction, InputRequest};
use serde_json::{Value, json};

use crate::error::McpError;

/// 一次 `input_required` 中等待客户端补全的单个请求（已脱离 rmcp 类型）。
#[derive(Debug, Clone)]
pub enum PendingInput {
    /// `elicitation/create` 表单模式：`schema` 是 `requestedSchema` 的原样 JSON。
    ElicitForm { message: String, schema: Value },
    /// `elicitation/create` URL 模式：用户需自行打开 `url` 完成带外交互。
    ElicitUrl { message: String, url: String },
    /// `roots/list`：应答方返回工作区根列表。
    ListRoots,
    /// `sampling/createMessage`：`params` 为原始请求参数 JSON。
    /// moc 未声明 sampling 能力，收到即视为 server 违规，履行端应返回错误。
    CreateMessage { params: Value },
}

/// 逐键履行一个 [`PendingInput`]，返回该键的结果 JSON。
///
/// 返回的 `Value` 直接进 `inputResponses` map；无法履行时返回 `Err` 中止整个调用。
pub type InputFulfiller =
    Arc<dyn Fn(PendingInput) -> BoxFuture<'static, Result<Value, McpError>> + Send + Sync>;

/// 把 rmcp 的 `InputRequest` 归一化为 [`PendingInput`]。
pub(crate) fn translate(request: &InputRequest) -> Result<PendingInput, McpError> {
    match request {
        InputRequest::Elicitation(elicit) => match &elicit.params {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => Ok(PendingInput::ElicitForm {
                message: message.clone(),
                schema: serde_json::to_value(requested_schema).map_err(|error| {
                    McpError::Tool(format!("elicitation schema is not serializable: {error}"))
                })?,
            }),
            ElicitRequestParams::UrlElicitationParams { message, url, .. } => {
                Ok(PendingInput::ElicitUrl {
                    message: message.clone(),
                    url: url.clone(),
                })
            }
            // rmcp 标注 non_exhaustive：未知模式按「不可履行」报错而非误答。
            _ => Err(McpError::Tool(
                "unsupported elicitation mode in inputRequests".into(),
            )),
        },
        InputRequest::ListRoots(_) => Ok(PendingInput::ListRoots),
        InputRequest::CreateMessage(request) => Ok(PendingInput::CreateMessage {
            params: serde_json::to_value(&request.params).map_err(|error| {
                McpError::Tool(format!("sampling request is not serializable: {error}"))
            })?,
        }),
        // non_exhaustive：协议只允许三种 inputRequests，未知变体不可静默忽略。
        _ => Err(McpError::Tool(
            "unsupported request type in inputRequests".into(),
        )),
    }
}

/// 无 fulfiller 注入时的兜底：elicitation 一律 decline（与 rmcp 默认 handler
/// 的语义一致），roots 回空列表，sampling 报错。
pub(crate) fn default_fulfiller() -> InputFulfiller {
    Arc::new(|pending| {
        Box::pin(async move {
            match pending {
                PendingInput::ElicitForm { .. } | PendingInput::ElicitUrl { .. } => {
                    serde_json::to_value(ElicitResult::new(ElicitationAction::Decline)).map_err(
                        |error| McpError::Tool(format!("failed to encode decline result: {error}")),
                    )
                }
                PendingInput::ListRoots => Ok(json!({ "roots": [] })),
                PendingInput::CreateMessage { .. } => Err(McpError::Tool(
                    "server requested sampling/createMessage, which is not supported".into(),
                )),
            }
        })
    })
}

/// 空 `inputRequests` map 的判定辅助：spec 允许只有 `requestState` 的轮次。
pub(crate) fn has_requests(requests: Option<&rmcp::model::InputRequests>) -> bool {
    requests.is_some_and(|requests| !requests.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 线形 JSON → `InputRequest`，与 server 实际发来的形状一致。
    fn input_request(wire: serde_json::Value) -> InputRequest {
        serde_json::from_value(wire).expect("inputRequests 元素可解析")
    }

    #[test]
    fn translates_form_and_url_elicitations() {
        let form = input_request(json!({
            "method": "elicitation/create",
            "params": {
                "mode": "form",
                "message": "需要用户名",
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "age": {"type": "number"}
                    },
                    "required": ["name"]
                }
            }
        }));
        match translate(&form).unwrap() {
            PendingInput::ElicitForm { message, schema } => {
                assert_eq!(message, "需要用户名");
                assert_eq!(schema["type"], json!("object"));
                assert!(schema["properties"]["name"].is_object());
            }
            other => panic!("expected form elicitation: {other:?}"),
        }

        let url = input_request(json!({
            "method": "elicitation/create",
            "params": {
                "mode": "url",
                "message": "请完成登录",
                "url": "https://example.com/login",
                "elicitationId": "el_1"
            }
        }));
        match translate(&url).unwrap() {
            PendingInput::ElicitUrl { message, url } => {
                assert_eq!(message, "请完成登录");
                assert_eq!(url, "https://example.com/login");
            }
            other => panic!("expected url elicitation: {other:?}"),
        }
    }

    #[test]
    fn translates_list_roots_and_sampling() {
        let roots = input_request(json!({"method": "roots/list"}));
        assert!(matches!(
            translate(&roots).unwrap(),
            PendingInput::ListRoots
        ));

        let sampling = input_request(json!({
            "method": "sampling/createMessage",
            "params": {"messages": [], "maxTokens": 10}
        }));
        match translate(&sampling).unwrap() {
            PendingInput::CreateMessage { params } => {
                assert_eq!(params["maxTokens"], json!(10));
            }
            other => panic!("expected sampling request: {other:?}"),
        }
    }

    #[test]
    fn empty_request_map_is_not_a_request_round() {
        assert!(!has_requests(None));
        assert!(!has_requests(Some(&Default::default())));
        let mut requests = rmcp::model::InputRequests::new();
        requests.insert("k".into(), input_request(json!({"method": "roots/list"})));
        assert!(has_requests(Some(&requests)));
    }

    #[tokio::test]
    async fn default_fulfiller_declines_and_answers_empty_roots() {
        let fulfiller = default_fulfiller();
        let decline = fulfiller(PendingInput::ElicitForm {
            message: "m".into(),
            schema: json!({}),
        })
        .await
        .unwrap();
        assert_eq!(decline["action"], json!("decline"));
        assert_eq!(
            fulfiller(PendingInput::ListRoots).await.unwrap(),
            json!({ "roots": [] })
        );
        assert!(
            fulfiller(PendingInput::CreateMessage { params: json!({}) })
                .await
                .is_err()
        );
    }
}

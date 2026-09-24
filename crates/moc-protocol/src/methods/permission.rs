//! `session/request_permission` 反向请求：core 向前端要一次授权。
//!
//! 方向是 服务端→客户端：只有 core 能发起，前端必须用
//! [`crate::ClientBuilder::on_request`] 注册处理器。
//!
//! 契约：
//!
//! - `options` 与 `title` / `body` 由 core 生成，前端只负责展示与回选。
//! - 前端返回的 `option_id` 必须命中 `options`，否则 core 按拒绝处理。
//! - 前端未注册处理器时协议层回 [`crate::RpcErrorCode::MethodNotFound`]，
//!   core 同样按拒绝处理，并把 `permission_resolved.option_id` 记为 `None`。
//! - `title` / `body` 里的文本是给用户看的，core 负责把工具名、命令、路径
//!   等细节拼进去（协议层不做任何本地化）。

use serde::{Deserialize, Serialize};

use crate::method::{Method, ServerRequest};

/// `session/request_permission` 的方法标记。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionRequestPermission;

impl Method for SessionRequestPermission {
    const NAME: &'static str = "session/request_permission";
    type Params = PermissionRequestParams;
    type Result = PermissionResult;
}

impl ServerRequest for SessionRequestPermission {}

/// `session/request_permission` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequestParams {
    /// 会话 ID。
    pub session_id: String,
    /// 轮次 ID。
    pub turn_id: String,
    /// 本次询问的 ID；与 [`super::update::SessionUpdate::PermissionRequested`] 一致。
    pub request_id: String,
    /// 需要授权的调用 ID。
    pub tool_call_id: String,
    /// 工具名。
    pub tool_name: String,
    /// 一行标题，如「执行命令：cargo test」。
    pub title: String,
    /// 详细内容（命令全文、diff 等）；`None` 表示无可展示的细节。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// 可选答案；前端必须回其中一个 `id`。
    pub options: Vec<PermissionOption>,
}

/// 一个可选答案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionOption {
    /// 选项 ID，由 core 定义（如 `allow_once`）。
    pub id: String,
    /// 展示文案。
    pub label: String,
    /// 选项语义分类，前端可据此决定默认高亮与颜色。
    pub kind: PermissionOptionKind,
}

/// 选项语义分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// 允许这一次。
    AllowOnce,
    /// 本会话内一直允许。
    AllowAlways,
    /// 拒绝这一次。
    RejectOnce,
    /// 本会话内一直拒绝。
    RejectAlways,
}

/// `session/request_permission` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionResult {
    /// 前端选中的 `PermissionOption::id`。
    pub option_id: String,
}

/// 便利构造：一个 `allow_once` / `reject_once` 的最小选项集。
pub fn default_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            id: "allow_once".to_owned(),
            label: "允许一次".to_owned(),
            kind: PermissionOptionKind::AllowOnce,
        },
        PermissionOption {
            id: "reject_once".to_owned(),
            label: "拒绝".to_owned(),
            kind: PermissionOptionKind::RejectOnce,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn method_name() {
        assert_eq!(
            <SessionRequestPermission as Method>::NAME,
            "session/request_permission"
        );
    }

    #[test]
    fn request_shape() {
        let params = PermissionRequestParams {
            session_id: "ses_7f3a".into(),
            turn_id: "turn_1".into(),
            request_id: "perm_1".into(),
            tool_call_id: "call_9".into(),
            tool_name: "bash".into(),
            title: "执行命令：cargo test".into(),
            body: None,
            options: default_options(),
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            json!({
                "session_id": "ses_7f3a",
                "turn_id": "turn_1",
                "request_id": "perm_1",
                "tool_call_id": "call_9",
                "tool_name": "bash",
                "title": "执行命令：cargo test",
                "options": [
                    { "id": "allow_once", "label": "允许一次", "kind": "allow_once" },
                    { "id": "reject_once", "label": "拒绝", "kind": "reject_once" }
                ]
            })
        );
        let round_trip: PermissionRequestParams =
            serde_json::from_value(serde_json::to_value(&params).expect("可序列化")).expect("可反序列化");
        assert_eq!(round_trip, params);
        assert_eq!(
            serde_json::to_value(PermissionResult {
                option_id: "allow_once".into()
            })
            .expect("可序列化"),
            json!({ "option_id": "allow_once" })
        );
        assert!(serde_json::from_value::<PermissionRequestParams>(json!({ "session_id": "s" })).is_err());
    }
}

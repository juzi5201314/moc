//! `initialize` 与 `ping`。

use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// `initialize` 的参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeParams {
    /// 前端声明的协议版本，形状 `major.minor`；major 参与协商。
    pub protocol_version: String,
    /// 前端自报家门。
    pub client: ClientInfo,
    /// 前端能力；缺省视为「无任何能力」（core 不会向它发权限询问）。
    #[serde(default)]
    pub capabilities: ClientCapabilities,
}

/// 前端自报家门。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientInfo {
    /// 前端程序名，如 `moc-tui`。
    pub name: String,
    /// 前端版本。
    pub version: String,
    /// 前端形态，约定值 `tui` / `webui` / `desktop`；其它取值自由。
    pub frontend: String,
}

/// 前端能力。
///
/// `permission_requests = false` 表示这个前端弹不出权限询问（如无交互的 `webui`
/// 后台任务），core 应改走自身策略而不是发反向请求。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientCapabilities {
    /// 能否处理 [`crate::methods::permission::PermissionRequestParams`]。
    #[serde(default)]
    pub permission_requests: bool,
}

/// `initialize` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeResult {
    /// 服务端协议版本；major 与前端不一致时前端必须放弃连接。
    pub protocol_version: String,
    /// 服务端自报家门。
    pub server: ServerInfo,
    /// 服务端能力；缺省视为「无任何能力」（老 core 不报能力时前端按最小能力渲染）。
    #[serde(default)]
    pub capabilities: ServerCapabilities,
}

/// 服务端自报家门。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerInfo {
    /// 程序名，如 `moc-core`。
    pub name: String,
    /// 版本。
    pub version: String,
}

/// 服务端能力。
///
/// `session_locking = false` 的 core：`SessionInfo.lock` 恒为 `None`，
/// `session/unlock` 返回 [`crate::RpcErrorCode::NotSupported`]。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCapabilities {
    /// 是否会发起 [`crate::methods::permission::PermissionRequestParams`] 反向请求。
    #[serde(default)]
    pub permission_requests: bool,
    /// 是否实现跨进程会话锁语义。
    #[serde(default)]
    pub session_locking: bool,
}

/// `ping` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PingResult {
    /// 服务端当前时间的 UNIX 毫秒时间戳。
    pub server_time_ms: u64,
}

/// `initialize`：握手与能力协商。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Initialize;

/// `ping`：连通性与时延探测，由协议层自答（不进入 core）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ping;

impl Method for Initialize {
    const NAME: &'static str = "initialize";
    type Params = InitializeParams;
    type Result = InitializeResult;
}

impl ClientRequest for Initialize {}

impl Method for Ping {
    const NAME: &'static str = "ping";
    type Params = EmptyParams;
    type Result = PingResult;
}

impl ClientRequest for Ping {}

/// `ping` 的参数：空对象。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;
    use serde_json::json;

    #[test]
    fn initialize_shapes() {
        let params = InitializeParams {
            protocol_version: "1.0".into(),
            client: ClientInfo {
                name: "moc-tui".into(),
                version: "0.1.0".into(),
                frontend: "tui".into(),
            },
            capabilities: ClientCapabilities {
                permission_requests: true,
            },
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            json!({
                "protocol_version": "1.0",
                "client": { "name": "moc-tui", "version": "0.1.0", "frontend": "tui" },
                "capabilities": { "permission_requests": true }
            })
        );
        // capabilities 整体与字段都可省略
        let parsed: InitializeParams = serde_json::from_value(json!({
            "protocol_version": "1.0",
            "client": { "name": "x", "version": "1", "frontend": "webui" }
        }))
        .expect("可反序列化");
        assert!(!parsed.capabilities.permission_requests);
        let parsed: InitializeParams = serde_json::from_value(json!({
            "protocol_version": "1.0",
            "client": { "name": "x", "version": "1", "frontend": "webui" },
            "capabilities": {}
        }))
        .expect("可反序列化");
        assert!(!parsed.capabilities.permission_requests);
        assert!(serde_json::from_value::<InitializeParams>(json!({
            "protocol_version": "1.0",
            "client": { "name": "x", "version": "1", "frontend": "webui" },
            "capabilities": {},
            "extra": 1
        }))
        .is_err());

        let result = InitializeResult {
            protocol_version: "1.0".into(),
            server: ServerInfo {
                name: "moc-core".into(),
                version: "0.1.0".into(),
            },
            capabilities: ServerCapabilities {
                permission_requests: true,
                session_locking: true,
            },
        };
        assert_eq!(
            serde_json::to_value(&result).expect("可序列化"),
            json!({
                "protocol_version": "1.0",
                "server": { "name": "moc-core", "version": "0.1.0" },
                "capabilities": { "permission_requests": true, "session_locking": true }
            })
        );
        // 老 core 不回 capabilities 时按「无能力」处理
        let parsed: InitializeResult = serde_json::from_value(json!({
            "protocol_version": "1.0",
            "server": { "name": "moc-core", "version": "0.1.0" }
        }))
        .expect("可反序列化");
        assert_eq!(parsed.capabilities, ServerCapabilities::default());
    }

    #[test]
    fn ping_shape() {
        assert_eq!(
            serde_json::to_value(PingResult {
                server_time_ms: 1_724_000_000_000
            })
            .expect("可序列化"),
            json!({ "server_time_ms": 1_724_000_000_000u64 })
        );
        assert_eq!(<Ping as Method>::NAME, "ping");
        assert_eq!(<Initialize as Method>::NAME, "initialize");
    }
}

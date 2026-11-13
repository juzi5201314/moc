use super::CallContext;
use super::PersistenceBridge;
use super::PersistenceOp;
use crate::descriptor::ConfigDecl;
use crate::error::PluginError;
use crate::error::Result;
use crate::runtime::HostRequest;
use serde_json::Value;
use std::path::Path;

pub(super) async fn request_persistence(
    bridge: &dyn PersistenceBridge,
    plugin: &str,
    path: &Path,
    context: Option<&CallContext>,
    fields: &[ConfigDecl],
    op: PersistenceOp,
) -> Result<Value> {
    match &op {
        PersistenceOp::ConfigSnapshot | PersistenceOp::ConfigGet { .. } => {
            let value = bridge
                .request(plugin, path, context, PersistenceOp::ConfigSnapshot)
                .await?;
            let object = value
                .as_object()
                .ok_or_else(|| PluginError::new("E_PLUGIN_CONFIG", "插件配置必须是对象"))?;
            for (key, value) in object {
                let declaration =
                    fields
                        .iter()
                        .find(|field| &field.key == key)
                        .ok_or_else(|| {
                            PluginError::new("E_PLUGIN_CONFIG", format!("配置 `{key}` 未注册"))
                        })?;
                declaration
                    .validate(value)
                    .map_err(|error| PluginError::new("E_PLUGIN_CONFIG", error))?;
            }
            if let PersistenceOp::ConfigGet { key } = op {
                let declaration =
                    fields
                        .iter()
                        .find(|field| field.key == key)
                        .ok_or_else(|| {
                            PluginError::new("E_PLUGIN_CONFIG", format!("配置 `{key}` 未注册"))
                        })?;
                let value = object.get(&key).or(declaration.default.as_ref());
                Ok(match value {
                    Some(value) => serde_json::json!({"found":true,"value":value}),
                    None => serde_json::json!({"found":false}),
                })
            } else {
                Ok(value)
            }
        }
        PersistenceOp::ConfigSet { key, value } => {
            if context.is_some_and(|context| !context.writable) {
                return Err(PluginError::new(
                    "E_PLUGIN_STATE",
                    "当前调用禁止修改插件配置",
                ));
            }
            let declaration = fields
                .iter()
                .find(|field| &field.key == key)
                .ok_or_else(|| {
                    PluginError::new("E_PLUGIN_CONFIG", format!("配置 `{key}` 未注册"))
                })?;
            if !value.is_null() {
                declaration
                    .validate(value)
                    .map_err(|error| PluginError::new("E_PLUGIN_CONFIG", error))?;
            }
            bridge.request(plugin, path, context, op).await
        }
        PersistenceOp::StateSet { .. } | PersistenceOp::StateDelete { .. } => {
            if context.is_none() {
                return Err(PluginError::new("E_PLUGIN_STATE", "当前调用没有会话"));
            }
            if context.is_some_and(|context| !context.writable) {
                return Err(PluginError::new(
                    "E_PLUGIN_STATE",
                    "当前调用禁止修改插件状态",
                ));
            }
            bridge.request(plugin, path, context, op).await
        }
        _ => {
            if context.is_none() {
                return Err(PluginError::new("E_PLUGIN_STATE", "当前调用没有会话"));
            }
            bridge.request(plugin, path, context, op).await
        }
    }
}

pub(super) async fn next_request(
    receiver: &mut Option<tokio::sync::mpsc::UnboundedReceiver<HostRequest>>,
) -> Option<HostRequest> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// 等一个在途的宿主工具调用；没有任务时永远挂起，让 `select!` 的分支保持安静。
pub(super) async fn host_reply(
    task: &mut Option<tokio::task::JoinHandle<Result<Value>>>,
) -> Result<Value> {
    let Some(handle) = task.take() else {
        return std::future::pending().await;
    };
    match handle.await {
        Ok(result) => result,
        Err(error) => Err(PluginError::new(
            "E_PLUGIN_CRASH",
            format!("宿主工具任务没有正常结束：{error}"),
        )),
    }
}

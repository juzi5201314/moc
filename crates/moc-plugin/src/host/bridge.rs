use super::CallContext;
use super::PersistenceBridge;
use super::PersistenceOp;
use crate::descriptor::ConfigDecl;
use crate::error::PluginError;
use crate::error::Result;
use crate::runtime::HostRequest;
use rust_i18n::t;
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
            let object = value.as_object().ok_or_else(|| {
                PluginError::new("E_PLUGIN_CONFIG", t!("plugin.err.config_object"))
            })?;
            for (key, value) in object {
                let declaration =
                    fields
                        .iter()
                        .find(|field| &field.key == key)
                        .ok_or_else(|| {
                            PluginError::new(
                                "E_PLUGIN_CONFIG",
                                t!("plugin.err.config_unregistered", key = key.as_str())
                                    .into_owned(),
                            )
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
                            PluginError::new(
                                "E_PLUGIN_CONFIG",
                                t!("plugin.err.config_unregistered", key = key.as_str())
                                    .into_owned(),
                            )
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
                    t!("plugin.err.config_write_denied"),
                ));
            }
            let declaration = fields
                .iter()
                .find(|field| &field.key == key)
                .ok_or_else(|| {
                    PluginError::new(
                        "E_PLUGIN_CONFIG",
                        t!("plugin.err.config_unregistered", key = key.as_str()).into_owned(),
                    )
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
                return Err(PluginError::new(
                    "E_PLUGIN_STATE",
                    t!("plugin.err.no_session"),
                ));
            }
            if context.is_some_and(|context| !context.writable) {
                return Err(PluginError::new(
                    "E_PLUGIN_STATE",
                    t!("plugin.err.state_write_denied"),
                ));
            }
            bridge.request(plugin, path, context, op).await
        }
        _ => {
            if context.is_none() {
                return Err(PluginError::new(
                    "E_PLUGIN_STATE",
                    t!("plugin.err.no_session"),
                ));
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
            t!("plugin.err.host_task_abnormal", error = error).into_owned(),
        )),
    }
}

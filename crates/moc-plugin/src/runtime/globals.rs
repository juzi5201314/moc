use super::API_VERSION;
use super::CallHost;
use super::HostOperation;
use super::HostRequest;
use super::Registrations;
use super::descriptor::agent_preset_declaration;
use super::descriptor::config_declaration;
use super::invocation::js_error;
use super::invocation::throw;
use crate::error::PluginError;
use crate::error::Result;
use crate::host::PersistenceOp;
use rquickjs::Ctx;
use rquickjs::Function;
use rquickjs::Object;
use rust_i18n::t;
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

/// 建宿主桥：`log`、`session`、`tools.call`。
pub(super) fn install_globals<'js>(
    ctx: &Ctx<'js>,
    host: &Rc<RefCell<Option<CallHost>>>,
    registrations: &Rc<RefCell<Registrations>>,
) -> Result<()> {
    let bridge = Object::new(ctx.clone()).map_err(|error| js_error(ctx, error))?;
    bridge
        .set("version", API_VERSION)
        .map_err(|error| js_error(ctx, error))?;

    let log = Function::new(
        ctx.clone(),
        move |level: String, message: String| match level.as_str() {
            "debug" => tracing::debug!(target: "moc_plugin", "{message}"),
            "warn" => tracing::warn!(target: "moc_plugin", "{message}"),
            "error" => tracing::error!(target: "moc_plugin", "{message}"),
            _ => tracing::info!(target: "moc_plugin", "{message}"),
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("log", log)
        .map_err(|error| js_error(ctx, error))?;

    let current = Rc::clone(host);
    let session = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>| -> rquickjs::Result<String> {
            let borrowed = current.borrow();
            match borrowed.as_ref() {
                Some(call) if call.session != "null" => Ok(call.session.clone()),
                _ => Err(throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_STATE", t!("plugin.err.no_invocation")),
                )),
            }
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("session", session)
        .map_err(|error| js_error(ctx, error))?;

    let current = Rc::clone(host);
    let call_tool = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, name: String, args: String| -> rquickjs::Result<String> {
            let (tx, deadline, allowed) = {
                let borrowed = current.borrow();
                match borrowed.as_ref() {
                    Some(call) => (call.tx.clone(), call.deadline, call.tools_allowed),
                    None => {
                        return Err(throw(
                            &ctx,
                            &PluginError::new("E_PLUGIN_STATE", t!("plugin.err.no_invocation")),
                        ));
                    }
                }
            };
            if !allowed {
                return Err(throw(
                    &ctx,
                    &PluginError::new(
                        "E_PLUGIN_TOOLS_UNAVAILABLE",
                        t!("plugin.err.tool_only"),
                    ),
                ));
            }
            let Some(tx) = tx else {
                return Err(throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_STATE", t!("plugin.err.bridge_no_call")),
                ));
            };
            let args: Value = serde_json::from_str(&args).map_err(|error| {
                throw(
                    &ctx,
                    &PluginError::new(
                        "E_PLUGIN_VALUE",
                        t!("plugin.err.args_json", error = error).into_owned(),
                    ),
                )
            })?;
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            if tx
                .send(HostRequest {
                    operation: HostOperation::Tool { name, args },
                    reply: reply_tx,
                })
                .is_err()
            {
                return Err(throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_STATE", t!("plugin.err.call_channel_closed")),
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match reply_rx.recv_timeout(remaining.max(Duration::from_millis(1))) {
                Ok(Ok(value)) => Ok(value.to_string()),
                Ok(Err(error)) => Err(throw(&ctx, &error)),
                Err(_) => Err(throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_TIMEOUT", t!("plugin.err.host_call_timeout")),
                )),
            }
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("toolCall", call_tool)
        .map_err(|error| js_error(ctx, error))?;

    let registry = Rc::clone(registrations);
    let current = Rc::clone(host);
    let register = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String, definition: String| -> rquickjs::Result<()> {
            let result = (|| -> Result<()> {
                let mut registry = registry.borrow_mut();
                let fields = registry
                    .fields
                    .as_mut()
                    .ok_or_else(|| PluginError::new("E_PLUGIN_CONFIG", t!("plugin.err.config_top_level")))?;
                let declaration = config_declaration(key, &definition)?;
                if fields.iter().any(|field| field.key == declaration.key) {
                    return Err(PluginError::new("E_PLUGIN_CONFIG", t!("plugin.err.config_dup")));
                }
                fields.push(declaration);
                Ok(())
            })();
            result.map_err(|error| {
                registry.borrow_mut().error.get_or_insert(error.clone());
                if let Some(call) = current.borrow_mut().as_mut() {
                    call.fault = Some(error.clone());
                }
                throw(&ctx, &error)
            })
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("configRegister", register)
        .map_err(|error| js_error(ctx, error))?;

    let registry = Rc::clone(registrations);
    let current = Rc::clone(host);
    let register_preset = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, definition: String| -> rquickjs::Result<()> {
            let result = (|| -> Result<()> {
                let mut registry = registry.borrow_mut();
                let presets = registry.agent_presets.as_mut().ok_or_else(|| {
                    PluginError::new("E_PLUGIN_AGENT", t!("plugin.err.preset_top_level"))
                })?;
                let declaration = agent_preset_declaration(&definition)?;
                if presets.iter().any(|preset| preset.name == declaration.name) {
                    return Err(PluginError::new(
                        "E_PLUGIN_AGENT",
                        t!("plugin.err.preset_dup", name = declaration.name.as_str())
                            .into_owned(),
                    ));
                }
                presets.push(declaration);
                Ok(())
            })();
            result.map_err(|error| {
                registry.borrow_mut().error.get_or_insert(error.clone());
                if let Some(call) = current.borrow_mut().as_mut() {
                    call.fault = Some(error.clone());
                }
                throw(&ctx, &error)
            })
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("agentPresetRegister", register_preset)
        .map_err(|error| js_error(ctx, error))?;

    let current = Rc::clone(host);
    let registry = Rc::clone(registrations);
    let invalid = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, message: String| -> rquickjs::Result<()> {
            let error = PluginError::new("E_PLUGIN_VALUE", message);
            registry.borrow_mut().error.get_or_insert(error.clone());
            if let Some(call) = current.borrow_mut().as_mut() {
                call.fault = Some(error.clone());
            }
            Err(throw(&ctx, &error))
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("invalidValue", invalid)
        .map_err(|error| js_error(ctx, error))?;

    let current = Rc::clone(host);
    let persistence = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              operation: String,
              key: String,
              value: String|
              -> rquickjs::Result<String> {
            let result = (|| -> Result<Value> {
                let (tx, deadline) = {
                    let borrowed = current.borrow();
                    let call = borrowed.as_ref().ok_or_else(|| {
                        PluginError::new("E_PLUGIN_STATE", t!("plugin.err.no_invocation"))
                    })?;
                    (
                        call.tx
                            .clone()
                            .ok_or_else(|| {
                                PluginError::new("E_PLUGIN_STATE", t!("plugin.err.persistence_unavailable"))
                            })?,
                        call.deadline,
                    )
                };
                let op = match operation.as_str() {
                    "configGet" => PersistenceOp::ConfigGet { key },
                    "configSet" => PersistenceOp::ConfigSet {
                        key,
                        value: serde_json::from_str(&value).map_err(|error| {
                            PluginError::new("E_PLUGIN_VALUE", error.to_string())
                        })?,
                    },
                    "stateGet" => PersistenceOp::StateGet { key },
                    "stateSet" => PersistenceOp::StateSet {
                        key,
                        value: serde_json::from_str(&value).map_err(|error| {
                            PluginError::new("E_PLUGIN_VALUE", error.to_string())
                        })?,
                    },
                    "stateDelete" => PersistenceOp::StateDelete { key },
                    "stateKeys" => PersistenceOp::StateKeys,
                    _ => return Err(PluginError::new("E_PLUGIN_STATE", t!("plugin.err.persistence_unknown_op"))),
                };
                let request = |op| -> Result<Value> {
                    let (reply, receiver) = std::sync::mpsc::channel();
                    tx.send(HostRequest {
                        operation: HostOperation::Persistence(op),
                        reply,
                    })
                    .map_err(|_| {
                        PluginError::new("E_PLUGIN_STATE", t!("plugin.err.persistence_closed"))
                    })?;
                    receiver
                        .recv_timeout(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .max(Duration::from_millis(1)),
                        )
                        .map_err(|_| {
                            PluginError::new("E_PLUGIN_TIMEOUT", t!("plugin.err.persistence_timeout"))
                        })?
                };
                if let PersistenceOp::StateGet { key }
                | PersistenceOp::StateSet { key, .. }
                | PersistenceOp::StateDelete { key } = &op
                    && (key.is_empty() || key.len() > 128)
                {
                    return Err(PluginError::new(
                        "E_PLUGIN_STATE",
                        t!("plugin.err.state_key"),
                    ));
                }
                if matches!(
                    &op,
                    PersistenceOp::StateGet { .. } | PersistenceOp::StateKeys
                ) {
                    let needs_snapshot = current
                        .borrow()
                        .as_ref()
                        .expect("当前调用存在")
                        .state
                        .is_none();
                    if needs_snapshot {
                        let snapshot = request(PersistenceOp::StateSnapshot)?;
                        let object = snapshot
                            .as_object()
                            .ok_or_else(|| {
                                PluginError::new("E_PLUGIN_STATE", t!("plugin.err.state_snapshot"))
                            })?
                            .clone();
                        current.borrow_mut().as_mut().expect("当前调用存在").state = Some(object);
                    }
                    let borrowed = current.borrow();
                    let state = borrowed
                        .as_ref()
                        .and_then(|call| call.state.as_ref())
                        .expect("快照已加载");
                    return Ok(match &op {
                        PersistenceOp::StateGet { key } => match state.get(key) {
                            Some(value) => serde_json::json!({"found":true,"value":value}),
                            None => serde_json::json!({"found":false}),
                        },
                        _ => serde_json::json!(state.keys().collect::<Vec<_>>()),
                    });
                }
                let result = request(op.clone())?;
                if let Some(state) = current
                    .borrow_mut()
                    .as_mut()
                    .and_then(|call| call.state.as_mut())
                {
                    match op {
                        PersistenceOp::StateSet { key, value } => {
                            state.insert(key, value);
                        }
                        PersistenceOp::StateDelete { key } => {
                            state.remove(&key);
                        }
                        _ => {}
                    }
                }
                Ok(result)
            })();
            match result {
                Ok(value) => Ok(value.to_string()),
                Err(error) => {
                    if let Some(call) = current.borrow_mut().as_mut() {
                        call.fault = Some(error.clone());
                    }
                    Err(throw(&ctx, &error))
                }
            }
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("persistence", persistence)
        .map_err(|error| js_error(ctx, error))?;

    ctx.globals()
        .set("__moc", bridge)
        .map_err(|error| js_error(ctx, error))
}

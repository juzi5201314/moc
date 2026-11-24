//! 插件持久化能力：来源决定配置路径，会话上下文决定 state 命名空间。
//!
//! 不持有 CoreInner，避免宿主缓存和桥之间形成 Arc 环；插件配置补丁不获取
//! config_epoch 写锁，否则插件在运行中保存自身配置会与提示持有的读锁死锁。

use moc_i18n::t;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use moc_plugin::{CallContext, PersistenceBridge, PersistenceOp, PluginError};
use serde_json::{Value, json};

use crate::config::editor;
use crate::session::SessionTable;
use crate::{CoreConfig, CoreError};

pub(super) struct Bridge {
    pub boot: CoreConfig,
    pub sessions: Arc<SessionTable>,
    pub cwd: std::path::PathBuf,
}

fn config_error(error: impl std::fmt::Display) -> PluginError {
    PluginError::new("E_PLUGIN_CONFIG", error.to_string())
}

/// 只解析配置值，不执行该文件中核心 provider 密钥的命令表达式。
fn snapshot(
    boot: &CoreConfig,
    cwd: &Path,
    source: &Path,
    plugin: &str,
) -> Result<Value, PluginError> {
    let global = crate::config::expand_user_path(&boot.home.to_string_lossy()).join("config.toml");
    let project = cwd.join(".moc/config.toml");
    let mut paths = vec![global.clone()];
    if source != global && source != project {
        paths.push(source.to_owned());
    }
    if project != global {
        paths.push(project);
    }
    paths.extend(boot.overlays.iter().cloned());
    let mut entry = json!({});
    for path in paths {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && !boot.overlays.contains(&path) =>
            {
                continue;
            }
            Err(error) => return Err(config_error(error)),
        };
        let raw: toml::Value = toml::from_str(&text).map_err(config_error)?;
        if let Some(value) = raw.get("plugins").and_then(|value| value.get(plugin)) {
            editor::merge_json(
                &mut entry,
                serde_json::to_value(value).map_err(config_error)?,
            );
        }
    }
    let entry = entry
        .as_object()
        .ok_or_else(|| config_error(t!("core.err.plugin_cfg_item_table")))?;
    if entry.keys().any(|key| key != "config") {
        return Err(config_error(t!(
            "core.err.plugin_entry_only_config_generic"
        )));
    }
    match entry.get("config") {
        None => Ok(json!({})),
        Some(value) if value.is_object() => Ok(value.clone()),
        Some(_) => Err(config_error(t!("core.err.plugin_config_table_generic"))),
    }
}

impl PersistenceBridge for Bridge {
    fn request<'a>(
        &'a self,
        plugin: &'a str,
        config_path: &'a Path,
        context: Option<&'a CallContext>,
        op: PersistenceOp,
    ) -> Pin<Box<dyn Future<Output = moc_plugin::Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            match op {
                PersistenceOp::ConfigSnapshot => {
                    snapshot(&self.boot, &self.cwd, config_path, plugin)
                }
                PersistenceOp::ConfigGet { key } => {
                    let config = snapshot(&self.boot, &self.cwd, config_path, plugin)?;
                    Ok(match config.get(&key) {
                        Some(value) => json!({"found":true,"value":value}),
                        None => json!({"found":false}),
                    })
                }
                PersistenceOp::ConfigSet { key, value } => {
                    let boot = self.boot.clone();
                    let path = config_path.to_owned();
                    let plugin = plugin.to_owned();
                    tokio::task::spawn_blocking(move || {
                        editor::patch_plugin(
                            &boot,
                            &path,
                            &plugin,
                            &key,
                            (!value.is_null()).then_some(value),
                        )
                    })
                    .await
                    .map_err(config_error)?
                    .map_err(config_error)?;
                    Ok(Value::Null)
                }
                state_op => {
                    let context = context.ok_or_else(|| {
                        PluginError::new(
                            "E_PLUGIN_STATE",
                            t!("core.err.plugin_state_needs_session").into_owned(),
                        )
                    })?;
                    self.state(plugin, &context.session_id, state_op).await
                }
            }
        })
    }
}

/// 动态目录来自插件声明，不能按字段名猜测核心设置语义。
pub(super) async fn settings(
    inner: &super::CoreInner,
    cwd: &Path,
    mut view: moc_protocol::SettingsDocument,
) -> Result<moc_protocol::SettingsDocument, CoreError> {
    let host = match inner.project_plugins(cwd).await {
        Ok(host) => host,
        Err(error) => {
            tracing::warn!(%error, "插件目录不可用；保留核心配置修复入口");
            return Ok(view);
        }
    };
    for (name, source, declarations) in host.config_declarations() {
        view.fields.retain(|field| {
            !(field.path.first().is_some_and(|p| p == "plugins")
                && field.path.get(1).is_some_and(|p| p == &name))
        });
        let effective = snapshot(&inner.boot, cwd, &source, &name).ok();
        for declaration in declarations {
            let path = vec![
                "plugins".into(),
                name.clone(),
                "config".into(),
                declaration.key.clone(),
            ];
            let value = effective
                .as_ref()
                .and_then(|values| values.get(&declaration.key))
                .cloned()
                .or_else(|| declaration.default.clone())
                .unwrap_or(Value::Null);
            set_display(&mut view.effective, &path, value);
            if let Some(default) = &declaration.default {
                set_display(&mut view.defaults, &path, default.clone());
            }
            let kind = field_kind(&declaration);
            view.fields.push(moc_protocol::ConfigField {
                path,
                tab: t!("core.config.tabs.plugins").into_owned(),
                label: declaration.label,
                description: t!(
                    "core.err.plugin_cfg_write_target",
                    desc = declaration.description,
                    path = source.display().to_string()
                )
                .into_owned(),
                kind,
                advanced: false,
            });
        }
    }
    Ok(view)
}

fn field_kind(declaration: &moc_plugin::ConfigDecl) -> moc_protocol::FieldKind {
    use moc_plugin::ConfigKind;
    use moc_protocol::FieldKind;
    match declaration.kind {
        ConfigKind::Bool => FieldKind::Bool,
        ConfigKind::Integer => FieldKind::Integer,
        ConfigKind::Number => FieldKind::Number,
        ConfigKind::Text => FieldKind::Text,
        ConfigKind::Secret => FieldKind::Secret,
        ConfigKind::Multiline => FieldKind::Multiline,
        ConfigKind::Strings => FieldKind::Strings,
        ConfigKind::Object => FieldKind::Object,
        ConfigKind::Choice => FieldKind::Choice(
            declaration
                .choices
                .iter()
                .map(|value| moc_protocol::ChoiceOption::new(value, value))
                .collect(),
        ),
    }
}

fn set_display(root: &mut Value, path: &[String], value: Value) {
    let mut slot = root;
    for part in path {
        if !slot.is_object() {
            *slot = json!({});
        }
        slot = slot
            .as_object_mut()
            .expect("刚转换为对象")
            .entry(part.clone())
            .or_insert(Value::Null);
    }
    *slot = value;
}

/// 用户输入错误只拒绝本次保存；不因已有坏配置阻止无关核心字段的修复。
pub(super) fn validate_changes(
    host: &moc_plugin::PluginHost,
    target: &Path,
    previous: &Value,
    candidate: &Value,
) -> Result<(), CoreError> {
    // 插件记录的 source 是规范化路径，target 由 cwd 拼出未必规范化
    // （macOS /var→/private/var）：两侧都归一化再比对，否则同一文件会判成不同层。
    let target = crate::sandbox::resolve_landing(target).unwrap_or_else(|| target.to_path_buf());
    for (name, source, declarations) in host.config_declarations() {
        let source = crate::sandbox::resolve_landing(&source).unwrap_or(source);
        if source != target {
            let before = previous.get("plugins").and_then(|p| p.get(&name));
            let after = candidate.get("plugins").and_then(|p| p.get(&name));
            if before != after {
                return Err(CoreError::Config(
                    t!(
                        "core.err.plugin_cfg_wrong_layer",
                        name = name,
                        path = source.display().to_string()
                    )
                    .into_owned(),
                ));
            }
            continue;
        }
        let before = previous.get("plugins").and_then(|p| p.get(&name));
        let after = candidate.get("plugins").and_then(|p| p.get(&name));
        if before == after || after.is_none_or(Value::is_null) {
            continue;
        }
        let entry = after.and_then(Value::as_object).ok_or_else(|| {
            CoreError::Config(t!("core.err.plugin_cfg_must_table", name = name).into_owned())
        })?;
        if entry.keys().any(|key| key != "config") {
            return Err(CoreError::Config(
                t!("core.err.plugin_entry_only_config", name = name).into_owned(),
            ));
        }
        let Some(values) = entry.get("config").filter(|v| !v.is_null()) else {
            continue;
        };
        let values = values.as_object().ok_or_else(|| {
            CoreError::Config(t!("core.err.plugin_config_must_table", name = name).into_owned())
        })?;
        for (key, value) in values {
            if value.is_null() {
                continue;
            }
            let declaration = declarations.iter().find(|d| &d.key == key).ok_or_else(|| {
                CoreError::Config(
                    t!("core.err.plugin_cfg_unregistered", name = name, key = key).into_owned(),
                )
            })?;
            declaration.validate(value).map_err(|error| {
                CoreError::Config(
                    t!(
                        "core.err.plugin_cfg_error",
                        name = name,
                        key = key,
                        error = error
                    )
                    .into_owned(),
                )
            })?;
        }
    }
    Ok(())
}

impl Bridge {
    async fn state(
        &self,
        plugin: &str,
        session: &str,
        op: PersistenceOp,
    ) -> moc_plugin::Result<Value> {
        let result: Result<Value, CoreError> = match op {
            PersistenceOp::StateSnapshot => self
                .sessions
                .plugin_state_snapshot(session, plugin)
                .await
                .map(|state| json!(state)),
            PersistenceOp::StateGet { key } => self
                .sessions
                .plugin_state_get(session, plugin, &key)
                .await
                .map(|value| match value {
                    Some(value) => json!({"found":true,"value":value}),
                    None => json!({"found":false}),
                }),
            PersistenceOp::StateSet { key, value } => self
                .sessions
                .plugin_state_set(session, plugin, &key, value)
                .await
                .map(|()| Value::Null),
            PersistenceOp::StateDelete { key } => self
                .sessions
                .plugin_state_remove(session, plugin, &key)
                .await
                .map(Value::Bool),
            PersistenceOp::StateKeys => self
                .sessions
                .plugin_state_keys(session, plugin)
                .await
                .map(|keys| json!(keys)),
            _ => unreachable!("配置操作已在上层处理"),
        };
        result.map_err(|error| PluginError::new("E_PLUGIN_STATE", error.to_string()))
    }
}

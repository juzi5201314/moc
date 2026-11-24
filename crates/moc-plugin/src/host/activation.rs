use super::PersistenceBridge;
use super::Plugin;
use super::PluginHost;
use super::Settings;
use super::inventory::discovery_config;
use super::inventory::fingerprint_of;
use crate::descriptor::CommandDecl;
use crate::descriptor::Descriptor;
use crate::descriptor::ToolDecl;
use crate::discovery;
use crate::error::PluginError;
use crate::manifest::Manifest;
use crate::runtime::CallOp;
use crate::runtime::Limits;
use crate::runtime::spawn_unactivated;
use parking_lot::Mutex;
use moc_i18n::t;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

impl PluginHost {
    /// 发现并加载一个工作目录下的全部插件。加载失败只影响该插件本身。
    ///
    /// `reserved` 是宿主已有的工具名（内置工具）；撞名的插件工具会被忽略并告警，
    /// 否则重名会让整轮提示的工具目录构建失败。
    pub async fn start(
        cwd: &Path,
        home: &Path,
        settings: &Settings,
        reserved: &[&str],
    ) -> Arc<Self> {
        Self::start_inner(cwd, home, settings, reserved, None).await
    }

    pub async fn start_with_persistence(
        cwd: &Path,
        home: &Path,
        settings: &Settings,
        reserved: &[&str],
        persistence: Arc<dyn PersistenceBridge>,
    ) -> Arc<Self> {
        Self::start_inner(cwd, home, settings, reserved, Some(persistence)).await
    }

    pub(super) async fn start_inner(
        cwd: &Path,
        home: &Path,
        settings: &Settings,
        reserved: &[&str],
        persistence: Option<Arc<dyn PersistenceBridge>>,
    ) -> Arc<Self> {
        let discovered = discovery::discover(cwd, home, &discovery_config(cwd, settings));
        let stamp = fingerprint_of(&discovered);
        let limits = Limits {
            memory_limit_bytes: settings.memory_limit_bytes,
            call_timeout: settings.call_timeout,
            load_timeout: settings.load_timeout,
        };
        let mut plugins = Vec::new();
        let mut tools: BTreeMap<String, (usize, ToolDecl)> = BTreeMap::new();
        let mut commands: BTreeMap<String, (usize, CommandDecl)> = BTreeMap::new();

        for candidate in discovered {
            if plugins.len() >= settings.max_plugins {
                tracing::warn!(
                    "插件数量达到 [plugins] max_plugins={}，其余目录已忽略",
                    settings.max_plugins
                );
                break;
            }
            let manifest = match Manifest::load(&candidate.dir) {
                Ok(manifest) => manifest,
                Err(error) => {
                    tracing::warn!(
                        "跳过插件目录 {}（{}）：{error}",
                        candidate.dir.display(),
                        candidate.dir_name
                    );
                    continue;
                }
            };
            if settings.disable.iter().any(|name| name == &manifest.name) {
                continue;
            }
            if plugins
                .iter()
                .any(|plugin: &Plugin| plugin.name == manifest.name)
            {
                tracing::warn!(
                    "插件 `{}` 重复（{}），保留更高优先级者",
                    manifest.name,
                    candidate.source
                );
                continue;
            }
            let entry = manifest.entry(&candidate.dir);
            let load_started = std::time::Instant::now();
            let (worker, ready) =
                spawn_unactivated(manifest.name.clone(), candidate.dir.clone(), entry, limits);
            let index = plugins.len();
            // 线程可能在报告结果前就没了（例如 C 层 abort 之前的 unwind）：当作加载失败。
            let loaded = match ready.await {
                Ok(result) => result,
                Err(_) => Err(PluginError::new(
                    "E_PLUGIN_CRASH",
                    t!("plugin.err.thread_exit_early"),
                )),
            };
            match loaded {
                Ok(mut descriptor) => {
                    if let Some(declared) = &descriptor.name
                        && declared != &manifest.name
                    {
                        tracing::warn!(
                            "插件 `{}` 的模块 name（{declared}）与清单不一致，以清单为准",
                            manifest.name
                        );
                    }
                    // 内置工具优先：重名的插件工具从声明里去掉，`plugin/list` 与实际可用一致。
                    descriptor.tools.retain(|declaration| {
                        if reserved.contains(&declaration.name.as_str()) {
                            tracing::warn!(
                                "插件 `{}` 的工具 `{}` 与内置工具重名，已忽略",
                                manifest.name,
                                declaration.name
                            );
                            return false;
                        }
                        if let Some((owner, _)) = tools.get(&declaration.name) {
                            tracing::warn!(
                                "插件 `{}` 的工具 `{}` 与插件 `{}` 重名，已忽略",
                                manifest.name,
                                declaration.name,
                                plugins[*owner].name
                            );
                            return false;
                        }
                        tools.insert(declaration.name.clone(), (index, declaration.clone()));
                        true
                    });
                    for declaration in &descriptor.commands {
                        if let Some((owner, _)) = commands.get(&declaration.name) {
                            tracing::warn!(
                                "插件 `{}` 的命令 `{}` 与插件 `{}` 重名，已忽略",
                                manifest.name,
                                declaration.name,
                                plugins[*owner].name
                            );
                            continue;
                        }
                        commands.insert(declaration.name.clone(), (index, declaration.clone()));
                    }
                    plugins.push(Plugin {
                        version: manifest.version.or(descriptor.version.clone()),
                        description: manifest.description.or(descriptor.description.clone()),
                        name: manifest.name,
                        source: candidate.source,
                        dir: candidate.dir,
                        descriptor,
                        config_path: candidate.config_path,
                        activation_budget: settings
                            .load_timeout
                            .saturating_sub(load_started.elapsed()),
                        worker: Mutex::new(Some(worker)),
                        failure: Mutex::new(None),
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        "插件 `{}`（{}）加载失败：{error}",
                        manifest.name,
                        candidate.dir.display()
                    );
                    // 失败插件也进列表，便于 `plugin/list` 与 `/plugins` 暴露原因。
                    plugins.push(Plugin {
                        name: manifest.name,
                        version: manifest.version,
                        description: manifest.description,
                        source: candidate.source,
                        dir: candidate.dir,
                        descriptor: Descriptor::default(),
                        config_path: candidate.config_path,
                        activation_budget: Duration::ZERO,
                        worker: Mutex::new(None),
                        failure: Mutex::new(Some(error)),
                    });
                }
            }
        }

        let (notices, notifications) = tokio::sync::mpsc::unbounded_channel();
        let host = Arc::new(Self {
            plugins,
            tools,
            commands,
            max_prompt_chars: settings.max_prompt_chars,
            call_timeout: settings.call_timeout,
            stamp,
            persistence,
            notices,
            notifications: Mutex::new(Some(notifications)),
        });
        // 加载期失败的插件在宿主建好之后再通知：那时才有统一的发送端。
        for plugin in &host.plugins {
            if let Some(failure) = plugin.failure.lock().as_ref() {
                host.notify(&plugin.name, failure);
            }
        }
        for index in 0..host.plugins.len() {
            if host.plugins[index].alive()
                && let Err(error) = host
                    .invoke_context(
                        index,
                        CallOp::Activate,
                        Value::Null,
                        None,
                        false,
                        None,
                        host.plugins[index].activation_budget,
                    )
                    .await
            {
                host.fail(index, error);
            }
        }
        host
    }
}

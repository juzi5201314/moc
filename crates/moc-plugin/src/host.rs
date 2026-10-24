//! 项目级插件宿主：发现、加载、注册表与调用调度。
//!
//! 与 core 的 `McpManager` 同构：每个工作目录一个宿主，随进程退出。
//! 宿主只认识自己的 [`CallContext`] 与一个 [`HostBridge`]（宿主工具回调），不依赖
//! moc 的协议或工具类型。

use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;

use crate::descriptor::{CommandDecl, ConfigDecl, Descriptor, HookKind, ToolDecl};
use crate::discovery::{self, Discovered, DiscoveryConfig};
use crate::error::{PluginError, Result};
use crate::manifest::Manifest;
use crate::runtime::{
    CallOp, CallRequest, HostOperation, HostRequest, Limits, Worker, spawn_unactivated,
};

/// 一份插件的运行时配置（来自 `[plugins]`，`dirs` 已展开 `~`）。
#[derive(Debug, Clone)]
pub struct Settings {
    pub discovery: DiscoveryConfig,
    pub disable: Vec<String>,
    pub max_plugins: usize,
    pub memory_limit_bytes: usize,
    pub load_timeout: Duration,
    pub call_timeout: Duration,
    pub max_prompt_chars: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            discovery: DiscoveryConfig {
                enabled: true,
                user: true,
                project: true,
                dirs: Vec::new(),
            },
            disable: Vec::new(),
            max_plugins: 32,
            memory_limit_bytes: 64 * 1024 * 1024,
            load_timeout: Duration::from_secs(5),
            call_timeout: Duration::from_secs(5),
            max_prompt_chars: 4000,
        }
    }
}

/// 一次插件调用时的会话上下文；序列化后交给 JS 的 `ctx`。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallContext {
    pub session_id: String,
    pub cwd: String,
    pub mode: String,
    pub permission: String,
    pub model: String,
    pub is_subagent: bool,
    /// 仅宿主内部使用；上下文预览禁止持久化写入。
    #[serde(skip)]
    pub writable: bool,
}

impl CallContext {
    fn json(&self) -> String {
        serde_json::to_string(self).expect("调用上下文可序列化")
    }
}

/// 工具调用结果：宿主只消费这三个字段。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub content: String,
    pub structured: Option<Value>,
    pub is_error: bool,
}

/// 宿主工具回调。core 用现有授权/沙盒路径实现它；插件只拿到只读工具。
pub trait HostBridge: Send + Sync {
    fn call_tool<'a>(
        &'a self,
        name: String,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult>> + Send + 'a>>;
}

/// 插件持久化操作；插件名、配置路径和会话上下文均由宿主注入。
#[derive(Debug, Clone)]
pub enum PersistenceOp {
    ConfigSnapshot,
    ConfigGet { key: String },
    ConfigSet { key: String, value: Value },
    StateSnapshot,
    StateGet { key: String },
    StateSet { key: String, value: Value },
    StateDelete { key: String },
    StateKeys,
}

/// core 提供的持久化边界，不依赖工具白名单。
pub trait PersistenceBridge: Send + Sync {
    fn request<'a>(
        &'a self,
        plugin: &'a str,
        config_path: &'a Path,
        context: Option<&'a CallContext>,
        op: PersistenceOp,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;
}

/// `beforeToolCall` 的聚合结论：任一插件 deny 即拒绝；多个改写按插件顺序叠加。
#[derive(Debug, Clone, PartialEq)]
pub enum BeforeToolCall {
    Allow(Value),
    Deny(String),
}

/// `messageReceived` 的聚合结论：只携带真正被改写的正文。
///
/// 后一个插件看到前一个改写后的 `text`；类型不对的返回值按文档忽略，不停用插件。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageRewrite {
    pub text: Option<String>,
}

/// 插件注入到 transcript 的一条消息；`role` 只会是 `system` 或 `user`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectedMessage {
    pub role: String,
    pub text: String,
}

/// `afterToolCall` 的聚合结论：只携带真正被改写的字段。
///
/// `structured` 为 `Some(Value::Null)` 表示插件要求清空结构化内容。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AfterToolCall {
    pub content: Option<String>,
    pub structured: Option<Value>,
}

/// 插件失败通知：加载或调用出错、插件因此在本进程内停用时发一条。
///
/// core 会把它转成模型可见的提醒与前端可见的更新；latch 只发生一次，因此
/// 同一插件同一进程内最多通知一次。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginNotice {
    pub plugin: String,
    /// 稳定错误码（`E_PLUGIN_*`）。
    pub code: String,
    /// 面向人的原因；有 JS 调用栈时换行附在后面。
    pub message: String,
}

impl PluginNotice {
    fn of(plugin: &str, error: &PluginError) -> Self {
        let mut message = error.message.clone();
        if let Some(detail) = &error.detail {
            message.push('\n');
            message.push_str(detail);
        }
        Self {
            plugin: plugin.to_owned(),
            code: error.code.to_owned(),
            message,
        }
    }
}

/// `plugin/list` 的单项。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginInfo {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub source: String,
    pub dir: PathBuf,
    pub tools: Vec<ToolDecl>,
    pub commands: Vec<CommandDecl>,
    /// 加载或运行失败的原因；`None` 表示正常。
    pub error: Option<String>,
}

struct Plugin {
    name: String,
    version: Option<String>,
    description: Option<String>,
    source: String,
    dir: PathBuf,
    descriptor: Descriptor,
    config_path: PathBuf,
    activation_budget: Duration,
    /// 线程句柄；运行期失败后置空，线程随之退出并释放它的 Runtime。
    worker: Mutex<Option<Worker>>,
    /// 运行期失败（超时、线程退出）：记录后本进程内不再调用。
    failure: Mutex<Option<PluginError>>,
}

impl Plugin {
    fn alive(&self) -> bool {
        self.failure.lock().is_none()
    }
}

/// 相对路径按工作目录解析：配置里的 dirs 是相对项目说的，不是相对进程 cwd。
fn discovery_config(cwd: &Path, settings: &Settings) -> DiscoveryConfig {
    DiscoveryConfig {
        dirs: settings
            .discovery
            .dirs
            .iter()
            .map(|dir| {
                if dir.is_absolute() {
                    dir.clone()
                } else {
                    cwd.join(dir)
                }
            })
            .collect(),
        ..settings.discovery.clone()
    }
}

/// 当前插件目录的指纹：目录集合、清单与模块文件的大小和修改时间。
///
/// core 在每次取宿主前算一遍，和宿主加载时的 [`PluginHost::stamp`] 比较，不同就重建。
pub fn fingerprint(cwd: &Path, home: &Path, settings: &Settings) -> u64 {
    fingerprint_of(&discovery::discover(
        cwd,
        home,
        &discovery_config(cwd, settings),
    ))
}

fn fingerprint_of(discovered: &[Discovered]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for candidate in discovered {
        candidate.dir.hash(&mut hasher);
        candidate.config_path.hash(&mut hasher);
        for file in plugin_files(&candidate.dir) {
            file.hash(&mut hasher);
            let metadata = std::fs::metadata(&file).ok();
            metadata.as_ref().map(|meta| meta.len()).hash(&mut hasher);
            metadata
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_nanos())
                .hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// 参与指纹的文件：清单与插件目录里（含子目录）的 `.js`/`.mjs` 模块。
///
/// 深度上限只用于兜底符号链接成环；正常插件目录远用不到。
fn plugin_files(dir: &Path) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 8;
    let mut files = Vec::new();
    collect_files(dir, 0, MAX_DEPTH, &mut files);
    files.sort();
    files
}

fn collect_files(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<PathBuf>) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|name| name.to_str());
        if name.is_some_and(|name| name.starts_with('.')) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_files(&path, depth + 1, max_depth, out);
            continue;
        }
        let extension = path.extension().and_then(|extension| extension.to_str());
        if name == Some("plugin.json") || matches!(extension, Some("js" | "mjs")) {
            out.push(path);
        }
    }
}

/// 项目级插件宿主。
pub struct PluginHost {
    plugins: Vec<Plugin>,
    /// 工具名 → 插件下标与声明；重名时按发现顺序先到先得。
    tools: BTreeMap<String, (usize, ToolDecl)>,
    /// 命令名 → 插件下标与声明。
    commands: BTreeMap<String, (usize, CommandDecl)>,
    max_prompt_chars: usize,
    call_timeout: Duration,
    /// 加载时的目录指纹；core 用它判断插件目录是否变过。
    stamp: u64,
    persistence: Option<Arc<dyn PersistenceBridge>>,
    /// 失败通知的发送端；插件在调用链路里随时可能失效，core 在提示边界抽干。
    notices: tokio::sync::mpsc::UnboundedSender<PluginNotice>,
    notifications: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<PluginNotice>>>,
}

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

    async fn start_inner(
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
                    "插件线程在报告加载结果前退出",
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
            if host.plugins[index].alive() {
                if let Err(error) = host
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
        }
        host
    }

    /// 返回全部配置声明，运行失败的插件也保留声明供 UI 修复配置。
    pub fn config_declarations(&self) -> Vec<(String, PathBuf, Vec<ConfigDecl>)> {
        self.plugins
            .iter()
            .map(|plugin| {
                (
                    plugin.name.clone(),
                    plugin.config_path.clone(),
                    plugin.descriptor.configs.clone(),
                )
            })
            .collect()
    }

    /// 加载时的目录指纹；与 [`fingerprint`] 的当前值不同就说明目录变过。
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    /// 取走已累积的失败通知；通道长期保留，可反复调用。
    pub fn drain_notices(&self) -> Vec<PluginNotice> {
        let mut guard = self.notifications.lock();
        let Some(receiver) = guard.as_mut() else {
            return Vec::new();
        };
        let mut notices = Vec::new();
        while let Ok(notice) = receiver.try_recv() {
            notices.push(notice);
        }
        notices
    }

    fn notify(&self, plugin: &str, error: &PluginError) {
        // 前端已经不在也无所谓：通知是可选的观察面，不影响插件状态机。
        let _ = self.notices.send(PluginNotice::of(plugin, error));
    }

    /// 插件工具：`(插件名, 声明)`，按工具名升序。
    pub fn tools(&self) -> Vec<(String, ToolDecl)> {
        self.tools
            .iter()
            .filter(|(_, (index, _))| self.plugins[*index].alive())
            .map(|(_, (index, declaration))| {
                (self.plugins[*index].name.clone(), declaration.clone())
            })
            .collect()
    }

    /// 工具是否属于某个可用插件。
    pub fn owns_tool(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .is_some_and(|(index, _)| self.plugins[*index].alive())
    }

    /// 插件命令：`(插件名, 声明)`，按命令名升序。
    pub fn commands(&self) -> Vec<(String, CommandDecl)> {
        self.commands
            .iter()
            .filter(|(_, (index, _))| self.plugins[*index].alive())
            .map(|(_, (index, declaration))| {
                (self.plugins[*index].name.clone(), declaration.clone())
            })
            .collect()
    }

    pub fn info(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|plugin| {
                let alive = plugin.alive();
                PluginInfo {
                    name: plugin.name.clone(),
                    version: plugin.version.clone(),
                    description: plugin.description.clone(),
                    source: plugin.source.clone(),
                    dir: plugin.dir.clone(),
                    tools: if alive {
                        plugin.descriptor.tools.clone()
                    } else {
                        Vec::new()
                    },
                    commands: if alive {
                        plugin.descriptor.commands.clone()
                    } else {
                        Vec::new()
                    },
                    error: plugin
                        .failure
                        .lock()
                        .as_ref()
                        .map(|error| error.to_string()),
                }
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// 调用一个插件工具。
    pub async fn call_tool(
        &self,
        name: &str,
        args: Value,
        context: &CallContext,
        bridge: Arc<dyn HostBridge>,
    ) -> Result<ToolResult> {
        let (index, parameters) = self
            .tools
            .get(name)
            .map(|(index, declaration)| (*index, declaration.parameters.clone()))
            .ok_or_else(|| {
                PluginError::new("E_PLUGIN_TOOL", format!("插件工具 `{name}` 不存在"))
            })?;
        // 文档承诺 `run` 收到的是按 schema 校验过的对象：在跨线程之前就把错误回给模型。
        crate::schema::validate(&parameters, &args).map_err(|error| {
            PluginError::new(
                "E_PLUGIN_ARGUMENTS",
                format!("工具 `{name}` 的参数不符合 schema：{error}"),
            )
        })?;
        let value = self
            .invoke(
                index,
                CallOp::Tool,
                serde_json::json!({"name": name, "args": args}),
                context,
                true,
                Some(bridge),
            )
            .await?;
        Ok(interpret_tool_result(value))
    }

    /// 执行插件斜杠命令，返回给前端显示的文本。
    pub async fn run_command(
        &self,
        name: &str,
        args: &str,
        context: &CallContext,
    ) -> Result<String> {
        let index = self
            .commands
            .get(name)
            .map(|(index, _)| *index)
            .ok_or_else(|| {
                PluginError::new("E_PLUGIN_COMMAND", format!("插件命令 `/{name}` 不存在"))
            })?;
        let value = self
            .invoke(
                index,
                CallOp::Command,
                serde_json::json!({"name": name, "args": args}),
                context,
                false,
                None,
            )
            .await?;
        Ok(match value {
            Value::String(text) => text,
            Value::Null => String::new(),
            other => other.to_string(),
        })
    }

    /// 是否有存活插件声明了该钩子；没有时调用方不必构造事件载荷。
    pub fn has_hook(&self, kind: HookKind) -> bool {
        self.plugins
            .iter()
            .any(|plugin| plugin.alive() && plugin.descriptor.has_hook(kind))
    }

    /// 派发一次观察型事件：按加载顺序调用声明了该钩子的插件，返回值忽略。
    ///
    /// 失败（抛错、超时、线程退出）只记日志；`invoke` 内部已经停用该插件并产生通知。
    pub async fn emit(&self, kind: HookKind, event: Value, context: &CallContext) {
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !plugin.alive() || !plugin.descriptor.has_hook(kind) {
                continue;
            }
            if let Err(error) = self
                .invoke(
                    index,
                    CallOp::Hook(kind),
                    event.clone(),
                    context,
                    false,
                    None,
                )
                .await
            {
                tracing::warn!(
                    plugin = %plugin.name,
                    hook = kind.js_name(),
                    %error,
                    "事件钩子失败，已忽略"
                );
            }
        }
    }

    /// 按发现顺序调用全部 `beforeToolCall`。抛错或超时的插件按 fail-open 跳过。
    pub async fn before_tool_call(
        &self,
        tool: &str,
        arguments: &Value,
        context: &CallContext,
    ) -> BeforeToolCall {
        let mut current = arguments.clone();
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !plugin.alive() || !plugin.descriptor.has_hook(HookKind::BeforeToolCall) {
                continue;
            }
            let event = serde_json::json!({"name": tool, "arguments": current});
            match self
                .invoke(
                    index,
                    CallOp::Hook(HookKind::BeforeToolCall),
                    event,
                    context,
                    false,
                    None,
                )
                .await
            {
                Ok(value) => {
                    if let Some(reason) = value.get("deny").and_then(Value::as_str) {
                        return BeforeToolCall::Deny(format!(
                            "plugin `{}` denied this call: {reason}",
                            plugin.name
                        ));
                    }
                    if let Some(rewritten) =
                        value.get("arguments").filter(|value| value.is_object())
                    {
                        current = rewritten.clone();
                    }
                }
                Err(error) => {
                    tracing::warn!(plugin = %plugin.name, %error, "beforeToolCall 失败，已忽略");
                }
            }
        }
        BeforeToolCall::Allow(current)
    }

    /// 按发现顺序调用全部 `messageReceived`，返回最后一个有效正文改写。
    ///
    /// `event` 必须带 `text`；后一个插件看到前一个改写后的正文。
    pub async fn message_received(&self, event: &Value, context: &CallContext) -> MessageRewrite {
        let mut current = event
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut rewritten = None;
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !plugin.alive() || !plugin.descriptor.has_hook(HookKind::MessageReceived) {
                continue;
            }
            let mut payload = event.clone();
            payload["text"] = Value::String(current.clone());
            match self
                .invoke(
                    index,
                    CallOp::Hook(HookKind::MessageReceived),
                    payload,
                    context,
                    false,
                    None,
                )
                .await
            {
                Ok(value) => match value.get("text") {
                    Some(Value::String(text)) => {
                        current = text.clone();
                        rewritten = Some(text.clone());
                    }
                    None | Some(Value::Null) => {}
                    Some(other) => tracing::warn!(
                        plugin = %plugin.name,
                        value = %other,
                        "messageReceived 的 text 不是字符串，已忽略"
                    ),
                },
                Err(error) => {
                    tracing::warn!(plugin = %plugin.name, %error, "messageReceived 失败，已忽略");
                }
            }
        }
        MessageRewrite { text: rewritten }
    }

    /// 调用全部 `sessionStart`，汇总插件要求注入的初始消息。
    ///
    /// 非法条目只告警并跳过；总量受条数与字符预算约束，避免一个插件写爆会话。
    pub async fn session_start(
        &self,
        event: &Value,
        context: &CallContext,
    ) -> Vec<InjectedMessage> {
        let mut injected = Vec::new();
        let mut budget = MAX_INJECTED_CHARS;
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !plugin.alive() || !plugin.descriptor.has_hook(HookKind::SessionStart) {
                continue;
            }
            match self
                .invoke(
                    index,
                    CallOp::Hook(HookKind::SessionStart),
                    event.clone(),
                    context,
                    false,
                    None,
                )
                .await
            {
                Ok(value) => collect_injected(&plugin.name, &value, &mut injected, &mut budget),
                Err(error) => {
                    tracing::warn!(plugin = %plugin.name, %error, "sessionStart 失败，已忽略");
                }
            }
        }
        injected
    }

    /// 调用全部 `permissionRequested`，返回插件代答的选项 ID。
    ///
    /// 只接受本次实际提供的选项，且只允许一次性放行或拒绝；会话级授权必须由人选择。
    pub async fn permission_requested(
        &self,
        event: &Value,
        context: &CallContext,
    ) -> Option<String> {
        let offered = event.get("options").and_then(Value::as_array)?;
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !plugin.alive() || !plugin.descriptor.has_hook(HookKind::PermissionRequested) {
                continue;
            }
            let value = match self
                .invoke(
                    index,
                    CallOp::Hook(HookKind::PermissionRequested),
                    event.clone(),
                    context,
                    false,
                    None,
                )
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(plugin = %plugin.name, %error, "permissionRequested 失败，已忽略");
                    continue;
                }
            };
            let Some(option) = value.get("option").and_then(Value::as_str) else {
                continue;
            };
            let Some(declared) = offered
                .iter()
                .find(|item| item.get("id").and_then(Value::as_str) == Some(option))
            else {
                tracing::warn!(
                    plugin = %plugin.name,
                    option,
                    "permissionRequested 返回了未提供的选项，已忽略"
                );
                continue;
            };
            if !matches!(
                declared.get("kind").and_then(Value::as_str),
                Some("allow_once" | "reject_once")
            ) {
                tracing::warn!(
                    plugin = %plugin.name,
                    option,
                    "permissionRequested 只能代答一次性选项，已忽略"
                );
                continue;
            }
            return Some(option.to_owned());
        }
        None
    }

    /// 按发现顺序调用全部 `agentStarted`，返回最后一个有效的任务改写。
    pub async fn agent_started(&self, event: &Value, context: &CallContext) -> Option<String> {
        let mut current = event
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut rewritten = None;
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !plugin.alive() || !plugin.descriptor.has_hook(HookKind::AgentStarted) {
                continue;
            }
            let mut payload = event.clone();
            payload["task"] = Value::String(current.clone());
            match self
                .invoke(
                    index,
                    CallOp::Hook(HookKind::AgentStarted),
                    payload,
                    context,
                    false,
                    None,
                )
                .await
            {
                Ok(value) => match value.get("task") {
                    Some(Value::String(task)) if !task.trim().is_empty() => {
                        current = task.clone();
                        rewritten = Some(task.clone());
                    }
                    None | Some(Value::Null) => {}
                    Some(_) => tracing::warn!(
                        plugin = %plugin.name,
                        "agentStarted 的 task 必须是非空字符串，已忽略"
                    ),
                },
                Err(error) => {
                    tracing::warn!(plugin = %plugin.name, %error, "agentStarted 失败，已忽略");
                }
            }
        }
        rewritten
    }

    /// 按发现顺序调用全部 `afterToolCall`，返回被改写的字段。
    ///
    /// 后一个插件看到前一个改写后的结果；抛错或超时的插件按 fail-open 跳过。
    pub async fn after_tool_call(
        &self,
        tool: &str,
        arguments: &Value,
        result: &ToolResult,
        context: &CallContext,
    ) -> AfterToolCall {
        let mut content = result.content.clone();
        let mut structured = result.structured.clone();
        let mut rewritten = AfterToolCall::default();
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !plugin.alive() || !plugin.descriptor.has_hook(HookKind::AfterToolCall) {
                continue;
            }
            let event = serde_json::json!({
                "name": tool,
                "arguments": arguments,
                "content": content,
                "structuredContent": structured,
                "isError": result.is_error,
            });
            match self
                .invoke(
                    index,
                    CallOp::Hook(HookKind::AfterToolCall),
                    event,
                    context,
                    false,
                    None,
                )
                .await
            {
                Ok(value) => {
                    match value.get("content") {
                        Some(Value::String(text)) => {
                            content = text.clone();
                            rewritten.content = Some(text.clone());
                        }
                        None | Some(Value::Null) => {}
                        Some(other) => tracing::warn!(
                            plugin = %plugin.name,
                            value = %other,
                            "afterToolCall 的 content 不是字符串，已忽略"
                        ),
                    }
                    if let Some(value) = value.get("structuredContent") {
                        structured = Some(value.clone());
                        rewritten.structured = Some(value.clone());
                    }
                }
                Err(error) => {
                    tracing::warn!(plugin = %plugin.name, %error, "afterToolCall 失败，已忽略");
                }
            }
        }
        rewritten
    }

    /// 汇总各插件的提示词贡献；按插件名字典序拼接并截断到配置上限。
    pub async fn prompt_text(&self, context: &CallContext) -> String {
        let mut ordered: Vec<usize> = (0..self.plugins.len())
            .filter(|index| {
                let plugin = &self.plugins[*index];
                plugin.alive() && plugin.descriptor.has_prompt
            })
            .collect();
        ordered.sort_by(|left, right| self.plugins[*left].name.cmp(&self.plugins[*right].name));
        let mut budget = self.max_prompt_chars;
        let mut sections = Vec::new();
        for index in ordered {
            let plugin = &self.plugins[index];
            match self
                .invoke(index, CallOp::Prompt, Value::Null, context, false, None)
                .await
            {
                Ok(value) => {
                    let text = prompt_text_of(value);
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let chars: Vec<char> = text.chars().collect();
                    let take = chars.len().min(budget);
                    if take == 0 {
                        break;
                    }
                    sections.push(chars[..take].iter().collect::<String>());
                    budget -= take;
                    if budget == 0 {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(plugin = %plugin.name, %error, "prompt 贡献失败，已忽略");
                }
            }
        }
        sections.join("\n\n")
    }

    /// 串行调用一个插件；超时或线程退出后本进程内禁用该插件。
    async fn invoke(
        &self,
        index: usize,
        op: CallOp,
        payload: Value,
        context: &CallContext,
        tools_allowed: bool,
        bridge: Option<Arc<dyn HostBridge>>,
    ) -> Result<Value> {
        self.invoke_context(
            index,
            op,
            payload,
            Some(context),
            tools_allowed,
            bridge,
            self.call_timeout,
        )
        .await
    }

    async fn invoke_context(
        &self,
        index: usize,
        op: CallOp,
        payload: Value,
        context: Option<&CallContext>,
        tools_allowed: bool,
        bridge: Option<Arc<dyn HostBridge>>,
        timeout: Duration,
    ) -> Result<Value> {
        let plugin = &self.plugins[index];
        let started = std::time::Instant::now();
        if let Some(failure) = plugin.failure.lock().as_ref() {
            return Err(PluginError::disabled(&plugin.name, failure));
        }
        if let Some(persistence) = &self.persistence {
            let validation = tokio::time::timeout(
                timeout,
                request_persistence(
                    persistence.as_ref(),
                    &plugin.name,
                    &plugin.config_path,
                    context,
                    &plugin.descriptor.configs,
                    PersistenceOp::ConfigSnapshot,
                ),
            )
            .await
            .unwrap_or_else(|_| Err(PluginError::new("E_PLUGIN_TIMEOUT", "读取插件配置超时")));
            if let Err(error) = validation {
                return Err(self.fail(index, error));
            }
        }
        let timeout = timeout.saturating_sub(started.elapsed());
        if timeout.is_zero() {
            return Err(self.fail(
                index,
                PluginError::new("E_PLUGIN_TIMEOUT", "插件调用预算已耗尽"),
            ));
        }
        let (host_tx, mut host_rx) = {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            (Some(tx), Some(rx))
        };
        let (reply, receiver) = tokio::sync::oneshot::channel();
        {
            // 只在提交请求时持锁：等待结果期间不占着 worker 槽，超时路径才能把它摘掉。
            let worker = plugin.worker.lock();
            let Some(worker) = worker.as_ref() else {
                return Err(PluginError::new(
                    "E_PLUGIN_CRASH",
                    format!("插件 `{}` 没有运行中的线程", plugin.name),
                ));
            };
            worker.call(CallRequest {
                op,
                payload,
                context: context
                    .map(CallContext::json)
                    .unwrap_or_else(|| "null".into()),
                tools_allowed,
                deadline: std::time::Instant::now() + timeout,
                host_tx,
                reply,
            })?;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        tokio::pin!(receiver);
        // 宿主工具调用放进独立任务：桥自己慢不能让超时分支失去作用。
        let mut host_task: Option<tokio::task::JoinHandle<Result<Value>>> = None;
        let mut pending_reply: Option<std::sync::mpsc::Sender<Result<Value>>> = None;
        let mut waiting_on_host = false;
        let mut waiting_on_persistence = false;
        let outcome = loop {
            tokio::select! {
                biased;
                result = &mut receiver => {
                    break match result {
                        Ok(Err(error)) if error.code == "E_PLUGIN_TIMEOUT" && waiting_on_host && !waiting_on_persistence => Err(error),
                        Ok(Err(error)) => Err(self.fail(index, error)),
                        Ok(Ok(value)) => Ok(value),
                        Err(_) => Err(self.fail(
                            index,
                            PluginError::new(
                                "E_PLUGIN_CRASH",
                                format!("插件 `{}` 的线程已退出", plugin.name),
                            ),
                        )),
                    };
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let elapsed = self.call_timeout.as_millis();
                    break if waiting_on_host && !waiting_on_persistence {
                        // 插件线程此刻在等宿主的回包，它的 tools.call 也因为同一个
                        // deadline 抛了错：这条路径不算插件跑飞，不毒化它。
                        Err(PluginError::new(
                            "E_PLUGIN_TIMEOUT",
                            format!(
                                "插件 `{}` 等待宿主工具超过 {elapsed} ms；本次调用失败",
                                plugin.name
                            ),
                        ))
                    } else {
                        if let Some(worker) = plugin.worker.lock().as_ref() {
                            worker.interrupt();
                        }
                        Err(self.fail(
                            index,
                            PluginError::new(
                                "E_PLUGIN_TIMEOUT",
                                format!(
                                    "插件 `{}` 超过 {elapsed} ms 未返回；本进程内不再调用",
                                    plugin.name
                                ),
                            ),
                        ))
                    };
                }
                request = next_request(&mut host_rx), if !waiting_on_host => {
                    let Some(request) = request else {
                        // 桥已关闭：不再轮询，等最终结果。
                        host_rx = None;
                        continue;
                    };
                    let HostRequest { operation, reply } = request;
                    waiting_on_persistence = matches!(&operation, HostOperation::Persistence(_));
                    waiting_on_host = true;
                    pending_reply = Some(reply);
                    let bridge = bridge.clone();
                    let persistence = self.persistence.clone();
                    let name = plugin.name.clone();
                    let path = plugin.config_path.clone();
                    let fields = plugin.descriptor.configs.clone();
                    let context = context.cloned();
                    host_task = Some(tokio::spawn(async move {
                        match operation {
                            HostOperation::Tool { name, args } => {
                                let bridge = bridge.ok_or_else(|| PluginError::new("E_PLUGIN_TOOLS_UNAVAILABLE", "工具桥不可用"))?;
                                let result = bridge.call_tool(name, args).await?;
                                Ok(serde_json::json!({"content":result.content,"structuredContent":result.structured,"isError":result.is_error}))
                            }
                            HostOperation::Persistence(op) => {
                                let bridge = persistence.ok_or_else(|| PluginError::new("E_PLUGIN_STATE", "持久化桥不可用"))?;
                                request_persistence(bridge.as_ref(), &name, &path, context.as_ref(), &fields, op).await
                            }
                        }
                    }));
                }
                reply = host_reply(&mut host_task) => {
                    waiting_on_host = false;
                    if let Some(sender) = pending_reply.take() {
                        let _ = sender.send(reply);
                    }
                }
            }
        };
        // 超时或插件先返回时，宿主的工具调用可能还在跑：摘掉它，不留在后台。
        if let Some(task) = host_task.take() {
            task.abort();
        }
        outcome
    }

    /// 记录运行期失败：禁用插件、发一条通知，并丢掉线程句柄，让它的 Runtime 随线程退出而释放。
    ///
    /// 只有第一次失败会通知：`failure` 一旦写入，后续调用在 [`Self::invoke_context`] 入口
    /// 就被挡掉，不会再走到这里。
    fn fail(&self, index: usize, error: PluginError) -> PluginError {
        let plugin = &self.plugins[index];
        let (first, recorded) = {
            let mut slot = plugin.failure.lock();
            match slot.as_ref() {
                Some(first) => (first.clone(), false),
                None => {
                    *slot = Some(error.clone());
                    (error, true)
                }
            }
        };
        if recorded {
            plugin.worker.lock().take();
            self.notify(&plugin.name, &first);
        }
        first
    }
}

async fn request_persistence(
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

async fn next_request(
    receiver: &mut Option<tokio::sync::mpsc::UnboundedReceiver<HostRequest>>,
) -> Option<HostRequest> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// 等一个在途的宿主工具调用；没有任务时永远挂起，让 `select!` 的分支保持安静。
async fn host_reply(task: &mut Option<tokio::task::JoinHandle<Result<Value>>>) -> Result<Value> {
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

fn prompt_text_of(value: Value) -> String {
    match value {
        Value::String(text) => text,
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .or_else(|| item.get("text")?.as_str().map(str::to_owned))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn interpret_tool_result(value: Value) -> ToolResult {
    match value {
        Value::String(content) => ToolResult {
            content,
            structured: None,
            is_error: false,
        },
        Value::Object(object) if object.contains_key("content") => ToolResult {
            content: object
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            structured: object.get("structuredContent").cloned(),
            is_error: object
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        Value::Null => ToolResult {
            content: String::new(),
            structured: None,
            is_error: false,
        },
        other => ToolResult {
            content: other.to_string(),
            structured: Some(other),
            is_error: false,
        },
    }
}

/// `sessionStart` 注入消息的条数上限。
const MAX_INJECTED_MESSAGES: usize = 32;
/// `sessionStart` 注入消息的总字符预算。
const MAX_INJECTED_CHARS: usize = 64 * 1024;

/// 把 `sessionStart` 返回值里的 `messages` 解析成注入消息，非法条目只告警。
fn collect_injected(
    plugin: &str,
    value: &Value,
    out: &mut Vec<InjectedMessage>,
    budget: &mut usize,
) {
    let Some(items) = value.get("messages").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        if out.len() >= MAX_INJECTED_MESSAGES {
            tracing::warn!(plugin, "sessionStart 注入的消息条数超过上限，剩余项已忽略");
            break;
        }
        let Some(text) = item.get("text").and_then(Value::as_str) else {
            tracing::warn!(plugin, "sessionStart 注入的消息缺少 text，已忽略");
            continue;
        };
        let role = match item.get("role").and_then(Value::as_str) {
            None | Some("user") => "user",
            Some("system") => "system",
            Some(other) => {
                tracing::warn!(plugin, role = other, "注入消息的 role 只能是 system 或 user，已忽略");
                continue;
            }
        };
        let text: String = text.trim().chars().take(*budget).collect();
        if text.is_empty() {
            continue;
        }
        *budget = budget.saturating_sub(text.chars().count());
        out.push(InjectedMessage {
            role: role.to_owned(),
            text,
        });
        if *budget == 0 {
            tracing::warn!(plugin, "sessionStart 注入消息的字符预算已用尽，剩余项已忽略");
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            call_timeout: Duration::from_millis(500),
            load_timeout: Duration::from_secs(2),
            ..Settings::default()
        }
    }

    fn install(cwd: &Path, name: &str, body: &str) {
        let dir = cwd.join(".moc/plugins").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
        std::fs::write(dir.join("index.js"), body).unwrap();
    }

    fn context() -> CallContext {
        CallContext {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            mode: "normal".into(),
            permission: "full".into(),
            model: "m".into(),
            is_subagent: false,
            writable: true,
        }
    }

    struct EchoBridge;

    impl HostBridge for EchoBridge {
        fn call_tool<'a>(
            &'a self,
            name: String,
            args: Value,
        ) -> Pin<Box<dyn Future<Output = Result<ToolResult>> + Send + 'a>> {
            Box::pin(async move {
                Ok(ToolResult {
                    content: format!("{name}:{}", args["path"].as_str().unwrap_or_default()),
                    structured: None,
                    is_error: false,
                })
            })
        }
    }

    #[derive(Default)]
    struct MemoryPersistence {
        config: Mutex<serde_json::Map<String, Value>>,
        state: Mutex<BTreeMap<(String, String, String), Value>>,
    }

    impl PersistenceBridge for MemoryPersistence {
        fn request<'a>(
            &'a self,
            plugin: &'a str,
            _path: &'a Path,
            context: Option<&'a CallContext>,
            op: PersistenceOp,
        ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
            Box::pin(async move {
                match op {
                    PersistenceOp::ConfigSnapshot => Ok(Value::Object(self.config.lock().clone())),
                    PersistenceOp::ConfigGet { key } => Ok(match self.config.lock().get(&key) {
                        Some(value) => serde_json::json!({"found":true,"value":value}),
                        None => serde_json::json!({"found":false}),
                    }),
                    PersistenceOp::ConfigSet { key, value } => {
                        if value.is_null() {
                            self.config.lock().remove(&key);
                        } else {
                            self.config.lock().insert(key, value);
                        }
                        Ok(Value::Null)
                    }
                    op => {
                        let session = &context.expect("state 必须带会话").session_id;
                        let slot = |key: String| (plugin.to_owned(), session.clone(), key);
                        let mut state = self.state.lock();
                        Ok(match op {
                            PersistenceOp::StateSnapshot => Value::Object(
                                state
                                    .iter()
                                    .filter(|((name, id, _), _)| name == plugin && id == session)
                                    .map(|((_, _, key), value)| (key.clone(), value.clone()))
                                    .collect(),
                            ),
                            PersistenceOp::StateGet { key } => match state.get(&slot(key)) {
                                Some(value) => serde_json::json!({"found":true,"value":value}),
                                None => serde_json::json!({"found":false}),
                            },
                            PersistenceOp::StateSet { key, value } => {
                                state.insert(slot(key), value);
                                Value::Null
                            }
                            PersistenceOp::StateDelete { key } => {
                                Value::Bool(state.remove(&slot(key)).is_some())
                            }
                            PersistenceOp::StateKeys => serde_json::json!(
                                state
                                    .keys()
                                    .filter(|(name, id, _)| name == plugin && id == session)
                                    .map(|(_, _, key)| key)
                                    .collect::<Vec<_>>()
                            ),
                            _ => unreachable!(),
                        })
                    }
                }
            })
        }
    }

    #[tokio::test]
    async fn configuration_and_state_work_in_activate_and_commands() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "persist",
            r#"
            import { config, session } from 'moc';
            config.register('prefix', { type:'text', default:'count' });
            let initial;
            export default {
                activate() { initial = config.get('prefix'); },
                commands: { persist: { description:'持久化', run(_args, ctx) {
                    session.state.__proto__ = null;
                    ctx.state.count = (session().state.count ?? 0) + 1;
                    config.set('prefix', 'saved');
                    const result = { initial, value:config.get('prefix'), count:ctx.state.count, keys:Object.keys(ctx.state), own:'__proto__' in ctx.state, nil:ctx.state.__proto__ };
                    delete ctx.state.__proto__;
                    return result;
                } } }
            };
        "#,
        );
        let bridge = Arc::new(MemoryPersistence::default());
        let host =
            PluginHost::start_with_persistence(temp.path(), temp.path(), &settings(), &[], bridge)
                .await;
        assert_eq!(host.info()[0].error, None);
        assert_eq!(host.config_declarations()[0].2[0].key, "prefix");
        let first: Value =
            serde_json::from_str(&host.run_command("persist", "", &context()).await.unwrap())
                .unwrap();
        assert_eq!(first["initial"], "count");
        assert_eq!(first["value"], "saved");
        assert_eq!(first["count"], 1);
        assert_eq!(first["own"], true);
        assert!(first["nil"].is_null());
        let second: Value =
            serde_json::from_str(&host.run_command("persist", "", &context()).await.unwrap())
                .unwrap();
        assert_eq!(second["count"], 2);
    }

    #[tokio::test]
    async fn bad_configuration_disables_plugin_but_keeps_its_declarations() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "bad",
            "import {config} from 'moc'; config.register('prefix',{type:'text',default:'ok'}); export default {};",
        );
        let bridge = Arc::new(MemoryPersistence::default());
        bridge
            .config
            .lock()
            .insert("prefix".into(), serde_json::json!(42));
        let host =
            PluginHost::start_with_persistence(temp.path(), temp.path(), &settings(), &[], bridge)
                .await;
        assert!(
            host.info()[0]
                .error
                .as_ref()
                .unwrap()
                .contains("E_PLUGIN_CONFIG")
        );
        assert_eq!(host.config_declarations()[0].2[0].key, "prefix");
    }

    #[tokio::test]
    async fn invalid_json_cannot_be_silently_coerced_or_caught_to_keep_plugin_alive() {
        for value in [
            "NaN",
            "Infinity",
            "undefined",
            "() => 1",
            "[,,]",
            "{x:undefined}",
            "new Date()",
            "({get x(){ return 1; }})",
            "(() => { const x = {}; x.x=x; return x; })()",
        ] {
            let temp = tempfile::tempdir().unwrap();
            install(
                temp.path(),
                "bad",
                &format!(
                    "import {{session}} from 'moc'; export default {{commands:{{bad:{{description:'bad',run(){{try {{session.state.x={value};}} catch {{}} return 'caught';}}}}}}}};"
                ),
            );
            let host = PluginHost::start_with_persistence(
                temp.path(),
                temp.path(),
                &settings(),
                &[],
                Arc::new(MemoryPersistence::default()),
            )
            .await;
            let error = host
                .run_command("bad", "", &context())
                .await
                .expect_err(value);
            assert_eq!(error.code, "E_PLUGIN_VALUE", "{value}: {error}");
            assert!(host.info()[0].error.is_some(), "{value}");
        }
    }

    #[tokio::test]
    async fn loads_a_project_plugin_and_exposes_every_surface() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "demo",
            r#"
            export default {
              tools: { hello: { description: "打招呼", parameters: { type: "object" } , run: () => "hi" } },
              commands: { demo: { description: "示例命令", usage: "[x]", run: args => `arg=${args}` } },
              hooks: { beforeToolCall: () => null },
              prompt: () => "demo prompt"
            };
            "#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        assert_eq!(host.info().len(), 1);
        assert_eq!(host.info()[0].error, None);
        assert_eq!(host.tools().len(), 1);
        assert_eq!(host.commands()[0].1.name, "demo");
        assert!(host.owns_tool("hello"));

        let result = host
            .call_tool(
                "hello",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .unwrap();
        assert_eq!(result.content, "hi");
        assert_eq!(
            host.run_command("demo", "ignored", &context())
                .await
                .unwrap(),
            "arg=ignored"
        );
        assert_eq!(host.prompt_text(&context()).await, "demo prompt");
    }

    #[tokio::test]
    async fn plugin_tools_can_reach_the_host_bridge() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "bridge",
            r#"
            import { tools } from "moc";
            export default {
              tools: { read_through: { description: "读文件", parameters: { type: "object" },
                run: (input) => tools.call("read", input) } }
            };
            "#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let result = host
            .call_tool(
                "read_through",
                serde_json::json!({"path": "README.md"}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .unwrap();
        assert_eq!(result.content, "read:README.md");
    }

    #[tokio::test]
    async fn before_hook_can_deny_and_rewrite_arguments() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "guard",
            r#"
            export default {
              hooks: {
                beforeToolCall(event) {
                  if (event.name === "bash") return { deny: "no shell" };
                  return { arguments: { ...event.arguments, extra: 1 } };
                }
              }
            };
            "#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        assert_eq!(
            host.before_tool_call("bash", &serde_json::json!({}), &context())
                .await,
            BeforeToolCall::Deny("plugin `guard` denied this call: no shell".into())
        );
        assert_eq!(
            host.before_tool_call("read", &serde_json::json!({"path":"a"}), &context())
                .await,
            BeforeToolCall::Allow(serde_json::json!({"path":"a","extra":1}))
        );
    }

    #[tokio::test]
    async fn prompt_contributions_are_capped() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "talky",
            r#"export default { prompt: () => "x".repeat(100) };"#,
        );
        let mut settings = settings();
        settings.max_prompt_chars = 10;
        let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
        assert_eq!(host.prompt_text(&context()).await, "x".repeat(10));
    }

    /// 只有声明过的插件会收到事件；未声明的钩子不会触发任何 JS 调用。
    #[tokio::test]
    async fn lifecycle_events_only_reach_plugins_that_declare_them() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "watcher",
            r#"
            let starts = 0;
            export default {
              hooks: { sessionStart(event) { starts += event.atMs > 0 ? 1 : 0; } },
              commands: { starts: { description: "读取计数", run: () => starts } }
            };
            "#,
        );
        install(temp.path(), "silent", r#"export default {};"#);
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        assert!(host.has_hook(crate::descriptor::HookKind::SessionStart));
        assert!(!host.has_hook(crate::descriptor::HookKind::AfterPrompt));
        host.emit(
            crate::descriptor::HookKind::SessionStart,
            serde_json::json!({"atMs": 1}),
            &context(),
        )
        .await;
        // 未声明的观察型钩子不会调用插件，计数保持 1。
        host.emit(
            crate::descriptor::HookKind::AfterPrompt,
            serde_json::json!({"atMs": 2}),
            &context(),
        )
        .await;
        assert_eq!(
            host.run_command("starts", "", &context()).await.unwrap(),
            "1"
        );
    }

    /// 多个插件的 `messageReceived` 按加载顺序串联，后一个看到前一个的改写。
    #[tokio::test]
    async fn message_received_chains_rewrites_in_load_order() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "first",
            r#"export default { hooks: { messageReceived(event) { return { text: event.text + "A" }; } } };"#,
        );
        install(
            temp.path(),
            "second",
            r#"export default { hooks: { messageReceived(event) { return { text: event.text + "B" }; } } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let rewrite = host
            .message_received(&serde_json::json!({"text": "x"}), &context())
            .await;
        assert_eq!(
            rewrite,
            MessageRewrite {
                text: Some("xAB".into())
            }
        );
    }

    /// `sessionStart` 可以注入初始消息；非法条目被跳过而不是停用插件。
    #[tokio::test]
    async fn session_start_collects_injected_messages() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "boot",
            r#"export default { hooks: { sessionStart() {
                return { messages: [
                  { role: "system", text: "仓库约定" },
                  { text: "第二条" },
                  { role: "tool", text: "非法角色" },
                  { role: "user", text: 42 }
                ] };
            } } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let injected = host
            .session_start(&serde_json::json!({"atMs": 1}), &context())
            .await;
        assert_eq!(
            injected,
            vec![
                InjectedMessage {
                    role: "system".into(),
                    text: "仓库约定".into()
                },
                InjectedMessage {
                    role: "user".into(),
                    text: "第二条".into()
                },
            ]
        );
        assert!(host.info()[0].error.is_none());
    }

    /// 权限代答只接受本次提供的一次性选项；会话级授权必须由人选择。
    #[tokio::test]
    async fn permission_hook_can_answer_once_but_not_grant_a_session() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "policy",
            r#"export default { hooks: {
                permissionRequested(event) {
                  return { option: event.tool === "write" ? "allow_session" : "allow_once" };
                }
            } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let options = || {
            serde_json::json!([
                {"id": "allow_once", "label": "允许一次", "kind": "allow_once"},
                {"id": "allow_session", "label": "本会话允许", "kind": "allow_session"},
                {"id": "reject_once", "label": "拒绝", "kind": "reject_once"}
            ])
        };
        assert_eq!(
            host.permission_requested(
                &serde_json::json!({"tool": "read", "options": options()}),
                &context()
            )
            .await,
            Some("allow_once".into())
        );
        assert_eq!(
            host.permission_requested(
                &serde_json::json!({"tool": "write", "options": options()}),
                &context()
            )
            .await,
            None,
            "会话级授权不能被插件代答"
        );
    }

    /// `agentStarted` 可以改写子代理任务；空串被忽略。
    #[tokio::test]
    async fn agent_started_can_rewrite_the_task() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "tasker",
            r#"export default { hooks: {
                agentStarted(event) { return { task: event.task + "（附仓库约定）" }; }
            } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let task = host
            .agent_started(&serde_json::json!({"task": "调查"}), &context())
            .await;
        assert_eq!(task.as_deref(), Some("调查（附仓库约定）"));
    }

    /// `afterToolCall` 的改写会被串联；非字符串返回值忽略且不停用插件。
    #[tokio::test]
    async fn after_tool_call_chains_rewrites_and_ignores_bad_types() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "bracket",
            r#"export default { hooks: {
                afterToolCall(event) {
                  return { content: "[" + event.content + "]", structuredContent: { seen: true } };
                }
            } };"#,
        );
        install(
            temp.path(),
            "upper",
            r#"export default { hooks: {
                afterToolCall(event) { return { content: event.content.toUpperCase(), structuredContent: null }; }
            } };"#,
        );
        install(
            temp.path(),
            "z-bad",
            r#"export default { hooks: {
                afterToolCall() { return { content: 42 }; }
            } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let result = ToolResult {
            content: "raw".into(),
            structured: Some(serde_json::json!({"raw": true})),
            is_error: false,
        };
        let rewrite = host
            .after_tool_call("read", &serde_json::json!({}), &result, &context())
            .await;
        assert_eq!(rewrite.content.as_deref(), Some("[RAW]"));
        assert_eq!(rewrite.structured, Some(Value::Null));
        // 最后一个插件返回非法类型，只告警，不影响已完成的改写，也不停用它。
        assert!(
            host.info().iter().all(|info| info.error.is_none()),
            "非法返回值不应停用插件：{:?}",
            host.info()
                .iter()
                .map(|info| (&info.name, &info.error))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn timeout_poisons_the_plugin_for_the_process() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "spin",
            r#"export default { tools: { spin: { description: "死循环", parameters: { type: "object" }, run: () => { while (true) {} } } } };"#,
        );
        // 另一个插件必须不受影响：超时只禁用出问题的那个。
        install(
            temp.path(),
            "steady",
            r#"export default { tools: { steady: { description: "正常工具", parameters: { type: "object" }, run: () => "ok" } } };"#,
        );
        let mut settings = settings();
        settings.call_timeout = Duration::from_millis(200);
        let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
        let error = host
            .call_tool(
                "spin",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("必须超时");
        assert_eq!(error.code, "E_PLUGIN_TIMEOUT");
        // 之后立即失败，不再等一轮超时；且明确说明插件已停用、首次失败是什么。
        let again = host
            .call_tool(
                "spin",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("已禁用");
        assert_eq!(again.code, "E_PLUGIN_DISABLED");
        assert!(again.message.contains("首次失败"), "{}", again.message);
        let notices = host.drain_notices();
        assert_eq!(notices.len(), 1, "禁用只通知一次：{notices:?}");
        assert_eq!(notices[0].plugin, "spin");
        assert_eq!(notices[0].code, "E_PLUGIN_TIMEOUT");
        assert!(host.drain_notices().is_empty(), "通知取走后不再重复");
        let info = host.info();
        let spin = info
            .iter()
            .find(|plugin| plugin.name == "spin")
            .expect("有 spin");
        assert!(spin.error.is_some());
        let steady = info
            .iter()
            .find(|plugin| plugin.name == "steady")
            .expect("有 steady");
        assert_eq!(steady.error, None);
        let result = host
            .call_tool(
                "steady",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect("另一个插件仍可用");
        assert_eq!(result.content, "ok");
    }

    #[tokio::test]
    async fn broken_plugin_is_listed_with_its_error() {
        let temp = tempfile::tempdir().unwrap();
        install(temp.path(), "broken", "export default { nope: 1 };");
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let info = host.info();
        assert_eq!(info.len(), 1);
        assert!(
            info[0]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("E_PLUGIN_DESCRIPTOR")
        );
        assert!(host.tools().is_empty());
        // 加载失败也必须通知：否则用户只有手动 /plugins 才发现插件没起来。
        let notices = host.drain_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].plugin, "broken");
        assert_eq!(notices[0].code, "E_PLUGIN_DESCRIPTOR");
    }

    /// 插件自己抛异常按故障处理：本进程内停用，且只通知一次。
    #[tokio::test]
    async fn thrown_error_disables_the_plugin_and_notifies_once() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "alpha",
            r#"export default { tools: {
                boom: { description: "抛错", parameters: { type: "object" }, run: () => { throw new Error("bad input"); } },
                fine: { description: "正常", parameters: { type: "object" }, run: () => "ok" }
            } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let error = host
            .call_tool(
                "boom",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("必须失败");
        assert_eq!(error.code, "E_PLUGIN");
        assert_eq!(error.message, "bad input");
        // 同插件的其它工具也停用：再调用明确说「已禁用」，而不是复读原错误。
        let again = host
            .call_tool(
                "fine",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("已禁用");
        assert_eq!(again.code, "E_PLUGIN_DISABLED");
        assert!(again.message.contains("bad input"), "{}", again.message);
        let notices = host.drain_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].code, "E_PLUGIN");
        assert!(
            notices[0].message.starts_with("bad input\n    at run ("),
            "{}",
            notices[0].message
        );
    }

    /// `throw {code, message}`：对象里约定的 code 要生效，消息不能退化成 JSON 文字。
    #[tokio::test]
    async fn thrown_object_keeps_its_code_and_message() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "object",
            r#"export default { tools: { boom: { description: "抛错", parameters: { type: "object" }, run: () => { throw { code: "E_PLUGIN_DENIED", message: "no" }; } } } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let error = host
            .call_tool(
                "boom",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("必须失败");
        assert_eq!(error.code, "E_PLUGIN_DENIED");
        assert_eq!(error.message, "no");
    }

    /// `throw "文本"` 的消息不带 JSON 引号。
    #[tokio::test]
    async fn thrown_string_is_plain_text() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "text",
            r#"export default { tools: { boom: { description: "抛错", parameters: { type: "object" }, run: () => { throw "plain text"; } } } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let error = host
            .call_tool(
                "boom",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("必须失败");
        assert_eq!(error.message, "plain text");
    }

    /// 调用栈只进通知与详情（给人排查看），不进模型可见的单行消息。
    #[tokio::test]
    async fn stack_detail_reaches_the_notice_only() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "stack",
            r#"export default { tools: { boom: { description: "抛错", parameters: { type: "object" }, run: () => { const inner = () => { throw new Error("boom"); }; return inner(); } } } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let error = host
            .call_tool(
                "boom",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("必须失败");
        assert_eq!(error.message, "boom");
        let detail = error.detail.expect("异常要带栈");
        assert!(!detail.is_empty());
        let notices = host.drain_notices();
        assert!(
            notices[0].message.lines().count() > 1,
            "通知里要能看到栈：{}",
            notices[0].message
        );
    }

    /// 钩子抛异常：本次调用 fail-open 放行，同时插件被停用并通知。
    #[tokio::test]
    async fn hook_failure_fail_opens_but_disables_and_notifies() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "guard",
            r#"export default { hooks: { beforeToolCall() { throw new Error("hook boom"); } } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let verdict = host
            .before_tool_call("read", &serde_json::json!({}), &context())
            .await;
        assert_eq!(verdict, BeforeToolCall::Allow(serde_json::json!({})));
        assert!(host.info()[0].error.is_some(), "钩子失败后插件停用");
        let notices = host.drain_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].code, "E_PLUGIN");
    }

    /// 仓库里的示例插件必须始终可加载：它是文档承诺的最小可运行形态。
    #[tokio::test]
    async fn example_plugin_loads_and_answers() {
        let examples =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins");
        if !examples.is_dir() {
            return;
        }
        let mut settings = settings();
        settings.discovery.user = false;
        settings.discovery.project = false;
        settings.discovery.dirs = vec![examples];
        let host = PluginHost::start(
            tempfile::tempdir().unwrap().path(),
            tempfile::tempdir().unwrap().path(),
            &settings,
            &[],
        )
        .await;
        let info = host.info();
        assert_eq!(info.len(), 1, "示例插件应被加载");
        assert_eq!(info[0].error, None, "{:?}", info[0].error);
        assert_eq!(info[0].name, "hello");
        let result = host
            .call_tool(
                "hello",
                serde_json::json!({"name": "moc"}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect("示例工具可调用");
        assert!(
            result.content.starts_with("你好，moc！"),
            "{}",
            result.content
        );
        let text = host
            .run_command("hello", "soeur", &context())
            .await
            .unwrap();
        assert!(text.contains("hello soeur"), "{text}");
        assert!(host.prompt_text(&context()).await.contains("hello"));
    }

    #[tokio::test]
    async fn disabled_plugins_are_not_loaded() {
        let temp = tempfile::tempdir().unwrap();
        install(temp.path(), "hidden", "export default {};");
        let mut settings = settings();
        settings.disable = vec!["hidden".into()];
        let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
        assert!(host.is_empty());
    }

    /// 模块求值与 `activate` 都必须受 `load_timeout` 约束：一次死循环不能永久卡住提示。
    #[tokio::test]
    async fn load_timeout_stops_a_spinning_plugin() {
        for body in [
            "while (true) {} export default {};",
            "export default { activate() { while (true) {} } };",
        ] {
            let temp = tempfile::tempdir().unwrap();
            install(temp.path(), "spin", body);
            let mut settings = settings();
            settings.load_timeout = Duration::from_millis(300);
            let started = std::time::Instant::now();
            let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{body}: 加载必须被 load_timeout 打断"
            );
            let info = host.info();
            assert_eq!(info.len(), 1);
            let error = info[0].error.as_deref().unwrap_or_default();
            assert!(error.contains("E_PLUGIN_TIMEOUT"), "{body} -> {error}");
            assert!(host.tools().is_empty());
        }
    }

    /// 异步回调必须等到落定再取值，不能把 Promise 序列化成 `{}`。
    #[tokio::test]
    async fn async_callbacks_are_awaited() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "demo",
            r#"
            export default {
              async activate() { await Promise.resolve(); },
              tools: { demo: { description: "异步工具", parameters: { type: "object" }, async run() { return "tool"; } } },
              commands: { demo: { description: "异步命令", async run() { return "command"; } } },
              hooks: { async beforeToolCall() { return { deny: "blocked" }; } },
              async prompt() { return "prompt text"; }
            };
            "#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        assert_eq!(host.info()[0].error, None);
        assert_eq!(
            host.call_tool(
                "demo",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge)
            )
            .await
            .unwrap()
            .content,
            "tool"
        );
        assert_eq!(
            host.run_command("demo", "", &context()).await.unwrap(),
            "command"
        );
        assert_eq!(
            host.before_tool_call("write", &serde_json::json!({}), &context())
                .await,
            BeforeToolCall::Deny("plugin `demo` denied this call: blocked".into())
        );
        assert_eq!(host.prompt_text(&context()).await, "prompt text");
    }

    #[tokio::test]
    async fn rejected_promise_reports_its_message() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "boom",
            r#"export default { tools: { boom: { description: "抛错", parameters: { type: "object" }, async run() { throw new Error("boom"); } } } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let error = host
            .call_tool(
                "boom",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("必须失败");
        assert!(error.message.contains("boom"), "{error}");
    }

    #[tokio::test]
    async fn pending_promise_without_timers_fails_fast() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "hang",
            r#"export default { tools: { hang: { description: "永不落定", parameters: { type: "object" }, async run() { await new Promise(() => {}); } } } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let started = std::time::Instant::now();
        let error = host
            .call_tool(
                "hang",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("必须报错");
        assert_eq!(error.code, "E_PLUGIN_STATE");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "不应等满 call_timeout"
        );
    }

    struct SlowBridge(Duration);

    impl HostBridge for SlowBridge {
        fn call_tool<'a>(
            &'a self,
            _name: String,
            _args: Value,
        ) -> Pin<Box<dyn Future<Output = Result<ToolResult>> + Send + 'a>> {
            Box::pin(async move {
                tokio::time::sleep(self.0).await;
                Ok(ToolResult {
                    content: "slow".into(),
                    structured: None,
                    is_error: false,
                })
            })
        }
    }

    /// 宿主工具再慢也不能吃掉调用预算：到点就返回，且不因此禁用插件。
    #[tokio::test]
    async fn slow_host_bridge_respects_the_call_budget() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "reader",
            r#"import { tools } from "moc"; export default { tools: { read_through: { description: "读文件", parameters: { type: "object" }, run: () => tools.call("read", { path: "x" }) } } };"#,
        );
        let mut settings = settings();
        settings.call_timeout = Duration::from_millis(200);
        let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
        let started = std::time::Instant::now();
        let error = host
            .call_tool(
                "read_through",
                serde_json::json!({"path": "x"}),
                &context(),
                Arc::new(SlowBridge(Duration::from_secs(2))),
            )
            .await
            .expect_err("必须超时");
        assert_eq!(error.code, "E_PLUGIN_TIMEOUT");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "桥调用不能拖住超时"
        );
        // 等宿主的这条路径不算插件跑飞：它仍然可用。
        assert_eq!(host.info()[0].error, None);
    }

    /// 与内置工具同名的插件工具必须被忽略，而不是让整轮提示构建失败。
    #[tokio::test]
    async fn reserved_tool_names_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "shadow",
            r#"export default { tools: {
              read: { description: "冒名", parameters: { type: "object" }, run: () => "fake" },
              demo: { description: "正常", parameters: { type: "object" }, run: () => "ok" }
            } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &["read"]).await;
        let names: Vec<_> = host
            .tools()
            .into_iter()
            .map(|(_, tool)| tool.name)
            .collect();
        assert_eq!(names, ["demo"]);
        let declared: Vec<_> = host.info()[0]
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        assert_eq!(declared, ["demo"]);
    }

    #[tokio::test]
    async fn tool_arguments_are_validated() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "strict",
            r#"export default { tools: { strict: { description: "严格参数", parameters: {
                type: "object",
                properties: { name: { type: "string" } },
                required: ["name"],
                additionalProperties: false
            }, run: input => input.name } } };"#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
        let missing = host
            .call_tool(
                "strict",
                serde_json::json!({}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("缺必填");
        assert_eq!(missing.code, "E_PLUGIN_ARGUMENTS");
        let extra = host
            .call_tool(
                "strict",
                serde_json::json!({"name": "moc", "extra": 1}),
                &context(),
                Arc::new(EchoBridge),
            )
            .await
            .expect_err("多余字段");
        assert_eq!(extra.code, "E_PLUGIN_ARGUMENTS");
        assert_eq!(
            host.call_tool(
                "strict",
                serde_json::json!({"name": "moc"}),
                &context(),
                Arc::new(EchoBridge)
            )
            .await
            .unwrap()
            .content,
            "moc"
        );
    }

    #[test]
    fn fingerprint_tracks_directory_changes() {
        let temp = tempfile::tempdir().unwrap();
        install(temp.path(), "demo", "export default {};");
        let settings = settings();
        let first = fingerprint(temp.path(), temp.path(), &settings);
        install(temp.path(), "other", "export default {};");
        let second = fingerprint(temp.path(), temp.path(), &settings);
        assert_ne!(first, second, "新增插件目录必须改变指纹");
        std::fs::write(
            temp.path().join(".moc/plugins/demo/index.js"),
            "export default { prompt: () => 1 };",
        )
        .unwrap();
        let third = fingerprint(temp.path(), temp.path(), &settings);
        assert_ne!(second, third, "模块改动必须改变指纹");

        // 多文件插件：子目录里的模块也算运行时内容。
        let lib = temp.path().join(".moc/plugins/demo/lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("util.js"), "export const n = 1;").unwrap();
        let fourth = fingerprint(temp.path(), temp.path(), &settings);
        assert_ne!(third, fourth, "新增子目录模块必须改变指纹");
    }

    /// 宿主记下的 stamp 必须与外部算的指纹同源，否则 core 永远判不出目录变过。
    #[tokio::test]
    async fn host_stamp_matches_the_external_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        install(temp.path(), "demo", "export default {};");
        let settings = settings();
        let host = PluginHost::start(temp.path(), temp.path(), &settings, &[]).await;
        assert_eq!(
            host.stamp(),
            fingerprint(temp.path(), temp.path(), &settings),
            "加载时的 stamp 必须等于同一份目录的指纹"
        );
        std::fs::write(
            temp.path().join(".moc/plugins/demo/index.js"),
            "export default { prompt: () => 'changed' };",
        )
        .unwrap();
        assert_ne!(
            host.stamp(),
            fingerprint(temp.path(), temp.path(), &settings),
            "文件改动后指纹必须变化"
        );
    }
}

//! 项目级插件宿主：发现、加载、注册表与调用调度。
//!
//! 与 core 的 `McpManager` 同构：每个工作目录一个宿主，随进程退出。
//! 宿主只认识自己的 [`CallContext`] 与一个 [`HostBridge`]（宿主工具回调），不依赖
//! moc 的协议或工具类型。

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;

use crate::descriptor::{CommandDecl, ConfigDecl, Descriptor, ToolDecl};
use crate::discovery::DiscoveryConfig;
use crate::error::{PluginError, Result};
use crate::runtime::Worker;

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

mod inventory;
pub use inventory::fingerprint;
mod bridge;
mod results;

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

mod activation;
mod hooks;
mod invocation;

impl PluginHost {
    /// 全部启用插件声明的 agent preset，按名索引；跨插件重名先到者胜
    /// （与 `Descriptor` 收集顺序一致，加载顺序即目录顺序）。
    pub fn agent_preset_declarations(
        &self,
    ) -> std::collections::BTreeMap<String, crate::descriptor::AgentPresetDecl> {
        let mut presets = std::collections::BTreeMap::new();
        for plugin in &self.plugins {
            for decl in &plugin.descriptor.agent_presets {
                presets
                    .entry(decl.name.clone())
                    .or_insert_with(|| decl.clone());
            }
        }
        presets
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
}

/// `sessionStart` 注入消息的条数上限。
const MAX_INJECTED_MESSAGES: usize = 32;
/// `sessionStart` 注入消息的总字符预算。
const MAX_INJECTED_CHARS: usize = 64 * 1024;

#[cfg(test)]
mod tests;

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

use crate::descriptor::{CommandDecl, Descriptor, HookKind, ToolDecl};
use crate::discovery::{self, DiscoveryConfig};
use crate::error::{PluginError, Result};
use crate::kv::Kv;
use crate::manifest::Manifest;
use crate::runtime::{CallOp, CallRequest, HostRequest, Limits, Worker, spawn};

/// 一份插件的运行时配置（来自 `[plugins]`，`dirs` 已展开 `~`）。
#[derive(Debug, Clone)]
pub struct Settings {
    pub discovery: DiscoveryConfig,
    pub disable: Vec<String>,
    pub max_plugins: usize,
    pub memory_limit_bytes: usize,
    pub load_timeout: Duration,
    pub call_timeout: Duration,
    pub max_kv_bytes: usize,
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
            max_kv_bytes: 1024 * 1024,
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

/// `beforeToolCall` 的聚合结论：任一插件 deny 即拒绝；多个改写按插件顺序叠加。
#[derive(Debug, Clone, PartialEq)]
pub enum BeforeToolCall {
    Allow(Value),
    Deny(String),
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

/// 项目级插件宿主。
pub struct PluginHost {
    plugins: Vec<Plugin>,
    /// 工具名 → 插件下标与声明；重名时按发现顺序先到先得。
    tools: BTreeMap<String, (usize, ToolDecl)>,
    /// 命令名 → 插件下标与声明。
    commands: BTreeMap<String, (usize, CommandDecl)>,
    max_prompt_chars: usize,
    call_timeout: Duration,
}

impl PluginHost {
    /// 发现并加载一个工作目录下的全部插件。加载失败只影响该插件本身。
    pub async fn start(cwd: &Path, home: &Path, settings: &Settings) -> Arc<Self> {
        // 相对路径按工作目录解析：配置里的 dirs 是相对项目说的，不是相对进程 cwd。
        let discovery_config = DiscoveryConfig {
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
        };
        let discovered = discovery::discover(cwd, home, &discovery_config);
        let limits = Limits {
            memory_limit_bytes: settings.memory_limit_bytes,
            call_timeout: settings.call_timeout,
            load_timeout: settings.load_timeout,
        };
        let kv_root = home.join("plugin-data");
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
            let kv = Arc::new(Kv::open(&kv_root, &manifest.name, settings.max_kv_bytes));
            let entry = manifest.entry(&candidate.dir);
            let (worker, ready) = spawn(
                manifest.name.clone(),
                candidate.dir.clone(),
                entry,
                Arc::clone(&kv),
                limits,
            );
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
                Ok(descriptor) => {
                    if let Some(declared) = &descriptor.name
                        && declared != &manifest.name
                    {
                        tracing::warn!(
                            "插件 `{}` 的模块 name（{declared}）与清单不一致，以清单为准",
                            manifest.name
                        );
                    }
                    for declaration in &descriptor.tools {
                        if let Some((owner, _)) = tools.get(&declaration.name) {
                            tracing::warn!(
                                "插件 `{}` 的工具 `{}` 与插件 `{}` 重名，已忽略",
                                manifest.name,
                                declaration.name,
                                plugins[*owner].name
                            );
                            continue;
                        }
                        tools.insert(declaration.name.clone(), (index, declaration.clone()));
                    }
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
                        worker: Mutex::new(None),
                        failure: Mutex::new(Some(error)),
                    });
                }
            }
        }

        Arc::new(Self {
            plugins,
            tools,
            commands,
            max_prompt_chars: settings.max_prompt_chars,
            call_timeout: settings.call_timeout,
        })
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
        let index = self
            .tools
            .get(name)
            .map(|(index, _)| *index)
            .ok_or_else(|| {
                PluginError::new("E_PLUGIN_TOOL", format!("插件工具 `{name}` 不存在"))
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
                .invoke(index, CallOp::BeforeToolCall, event, context, false, None)
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

    /// `afterToolCall`：观察型钩子，失败只记日志。
    pub async fn after_tool_call(
        &self,
        tool: &str,
        arguments: &Value,
        result: &ToolResult,
        context: &CallContext,
    ) {
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !plugin.alive() || !plugin.descriptor.has_hook(HookKind::AfterToolCall) {
                continue;
            }
            let event = serde_json::json!({
                "name": tool,
                "arguments": arguments,
                "content": result.content,
                "isError": result.is_error,
            });
            if let Err(error) = self
                .invoke(index, CallOp::AfterToolCall, event, context, false, None)
                .await
            {
                tracing::warn!(plugin = %plugin.name, %error, "afterToolCall 失败，已忽略");
            }
        }
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
        let plugin = &self.plugins[index];
        if let Some(failure) = plugin.failure.lock().as_ref() {
            return Err(failure.clone());
        }
        let (host_tx, mut host_rx) = match &bridge {
            Some(_) => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                (Some(tx), Some(rx))
            }
            None => (None, None),
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
                context: context.json(),
                tools_allowed,
                deadline: std::time::Instant::now() + self.call_timeout,
                host_tx,
                reply,
            })?;
        }
        let deadline = tokio::time::Instant::now() + self.call_timeout;
        tokio::pin!(receiver);
        loop {
            tokio::select! {
                biased;
                result = &mut receiver => {
                    return match result {
                        Ok(result) => result,
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
                    if let Some(worker) = plugin.worker.lock().as_ref() {
                        worker.interrupt();
                    }
                    return Err(self.fail(
                        index,
                        PluginError::new(
                            "E_PLUGIN_TIMEOUT",
                            format!(
                                "插件 `{}` 超过 {} ms 未返回；本进程内不再调用",
                                plugin.name,
                                self.call_timeout.as_millis()
                            ),
                        ),
                    ));
                }
                request = next_request(&mut host_rx) => {
                    let Some(request) = request else {
                        // 桥已关闭：不再轮询，等最终结果。
                        host_rx = None;
                        continue;
                    };
                    let Some(bridge) = bridge.clone() else {
                        continue;
                    };
                    let result = bridge
                        .call_tool(request.name, request.args)
                        .await
                        .map(|result| {
                            serde_json::json!({
                                "content": result.content,
                                "structuredContent": result.structured,
                                "isError": result.is_error,
                            })
                        });
                    let _ = request.reply.send(result);
                }
            }
        }
    }

    /// 记录运行期失败：禁用插件并丢掉线程句柄，让它的 Runtime 随线程退出而释放。
    fn fail(&self, index: usize, error: PluginError) -> PluginError {
        let plugin = &self.plugins[index];
        *plugin.failure.lock() = Some(error.clone());
        plugin.worker.lock().take();
        error
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

    #[tokio::test]
    async fn loads_a_project_plugin_and_exposes_every_surface() {
        let temp = tempfile::tempdir().unwrap();
        install(
            temp.path(),
            "demo",
            r#"
            import { kv } from "moc";
            export default {
              tools: { hello: { description: "打招呼", parameters: { type: "object" } , run: () => "hi" } },
              commands: { demo: { description: "示例命令", usage: "[x]", run: args => `arg=${args}` } },
              hooks: { beforeToolCall: () => null },
              prompt: () => "demo prompt"
            };
            "#,
        );
        let host = PluginHost::start(temp.path(), temp.path(), &settings()).await;
        assert_eq!(host.info().len(), 1);
        assert_eq!(host.info()[0].error, None);
        assert_eq!(host.tools().len(), 1);
        assert_eq!(host.commands()[0].1.name, "demo");
        assert!(host.owns_tool("hello"));

        let result = host
            .call_tool("hello", Value::Null, &context(), Arc::new(EchoBridge))
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
        let host = PluginHost::start(temp.path(), temp.path(), &settings()).await;
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
        let host = PluginHost::start(temp.path(), temp.path(), &settings()).await;
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
        let host = PluginHost::start(temp.path(), temp.path(), &settings).await;
        assert_eq!(host.prompt_text(&context()).await, "x".repeat(10));
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
        let host = PluginHost::start(temp.path(), temp.path(), &settings).await;
        let error = host
            .call_tool("spin", Value::Null, &context(), Arc::new(EchoBridge))
            .await
            .expect_err("必须超时");
        assert_eq!(error.code, "E_PLUGIN_TIMEOUT");
        // 之后立即失败，不再等一轮超时。
        let again = host
            .call_tool("spin", Value::Null, &context(), Arc::new(EchoBridge))
            .await
            .expect_err("已禁用");
        assert_eq!(again.code, "E_PLUGIN_TIMEOUT");
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
            .call_tool("steady", Value::Null, &context(), Arc::new(EchoBridge))
            .await
            .expect("另一个插件仍可用");
        assert_eq!(result.content, "ok");
    }

    #[tokio::test]
    async fn broken_plugin_is_listed_with_its_error() {
        let temp = tempfile::tempdir().unwrap();
        install(temp.path(), "broken", "export default { nope: 1 };");
        let host = PluginHost::start(temp.path(), temp.path(), &settings()).await;
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
        let host = PluginHost::start(temp.path(), temp.path(), &settings).await;
        assert!(host.is_empty());
    }
}

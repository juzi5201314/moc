use super::CallContext;
use super::HostBridge;
use super::PersistenceOp;
use super::PluginHost;
use super::ToolResult;
use super::bridge::host_reply;
use super::bridge::next_request;
use super::bridge::request_persistence;
use super::results::interpret_tool_result;
use crate::error::PluginError;
use crate::error::Result;
use crate::runtime::CallOp;
use crate::runtime::CallRequest;
use crate::runtime::HostOperation;
use crate::runtime::HostRequest;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

impl PluginHost {
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

    /// 串行调用一个插件；超时或线程退出后本进程内禁用该插件。
    pub(super) async fn invoke(
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

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn invoke_context(
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
    pub(super) fn fail(&self, index: usize, error: PluginError) -> PluginError {
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

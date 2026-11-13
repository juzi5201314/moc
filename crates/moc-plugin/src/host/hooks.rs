use super::AfterToolCall;
use super::BeforeToolCall;
use super::CallContext;
use super::InjectedMessage;
use super::MAX_INJECTED_CHARS;
use super::MessageRewrite;
use super::PluginHost;
use super::ToolResult;
use super::results::collect_injected;
use super::results::prompt_text_of;
use crate::descriptor::HookKind;
use crate::runtime::CallOp;
use serde_json::Value;

impl PluginHost {
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
}

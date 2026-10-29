//! agent preset：一套固定的系统提示片段 + 工具面策略，spawn 时按名选用。
//!
//! preset 只能**收缩**子代理的工具面，不能放大：引擎、模式、配置与权限检查
//! 原样生效。内置 preset 与插件注册的 preset 走同一张表，但模型只能按名
//! 选用，永远看不到注册接口本身。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::Config(format!("INVALID_PARAMS: {}", message.into()))
}

/// 子代理的工具面策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// 白名单：仅列出的工具可用。`yield` 由注册方保证在列，交付通道不可剥夺。
    Allow(BTreeSet<String>),
    /// 拒绝列表：除列出外全部可用（Claude Code Explore 的模型）。
    Deny(BTreeSet<String>),
}

impl ToolPolicy {
    /// 该策略下 `name` 工具是否可用。
    pub fn permits(&self, name: &str) -> bool {
        match self {
            Self::Allow(names) => names.contains(name),
            Self::Deny(names) => !names.contains(name),
        }
    }

    /// Allow 策略必须保留 `yield`，否则子代理没有交付通道。
    fn ensure_deliverable(&mut self) {
        if let Self::Allow(names) = self {
            names.insert("yield".to_owned());
        }
    }
}

/// 一个 agent preset 的完整定义。
#[derive(Debug, Clone)]
pub struct AgentPreset {
    /// 稳定标识：spawn 参数与持久化都用它。
    pub name: String,
    /// 注入 `<subagent>` 块的系统提示片段（模型可见文本，用英文写）。
    pub system_prompt: String,
    pub tools: ToolPolicy,
}

/// preset 系统提示的长度上限；与插件提示词预算同量级。
pub const MAX_PRESET_PROMPT_CHARS: usize = 4000;

/// preset 名：小写字母/数字/连字符，小写字母开头（与插件名同一形状）。
pub fn valid_preset_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// 已知 preset 的注册表：内置 preset 启动时装入，代码级注册入口预留给
/// 未来的动态来源（如插件桥）；重名一律拒绝，先到者胜。
#[derive(Debug, Default)]
pub struct PresetRegistry {
    presets: BTreeMap<String, AgentPreset>,
}

impl PresetRegistry {
    /// 装入全部内置 preset 的注册表。
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry
            .register(builtin_explore())
            .expect("内置 preset 名不冲突");
        registry
    }

    /// 注册一个 preset；重名或非法定义返回错误。
    ///
    /// 这是代码级动态注册的预留接口：调用方负责来源校验（内置 / 插件声明），
    /// 模型侧永远看不到它。
    pub fn register(&mut self, preset: AgentPreset) -> Result<(), CoreError> {
        if !valid_preset_name(&preset.name) {
            return Err(invalid(format!(
                "agent preset 名 `{}` 非法：须为小写字母开头的小写字母/数字/连字符",
                preset.name
            )));
        }
        let prompt = preset.system_prompt.trim();
        if prompt.is_empty() {
            return Err(invalid(format!(
                "agent preset `{}` 的 system_prompt 不能为空",
                preset.name
            )));
        }
        if prompt.chars().count() > MAX_PRESET_PROMPT_CHARS {
            return Err(invalid(format!(
                "agent preset `{}` 的 system_prompt 超过 {} 字符上限",
                preset.name, MAX_PRESET_PROMPT_CHARS
            )));
        }
        if self.presets.contains_key(&preset.name) {
            return Err(invalid(format!("agent preset `{}` 已注册", preset.name)));
        }
        self.presets.insert(preset.name.clone(), preset);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&AgentPreset> {
        self.presets.get(name)
    }

    /// 全部已注册 preset 名，按字典序；供错误提示列出可选值。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.presets.keys().map(String::as_str)
    }
}

/// 内置 `explore`：只读侦察子代理，对齐 Claude Code Explore 的拒绝列表模型。
///
/// 禁项逐项对应 CC 的 `disallowedTools`：`edit`/`write`（含 `clm_context` 这类
/// 写会话上下文的）↔ Edit/Write/NotebookEdit，`spawn_agent` ↔ Agent，
/// `stop_agent`/`wait_agent` 随之而去（explore 恒为叶节点），`write_plan`/
/// `exit_plan` ↔ ExitPlanMode。`ask` 排除：子代理的提问应经 `message_agent`
/// 路由给父代理，直接弹用户违反委托契约。
fn builtin_explore() -> AgentPreset {
    let denied: BTreeSet<String> = [
        "edit",
        "write",
        "spawn_agent",
        "stop_agent",
        "wait_agent",
        "write_plan",
        "exit_plan",
        "clm_context",
        "ask",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    AgentPreset {
        name: "explore".to_owned(),
        system_prompt: "\
Preset: explore — read-only reconnaissance agent.
- Investigate and report; never modify files, run mutating commands, or change system state.
- The parent cannot see your transcript, tool calls, or reasoning: everything it needs must be in the `yield` report.
- Cite concrete evidence for every finding: `path:line` references, symbol names, the commands you ran.
- Cover the task scope thoroughly; if something is ambiguous or missing, say so plainly instead of guessing."
            .to_owned(),
        tools: ToolPolicy::Deny(denied),
    }
}

/// 校验并规整一个 preset 定义（插件声明与代码注册共用）。
///
/// `yield` 会被补进 Allow 白名单：交付通道是基础设施，不是能力授权。
pub fn sanitize_preset(mut preset: AgentPreset) -> AgentPreset {
    preset.tools.ensure_deliverable();
    preset
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(name: &str, policy: ToolPolicy) -> AgentPreset {
        AgentPreset {
            name: name.to_owned(),
            system_prompt: "test prompt".to_owned(),
            tools: policy,
        }
    }

    #[test]
    fn registry_rejects_duplicates_and_bad_names() {
        let mut registry = PresetRegistry::with_builtins();
        assert!(registry.get("explore").is_some());
        assert!(
            registry
                .register(preset("explore", ToolPolicy::Deny(BTreeSet::new())))
                .is_err(),
            "与内置名冲突必须拒绝"
        );
        for bad in ["", "UPPER", "-lead", "with space", "with_underscore"] {
            assert!(
                registry
                    .register(preset(bad, ToolPolicy::Deny(BTreeSet::new())))
                    .is_err(),
                "非法名 {bad:?} 必须拒绝"
            );
        }
        assert!(
            registry
                .register(preset("scout", ToolPolicy::Deny(BTreeSet::new())))
                .is_ok()
        );
        assert_eq!(
            registry.names().collect::<Vec<_>>(),
            vec!["explore", "scout"]
        );
    }

    #[test]
    fn policy_permits_and_deliverable_allowlist() {
        let deny = ToolPolicy::Deny(["edit".to_owned()].into_iter().collect());
        assert!(deny.permits("read"));
        assert!(!deny.permits("edit"));

        let mut allow = ToolPolicy::Allow(["read".to_owned()].into_iter().collect());
        allow.ensure_deliverable();
        assert!(allow.permits("read"));
        assert!(allow.permits("yield"), "Allow 必须保住交付通道");
        assert!(!allow.permits("edit"));
    }

    #[test]
    fn policy_serializes_as_externally_tagged_map() {
        let deny = ToolPolicy::Deny(["edit".to_owned()].into_iter().collect());
        assert_eq!(
            serde_json::to_value(&deny).expect("可序列化"),
            serde_json::json!({"deny": ["edit"]})
        );
        let allow = ToolPolicy::Allow(["read".to_owned()].into_iter().collect());
        assert_eq!(
            serde_json::to_value(&allow).expect("可序列化"),
            serde_json::json!({"allow": ["read"]})
        );
    }
}

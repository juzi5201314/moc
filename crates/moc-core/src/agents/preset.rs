//! 固定角色定义：用途说明、模型、系统提示与工具策略；普通 spawn 只能按 role 选用。
//!
//! 角色只能收缩工具面，引擎、模式、配置与权限检查原样生效。用户插件在加载时
//! 注册 custom；内置 Explorer 与 custom 经角色目录统一筛选，Fusion 使用专用入口。
//! 模型没有注册角色或覆盖角色定义的接口。

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
    /// 拒绝列表：仅排除列出的工具，其余仍受引擎、模式与会话权限约束。
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
    pub(super) fn ensure_deliverable(&mut self) {
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
    /// 主代理角色目录中的用途说明，不向父代理暴露角色系统提示。
    pub description: String,
    /// 角色固定模型；inherit 沿用父模型，none 禁用，不能由 spawn 覆盖。
    pub model: String,
    /// 注入 `<subagent>` 块的系统提示片段（模型可见文本，用英文写）。
    pub system_prompt: String,
    pub tools: ToolPolicy,
}

/// preset 系统提示的长度上限；与插件提示词预算同量级。
pub const MAX_PRESET_PROMPT_CHARS: usize = 4000;

/// Fusion 会话执行侧的 preset 名；只能经 `sidekick` 工具的内部 spawn 路径创建。
pub const SIDEKICK_PRESET: &str = "sidekick";
pub const EXPLORER_ROLE: &str = "explorer";
pub const EXPLORER_DESCRIPTION: &str = "Read-only codebase exploration: locate code, trace behavior, and report evidence. No workspace writes.";

/// sidekick `yield` 的结构化交付契约：spawn 时随 `output_schema` 注入。
///
/// 已存在的旧 sidekick 会话没有这条契约，照旧允许纯文本 `yield`。
pub(crate) fn sidekick_report_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["status", "changes", "verification", "open_issues"],
        "properties": {
            "status": {"type": "string", "enum": ["done", "partial", "blocked", "needs_decision"]},
            "changes": {"type": "array", "items": {"type": "object", "additionalProperties": false, "required": ["path", "summary"], "properties": {"path": {"type": "string"}, "summary": {"type": "string"}}}},
            "verification": {"type": "array", "items": {"type": "object", "additionalProperties": false, "required": ["command", "outcome"], "properties": {"command": {"type": "string"}, "outcome": {"type": "string", "enum": ["passed", "failed", "not_run"]}, "detail": {"type": "string"}}}},
            "artifacts": {"type": "array", "items": {"type": "string"}},
            "open_issues": {"type": "array", "items": {"type": "string"}}
        }
    })
}

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
            .register(builtin_sidekick())
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
}

/// Explorer 沿用拒绝列表，保留 shell、MCP 和插件工具用于调查。
fn builtin_explore() -> AgentPreset {
    AgentPreset {
        name: EXPLORER_ROLE.to_owned(),
        description: EXPLORER_DESCRIPTION.to_owned(),
        model: "inherit".to_owned(),
        system_prompt: "\
You are Explorer, a read-only codebase investigator. Do not perform any workspace writes. Report findings with path:line evidence and note uncertainties."
            .to_owned(),
        tools: ToolPolicy::Deny(
            ["edit", "write", "spawn_agent", "stop_agent", "wait_agent", "plan", "exit_plan", "clm_context", "ask"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ),
    }
}

/// 内置 `sidekick`：Fusion 会话的执行侧。lead 是负责规划与对话的前沿模型，
/// sidekick 在其指挥下做实现、环境、构建与验证；跨 handoff 复用同一会话。
///
/// 禁项对应它的协作契约：不直接问用户（`ask`）、不做计划交接（`plan`/`exit_plan`）、
/// 不再派发或控制代理（`spawn_agent`/`stop_agent`/`wait_agent`），也不能冒用
/// `sidekick` 通道再套一层。
fn builtin_sidekick() -> AgentPreset {
    let denied: BTreeSet<String> = [
        "ask",
        "plan",
        "exit_plan",
        "spawn_agent",
        "stop_agent",
        "wait_agent",
        "sidekick",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    AgentPreset {
        name: SIDEKICK_PRESET.to_owned(),
        description: String::new(),
        model: "inherit".to_owned(),
        system_prompt: "\
Preset: sidekick — the execution partner of a Fusion lead.
- The lead is a frontier model that owns the task: it plans, makes the design decisions, talks to the user, and reviews your work. You execute its handoffs: implement changes, set up or repair the environment, run builds, linters, type-checks and tests, and verify results.
- Follow the handoff as written. Do not redesign, widen the scope, or choose among alternatives it left open. If the brief is ambiguous, contradicts the code, or would cause harm, stop and say so in your report; use `message_agent` to the parent only when you cannot proceed without an answer.
- You persist across handoffs: later handoffs arrive as <agent-message> and build on what you already did. Reuse what you learned and the processes you started; leave long-running processes running unless told otherwise.
- A new message from the lead while you work is an interrupt: fold it in at once; where it conflicts with the earlier brief, it wins.
- Run every verification command the lead names; that list is mandatory. If something fails for reasons outside the task, report it instead of working around it. Never weaken tests, checks, or security settings to make them pass.
- Never commit, push, or take other actions with external side effects unless the handoff explicitly asks for them.
- End every handoff with `yield`. Put a concise narrative in `report`: what you did, deviations from the brief, and what is unverified. Fill `result` per the declared schema: `status` (done / partial / blocked / needs_decision), `changes` (every file you changed), `verification` (each command you ran or owe, with its outcome), `artifacts` (paths to logs, screenshots or other kept output), and `open_issues` (anything unfinished or needing the lead's decision)."
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
            description: "测试角色".to_owned(),
            model: "inherit".to_owned(),
            system_prompt: "test prompt".to_owned(),
            tools: policy,
        }
    }

    #[test]
    fn registry_rejects_duplicates_and_bad_names() {
        let mut registry = PresetRegistry::with_builtins();
        assert!(registry.get(EXPLORER_ROLE).is_some());
        assert!(
            registry
                .register(preset(EXPLORER_ROLE, ToolPolicy::Deny(BTreeSet::new())))
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

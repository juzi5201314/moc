//! 插件模块 `default` 导出的形状。
//!
//! 声明式：一次读取全部注册项（工具、命令、钩子、提示词贡献），之后宿主只按名字调用，
//! 不在运行时重新询问插件注册了什么。

use serde_json::Value;

/// 插件工具可见的会话模式集合。
///
/// 固定三个标志，用 `u8` 位图即可，不为每个工具分配 `Vec`。`PTC` 不在集合里：
/// 该模式下模型只能看到 `run_code`，插件工具也会被宿主以 `E_PLUGIN_RECURSION` 拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeMask(u8);

impl ModeMask {
    pub const NORMAL: Self = Self(1 << 0);
    pub const ASK: Self = Self(1 << 1);
    pub const PLAN: Self = Self(1 << 2);
    /// 省略 `modes` 时的默认值：三种模式都可见。
    pub const ALL: Self = Self(Self::NORMAL.0 | Self::ASK.0 | Self::PLAN.0);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "normal" => Some(Self::NORMAL),
            "ask" => Some(Self::ASK),
            "plan" => Some(Self::PLAN),
            _ => None,
        }
    }

    /// 按固定顺序给出模式名，供 `plugin/list` 与前端展示。
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        [
            (Self::NORMAL, "normal"),
            (Self::ASK, "ask"),
            (Self::PLAN, "plan"),
        ]
        .into_iter()
        .filter(move |(flag, _)| self.contains(*flag))
        .map(|(_, name)| name)
    }
}

/// 插件配置字段的基础类型；与设置 UI 的字段类型一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    Bool,
    Integer,
    Number,
    Text,
    Secret,
    Multiline,
    Strings,
    Object,
    Choice,
}

/// 插件在模块顶层注册的 agent preset 声明。
///
/// `allow` 与 `deny` 互斥：allow 是白名单（`yield` 由宿主保留），deny 是拒绝
/// 列表；都省略时 preset 只提供人格提示，工具面与父代理相同。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentPresetDecl {
    pub name: String,
    pub system_prompt: String,
    pub allow: Option<Vec<String>>,
    pub deny: Option<Vec<String>>,
}

/// 一个插件配置字段的静态声明。
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDecl {
    pub key: String,
    pub kind: ConfigKind,
    pub label: String,
    pub description: String,
    pub default: Option<Value>,
    pub choices: Vec<String>,
}

impl ConfigDecl {
    pub fn validate(&self, value: &Value) -> Result<(), String> {
        let valid = match self.kind {
            ConfigKind::Bool => value.is_boolean(),
            ConfigKind::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            ConfigKind::Number => value.is_number(),
            ConfigKind::Text | ConfigKind::Secret | ConfigKind::Multiline => value.is_string(),
            ConfigKind::Strings => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
            ConfigKind::Object => value.is_object(),
            ConfigKind::Choice => value
                .as_str()
                .is_some_and(|value| self.choices.iter().any(|choice| choice == value)),
        };
        fn toml_value(value: &Value) -> bool {
            match value {
                Value::Null => false,
                Value::Number(number) => {
                    number.as_i64().is_some()
                        || (!number.is_u64() && number.as_f64().is_some_and(f64::is_finite))
                }
                Value::Array(items) => items.iter().all(toml_value),
                Value::Object(object) => object.values().all(toml_value),
                _ => true,
            }
        }
        if valid && toml_value(value) {
            Ok(())
        } else {
            Err(format!(
                "配置 `{}` 的值类型、取值或 TOML 表示无效",
                self.key
            ))
        }
    }
}

/// 一个插件工具的静态声明。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    /// JSON Schema（`type: "object"`）。
    pub parameters: Value,
    /// 工具可见的会话模式；默认 [`ModeMask::ALL`]。
    pub modes: ModeMask,
    /// 是否可与其它只读工具并行；默认 `false`，作为批次屏障。
    pub parallel: bool,
}

/// 一个斜杠命令的静态声明。
#[derive(Debug, Clone, PartialEq)]
pub struct CommandDecl {
    pub name: String,
    pub description: String,
    /// 用法示例（前端提示用），可为空串。
    pub usage: String,
}

/// 支持的生命周期钩子。
///
/// 顺序只影响 [`HookKind::ALL`] 的展示顺序与描述符校验；实际调用顺序始终是插件
/// 加载顺序。会消费返回值的只有 `beforeToolCall`、`beforePrompt` 与 `afterToolCall`，
/// 其余钩子的返回值被忽略（见 `docs/plugins.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookKind {
    BeforeToolCall,
    AfterToolCall,
    BeforePrompt,
    AfterPrompt,
    SessionStart,
    SessionEnd,
    SessionConfigChanged,
    MessageReceived,
    BeforeModelCall,
    AfterModelCall,
    BeforeCompaction,
    AfterCompaction,
    PermissionRequested,
    PermissionResolved,
    WorkspaceChanged,
    PlanDecided,
    AgentStarted,
    AgentFinished,
    JobFinished,
    UserCommand,
}

impl HookKind {
    /// 全部钩子；描述符校验、错误信息与文档都以它为准。
    pub const ALL: [Self; 20] = [
        Self::BeforeToolCall,
        Self::AfterToolCall,
        Self::BeforePrompt,
        Self::AfterPrompt,
        Self::SessionStart,
        Self::SessionEnd,
        Self::SessionConfigChanged,
        Self::MessageReceived,
        Self::BeforeModelCall,
        Self::AfterModelCall,
        Self::BeforeCompaction,
        Self::AfterCompaction,
        Self::PermissionRequested,
        Self::PermissionResolved,
        Self::WorkspaceChanged,
        Self::PlanDecided,
        Self::AgentStarted,
        Self::AgentFinished,
        Self::JobFinished,
        Self::UserCommand,
    ];

    /// 模块 `hooks` 对象里的键名。
    pub fn js_name(self) -> &'static str {
        match self {
            Self::BeforeToolCall => "beforeToolCall",
            Self::AfterToolCall => "afterToolCall",
            Self::BeforePrompt => "beforePrompt",
            Self::AfterPrompt => "afterPrompt",
            Self::SessionStart => "sessionStart",
            Self::SessionEnd => "sessionEnd",
            Self::SessionConfigChanged => "sessionConfigChanged",
            Self::MessageReceived => "messageReceived",
            Self::BeforeModelCall => "beforeModelCall",
            Self::AfterModelCall => "afterModelCall",
            Self::BeforeCompaction => "beforeCompaction",
            Self::AfterCompaction => "afterCompaction",
            Self::PermissionRequested => "permissionRequested",
            Self::PermissionResolved => "permissionResolved",
            Self::WorkspaceChanged => "workspaceChanged",
            Self::PlanDecided => "planDecided",
            Self::AgentStarted => "agentStarted",
            Self::AgentFinished => "agentFinished",
            Self::JobFinished => "jobFinished",
            Self::UserCommand => "userCommand",
        }
    }

    /// 从模块 `hooks` 的键名解析；未知名字返回 `None`。
    pub fn from_js_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.js_name() == name)
    }

    /// 错误信息用的「支持哪些钩子」列表。
    pub fn supported() -> String {
        Self::ALL
            .iter()
            .map(|kind| kind.js_name())
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

/// 模块导出解析结果。名字与版本只做一致性检查，清单才是权威。
#[derive(Debug, Clone, Default)]
pub struct Descriptor {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub tools: Vec<ToolDecl>,
    pub commands: Vec<CommandDecl>,
    pub hooks: Vec<HookKind>,
    pub configs: Vec<ConfigDecl>,
    pub agent_presets: Vec<AgentPresetDecl>,
    /// 是否导出 `prompt`；没有就不占提示词预算。
    pub has_prompt: bool,
}

impl Descriptor {
    pub fn tool(&self, name: &str) -> Option<&ToolDecl> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    pub fn command(&self, name: &str) -> Option<&CommandDecl> {
        self.commands.iter().find(|command| command.name == name)
    }

    pub fn has_hook(&self, kind: HookKind) -> bool {
        self.hooks.contains(&kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_names_round_trip_and_are_unique() {
        let mut names = std::collections::BTreeSet::new();
        for kind in HookKind::ALL {
            assert!(
                names.insert(kind.js_name()),
                "钩子名重复：{}",
                kind.js_name()
            );
            assert_eq!(HookKind::from_js_name(kind.js_name()), Some(kind));
        }
        assert_eq!(
            HookKind::from_js_name("beforeToolCall"),
            Some(HookKind::BeforeToolCall)
        );
        assert_eq!(HookKind::from_js_name("eventLoop"), None);
        assert!(HookKind::supported().contains("userCommand"));
    }

    #[test]
    fn mode_masks_parse_and_render_in_order() {
        assert_eq!(ModeMask::parse("normal"), Some(ModeMask::NORMAL));
        assert_eq!(ModeMask::parse("ask"), Some(ModeMask::ASK));
        assert_eq!(ModeMask::parse("plan"), Some(ModeMask::PLAN));
        assert_eq!(ModeMask::parse("ptc"), None);
        assert_eq!(ModeMask::parse("admin"), None);
        assert_eq!(
            ModeMask::NORMAL
                .union(ModeMask::PLAN)
                .names()
                .collect::<Vec<_>>(),
            vec!["normal", "plan"]
        );
        assert_eq!(
            ModeMask::ALL.names().collect::<Vec<_>>(),
            vec!["normal", "ask", "plan"]
        );
        assert!(ModeMask::ALL.contains(ModeMask::ASK));
        assert!(!ModeMask::NORMAL.contains(ModeMask::ASK));
        assert!(ModeMask::empty().is_empty());
    }
}

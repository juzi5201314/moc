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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookKind {
    BeforeToolCall,
    AfterToolCall,
}

impl HookKind {
    /// 模块 `hooks` 对象里的键名。
    pub fn js_name(self) -> &'static str {
        match self {
            Self::BeforeToolCall => "beforeToolCall",
            Self::AfterToolCall => "afterToolCall",
        }
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

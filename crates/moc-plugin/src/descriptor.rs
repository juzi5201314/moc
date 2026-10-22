//! 插件模块 `default` 导出的形状。
//!
//! 声明式：一次读取全部注册项（工具、命令、钩子、提示词贡献），之后宿主只按名字调用，
//! 不在运行时重新询问插件注册了什么。

use serde_json::Value;

/// 工具的风险等级，与宿主 `tools::Tier` 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTier {
    Read,
    Write,
    Exec,
}

impl ToolTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exec => "exec",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "exec" => Some(Self::Exec),
            _ => None,
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
    pub tier: ToolTier,
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
    fn tier_names_round_trip() {
        for tier in [ToolTier::Read, ToolTier::Write, ToolTier::Exec] {
            assert_eq!(ToolTier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(ToolTier::parse("admin"), None);
    }
}

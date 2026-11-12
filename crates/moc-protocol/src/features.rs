//! 特性开关注册表（Codex 式数据化）：每个特性以一条 `FeatureSpec` 数据声明
//! 生命周期阶段、默认开关、生效的配置路径与上下文 token 成本估计。
//!
//! 注册表是唯一的特性清单：前端用它过滤与展示功能入口（命令门控、引擎菜单、
//! `/features` 一览），core 用它把 `[features]` 覆盖值解析成各功能的实际开关。
//! 新增实验特性 = 加一个 `FeatureFlag` 成员和一行 `FEATURES` 记录，而不是散落
//! 各处的 ad-hoc `enabled` 判定。

use serde::{Deserialize, Serialize};

use crate::SessionEngine;

/// 特性开关生命周期（五档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagStage {
    /// 开发中：默认关闭，接口随时会变。
    UnderDevelopment,
    /// 实验：可用但不承诺稳定。
    Experimental,
    /// 公测：功能完整，仍可能按反馈调整。
    Beta,
    /// 稳定：常规功能面。
    Stable,
    /// 弃用：保留但将移除，不推荐启用。
    Deprecated,
}

impl FlagStage {
    /// `/features` 与日志里展示的短名。
    pub fn label(self) -> &'static str {
        match self {
            Self::UnderDevelopment => "dev",
            Self::Experimental => "experimental",
            Self::Beta => "beta",
            Self::Stable => "stable",
            Self::Deprecated => "deprecated",
        }
    }
}

/// 特性开关：moc 中可按配置打开/关闭的功能面。
///
/// 成员序就是 [`FeatureStates`] 位掩码里的位序；新增成员追加在末尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FeatureFlag {
    /// CLM 实验引擎（`SessionEngine::ClmExperimental`）。
    Clm,
    /// PTC 引擎（`SessionEngine::Ptc`）。
    Ptc,
    /// 动态 workflow 编排（`/workflow` 激活的 9 个工具与编排面）。
    Workflow,
    /// 插件宿主（插件工具、斜杠命令、钩子与提示词贡献）。
    Plugins,
    /// MCP 服务器连接与 `mcp__*` 工具桥接。
    Mcp,
    /// skills 目录注入与 `skill` 工具。
    Skills,
    /// `web_search` 工具。
    WebSearch,
    /// `web_fetch` 工具。
    WebFetch,
}

/// 开关的真实来源：注册表自有键，或描述已有的独立配置项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagToggle {
    /// `[features] <key>`：注册表自有开关，`key` 即 `FeatureSpec::key`。
    /// core 校验 `[features]` 下只接受这类键。
    Features,
    /// 已存在的独立配置路径（如 `workflow.enabled`）；flag 只描述不拥有开关，
    /// 开/关仍由对应配置段的 `enabled` 决定。
    Existing(&'static str),
}

/// 一条特性记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureSpec {
    pub flag: FeatureFlag,
    /// 展示键名；`toggle == Features` 时也是 `[features]` 的覆盖键。
    pub key: &'static str,
    pub toggle: FlagToggle,
    pub stage: FlagStage,
    /// 未配置时的默认状态。实验档默认开表示「已对外可见、可显式关闭」，
    /// 默认关表示「仅显式开启后可见」。
    pub default_enabled: bool,
    /// 启用后对每轮上下文的附加成本估计（token，按注入的工具签名/prompt 段
    /// 序列化后估得）。`None` 表示成本随环境动态变化（MCP 工具数、插件提示词、
    /// skills 目录、CLM 实时上下文都不是常量）。
    pub context_tokens: Option<u32>,
    /// 一句话说明（开关控制什么、成本花在哪）。
    pub summary: &'static str,
}

/// 全部特性开关，按 [`FeatureFlag`] 成员序排列。
pub const FEATURES: &[FeatureSpec] = &[
    FeatureSpec {
        flag: FeatureFlag::Clm,
        key: "clm",
        toggle: FlagToggle::Features,
        stage: FlagStage::Experimental,
        default_enabled: true,
        // clm_context 工具声明 + 每轮注入的 CLM 实时上下文块（随会话增长，上限
        // MAX_CONTEXT_BYTES）。
        context_tokens: None,
        summary: "CLM 实验引擎：模型自主维护实时工作上下文",
    },
    FeatureSpec {
        flag: FeatureFlag::Ptc,
        key: "ptc",
        toggle: FlagToggle::Features,
        stage: FlagStage::Beta,
        default_enabled: true,
        // <tool-declarations> 段：全部内置工具的 TS 签名（实测 ≈980 token）。
        context_tokens: Some(1_000),
        summary: "PTC 引擎：模型只调 execute，工具经代码沙盒触达",
    },
    FeatureSpec {
        flag: FeatureFlag::Workflow,
        key: "workflow",
        toggle: FlagToggle::Existing("workflow.enabled"),
        stage: FlagStage::Beta,
        default_enabled: true,
        // 会话激活后才注入：9 个 workflow 工具声明 ≈855 token + workflow-authoring
        // skill 目录行 ≈29 token；未激活时成本为 0（零泄漏）。
        context_tokens: Some(900),
        summary: "动态 workflow：模型以 TypeScript 脚本编排多阶段协作",
    },
    FeatureSpec {
        flag: FeatureFlag::Plugins,
        key: "plugins",
        toggle: FlagToggle::Existing("plugins.enabled"),
        stage: FlagStage::Stable,
        default_enabled: true,
        // 每个插件的提示词贡献之和受 plugins.max_prompt_chars 钳制；工具 schema
        // 随已加载插件数变化。
        context_tokens: None,
        summary: "插件宿主：插件工具、斜杠命令、钩子与提示词贡献",
    },
    FeatureSpec {
        flag: FeatureFlag::Mcp,
        key: "mcp",
        toggle: FlagToggle::Existing("mcp.enabled"),
        stage: FlagStage::Stable,
        default_enabled: true,
        // 每个已连接服务器的工具 schema 与 <mcp-servers> 目录；数量由配置决定。
        context_tokens: None,
        summary: "MCP 服务器：外部工具经 mcp__* 桥接进入工具表",
    },
    FeatureSpec {
        flag: FeatureFlag::Skills,
        key: "skills",
        toggle: FlagToggle::Existing("skills.enabled"),
        stage: FlagStage::Stable,
        default_enabled: true,
        // <skills> 目录按 catalog_budget_* 预算注入；skill 工具本体声明固定。
        context_tokens: None,
        summary: "skills 目录注入与 skill 工具（渐进披露）",
    },
    FeatureSpec {
        flag: FeatureFlag::WebSearch,
        key: "web_search",
        toggle: FlagToggle::Existing("web_search.enabled"),
        stage: FlagStage::Stable,
        default_enabled: true,
        // 工具 schema 实测 ≈130 token（含参数说明）。
        context_tokens: Some(150),
        summary: "web_search 工具（需配置供应商）",
    },
    FeatureSpec {
        flag: FeatureFlag::WebFetch,
        key: "web_fetch",
        toggle: FlagToggle::Existing("web_fetch.enabled"),
        stage: FlagStage::Stable,
        default_enabled: true,
        context_tokens: Some(150),
        summary: "web_fetch 工具",
    },
];

impl FeatureFlag {
    /// 该 flag 的静态记录。
    pub fn spec(self) -> &'static FeatureSpec {
        &FEATURES[self as usize]
    }

    /// 注册表全部成员（与 [`FEATURES`] 顺序一致）。
    pub const ALL: &[FeatureFlag] = &[
        Self::Clm,
        Self::Ptc,
        Self::Workflow,
        Self::Plugins,
        Self::Mcp,
        Self::Skills,
        Self::WebSearch,
        Self::WebFetch,
    ];
}

/// `[features]` 下注册表自有键的清单（core 校验未知键时用）。
pub fn features_owned_keys() -> impl Iterator<Item = &'static str> {
    FEATURES.iter().filter_map(|spec| match spec.toggle {
        FlagToggle::Features => Some(spec.key),
        FlagToggle::Existing(_) => None,
    })
}

/// `/features` 一览文本：阶段、有效状态、上下文成本与生效的配置来源，
/// TUI 与 CLI REPL 共用同一份格式。
pub fn format_features(states: &FeatureStates) -> String {
    use std::fmt::Write;
    let mut text = String::from("特性开关（生效值 = [features] 覆盖或原配置段）：\n");
    for spec in FEATURES {
        let enabled = states.enabled(spec.flag);
        let cost = spec
            .context_tokens
            .map(|tokens| format!("~{tokens} tok"))
            .unwrap_or_else(|| "随环境".to_owned());
        let toggle = match spec.toggle {
            FlagToggle::Features => format!("[features] {}", spec.key),
            FlagToggle::Existing(path) => path.to_owned(),
        };
        let _ = writeln!(
            text,
            "  {:<12} {:<12} {} {:<8} {}（{}）",
            spec.key,
            spec.stage.label(),
            if enabled { "开" } else { "关" },
            cost,
            spec.summary,
            toggle,
        );
    }
    text
}

/// 会话引擎对应的特性开关；`None` 表示该引擎不受 flag 门控。
pub fn flag_for_engine(engine: SessionEngine) -> Option<FeatureFlag> {
    match engine {
        SessionEngine::Standard => None,
        SessionEngine::Ptc => Some(FeatureFlag::Ptc),
        SessionEngine::ClmExperimental => Some(FeatureFlag::Clm),
    }
}

/// 特性开关状态快照：flag 序数即位下标的位掩码。
///
/// 位掩码而非 `HashMap`/`Vec<bool>`：flag 数有界（<64），一字足以放下且零分配。
/// 线上序列化成 `key -> bool` 表而不是位掩码：键名自描述，未来新增 flag 不会让
/// 旧前端把位序读错。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeatureStates(u64);

impl Serialize for FeatureStates {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(FEATURES.len()))?;
        for spec in FEATURES {
            map.serialize_entry(spec.key, &self.enabled(spec.flag))?;
        }
        map.end()
    }
}

// 线上是 `key -> bool` 表：schema 直接复用 map 形状，不单独建 $def。
impl schemars::JsonSchema for FeatureStates {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FeatureStates".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        std::collections::BTreeMap::<String, bool>::json_schema(generator)
    }
    fn inline_schema() -> bool {
        true
    }
}

impl<'de> Deserialize<'de> for FeatureStates {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // 未知键不报错：旧前端读到新 flag 直接忽略，默认状态见 `defaults()`。
        let table = std::collections::BTreeMap::<String, bool>::deserialize(deserializer)?;
        let mut states = Self::defaults();
        for (key, enabled) in table {
            if let Some(spec) = FEATURES.iter().find(|spec| spec.key == key) {
                states.set(spec.flag, enabled);
            }
        }
        Ok(states)
    }
}

impl FeatureStates {
    /// 全量状态：对每个 flag 调 `enabled` 取值。
    pub fn resolve(mut enabled: impl FnMut(FeatureFlag) -> bool) -> Self {
        let mut states = Self::default();
        for &flag in FeatureFlag::ALL {
            states.set(flag, enabled(flag));
        }
        states
    }

    /// 未上报任何状态时按注册表默认处理。
    pub fn defaults() -> Self {
        Self::resolve(|flag| flag.spec().default_enabled)
    }

    pub fn enabled(&self, flag: FeatureFlag) -> bool {
        self.0 & (1u64 << flag as u8) != 0
    }

    pub fn set(&mut self, flag: FeatureFlag, enabled: bool) {
        if enabled {
            self.0 |= 1u64 << flag as u8;
        } else {
            self.0 &= !(1u64 << flag as u8);
        }
    }

    /// `set_engine`/菜单的引擎过滤：引擎对应 flag 关闭时不可用。
    pub fn engine_available(&self, engine: SessionEngine) -> bool {
        flag_for_engine(engine).is_none_or(|flag| self.enabled(flag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_flag_has_a_spec_in_order() {
        for (index, spec) in FEATURES.iter().enumerate() {
            assert_eq!(
                spec.flag as usize, index,
                "FEATURES 必须与 FeatureFlag 同序"
            );
            assert_eq!(spec.flag.spec().key, spec.key);
        }
        assert_eq!(FEATURES.len(), FeatureFlag::ALL.len());
    }

    #[test]
    fn features_owned_keys_are_declared() {
        let keys: Vec<_> = features_owned_keys().collect();
        assert_eq!(keys, ["clm", "ptc"], "自有键只有 clm/ptc，新增时更新此断言");
        for key in keys {
            assert!(
                FEATURES
                    .iter()
                    .any(|spec| spec.key == key && spec.toggle == FlagToggle::Features)
            );
        }
    }

    #[test]
    fn states_mask_round_trip() {
        let mut states = FeatureStates::defaults();
        assert!(states.enabled(FeatureFlag::Clm));
        states.set(FeatureFlag::Clm, false);
        assert!(!states.enabled(FeatureFlag::Clm));
        assert!(!states.engine_available(SessionEngine::ClmExperimental));
        assert!(states.engine_available(SessionEngine::Standard));
        states.set(FeatureFlag::Ptc, false);
        assert!(!states.engine_available(SessionEngine::Ptc));
    }
}

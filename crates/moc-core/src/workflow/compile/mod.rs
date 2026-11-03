//! Workflow 脚本编译器：TS 解析 + 静态诊断 + WorkflowPlan + 降级 JS。
//!
//! 不做全量类型检查（Rust 生态无 tsc）；诊断集合与降级规则见 spec §1。
//! 降级策略：类型语法「空白化」（非换行字符替换为空格）+ facade 调用点
//! 插入 `__wf.` 前缀与 site_id 实参。两者都不增删行，产物行号与源 1:1。

mod analyze;
mod lower;
mod schema;

// artifact id 校验：编译期分析与运行期（driver/service）共用。
pub(crate) use analyze::valid_artifact_id;

use serde::Serialize;
use serde_json::Value;
use std::fmt;

/// 一条静态诊断：`line`/`col` 是 1 起始的源码位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub line: u32,
    pub col: u32,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L{}:C{} [{}] {}", self.line, self.col, self.code, self.message)
    }
}

// —— 诊断码（spec §1；语义对齐 zcode，编号自定）——
pub const SYNTAX_ERROR: &str = "SYNTAX_ERROR";
/// `import`/`export`/`declare`/`with` 等脚本体外禁止的语法。
pub const FORBIDDEN_SYNTAX: &str = "FORBIDDEN_SYNTAX";
/// 引用非 ES2022 / 非 facade 全局（process、fetch、require、Date.now、Math.random、
/// 无参 `new Date()` 等）或声明 `__wf`。
pub const FORBIDDEN_GLOBAL: &str = "FORBIDDEN_GLOBAL";
/// 不可空白擦除的 TS 语法：enum/namespace/decorator/参数属性。
pub const TS_UNSUPPORTED: &str = "TS_UNSUPPORTED";
pub const DUP_AGENT_NAME: &str = "DUP_AGENT_NAME";
pub const FANOUT_LITERAL_NAME: &str = "FANOUT_LITERAL_NAME";
pub const NON_LITERAL_COMMAND: &str = "NON_LITERAL_COMMAND";
pub const NON_LITERAL_MODEL: &str = "NON_LITERAL_MODEL";
pub const UNKNOWN_MODEL: &str = "UNKNOWN_MODEL";
pub const PHASE_REQUIRED: &str = "PHASE_REQUIRED";
pub const PHASE_EMPTY: &str = "PHASE_EMPTY";
/// phase/hole 名必须非空字面量且 ≤128 字符。
pub const NON_LITERAL_NAME: &str = "NON_LITERAL_NAME";
pub const HOLE_NAME_DUP: &str = "HOLE_NAME_DUP";
pub const SCHEMA_UNREPRESENTABLE: &str = "SCHEMA_UNREPRESENTABLE";
pub const INDIRECT_FACADE: &str = "INDIRECT_FACADE";
pub const ARTIFACT_ID: &str = "ARTIFACT_ID";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SiteKind {
    Actor,
    Ask,
    Model,
    Phase,
    Hole,
    Log,
    Report,
    ArtifactDecl,
    ArtifactPublish,
    WorldRead,
    WorldRun,
    Channel,
    Future,
}

impl SiteKind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Actor => "actor",
            Self::Ask => "ask",
            Self::Model => "model",
            Self::Phase => "phase",
            Self::Hole => "hole",
            Self::Log => "log",
            Self::Report => "report",
            Self::ArtifactDecl | Self::ArtifactPublish => "artifact",
            Self::WorldRead => "world-read",
            Self::WorldRun => "world-run",
            Self::Channel => "channel",
            Self::Future => "future",
        }
    }
}

/// 一个 facade 调用点。
#[derive(Debug, Clone, Serialize)]
pub struct Site {
    /// `<kind>#<n>`；hole-fill 等内嵌编译可加前缀（`site_prefix`）。
    pub site_id: String,
    pub kind: SiteKind,
    /// 覆盖该点的 phase 名；未被任何 phase 覆盖为 `null`（ask/world.run 同时报 PHASE_REQUIRED）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub line: u32,
    pub col: u32,
    /// 人类可读标签（ask 用 actor 名，world.run 用命令，phase/hole 用名字）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    Phase,
    Hole,
}

/// 图节点：phase 按名合并为一个节点；hole 也算一个节点。
#[derive(Debug, Clone, Serialize)]
pub struct PhaseNode {
    /// phase 用名字（合并后唯一）；hole 用其 site_id。
    pub id: String,
    pub name: String,
    pub kind: PhaseKind,
    pub sites: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// 有值依赖（def-use 可追踪到产出站点）。
    Data,
    /// 无法追踪时的保守顺序边。
    Order,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelSource {
    /// persona.model 缺省或继承。
    Inherit,
    /// 字面量 model id。
    Literal { model: String },
    /// `model(...)` ModelRef 列表（引用 `model_refs` 的 ref_id）。
    Routed { refs: Vec<String> },
}

/// 每个 `agent()` 调用点一个 role。
#[derive(Debug, Clone, Serialize)]
pub struct Role {
    /// = agent 站点的 site_id。
    pub role_id: String,
    /// 字面量名或模板形（如 `reviewer-${i}` 的源码形）；匿名/computed 为 null。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_pattern: Option<String>,
    /// 位于 fan-out 位置（循环体内，或经本地函数由循环内调用）。
    pub fan_out: bool,
    /// persona 系统提示前 200 字符。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona_excerpt: Option<String>,
    pub model_source: ModelSource,
    /// receiver 可解析回本 agent() 站点的 ask 站点。
    pub ask_sites: Vec<String>,
}

/// 每个 `model("x")` 声明。
#[derive(Debug, Clone, Serialize)]
pub struct ModelRef {
    /// = model() 站点的 site_id。
    pub ref_id: String,
    pub model_id: String,
}

/// `world.run` 命令（字面量）去重集合。
#[derive(Debug, Clone, Serialize)]
pub struct CommandSite {
    pub command: String,
    pub sites: Vec<String>,
}

/// 编译期分析产物；序列化给审批窗与 journal。
#[derive(Debug, Clone, Default, Serialize)]
pub struct WorkflowPlan {
    pub sites: Vec<Site>,
    pub phases: Vec<PhaseNode>,
    pub edges: Vec<PlanEdge>,
    pub roles: Vec<Role>,
    pub model_refs: Vec<ModelRef>,
    pub commands: Vec<CommandSite>,
    /// ask 站点 site_id → JSON Schema（`ask<T>` 的 T 合成；无 T 时为 string）。
    pub schemas: Vec<(String, Value)>,
}

/// 编译成功产物。
#[derive(Debug)]
pub struct CompiledWorkflow {
    /// 降级后的 JS 函数体（`(async (__wf) => { <body> })` 形态可运行）。
    pub lowered: String,
    pub plan: WorkflowPlan,
}

/// 编译输入的可调面。
#[derive(Default)]
pub struct CompileOptions<'a> {
    /// hole-fill 等内嵌编译的 site_id 前缀（如 `hole@h1/`）。
    pub site_prefix: Option<String>,
    /// 模型目录解析器；提供时启用 UNKNOWN_MODEL（SaveWorkflow 不提供）。
    pub model_exists: Option<&'a dyn Fn(&str) -> bool>,
}

/// 编译入口：脚本体为 async 函数体（允许顶层 await 与 `return`；禁 import/export/declare）。
///
/// 有诊断即失败（整条诊断作为工具错误回给模型）；无诊断才产出降级体与 plan。
pub fn compile(
    source: &str,
    options: &CompileOptions<'_>,
) -> std::result::Result<CompiledWorkflow, Vec<Diagnostic>> {
    analyze::compile_inner(source, options)
}

// —— 编译期共享小工具 ——

/// 1 起始的 (行, 列)，列按 UTF-16 code unit 计（对齐编辑器/ts 习惯）。
pub(crate) fn line_col(source: &str, offset: u32) -> (u32, u32) {
    let mut line = 1u32;
    let mut line_start = 0usize;
    let offset = offset as usize;
    for (i, b) in source.bytes().enumerate() {
        if i >= offset {
            break;
        }
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    let col = source[line_start..offset.min(source.len())]
        .encode_utf16()
        .count() as u32
        + 1;
    (line, col)
}

#[cfg(test)]
mod tests;

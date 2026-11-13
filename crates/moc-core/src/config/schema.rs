//! 配置文件的结构化 schema 与项目默认值。

use std::collections::BTreeMap;

use serde::Deserialize;

use super::model::ModelCatalog;
use crate::error::CoreError;
use moc_protocol::SessionPermission;

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ConfigDocument {
    pub models: ModelCatalog,
    pub providers: BTreeMap<String, super::model::ProviderDocument>,
    pub tools: ToolsConfig,
    pub sandbox: SandboxConfig,
    pub compaction: CompactionConfig,
    pub limits: LimitsConfig,
    pub system_prompt: SystemPromptConfig,
    pub context_files: ContextFilesConfig,
    pub workspace: WorkspaceConfig,
    pub session: SessionDocument,
    pub mcp: moc_mcp::McpDocument,
    pub skills: SkillsConfig,
    pub plugins: PluginsConfig,
    pub web_search: WebSearchConfig,
    pub web_fetch: WebFetchConfig,
    pub agents: AgentsConfig,
    pub workflow: WorkflowConfig,
    pub features: FeaturesConfig,
}

/// `[features]` 段：特性注册表自有开关的覆盖值；键=bool。
///
/// 只接受 [`moc_protocol::features::features_owned_keys`] 列出的键（当前
/// `clm`、`ptc`）；挂在已有配置段上的 flag（如 `workflow.enabled`）须回到
/// 原段配置，在 `[features]` 里写会被校验拒绝并指向正确路径。
pub type FeaturesConfig = BTreeMap<String, bool>;

pub(super) fn validate_features(features: &FeaturesConfig) -> Result<(), CoreError> {
    use moc_protocol::features::{FEATURES, FlagToggle, features_owned_keys};
    for key in features.keys() {
        match FEATURES.iter().find(|spec| spec.key == key.as_str()) {
            Some(spec) if spec.toggle == FlagToggle::Features => {}
            Some(spec) => {
                let FlagToggle::Existing(path) = spec.toggle else {
                    unreachable!("刚判过非 Features")
                };
                return Err(CoreError::Config(format!(
                    "features.{key}: 该特性由 {path} 控制，请直接配置它"
                )));
            }
            None => {
                let keys: Vec<_> = features_owned_keys().collect();
                return Err(CoreError::Config(format!(
                    "features.{key}: 未知特性开关（可用键: {}）",
                    keys.join(", ")
                )));
            }
        }
    }
    Ok(())
}

/// `[workflow]` 段：动态 workflow 开关与上限。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowConfig {
    /// 总开关；false 时 `/workflow` 与 `session/workflow_activate` 报明确错误。
    pub enabled: bool,
    /// 在飞 ask 上限的天花板（create_workflow.max_concurrency 钳到 [1, 该值]）。
    pub max_concurrency_ceiling: usize,
    /// world.run 默认超时（毫秒，opts.timeoutMs 覆盖）。
    pub world_run_timeout_ms: u64,
    /// files.read 单文件上限。
    pub file_read_bytes: usize,
    /// 单条 log 上限。
    pub log_bytes: usize,
    /// schema 修复重试上限（ask 结构化交付不合规时）。
    pub ask_max_repairs: u32,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrency_ceiling: 16,
            world_run_timeout_ms: 300_000,
            file_read_bytes: 1 << 20,
            log_bytes: 8 * 1024,
            ask_max_repairs: 3,
        }
    }
}

/// `[agents]` 段：子代理委托约束。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentsConfig {
    /// 允许的最大委托深度：`1` 表示子代理不能再 spawn（旧行为）。
    pub max_depth: u32,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self { max_depth: 1 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    pub enabled: bool,
    pub moc_user: bool,
    pub moc_project: bool,
    pub agents_user: bool,
    pub agents_project: bool,
    /// 额外扫描目录；支持 `~` 展开。
    pub dirs: Vec<String>,
    /// 按名称禁用的 skill。
    pub disable: Vec<String>,
    /// `<skills>` 目录预算：占上下文窗口的百分比（0 表示只用下限）。
    pub catalog_budget_percent: u64,
    /// `<skills>` 目录预算下限（token）；预算取比例值与它的较大者。
    pub catalog_budget_min_tokens: u64,
    /// 单条 description 的截断长度（字符）。
    pub description_max_chars: usize,
}

impl SkillsConfig {
    /// `<skills>` 目录预算（token）：上下文窗口的百分比预算与下限取较大者。
    pub fn catalog_budget_tokens(&self, context_window: u64) -> u64 {
        let by_percent = context_window.saturating_mul(self.catalog_budget_percent) / 100;
        by_percent.max(self.catalog_budget_min_tokens)
    }
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            moc_user: true,
            moc_project: true,
            agents_user: true,
            agents_project: true,
            dirs: Vec::new(),
            disable: Vec::new(),
            catalog_budget_percent: 1,
            catalog_budget_min_tokens: 8000,
            description_max_chars: 1024,
        }
    }
}

mod plugins;
pub use plugins::HOST_PLUGIN_KEYS;
pub use plugins::PluginsConfig;
pub use plugins::is_plugin_key;
pub(super) use plugins::validate_plugins;
mod tools;
pub use tools::BashConfig;
pub use tools::EditConfig;
pub use tools::FindConfig;
pub use tools::GrepConfig;
pub use tools::PtcConfig;
pub use tools::ReadConfig;
pub use tools::TodoConfig;
pub use tools::ToolsConfig;
pub use tools::WriteConfig;
pub(super) use tools::validate_ptc;
pub(super) use tools::validate_tools;

/// 沙盒拒绝访问时的策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDenied {
    /// 向用户申请一次（或本会话）授权。
    #[default]
    Ask,
    /// 直接拒绝，不发起权限询问。
    Deny,
}

/// 沙盒与路径权限配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    /// 新会话的默认模式；`full` 表示不隔离。
    pub default_mode: SessionPermission,
    /// 沙盒内 bash 是否允许网络。关闭后 Linux 使用独立网络命名空间，macOS 拒绝所有网络。
    pub network: bool,
    /// 沙盒拒绝时的策略。
    pub on_denied: OnDenied,
    /// 是否允许模型申请 `full`（沙盒外）执行一次。
    pub allow_unsandboxed: bool,
    /// Linux 下是否把 `$HOME` 挂成会话级 copy-on-write 层。
    pub overlay_home: bool,
    /// 额外只读路径；支持 `~`，加载时必须存在。
    pub extra_read_paths: Vec<String>,
    /// 额外可读写路径；支持 `~`，加载时必须存在。
    pub extra_write_paths: Vec<String>,
    /// 强制拒绝读取的路径；优先于允许列表。
    pub deny_read_paths: Vec<String>,
    /// 强制拒绝写入的路径；优先于允许列表。
    pub deny_write_paths: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            default_mode: SessionPermission::Full,
            network: true,
            on_denied: OnDenied::Ask,
            allow_unsandboxed: true,
            overlay_home: true,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
            deny_read_paths: Vec::new(),
            deny_write_paths: Vec::new(),
        }
    }
}

/// 上下文压缩的实现。
///
/// `moc` 是「本地修剪兜底摘要」管线：先 `shake` 裁剪旧工具结果，腾不出空间再用
/// `soft` 保留尾部 + 8 段摘要。`grok_build` 复刻 Grok Build：9 段摘要 +
/// full-replace 历史 + 阈值前预火两遍，不做本地修剪。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMethod {
    /// moc 自有管线：shake → soft。
    #[default]
    Moc,
    /// Grok Build 复刻管线。
    GrokBuild,
    /// NVIDIA EoCC 实验算法：有界任务原文与面向剩余工作的交接状态。
    Eocc,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// 总开关；关闭后手动与自动压缩一并禁用。
    pub enabled: bool,
    /// 压缩实现；仅 `moc` 先做本地修剪，其他算法直接生成交接状态。
    pub method: CompactionMethod,
    pub threshold: CompactionThreshold,
    pub keep_recent_tokens: u32,
    pub protect_recent_tool_tokens: u32,
    pub mid_prompt: bool,
    /// 阈值自动触发开关；关闭后仅剩手动压缩与 Overflow 恢复，不影响 [`Self::enabled`]。
    pub auto: bool,
    pub auto_continue: bool,
    pub shake: ShakeConfig,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            method: CompactionMethod::default(),
            threshold: CompactionThreshold::default(),
            keep_recent_tokens: 20_000,
            protect_recent_tool_tokens: 20_000,
            mid_prompt: true,
            auto: true,
            auto_continue: true,
            shake: ShakeConfig::default(),
        }
    }
}

/// 百分比与固定 token 二选一，不使用哨兵值或输出上限推导阈值。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(
    tag = "mode",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CompactionThreshold {
    Percent(u64),
    Tokens(u64),
}

impl Default for CompactionThreshold {
    fn default() -> Self {
        Self::Percent(85)
    }
}

pub(super) fn validate_compaction(config: &CompactionConfig) -> Result<(), CoreError> {
    match config.threshold {
        CompactionThreshold::Percent(1..=100) | CompactionThreshold::Tokens(1..) => Ok(()),
        _ => Err(CoreError::Config(
            "压缩阈值：百分比须为 1–100，token 数须大于 0".into(),
        )),
    }
}

pub(super) fn validate_skills(config: &SkillsConfig) -> Result<(), CoreError> {
    if config.catalog_budget_percent > 100 {
        return Err(CoreError::Config(
            "技能列表预算比例：百分比须为 0–100".into(),
        ));
    }
    Ok(())
}

/// web_search 的供应商与凭据配置；web_fetch 使用独立的本地抓取配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    /// 关闭后 web_search 不再向模型定义暴露（无 api_key 时同样隐藏）。
    pub enabled: bool,
    /// 供应商名；当前仅支持 tavily，大小写不敏感。
    pub provider: String,
    /// 字面量 / `env:NAME` / `!command`，与 providers 的 api_key 语法一致。
    pub api_key: String,
    /// 供应商接口基地址；留空使用官方地址。
    pub base_url: String,
    pub timeout_secs: u64,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "tavily".into(),
            api_key: String::new(),
            base_url: String::new(),
            timeout_secs: 30,
        }
    }
}

/// web_fetch 的本地抓取配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebFetchConfig {
    /// 关闭后 web_fetch 工具不再向模型定义暴露。
    pub enabled: bool,
    /// 单次请求（含站点 API 调用）的最长等待时间。
    pub timeout_secs: u64,
    /// 单页原始响应的最大字节数。
    pub max_bytes: usize,
    /// 单页预览的字符上限；正文快照不裁切，仍受响应字节上限约束。
    pub max_chars: usize,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: 30,
            max_bytes: 50 * 1024 * 1024,
            max_chars: 500_000,
        }
    }
}

pub(super) fn validate_web_fetch(config: &WebFetchConfig) -> Result<(), CoreError> {
    if config.timeout_secs == 0 {
        return Err(CoreError::Config(
            "web_fetch.timeout_secs: 必须大于 0".into(),
        ));
    }
    if config.max_bytes == 0 {
        return Err(CoreError::Config("web_fetch.max_bytes: 必须大于 0".into()));
    }
    if config.max_chars == 0 {
        return Err(CoreError::Config("web_fetch.max_chars: 必须大于 0".into()));
    }
    Ok(())
}

pub(super) fn validate_web_search(config: &WebSearchConfig) -> Result<String, CoreError> {
    let provider = config.provider.trim().to_ascii_lowercase();
    if provider != "tavily" {
        return Err(CoreError::Config(
            "web_search.provider: 仅支持 tavily".into(),
        ));
    }
    if !config.base_url.is_empty()
        && !config.base_url.starts_with("http://")
        && !config.base_url.starts_with("https://")
    {
        return Err(CoreError::Config(
            "web_search.base_url: 必须以 http:// 或 https:// 开头".into(),
        ));
    }
    if config.timeout_secs == 0 {
        return Err(CoreError::Config(
            "web_search.timeout_secs: 必须大于 0".into(),
        ));
    }
    Ok(provider)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShakeConfig {
    pub soft_trim_chars: usize,
    pub head_chars: usize,
    pub tail_chars: usize,
    pub hard_clear_prompts: usize,
    pub keep_last_prompts: usize,
    pub supersede_reads: bool,
}

impl Default for ShakeConfig {
    fn default() -> Self {
        Self {
            soft_trim_chars: 4000,
            head_chars: 1500,
            tail_chars: 1500,
            hard_clear_prompts: 10,
            keep_last_prompts: 3,
            supersede_reads: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub max_length_continues: usize,
    pub loop_detection: bool,
    /// 单次请求连接与重试阶段的硬时限（秒）；模型生成阶段不受此约束。
    pub connect_deadline_secs: u64,
    /// 单次请求耗时警告阈值（秒）：到点只发一条通知，不终止请求。
    pub request_warn_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_length_continues: 2,
            loop_detection: true,
            connect_deadline_secs: 900,
            request_warn_secs: 900,
        }
    }
}

impl LimitsConfig {
    pub fn stream_guard(&self) -> moc_llm::StreamGuardConfig {
        moc_llm::StreamGuardConfig {
            loop_detection: self.loop_detection,
        }
    }
}

pub(super) fn validate_limits(config: &LimitsConfig) -> Result<(), CoreError> {
    if config.connect_deadline_secs == 0 || config.request_warn_secs == 0 {
        return Err(CoreError::Config(
            "limits：连接与重试时限和请求耗时警告必须大于 0".into(),
        ));
    }
    if config.connect_deadline_secs > 604_800 {
        return Err(CoreError::Config(
            "limits：连接与重试时限不得超过 7 天".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_agents(config: &AgentsConfig) -> Result<(), CoreError> {
    if config.max_depth == 0 {
        return Err(CoreError::Config("agents：max_depth 必须大于 0".into()));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SystemPromptConfig {
    #[serde(rename = "override")]
    pub override_text: String,
    pub append: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextFilesConfig {
    pub enabled: bool,
    pub user: bool,
    pub project: bool,
    pub max_tokens: u64,
    pub import_depth: usize,
}

impl Default for ContextFilesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            user: true,
            project: true,
            max_tokens: 16_000,
            import_depth: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub watch: bool,
    pub max_scan_entries: usize,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            watch: true,
            max_scan_entries: 100_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionDocument {
    pub dir: String,
    /// 新建会话是否使用 zstd 压缩；默认开启，已有会话保持原格式。
    pub compress: bool,
    pub retention: RetentionConfig,
}

impl Default for SessionDocument {
    fn default() -> Self {
        Self {
            dir: "~/.moc/sessions".into(),
            compress: true,
            retention: RetentionConfig::default(),
        }
    }
}

/// 文件容量与工具输出预算属于公共工具策略，不归某个具体工具所有。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub head_bytes: usize,
    pub tail_bytes: usize,
    pub max_file_bytes: u64,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            head_bytes: 20_480,
            tail_bytes: 20_480,
            max_file_bytes: 104_857_600,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOverflow {
    Error,
    Wait,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobConfig {
    pub enabled: bool,
    /// 每个会话可同时运行的后台任务数。
    pub max_running: usize,
    /// `job wait` 单次最多阻塞的秒数。
    pub max_wait_secs: u64,
    pub overflow: JobOverflow,
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_running: 8,
            max_wait_secs: 600,
            overflow: JobOverflow::Error,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    pub enabled: bool,
    pub artifacts_days: u64,
    pub logs_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            artifacts_days: 30,
            logs_days: 14,
        }
    }
}

#[cfg(test)]
mod bash_config_tests;

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
    pub sources: SourcesConfig,
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

/// 插件系统配置；插件是本地目录里会被执行的 JS 代码，项目级来源等于信任该仓库。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    pub enabled: bool,
    /// 是否扫描用户目录 `~/.moc/plugins`。
    pub user: bool,
    /// 是否扫描项目目录 `.moc/plugins`（cwd 向上到 git root）；开启即信任仓库。
    pub project: bool,
    /// 额外扫描目录；支持 `~` 展开。
    pub dirs: Vec<String>,
    /// 按插件名禁用。
    pub disable: Vec<String>,
    /// 一次最多加载的插件数。
    pub max_plugins: usize,
    /// 单个插件运行时的内存上限。
    pub memory_limit_bytes: usize,
    /// 模块求值（含 `activate`）的最长等待。
    pub load_timeout_ms: u64,
    /// 单次工具/命令/钩子调用的最长等待；超时后本进程内禁用该插件。
    pub call_timeout_ms: u64,
    /// 全部插件提示词贡献的总字符上限。
    pub max_prompt_chars: usize,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            user: true,
            project: true,
            dirs: Vec::new(),
            disable: Vec::new(),
            max_plugins: 32,
            memory_limit_bytes: 64 * 1024 * 1024,
            load_timeout_ms: 5000,
            call_timeout_ms: 5000,
            max_prompt_chars: 4000,
        }
    }
}

impl PluginsConfig {
    /// 转成运行时设置；`dirs` 在这里展开 `~`。
    pub fn settings(&self) -> moc_plugin::Settings {
        moc_plugin::Settings {
            discovery: moc_plugin::discovery::DiscoveryConfig {
                enabled: self.enabled,
                user: self.user,
                project: self.project,
                dirs: self
                    .dirs
                    .iter()
                    .map(|dir| crate::config::expand_user_path(dir))
                    .collect(),
            },
            disable: self.disable.clone(),
            max_plugins: self.max_plugins,
            memory_limit_bytes: self.memory_limit_bytes,
            load_timeout: std::time::Duration::from_millis(self.load_timeout_ms),
            call_timeout: std::time::Duration::from_millis(self.call_timeout_ms),
            max_prompt_chars: self.max_prompt_chars,
        }
    }
}

/// 插件配置的数值校验；0 值一律拒绝，避免出现「永远超时」这类静默失效。
pub(super) fn validate_plugins(plugins: &PluginsConfig) -> Result<(), CoreError> {
    for (name, value) in [
        ("plugins.max_plugins", plugins.max_plugins as u64),
        (
            "plugins.memory_limit_bytes",
            plugins.memory_limit_bytes as u64,
        ),
        ("plugins.max_prompt_chars", plugins.max_prompt_chars as u64),
        ("plugins.load_timeout_ms", plugins.load_timeout_ms),
        ("plugins.call_timeout_ms", plugins.call_timeout_ms),
    ] {
        if value == 0 {
            return Err(CoreError::Config(format!("{name} 必须大于 0")));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    /// 标准模式一次模型工具批次中允许同时执行的只读工具数量。
    pub max_concurrent_calls: usize,
    pub output: OutputConfig,
    pub bash: BashConfig,
    pub read: ReadConfig,
    pub edit: EditConfig,
    pub write: WriteConfig,
    pub grep: GrepConfig,
    pub find: FindConfig,
    pub todo: TodoConfig,
    pub job: JobConfig,
    pub ptc: PtcConfig,
}

pub(super) fn validate_tools(config: &ToolsConfig) -> Result<(), crate::error::CoreError> {
    if config.max_concurrent_calls == 0 {
        return Err(crate::error::CoreError::Config(
            "tools.max_concurrent_calls: 必须大于 0".into(),
        ));
    }
    if config.bash.monitored_timeout_secs == 0
        || config.bash.slow_warning_interval_secs == 0
        || config.bash.max_timeout_secs == 0
        || config.bash.max_output_bytes == 0
        || config.bash.persistent_log_bytes == 0
        || config.bash.monitored_timeout_secs > config.bash.max_timeout_secs
    {
        return Err(crate::error::CoreError::Config(
            "tools.bash: monitored_timeout_secs、slow_warning_interval_secs、max_timeout_secs、max_output_bytes、persistent_log_bytes 必须大于 0，且 monitored_timeout_secs 不得超过 max_timeout_secs".into(),
        ));
    }
    if config.todo.reminders_after == 0 || config.todo.reminders_max == 0 {
        return Err(crate::error::CoreError::Config(
            "tools.todo: reminders_after 与 reminders_max 必须大于 0".into(),
        ));
    }
    config
        .bash
        .limits
        .validate()
        .map_err(crate::error::CoreError::Config)?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PtcConfig {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub memory_limit_bytes: usize,
    pub max_output_bytes: usize,
    pub max_concurrent_calls: usize,
    pub max_result_bytes: usize,
    pub max_pending_calls: usize,
    pub max_state_bytes: usize,
}

impl Default for PtcConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 300,
            max_timeout_secs: 1800,
            memory_limit_bytes: 128 * 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_concurrent_calls: 16,
            max_result_bytes: 16 * 1024 * 1024,
            max_pending_calls: 256,
            max_state_bytes: 16 * 1024 * 1024,
        }
    }
}

pub(super) fn validate_ptc(config: &PtcConfig) -> Result<(), crate::error::CoreError> {
    let fail = |field: &str, reason: &str| {
        crate::error::CoreError::Config(format!("tools.ptc.{field}: {reason}"))
    };
    if config.default_timeout_secs == 0 || config.default_timeout_secs > config.max_timeout_secs {
        return Err(fail(
            "default_timeout_secs",
            "必须大于 0 且不超过 max_timeout_secs",
        ));
    }
    if config.max_timeout_secs == 0 {
        return Err(fail("max_timeout_secs", "必须大于 0"));
    }
    if config.max_concurrent_calls == 0 {
        return Err(fail("max_concurrent_calls", "必须大于 0"));
    }
    if config.max_pending_calls < config.max_concurrent_calls || config.max_pending_calls > 65536 {
        return Err(fail(
            "max_pending_calls",
            "必须不小于并发上限且不超过 65536",
        ));
    }
    if !(8 * 1024 * 1024..=u32::MAX as usize).contains(&config.memory_limit_bytes) {
        return Err(fail("memory_limit_bytes", "必须在 8 MiB..=u32::MAX 范围内"));
    }
    for (name, value) in [
        ("max_output_bytes", config.max_output_bytes),
        ("max_result_bytes", config.max_result_bytes),
        ("max_state_bytes", config.max_state_bytes),
    ] {
        if value == 0 || value > config.memory_limit_bytes {
            return Err(fail(name, "必须大于 0 且不超过 memory_limit_bytes"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BashConfig {
    pub foreground_after_secs: u64,
    pub monitored_timeout_secs: u64,
    pub slow_warning_interval_secs: u64,
    pub max_timeout_secs: u64,
    pub max_output_bytes: u64,
    pub persistent_log_bytes: u64,
    pub deny_patterns: Vec<String>,
    /// 每条 bash 的资源上限；默认关闭。
    pub limits: crate::limits::BashLimitsConfig,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            foreground_after_secs: 60,
            monitored_timeout_secs: 1800,
            slow_warning_interval_secs: 180,
            max_timeout_secs: 3600,
            max_output_bytes: 16 * 1024 * 1024,
            persistent_log_bytes: 16 * 1024 * 1024,
            deny_patterns: vec![
                r"(?i)rm\s+-rf\s+/(\s|$)".into(),
                r":\(\)\s*\{\s*:\|\s*:\s*&\s*\};:".into(),
                r"(?i)mkfs\.".into(),
                r"(?i)dd\s+.*of=/dev/".into(),
                r"(?i)\b(shutdown|reboot|format|diskpart|del|erase|rd|rmdir|stop-computer|restart-computer)\b".into(),
                r"(?i)\b(remove-item|clear-disk|initialize-disk|remove-partition)\b".into(),
                r">\s*/etc/passwd".into(),
            ],
            limits: crate::limits::BashLimitsConfig::default(),
        }
    }
}

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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadConfig {
    pub default_limit: usize,
    pub max_bytes: usize,
    pub line_max_bytes: usize,
}

impl Default for ReadConfig {
    fn default() -> Self {
        Self {
            default_limit: 300,
            max_bytes: 51_200,
            line_max_bytes: 4_096,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditConfig {
    pub max_edits: usize,
    pub noop_loop_threshold: usize,
}

impl Default for EditConfig {
    fn default() -> Self {
        Self {
            max_edits: 32,
            noop_loop_threshold: 3,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WriteConfig {
    pub hash_echo_min_hits: usize,
}

impl Default for WriteConfig {
    fn default() -> Self {
        Self {
            hash_echo_min_hits: 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GrepConfig {
    pub max_files: usize,
    pub max_matches_per_file: usize,
    pub max_matches_total: usize,
    pub line_max_chars: usize,
    pub output_bytes: usize,
    pub context_before: usize,
    pub context_after: usize,
    pub max_file_bytes: u64,
    pub timeout_ms: u64,
    pub max_candidate_files: usize,
}

impl Default for GrepConfig {
    fn default() -> Self {
        Self {
            max_files: 20,
            max_matches_per_file: 20,
            max_matches_total: 200,
            line_max_chars: 200,
            output_bytes: 24_576,
            context_before: 0,
            context_after: 0,
            max_file_bytes: 4_194_304,
            timeout_ms: 30_000,
            max_candidate_files: 20_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FindConfig {
    pub limit: usize,
    pub timeout_ms: u64,
}

impl Default for FindConfig {
    fn default() -> Self {
        Self {
            limit: 200,
            timeout_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TodoConfig {
    pub enabled: bool,
    /// 停滞提醒：距上次 todo 变更的工具调用数达到 `reminders_after` 时注入一次。
    pub reminders: bool,
    /// 触发提醒所需的工具调用数。
    pub reminders_after: u32,
    /// 单个提示内最多提醒次数，避免长任务被反复打断。
    pub reminders_max: u32,
}

impl Default for TodoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            reminders: true,
            reminders_after: 12,
            reminders_max: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMethod {
    Remote,
    Shake,
    Soft,
}

/// soft 压缩使用的摘要实现。
///
/// 只切换走到 `soft` 之后的行为；`remote` 与压缩前的 `shake` 顺序、参数与
/// 默认值不受影响，因此它不参与 [`CompactionConfig::method_order`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SoftMethod {
    /// moc 自有的保留尾部 + 8 段摘要实现。
    #[default]
    Moc,
    /// Grok Build 的 9 段摘要 + full-replace 历史实现。
    GrokBuild,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// 总开关；关闭后手动与自动压缩一并禁用。
    pub enabled: bool,
    /// 压缩方法尝试顺序。
    pub method_order: Vec<CompactionMethod>,
    /// `soft` 方法的摘要实现。
    pub soft_method: SoftMethod,
    pub threshold: CompactionThreshold,
    pub keep_recent_tokens: u32,
    pub protect_recent_tool_tokens: u32,
    pub mid_prompt: bool,
    /// 阈值自动触发开关；关闭后仅剩手动压缩与 Overflow 恢复，不影响 [`Self::enabled`]。
    pub auto: bool,
    pub auto_continue: bool,
    pub shake: ShakeConfig,
    pub remote: RemoteCompactionConfig,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            method_order: vec![
                CompactionMethod::Remote,
                CompactionMethod::Shake,
                CompactionMethod::Soft,
            ],
            soft_method: SoftMethod::default(),
            threshold: CompactionThreshold::default(),
            keep_recent_tokens: 20_000,
            protect_recent_tool_tokens: 20_000,
            mid_prompt: true,
            auto: true,
            auto_continue: true,
            shake: ShakeConfig::default(),
            remote: RemoteCompactionConfig::default(),
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
    /// 单页转换后正文的最大字符数。
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
pub struct RemoteCompactionConfig {
    pub enabled: bool,
    pub server_side: bool,
    pub retained_message_tokens: u32,
}

impl Default for RemoteCompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            server_side: true,
            retained_message_tokens: 64_000,
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
    /// 新建会话是否使用 zstd 压缩；默认为关闭。
    pub compress: bool,
    pub retention: RetentionConfig,
}

impl Default for SessionDocument {
    fn default() -> Self {
        Self {
            dir: "~/.moc/sessions".into(),
            compress: false,
            retention: RetentionConfig::default(),
        }
    }
}

/// 配置文件来源；显式 CLI 覆盖层始终加载。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourcesConfig {
    /// 项目级 `.moc/config.toml` 能改写全局设置、触发其中的密钥命令，默认关闭。
    pub project_config: bool,
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
    pub max_running: usize,
    pub overflow: JobOverflow,
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_running: 4,
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
mod bash_config_tests {
    use super::*;

    #[test]
    fn bash_defaults_match_lifecycle_contract() {
        let config = BashConfig::default();
        assert_eq!(config.foreground_after_secs, 60);
        assert_eq!(config.monitored_timeout_secs, 1800);
        assert_eq!(config.slow_warning_interval_secs, 180);
        assert_eq!(config.max_timeout_secs, 3600);
        assert_eq!(config.max_output_bytes, 16 * 1024 * 1024);
        assert_eq!(config.persistent_log_bytes, 16 * 1024 * 1024);
    }
}

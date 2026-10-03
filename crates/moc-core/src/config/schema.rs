//! 配置文件的结构化 schema 与项目默认值。

use std::collections::BTreeMap;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use super::model::ModelCatalog;
use crate::error::CoreError;

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ConfigDocument {
    pub models: ModelCatalog,
    pub providers: BTreeMap<String, super::model::ProviderDocument>,
    pub sources: SourcesConfig,
    pub tools: ToolsConfig,
    pub compaction: CompactionConfig,
    pub limits: LimitsConfig,
    pub system_prompt: SystemPromptConfig,
    pub context_files: ContextFilesConfig,
    pub workspace: WorkspaceConfig,
    pub openviking: moc_openviking::OpenVikingConfig,
    pub session: SessionDocument,
    pub mcp: moc_mcp::McpDocument,
    pub skills: SkillsConfig,
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

/// 工具授权策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// 自动允许（deny 规则仍然优先）。
    #[default]
    Yolo,
    /// 对写入或执行类工具询问用户。
    Prompt,
}

/// 单项工具的授权覆盖规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApproval {
    Allow,
    Deny,
    Prompt,
}

/// `[tools.approval]` 的预编译匹配器。
///
/// 精确键 O(log n) 命中；含通配符的键在配置加载期编译为 [`GlobSet`]，
/// 运行期按「模式更长 = 更具体」排序取首个命中。`mcp__github__*` 这类
/// 前缀规则由此生效，MCP 工具无需逐个登记。
#[derive(Debug, Clone, Default)]
pub struct ApprovalMatcher {
    exact: BTreeMap<String, ToolApproval>,
    /// (模式长度, 匹配器, 策略)，按模式长度降序。
    globs: Vec<(usize, GlobSet, ToolApproval)>,
}

impl ApprovalMatcher {
    pub fn build(approval: &BTreeMap<String, ToolApproval>) -> Self {
        let mut exact = BTreeMap::new();
        let mut globs = Vec::new();
        for (key, policy) in approval {
            let is_pattern =
                key.contains('*') || key.contains('?') || key.contains('[') || key.contains('{');
            if !is_pattern {
                exact.insert(key.clone(), *policy);
                continue;
            }
            match GlobBuilder::new(key).literal_separator(false).build() {
                Ok(glob) => {
                    let mut builder = GlobSetBuilder::new();
                    builder.add(glob);
                    match builder.build() {
                        Ok(set) => globs.push((key.len(), set, *policy)),
                        Err(error) => {
                            tracing::warn!("审批规则 `{key}` 编译失败，已忽略：{error}")
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!("审批规则 `{key}` 不是合法 glob，已忽略：{error}")
                }
            }
        }
        globs.sort_by_key(|(pattern_len, _, _)| std::cmp::Reverse(*pattern_len));
        Self { exact, globs }
    }

    /// 解析工具名对应的策略：精确优先，glob 按具体度取首个命中。
    pub fn lookup(&self, name: &str) -> Option<ToolApproval> {
        if let Some(policy) = self.exact.get(name) {
            return Some(*policy);
        }
        self.globs
            .iter()
            .find(|(_, set, _)| set.is_match(name))
            .map(|(_, _, policy)| *policy)
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.globs.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub approval_mode: ApprovalMode,
    pub approval: BTreeMap<String, ToolApproval>,
    pub output: OutputConfig,
    pub bash: BashConfig,
    pub read: ReadConfig,
    pub edit: EditConfig,
    pub write: WriteConfig,
    pub grep: GrepConfig,
    pub find: FindConfig,
    pub todo: FeatureConfig,
    pub job: JobConfig,
    pub ptc: PtcConfig,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            approval_mode: ApprovalMode::Yolo,
            approval: BTreeMap::new(),
            output: OutputConfig::default(),
            bash: BashConfig::default(),
            read: ReadConfig::default(),
            edit: EditConfig::default(),
            write: WriteConfig::default(),
            grep: GrepConfig::default(),
            find: FindConfig::default(),
            todo: FeatureConfig::default(),
            job: JobConfig::default(),
            ptc: PtcConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PtcConfig {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub memory_limit_bytes: usize,
    pub max_output_bytes: usize,
    pub max_concurrent_calls: usize,
}

impl Default for PtcConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 300,
            max_timeout_secs: 1800,
            memory_limit_bytes: 128 * 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_concurrent_calls: 16,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BashConfig {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub auto_background_ms: u64,
    pub deny_patterns: Vec<String>,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 120,
            max_timeout_secs: 3600,
            auto_background_ms: 0,
            deny_patterns: vec![
                r"(?i)rm\s+-rf\s+/(\s|$)".into(),
                r":\(\)\s*\{\s*:\|\s*:\s*&\s*\};:".into(),
                r"(?i)mkfs\.".into(),
                r"(?i)dd\s+.*of=/dev/".into(),
                r"(?i)\b(shutdown|reboot|format|diskpart|del|erase|rd|rmdir|stop-computer|restart-computer)\b".into(),
                r"(?i)\b(remove-item|clear-disk|initialize-disk|remove-partition)\b".into(),
                r">\s*/etc/passwd".into(),
            ],
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
            line_max_bytes: 204_800,
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
            line_max_chars: 512,
            context_before: 1,
            context_after: 3,
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
pub struct FeatureConfig {
    pub enabled: bool,
}

impl Default for FeatureConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMethod {
    Remote,
    Shake,
    Soft,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// 总开关；关闭后手动与自动压缩一并禁用。
    pub enabled: bool,
    /// 压缩方法尝试顺序。
    pub method_order: Vec<CompactionMethod>,
    pub threshold: CompactionThreshold,
    pub keep_recent_tokens: u32,
    pub protect_recent_tool_tokens: u32,
    pub mid_turn: bool,
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
            threshold: CompactionThreshold::default(),
            keep_recent_tokens: 20_000,
            protect_recent_tool_tokens: 20_000,
            mid_turn: true,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShakeConfig {
    pub soft_trim_chars: usize,
    pub head_chars: usize,
    pub tail_chars: usize,
    pub hard_clear_turns: usize,
    pub keep_last_turns: usize,
    pub supersede_reads: bool,
}

impl Default for ShakeConfig {
    fn default() -> Self {
        Self {
            soft_trim_chars: 4000,
            head_chars: 1500,
            tail_chars: 1500,
            hard_clear_turns: 10,
            keep_last_turns: 3,
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
    pub max_steps_per_turn: usize,
    pub max_length_continues: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_steps_per_turn: 128,
            max_length_continues: 2,
        }
    }
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
    pub retention: RetentionConfig,
}

impl Default for SessionDocument {
    fn default() -> Self {
        Self {
            dir: "~/.moc/sessions".into(),
            retention: RetentionConfig::default(),
        }
    }
}

/// 配置文件来源；显式 CLI 覆盖层始终加载。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourcesConfig {
    pub project_config: bool,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            project_config: true,
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
    pub archive_after_days: u64,
    pub retain_newest_global: usize,
    pub retain_newest_per_workspace: usize,
    pub artifacts_days: u64,
    pub logs_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            archive_after_days: 30,
            retain_newest_global: 20,
            retain_newest_per_workspace: 10,
            artifacts_days: 30,
            logs_days: 14,
        }
    }
}

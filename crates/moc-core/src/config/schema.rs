//! 配置文件的结构化 schema 与项目默认值。

use std::collections::BTreeMap;

use serde::Deserialize;

use super::model::ModelCatalog;

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ConfigDocument {
    pub models: ModelCatalog,
    pub providers: BTreeMap<String, super::model::ProviderDocument>,
    pub tools: ToolsConfig,
    pub compaction: CompactionConfig,
    pub limits: LimitsConfig,
    pub system_prompt: SystemPromptConfig,
    pub context_files: ContextFilesConfig,
    pub workspace: WorkspaceConfig,
    pub session: SessionDocument,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub approval_mode: ApprovalMode,
    pub approval: BTreeMap<String, ToolApproval>,
    pub bash: BashConfig,
    pub read: ReadConfig,
    pub edit: EditConfig,
    pub write: WriteConfig,
    pub grep: GrepConfig,
    pub find: FindConfig,
    pub todo: FeatureConfig,
    pub job: FeatureConfig,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            approval_mode: ApprovalMode::Yolo,
            approval: BTreeMap::new(),
            bash: BashConfig::default(),
            read: ReadConfig::default(),
            edit: EditConfig::default(),
            write: WriteConfig::default(),
            grep: GrepConfig::default(),
            find: FindConfig::default(),
            todo: FeatureConfig::default(),
            job: FeatureConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BashConfig {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub output_head_bytes: usize,
    pub output_tail_bytes: usize,
    pub auto_background_ms: u64,
    pub deny_patterns: Vec<String>,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 120,
            max_timeout_secs: 3600,
            output_head_bytes: 20_480,
            output_tail_bytes: 20_480,
            auto_background_ms: 0,
            deny_patterns: vec![
                r"rm\s+-rf\s+/(\s|$)".into(),
                r":\(\)\s*\{\s*:\|\s*:\s*&\s*\};:".into(),
                r"mkfs\.".into(),
                r"dd\s+.*of=/dev/".into(),
                r"shutdown|reboot".into(),
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
    pub max_file_bytes: u64,
}

impl Default for ReadConfig {
    fn default() -> Self {
        Self {
            default_limit: 300,
            max_bytes: 51_200,
            line_max_bytes: 204_800,
            max_file_bytes: 104_857_600,
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
    pub enabled: bool,
    pub method_order: Vec<CompactionMethod>,
    pub reserve_tokens: u32,
    pub threshold_percent: i32,
    pub threshold_tokens: i64,
    pub keep_recent_tokens: u32,
    pub protect_recent_tool_tokens: u32,
    pub mid_turn: bool,
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
            reserve_tokens: 0,
            threshold_percent: -1,
            threshold_tokens: -1,
            keep_recent_tokens: 20_000,
            protect_recent_tool_tokens: 20_000,
            mid_turn: true,
            auto_continue: true,
            shake: ShakeConfig::default(),
            remote: RemoteCompactionConfig::default(),
        }
    }
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
    pub max_output_tokens: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_steps_per_turn: 128,
            max_length_continues: 2,
            max_output_tokens: 32_000,
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
    pub import_depth: usize,
}

impl Default for ContextFilesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
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
    pub locking: bool,
}

impl Default for SessionDocument {
    fn default() -> Self {
        Self {
            dir: "~/.moc/sessions".into(),
            locking: true,
        }
    }
}

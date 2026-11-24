use super::JobConfig;
use super::OutputConfig;
use moc_i18n::t;
use serde::Deserialize;

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

pub(in super::super) fn validate_tools(
    config: &ToolsConfig,
) -> Result<(), crate::error::CoreError> {
    if config.max_concurrent_calls == 0 {
        return Err(crate::error::CoreError::Config(
            t!("core.err.tools_max_concurrent_positive").into_owned(),
        ));
    }
    if config.bash.monitored_timeout_secs == 0
        || config.bash.stall_after_secs == 0
        || config.bash.max_timeout_secs == 0
        || config.bash.max_output_bytes == 0
        || config.bash.persistent_log_bytes == 0
        || config.bash.monitored_timeout_secs > config.bash.max_timeout_secs
    {
        return Err(crate::error::CoreError::Config(
            t!("core.err.tools_bash_invalid").into_owned(),
        ));
    }
    if config.todo.reminders_after == 0 || config.todo.reminders_max == 0 {
        return Err(crate::error::CoreError::Config(
            t!("core.err.tools_todo_positive").into_owned(),
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
    pub max_total_calls: usize,
    pub max_concurrent_runs: usize,
    pub host_idle_timeout_secs: u64,
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
            max_total_calls: 1000,
            max_concurrent_runs: 16,
            host_idle_timeout_secs: 600,
            max_state_bytes: 16 * 1024 * 1024,
        }
    }
}

pub(in super::super) fn validate_ptc(config: &PtcConfig) -> Result<(), crate::error::CoreError> {
    let fail = |field: &str, reason: &str| {
        crate::error::CoreError::Config(
            t!("core.err.ptc_field_err", field = field, reason = reason).into_owned(),
        )
    };
    if config.default_timeout_secs == 0 || config.default_timeout_secs > config.max_timeout_secs {
        return Err(fail("default_timeout_secs", &t!("core.err.ptc_default_timeout")));
    }
    if config.max_timeout_secs == 0 {
        return Err(fail("max_timeout_secs", &t!("core.err.ptc_positive")));
    }
    // `Instant + Duration` 在超过平台可表示范围时会 panic，配置加载阶段直接拒绝。
    const MAX_INSTANT_SECS: u64 = u64::MAX / 1_000_000_000;
    if config.max_timeout_secs > MAX_INSTANT_SECS {
        return Err(fail("max_timeout_secs", &t!("core.err.ptc_time_range")));
    }
    if config.max_concurrent_calls == 0 {
        return Err(fail("max_concurrent_calls", &t!("core.err.ptc_positive")));
    }
    if config.max_pending_calls < config.max_concurrent_calls || config.max_pending_calls > 65536 {
        return Err(fail("max_pending_calls", &t!("core.err.ptc_pending_range")));
    }
    if config.max_total_calls == 0 || config.max_total_calls > 1_000_000 {
        return Err(fail("max_total_calls", &t!("core.err.ptc_total_range")));
    }
    if config.max_concurrent_runs == 0 {
        return Err(fail("max_concurrent_runs", &t!("core.err.ptc_positive")));
    }
    if config.host_idle_timeout_secs == 0 {
        return Err(fail("host_idle_timeout_secs", &t!("core.err.ptc_positive")));
    }
    if config.host_idle_timeout_secs > MAX_INSTANT_SECS {
        return Err(fail("host_idle_timeout_secs", &t!("core.err.ptc_time_range")));
    }
    if !(8 * 1024 * 1024..=u32::MAX as usize).contains(&config.memory_limit_bytes) {
        return Err(fail("memory_limit_bytes", &t!("core.err.ptc_memory_range")));
    }
    for (name, value) in [
        ("max_output_bytes", config.max_output_bytes),
        ("max_result_bytes", config.max_result_bytes),
        ("max_state_bytes", config.max_state_bytes),
    ] {
        if value == 0 || value > config.memory_limit_bytes {
            return Err(fail(name, &t!("core.err.ptc_under_memory")));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BashConfig {
    pub foreground_after_secs: u64,
    pub monitored_timeout_secs: u64,
    /// monitored 后台任务多久没有新输出就提醒模型可能卡住。
    pub stall_after_secs: u64,
    pub max_timeout_secs: u64,
    pub max_output_bytes: u64,
    pub persistent_log_bytes: u64,
    /// 开启后，只读/搜索/列目录/改工作区文件的 bash 命令被拒并给出内置工具建议。
    pub prefer_builtin: bool,
    pub deny_patterns: Vec<String>,
    /// 每条 bash 的资源上限；默认关闭。
    pub limits: crate::limits::BashLimitsConfig,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            foreground_after_secs: 60,
            monitored_timeout_secs: 1800,
            stall_after_secs: 120,
            max_timeout_secs: 3600,
            max_output_bytes: 16 * 1024 * 1024,
            persistent_log_bytes: 16 * 1024 * 1024,
            prefer_builtin: true,
            // 与 defaults.toml 中的 deny_patterns 保持一致：动词本身不拦，拦“作用于整个
            // 文件环境（/、~、$HOME、顶层系统目录及其一级子项）”的组合与必然毁灭性的命令；
            // 命中后向用户申请确认而非直接拒绝。
            deny_patterns: vec![
                // 递归删除作用于整个文件环境；rm 标志可前可后。
                r#"(?i)\brm\b[^;&|]*?(?:-[a-z]*r[a-z]*f[a-z]*|-[a-z]*f[a-z]*r[a-z]*|-[a-z]*r[a-z]*\s+-[a-z]*f[a-z]*|-[a-z]*f[a-z]*\s+-[a-z]*r[a-z]*|--recursive[^;&|]*--force|--force[^;&|]*--recursive)[^;&|]*?(?:^|[\s;&|"'{(=,:])(?:/{1,2}(?:\.{0,2}|\*)?/?\*?|~[-\w]*/?\*?|\$\{?HOME\}?/?\*?|/(?:etc|usr|var|bin|sbin|lib(?:32|64)?|boot|root|home|opt|srv|sys|proc|dev|mnt|media|run)(?:/[-\w.]+)?/?\*?|/tmp/?\*?)(?:[\s;&|"'{)`]|$)"#.into(),
                r#"(?i)\brm\b[^;&|]*?(?:^|[\s;&|"'{(=,:])(?:/{1,2}(?:\.{0,2}|\*)?/?\*?|~[-\w]*/?\*?|\$\{?HOME\}?/?\*?|/(?:etc|usr|var|bin|sbin|lib(?:32|64)?|boot|root|home|opt|srv|sys|proc|dev|mnt|media|run)(?:/[-\w.]+)?/?\*?|/tmp/?\*?)(?:[\s;&|"'{)`]|$)[^;&|]*?(?:-[a-z]*r[a-z]*f[a-z]*|-[a-z]*f[a-z]*r[a-z]*|-[a-z]*r[a-z]*\s+-[a-z]*f[a-z]*|-[a-z]*f[a-z]*\s+-[a-z]*r[a-z]*|--recursive[^;&|]*--force|--force[^;&|]*--recursive)"#.into(),
                // find 对整个环境 -delete / -exec rm / 管道进 xargs rm。
                r#"(?i)\bfind\b[^;&|]*?(?:^|[\s;&|"'{(=,:])(?:/{1,2}(?:\.{0,2}|\*)?/?\*?|~[-\w]*/?\*?|\$\{?HOME\}?/?\*?|/(?:etc|usr|var|bin|sbin|lib(?:32|64)?|boot|root|home|opt|srv|sys|proc|dev|mnt|media|run)(?:/[-\w.]+)?/?\*?|/tmp/?\*?)(?:[\s;&|"'{)`]|$)[^;&|]*(?:-delete\b|-exec(?:dir)?[^;&|]*\brm\b)"#.into(),
                r#"(?i)\bfind\b[^;&|]*?(?:^|[\s;&|"'{(=,:])(?:/{1,2}(?:\.{0,2}|\*)?/?\*?|~[-\w]*/?\*?|\$\{?HOME\}?/?\*?|/(?:etc|usr|var|bin|sbin|lib(?:32|64)?|boot|root|home|opt|srv|sys|proc|dev|mnt|media|run)(?:/[-\w.]+)?/?\*?|/tmp/?\*?)(?:[\s;&|"'{)`]|$)[^;&]*\|\s*xargs[^;&|]*\brm\b"#.into(),
                // chmod/chown/chgrp 递归作用于整个环境。
                r#"(?i)\b(?:chmod|chown|chgrp)\s+[^;&|]*-[a-z]*r[a-z]*\s[^;&|]*?(?:^|[\s;&|"'{(=,:])(?:/{1,2}(?:\.{0,2}|\*)?/?\*?|~[-\w]*/?\*?|\$\{?HOME\}?/?\*?|/(?:etc|usr|var|bin|sbin|lib(?:32|64)?|boot|root|home|opt|srv|sys|proc|dev|mnt|media|run)(?:/[-\w.]+)?/?\*?|/tmp/?\*?)(?:[\s;&|"'{)`]|$)"#.into(),
                // fork 炸弹。
                r":\(\)\s*\{\s*:\|\s*:\s*&\s*\};:".into(),
                // 格式化或覆写块设备与分区表。
                r#"(?i)\b(?:mkfs(?:\.[\w-]+)?|mke2fs|mkdosfs|mkntfs|mkexfatfs|mkudffs|mkhfs|mkswap)\b"#.into(),
                r"(?i)\bdd\s+.*of=/dev/(?:sd|hd|vd|xvd|nvme|mmcblk|loop|dm-|disk)".into(),
                r"(?i)(?:>\s*|\btee\b[^;&|]*?)/dev/(?:sd|hd|vd|xvd|nvme|mmcblk|loop|dm-|disk)".into(),
                r"(?i)\b(?:cp|mv|install|rsync)\s+\S+\s+[^;&|]*/dev/(?:sd|hd|vd|xvd|nvme|mmcblk|loop|dm-|disk)".into(),
                r"(?i)\btar\s+[^;&|]*-[a-z]*c[a-z]*f[a-z]*\s*/dev/(?:sd|hd|vd|xvd|nvme|mmcblk|loop|dm-|disk)".into(),
                r"(?i)\bshred\b[^;&|]*/dev/(?:sd|hd|vd|xvd|nvme|mmcblk|loop|dm-|disk)".into(),
                r"(?i)\bwipefs\b[^;&|]*(?:-a\b|--all\b)".into(),
                r"(?i)\b(?:fdisk|sfdisk)\s+/dev/|\bparted\s+(?:-\S+\s+)*/dev/\S+\s+(?:mklabel|mkpart|rm|move|resize|rescue|set)\b|\bsgdisk\b[^;&|]*(?:-Z\b|--zap-all\b)".into(),
                // 覆写关键系统账户/引导文件。
                r"(?i)(?:>\s*|\btee\b[^;&|]*?)/etc/(?:passwd|shadow|sudoers|gshadow|fstab)\b".into(),
                // 命令位置的关机/格式化/Windows 危险动词；分隔符锚定避免误伤 clang-format 等命令名。
                r#"(?i)(?:^|[;&|`'"\n])\s*(?:(?:sudo|doas|pkexec|run0|busybox)\s+)?(?:shutdown|reboot|halt|poweroff|format|diskpart|del|erase|rd|rmdir|stop-computer|restart-computer|remove-item|clear-disk|initialize-disk|remove-partition)\b"#.into(),
                r"(?i)\bsystemctl\s+[^;&|]*\b(?:halt|poweroff|reboot|kexec|soft-reboot)\b".into(),
                // 销毁存储卷、系统账户、防火墙规则、swap、cron。
                r"(?i)\b(?:lvremove|vgremove|pvremove|userdel|groupdel|deluser|delgroup)\b|\bcryptsetup\s+(?:luksFormat|luksErase|erase|reencrypt)\b|\bzpool\s+(?:destroy|labelclear)\b|\bzfs\s+destroy\b[^;&|]*-[a-z]*r[a-z]*".into(),
                r"(?i)\bip6?tables\b[^;&|]*-F\b|\bnft\s+flush\b|\bswapoff\s+[^;&|]*-a\b|\bcrontab\s+-[a-z]*r".into(),
                // 向全部进程发信号。
                r#"\b(?:kill|skill)\b[^;&|]*\s-1\s*(?:[;&|]|$)|\bpkill\s+[^;&|]*-f\s*['"]?\.['"]?\s*(?:[;&|]|$)"#.into(),
            ],
            limits: crate::limits::BashLimitsConfig::default(),
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
            max_bytes: 262_144,
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
            output_bytes: 131_072,
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

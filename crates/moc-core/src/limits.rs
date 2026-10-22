//! bash 子进程的资源上限：Linux 走 cgroup v2（经 systemd user manager），Windows 走 Job Object。
//!
//! 参数全部来自 `[tools.bash.limits]`；0 表示该项不限。Linux 的 `memory.high` 依赖宿主
//! swap 才能把匿名页换出并继续推进，无 swap 时自动跳过，避免进程无限期停等。

use std::ffi::OsString;
use std::sync::OnceLock;

use serde::Deserialize;
use uuid::Uuid;

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// 配置形状；与 `[tools.bash.limits]` 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BashLimitsConfig {
    pub enabled: bool,
    /// CPU 核数上限；0 表示不限。
    pub cpu_cores: u32,
    /// 内存软限（字节）：超过就节流；0 表示不节流。仅 Linux 生效。
    pub memory_high_bytes: u64,
    /// 内存硬顶（字节）：兜底 OOM；0 表示不设。仅 Linux 生效。
    pub memory_max_bytes: u64,
    /// swap 预算（字节）；0 表示不限制。仅 Linux 生效。
    pub memory_swap_bytes: u64,
}

impl Default for BashLimitsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cpu_cores: 4,
            memory_high_bytes: 8 * GIB,
            memory_max_bytes: 12 * GIB,
            memory_swap_bytes: 4 * GIB,
        }
    }
}

impl BashLimitsConfig {
    /// 关闭时返回 `None`，调用方据此完全跳过探测与包装。
    pub fn resolve(&self) -> Option<BashLimits> {
        self.enabled.then_some(BashLimits {
            cpu_cores: self.cpu_cores,
            memory_high_bytes: self.memory_high_bytes,
            memory_max_bytes: self.memory_max_bytes,
            memory_swap_bytes: self.memory_swap_bytes,
        })
    }

    /// 只在启用时校验；返回的错误已带完整字段路径。
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.cpu_cores == 0
            && self.memory_high_bytes == 0
            && self.memory_max_bytes == 0
            && self.memory_swap_bytes == 0
        {
            return Err("tools.bash.limits: 启用后至少要配置一项限制（cpu_cores 或内存项）".into());
        }
        if self.cpu_cores > 256 {
            return Err("tools.bash.limits.cpu_cores: 不得超过 256".into());
        }
        for (field, value) in [
            ("memory_high_bytes", self.memory_high_bytes),
            ("memory_max_bytes", self.memory_max_bytes),
        ] {
            if value != 0 && value < 64 * MIB {
                return Err(format!("tools.bash.limits.{field}: 必须为 0 或至少 64 MiB"));
            }
        }
        if self.memory_high_bytes != 0
            && self.memory_max_bytes != 0
            && self.memory_high_bytes > self.memory_max_bytes
        {
            return Err("tools.bash.limits.memory_high_bytes: 不得超过 memory_max_bytes".into());
        }
        Ok(())
    }
}

/// 已解析的资源上限；0 表示该项不限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BashLimits {
    pub cpu_cores: u32,
    pub memory_high_bytes: u64,
    pub memory_max_bytes: u64,
    pub memory_swap_bytes: u64,
}

impl BashLimits {
    /// 按平台能力裁剪：Windows 不做内存限制；Linux 无 swap 时跳过软限。
    pub fn effective(self) -> Self {
        trim(self, support())
    }
}

/// 平台能力；进程内只探测一次。
#[derive(Debug, Clone, Copy)]
pub struct Support {
    /// 当前平台能否施加限制。
    pub available: bool,
    /// 宿主是否有 swap；只有 Linux 的 `memory.high` 依赖它。
    pub swap: bool,
}

pub fn support() -> Support {
    static SUPPORT: OnceLock<Support> = OnceLock::new();
    *SUPPORT.get_or_init(detect)
}

/// 把命令包装成受限执行；不可用或没有可施加的属性时原样返回。
pub(crate) fn wrap(
    program: OsString,
    args: Vec<OsString>,
    limits: Option<BashLimits>,
) -> (OsString, Vec<OsString>) {
    let Some(limits) = limits else {
        return (program, args);
    };
    if !support().available {
        return (program, args);
    }
    #[cfg(target_os = "linux")]
    {
        let mut wrapped: Vec<OsString> = vec![
            "--user".into(),
            "--scope".into(),
            "--quiet".into(),
            "--collect".into(),
            "--unit".into(),
            short_unit_id().into(),
        ];
        wrapped.extend(properties(&limits.effective()));
        wrapped.push("--".into());
        wrapped.push(program);
        wrapped.extend(args);
        ("systemd-run".into(), wrapped)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Windows 的 CPU 限速挂在 Job Object 上，不改命令行。
        let _ = limits;
        (program, args)
    }
}

/// `<env>` 里的那一行；调用方保证 `enabled=true`。
pub fn prompt_line(config: &BashLimitsConfig) -> Option<String> {
    let limits = config.resolve()?;
    support()
        .available
        .then(|| format!("bash limits: {}", describe(&limits)))
}

/// 模型可见的一句话事实说明，例如 `4 cores, memory high 8.0 GiB / max 12.0 GiB (cgroup v2)`。
pub fn describe(limits: &BashLimits) -> String {
    render(&limits.effective())
}

/// scope 创建失败的 stderr 签名；只用于给模型追加提示，不改变退出码。
pub(crate) fn failure_in(text: &str) -> bool {
    text.contains("Failed to start transient scope unit")
}

/// Windows Job Object 的 `CpuRate`（1/100 百分比）；`None` 表示不需要设置。
///
/// 纯计算，便于在任意平台测试；实际设置见 `tools/bash/group.rs`。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_cpu_rate(limits: BashLimits, total_cores: usize) -> Option<u32> {
    let cores = usize::try_from(limits.cpu_cores).ok()?;
    if cores == 0 || total_cores == 0 || cores >= total_cores {
        return None;
    }
    let rate = cores.saturating_mul(10_000) / total_cores;
    u32::try_from(rate.clamp(1, 10_000)).ok()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn properties(limits: &BashLimits) -> Vec<OsString> {
    let mut properties = Vec::new();
    if limits.cpu_cores > 0 {
        properties.push("-p".into());
        properties.push(format!("CPUQuota={}%", u64::from(limits.cpu_cores) * 100).into());
    }
    for (name, value) in [
        ("MemoryHigh", limits.memory_high_bytes),
        ("MemoryMax", limits.memory_max_bytes),
        ("MemorySwapMax", limits.memory_swap_bytes),
    ] {
        if value > 0 {
            properties.push("-p".into());
            properties.push(format!("{name}={value}").into());
        }
    }
    properties
}

fn trim(limits: BashLimits, support: Support) -> BashLimits {
    let mut limits = limits;
    if cfg!(windows) {
        limits.memory_high_bytes = 0;
        limits.memory_max_bytes = 0;
        limits.memory_swap_bytes = 0;
    } else if !support.swap {
        limits.memory_high_bytes = 0;
    }
    limits
}

fn render(limits: &BashLimits) -> String {
    let mut parts = Vec::new();
    if limits.cpu_cores > 0 {
        parts.push(format!("{} cores", limits.cpu_cores));
    }
    if limits.memory_max_bytes > 0 {
        if limits.memory_high_bytes > 0 {
            parts.push(format!(
                "memory high {} / max {}",
                gib(limits.memory_high_bytes),
                gib(limits.memory_max_bytes)
            ));
        } else {
            parts.push(format!("memory max {}", gib(limits.memory_max_bytes)));
        }
    }
    if limits.memory_swap_bytes > 0 {
        parts.push(format!("swap max {}", gib(limits.memory_swap_bytes)));
    }
    let mechanism = if cfg!(windows) {
        "Windows Job Object"
    } else {
        "cgroup v2"
    };
    format!("{} ({mechanism})", parts.join(", "))
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn short_unit_id() -> String {
    // UUIDv7 的前 12 个十六进制字符是毫秒时间戳，同一毫秒内会重复，必须用完整随机尾部。
    format!("moc-bash-{}", Uuid::now_v7().simple())
}

#[cfg(target_os = "linux")]
fn detect() -> Support {
    let swap = swap_present();
    let available = probe();
    if !available {
        tracing::warn!("bash 资源限制不可用：systemd user manager 探测失败，命令将在无限制下执行");
    }
    Support { available, swap }
}

#[cfg(windows)]
fn detect() -> Support {
    Support {
        available: true,
        swap: false,
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn detect() -> Support {
    tracing::warn!("bash 资源限制不可用：当前平台没有可用的限流后端");
    Support {
        available: false,
        swap: false,
    }
}

#[cfg(target_os = "linux")]
fn probe() -> bool {
    use std::process::Stdio;

    std::process::Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "CPUQuota=100%",
            "-p",
            "MemoryMax=67108864",
            "--",
            "true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
fn swap_present() -> bool {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("SwapTotal:"))
                .and_then(|rest| rest.trim().strip_suffix("kB"))
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .is_some_and(|kib| kib > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> BashLimits {
        BashLimits {
            cpu_cores: 4,
            memory_high_bytes: 8 * GIB,
            memory_max_bytes: 12 * GIB,
            memory_swap_bytes: 4 * GIB,
        }
    }

    fn text(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn properties_follow_the_configured_values() {
        assert_eq!(
            text(properties(&limits())),
            vec![
                "-p",
                "CPUQuota=400%",
                "-p",
                "MemoryHigh=8589934592",
                "-p",
                "MemoryMax=12884901888",
                "-p",
                "MemorySwapMax=4294967296",
            ]
        );
        let only_cpu = BashLimits {
            cpu_cores: 2,
            memory_high_bytes: 0,
            memory_max_bytes: 0,
            memory_swap_bytes: 0,
        };
        assert_eq!(text(properties(&only_cpu)), vec!["-p", "CPUQuota=200%"]);
        let none = BashLimits {
            cpu_cores: 0,
            ..only_cpu
        };
        assert!(properties(&none).is_empty());
    }

    #[test]
    fn trim_skips_memory_high_without_swap() {
        let trimmed = trim(
            limits(),
            Support {
                available: true,
                swap: false,
            },
        );
        assert_eq!(trimmed.memory_high_bytes, 0);
        assert_eq!(trimmed.memory_max_bytes, 12 * GIB);
        if !cfg!(windows) {
            let kept = trim(
                limits(),
                Support {
                    available: true,
                    swap: true,
                },
            );
            assert_eq!(kept.memory_high_bytes, 8 * GIB);
        }
    }

    #[test]
    fn resolve_only_returns_limits_when_enabled() {
        let mut config = BashLimitsConfig::default();
        assert!(config.resolve().is_none());
        config.enabled = true;
        assert_eq!(config.resolve(), Some(limits()));
    }

    #[test]
    fn validation_names_the_field_and_rejects_empty_or_inverted_limits() {
        let mut config = BashLimitsConfig::default();
        config.enabled = true;
        assert!(config.validate().is_ok());

        config.cpu_cores = 0;
        config.memory_high_bytes = 0;
        config.memory_max_bytes = 0;
        config.memory_swap_bytes = 0;
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("至少要配置一项限制")
        );

        config.memory_max_bytes = 12 * GIB;
        config.memory_high_bytes = 16 * GIB;
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("不得超过 memory_max_bytes")
        );

        config.memory_high_bytes = 16 * MIB;
        assert!(config.validate().unwrap_err().contains("至少 64 MiB"));

        config.memory_high_bytes = 8 * GIB;
        config.cpu_cores = 257;
        assert!(config.validate().unwrap_err().contains("不得超过 256"));

        config.cpu_cores = 4;
        config.enabled = false;
        config.memory_high_bytes = 1;
        assert!(config.validate().is_ok(), "关闭时不做限制性校验");
    }

    #[test]
    fn windows_cpu_rate_is_a_hundredth_of_a_percent_of_total_cores() {
        let limits = limits();
        assert_eq!(windows_cpu_rate(limits, 12), Some(3333));
        assert_eq!(windows_cpu_rate(limits, 4), None, "核数不少于总数时不限制");
        assert_eq!(windows_cpu_rate(limits, 0), None);
        let mut unlimited = limits;
        unlimited.cpu_cores = 0;
        assert_eq!(windows_cpu_rate(unlimited, 12), None);
        let mut one = limits;
        one.cpu_cores = 1;
        assert_eq!(windows_cpu_rate(one, 12), Some(833));
    }

    #[test]
    fn render_mentions_each_configured_cap() {
        let rendered = render(&limits());
        assert!(rendered.contains("4 cores"));
        assert!(rendered.contains("memory high 8.0 GiB / max 12.0 GiB"));
        assert!(rendered.contains("swap max 4.0 GiB"));
        let without_high = render(&BashLimits {
            memory_high_bytes: 0,
            ..limits()
        });
        assert!(without_high.contains("memory max 12.0 GiB"));
        assert!(!without_high.contains("memory high"));
    }

    #[test]
    fn unit_ids_are_unique_per_command() {
        let first = short_unit_id();
        let second = short_unit_id();
        assert_ne!(first, second);
        assert!(first.starts_with("moc-bash-"));
        assert!(first.len() <= 64);
    }

    #[test]
    fn scope_failures_are_recognized_from_stderr() {
        assert!(failure_in(
            "Failed to start transient scope unit: Unit run-p1-i2.scope was already loaded"
        ));
        assert!(!failure_in("compile error"));
    }
}

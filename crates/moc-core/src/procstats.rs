//! 进程资源采样：RSS / CPU / 磁盘与网络流量的进程级环形记录。
//!
//! 采样在独立线程上以 1Hz 进行，缓冲生命周期等于进程：会话切换
//! （`/new` `/resume` `/close`）不清空，进程退出即失，不写入任何会话储存。
//! 网络字节是经 reqwest 收发的 HTTP(S) 流量（见 `moc_llm::HTTP_RX_BYTES`），
//! 不含 stdio 传输的 MCP 流量。

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use moc_llm::{HTTP_RX_BYTES, HTTP_TX_BYTES};

/// 采样间隔。
pub const INTERVAL: Duration = Duration::from_secs(1);
/// 环形缓冲容量：1Hz × 600 样本 = 10 分钟窗口。
pub const WINDOW_SECS: u64 = 600;

/// 一次采样的原始快照；累计字段保存计数器原值，速率由相邻样本求差得出。
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// 采样时刻（单调时钟，仅用于横轴与差分）。
    pub at: Instant,
    /// 常驻内存（字节）。
    pub rss_bytes: u64,
    /// CPU 使用率（单核=100% 口径，多核可超过 100）。
    pub cpu_pct: f32,
    /// 进程启动以来磁盘读字节。
    pub disk_read_bytes: u64,
    /// 进程启动以来磁盘写字节。
    pub disk_write_bytes: u64,
    /// 进程启动以来 HTTP(S) 收字节。
    pub net_rx_bytes: u64,
    /// 进程启动以来 HTTP(S) 发字节。
    pub net_tx_bytes: u64,
}

struct Recorder {
    buf: VecDeque<Sample>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(WINDOW_SECS as usize),
        }
    }

    fn push(&mut self, sample: Sample) {
        if self.buf.len() == WINDOW_SECS as usize {
            self.buf.pop_front();
        }
        self.buf.push_back(sample);
    }
}

static RECORDER: LazyLock<Mutex<Recorder>> = LazyLock::new(|| Mutex::new(Recorder::new()));
static SPAWNED: OnceLock<()> = OnceLock::new();

/// 启动后台采样线程；幂等，重复调用不产生额外线程。
pub fn start() {
    SPAWNED.get_or_init(|| {
        std::thread::Builder::new()
            .name("procstats".into())
            .spawn(sample_loop)
            .expect("spawn procstats thread");
    });
}

/// 窗口内全部样本（最老在前）；未启动或尚无样本时为空。
pub fn samples() -> Vec<Sample> {
    RECORDER.lock().buf.iter().copied().collect()
}

/// 最新一份样本。
pub fn latest() -> Option<Sample> {
    RECORDER.lock().buf.back().copied()
}

/// 由相邻样本差分得到每秒速率序列：每项是 `(后一个样本的时刻, 速率)`。
///
/// 计数器回退（理论上不会发生）或时间倒退的样本对被跳过。
pub fn rates(samples: &[Sample], field: impl Fn(&Sample) -> u64) -> Vec<(Instant, f64)> {
    let mut out = Vec::with_capacity(samples.len().saturating_sub(1));
    for pair in samples.windows(2) {
        let (prev, cur) = (pair[0], pair[1]);
        let dt = (cur.at - prev.at).as_secs_f64();
        if let Some(delta) = field(&cur).checked_sub(field(&prev))
            && dt > 0.0
        {
            out.push((cur.at, delta as f64 / dt));
        }
    }
    out
}

fn sample_loop() {
    let Ok(mut system) = System::new() else {
        tracing::warn!("procstats: System::new 失败，采样未启动");
        return;
    };
    let pid = Pid::from_u32(std::process::id());
    let kind = ProcessRefreshKind::nothing()
        .with_memory()
        .with_cpu()
        .with_disk_usage();
    let mut next = Instant::now();
    loop {
        next += INTERVAL;
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind);
        if let Some(process) = system.process(pid) {
            let disk = process.disk_usage();
            RECORDER.lock().push(Sample {
                at: Instant::now(),
                rss_bytes: process.memory(),
                cpu_pct: process.cpu_usage(),
                disk_read_bytes: disk.total_read_bytes,
                disk_write_bytes: disk.total_written_bytes,
                net_rx_bytes: HTTP_RX_BYTES.load(Ordering::Relaxed),
                net_tx_bytes: HTTP_TX_BYTES.load(Ordering::Relaxed),
            });
        }
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at: Instant, rss: u64, counter: u64) -> Sample {
        Sample {
            at,
            rss_bytes: rss,
            cpu_pct: 0.0,
            disk_read_bytes: counter,
            disk_write_bytes: counter,
            net_rx_bytes: counter,
            net_tx_bytes: counter,
        }
    }

    #[test]
    fn ring_evicts_oldest_at_capacity() {
        let mut recorder = Recorder::new();
        let start = Instant::now();
        for i in 0..WINDOW_SECS + 10 {
            recorder.push(sample(start + Duration::from_secs(i), i, i));
        }
        assert_eq!(recorder.buf.len(), WINDOW_SECS as usize);
        // 最老的 10 个被淘汰，缓冲里第一个应是 i=10。
        assert_eq!(recorder.buf.front().unwrap().rss_bytes, 10);
        assert_eq!(recorder.buf.back().unwrap().rss_bytes, WINDOW_SECS + 9);
    }

    #[test]
    fn rates_are_per_second_deltas() {
        let start = Instant::now();
        let samples = [
            sample(start, 0, 1000),
            sample(start + Duration::from_secs(1), 0, 1600),
            sample(start + Duration::from_secs(2), 0, 2000),
        ];
        let rates: Vec<f64> = rates(&samples, |s| s.net_rx_bytes)
            .into_iter()
            .map(|(_, rate)| rate)
            .collect();
        assert_eq!(rates, vec![600.0, 400.0]);
    }

    #[test]
    fn rates_skip_counter_resets() {
        let start = Instant::now();
        let samples = [
            sample(start, 0, 1000),
            sample(start + Duration::from_secs(1), 0, 500),
            sample(start + Duration::from_secs(2), 0, 700),
        ];
        let rates: Vec<f64> = rates(&samples, |s| s.disk_read_bytes)
            .into_iter()
            .map(|(_, rate)| rate)
            .collect();
        assert_eq!(rates, vec![200.0]);
    }
}

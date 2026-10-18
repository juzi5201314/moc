#!/usr/bin/env python3
"""soft 压缩的可复现对比基准。

同一份真实会话（固定剧本 + 真实模型 + 真实工具）分别用不同的 soft 摘要实现压缩，
然后测量「压缩后还记得多少」与「是否像没压缩过」：

  - 事实保真：18 个预埋事实在最终 checkpoint 里的命中数；
  - 探针正确率：18 道单选题（含两条被否决方案、两段长粘贴里的数字），答案唯一；
  - 连贯性：当前进度、下一步、第 4/5 步是否与既定计划一致（0-4）；
  - 检索回填：回答探针时被迫回查 history:// 或磁盘的工具调用数；
  - 体积：每代 checkpoint 字符数、tokens_before/after；
  - 退化：checkpoint 短于最短长度视为塌缩（脚本以退出码 1 报告）。

用法：
    scripts/soft-compaction-bench.py --model <provider>/<model> --methods moc,grok_build
    scripts/soft-compaction-bench.py --methods moc --generations 1
    scripts/soft-compaction-bench.py --model p/m --methods grok_build --keep
    scripts/soft-compaction-bench.py --session ~/.omp/agent/sessions/<项目>/<时间>_<id>.jsonl --methods moc

依赖：Python 3.8+、仓库根已构建的 `target/debug/moc`，以及可用的 `~/.moc/config.toml`
（模型与供应商配置只读复制到临时 HOME，脚本不会写你的会话目录）。

默认 `--generations 2`：连续压缩两次，专门抓「第二次压缩把上下文丢成一句话」这类问题。

`--session` 导入真实 omp 会话（只读，源文件不动）：把 omp 的 message 条目按 1:1 转成 moc
会话，cwd 指向临时目录、权限固定为 workspace（bash 走 bwrap，模型无法改到真实仓库）。
出题只取「压缩后会被丢弃」的那段历史：头部 24 条正文考长程记忆，最后一条用户指令之前的
24 条考当前状态；文件召回取成功 edit/write 的路径。报告默认只给命中率，加 `--show-anchors`
才打印锚点原文。真实的 300k 级会话需要 `--max-output-tokens 32768` 之类的输出上限，
否则压缩输入预算装不下全部历史。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_BIN = REPO / "target" / "debug" / "moc"
DEFAULT_CONFIG = Path.home() / ".moc" / "config.toml"
MIN_CHECKPOINT_CHARS = 600

# ---------------------------------------------------------------------------
# 固定剧本：一个小的 Rust crate，事实分散在源码、文档、基准数据与用户决策里。
# ---------------------------------------------------------------------------

SCENARIO: dict[str, str] = {
    # 故意不放 Cargo.toml：模型若自作主张跑 cargo，会在毫秒级报“找不到清单”，
    # 不会下载依赖、也不会真的编译。
    "crates/ingest/src/lib.rs": """//! ingest：上游事件聚合与窗口管理。

pub mod legacy;
pub mod metrics;
pub mod window;

use thiserror::Error;

/// ingest 的统一错误类型。
#[derive(Debug, Error)]
pub enum IngestError {
    #[error("io 失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("窗口已过期: id={id} age_ms={age_ms}")]
    Expired { id: u64, age_ms: u64 },
}
""",
    "crates/ingest/src/window.rs": """//! 事件窗口的存活期管理与轮转。

use smallvec::SmallVec;
use std::collections::HashMap;

/// 窗口空闲多久后视为过期。
pub const WINDOW_TTL_MS: u64 = 30_000;

/// 单个窗口聚合的上游会话状态。
#[derive(Debug, Clone)]
pub struct Window {
    pub id: u64,
    pub opened_at_ms: u64,
    pub last_seen_ms: u64,
    pub pinned: bool,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl Window {
    pub fn new(id: u64, now_ms: u64) -> Self {
        Self {
            id,
            opened_at_ms: now_ms,
            last_seen_ms: now_ms,
            pinned: false,
            bytes_in: 0,
            bytes_out: 0,
        }
    }

    /// 记录一次心跳，刷新存活时间。
    pub fn touch(&mut self, now_ms: u64) {
        self.last_seen_ms = now_ms;
    }

    /// 距上次心跳的空闲时长。
    pub fn idle_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_seen_ms)
    }
}

/// 全部活跃窗口的容器。
#[derive(Debug, Default)]
pub struct WindowState {
    pub windows: HashMap<u64, Window>,
    pub next_id: u64,
    pub rotated_total: u64,
}

impl WindowState {
    pub fn open(&mut self, now_ms: u64) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.windows.insert(id, Window::new(id, now_ms));
        id
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    /// 标记窗口为 pinned，pinned 窗口不参与淘汰。
    pub fn pin(&mut self, id: u64) -> bool {
        match self.windows.get_mut(&id) {
            Some(window) => {
                window.pinned = true;
                true
            }
            None => false,
        }
    }
}

/// 把已结算的窗口写入下游。
///
/// TODO(rotate): 这里目前写死了 `HashMap` 的迭代顺序，接入 rotate_windows
/// 后改成按 recency 顺序 flush。
pub fn flush(state: &mut WindowState, now_ms: u64) -> usize {
    let mut flushed = 0usize;
    for window in state.windows.values_mut() {
        if window.idle_ms(now_ms) > WINDOW_TTL_MS {
            flushed += 1;
        }
    }
    flushed
}

/// 保留的容量类型别名：窗口数量有界，热路径上避免 HashMap 的散列与分配。
pub type WindowVec = SmallVec<[Window; 4]>;
""",
    "crates/ingest/src/legacy.rs": """//! 旧路径：上游连接复用与同步写盘，线上仍有流量，暂不迁移。

use std::collections::VecDeque;
use std::io::{self, Write};

/// 旧路径的批量写盘器：攒够一批就同步写一次。
pub struct LegacyWriter<W: Write> {
    inner: W,
    pending: VecDeque<Vec<u8>>,
    batch: usize,
}

impl<W: Write> LegacyWriter<W> {
    pub fn new(inner: W, batch: usize) -> Self {
        Self {
            inner,
            pending: VecDeque::new(),
            batch: batch.max(1),
        }
    }

    pub fn push(&mut self, line: &[u8]) -> io::Result<()> {
        self.pending.push_back(line.to_vec());
        if self.pending.len() >= self.batch {
            self.drain()?;
        }
        Ok(())
    }

    fn drain(&mut self) -> io::Result<()> {
        while let Some(line) = self.pending.pop_front() {
            self.inner.write_all(&line)?;
            self.inner.write_all(b"\\n")?;
        }
        Ok(())
    }
}
""",
    "crates/ingest/src/metrics.rs": """//! ingest 的指标采集；导出侧按快照读，避免读写争用同一把锁。

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Metrics {
    windows_rotated: AtomicU64,
    flush_calls: AtomicU64,
    flush_nanos: AtomicU64,
    rotate_calls: AtomicU64,
    rotate_nanos: AtomicU64,
    pinned_hits: AtomicU64,
}

impl Metrics {
    /// 记录一次 flush 的耗时与调用次数。
    pub fn record_flush(&self, nanos: u64) {
        self.flush_calls.fetch_add(1, Ordering::Relaxed);
        self.flush_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// 记录一次轮转的耗时与调用次数。
    pub fn record_rotate(&self, nanos: u64) {
        self.rotate_calls.fetch_add(1, Ordering::Relaxed);
        self.rotate_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// 记录本轮回收的窗口数。
    pub fn record_rotated(&self, count: u64) {
        if count == 0 {
            return;
        }
        self.windows_rotated.fetch_add(count, Ordering::Relaxed);
    }

    /// 记录一次 pinned 窗口跳过淘汰。
    pub fn record_pinned_skip(&self) {
        self.pinned_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricSnapshot {
        MetricSnapshot {
            windows_rotated: self.windows_rotated.load(Ordering::Relaxed),
            flush_calls: self.flush_calls.load(Ordering::Relaxed),
            flush_nanos: self.flush_nanos.load(Ordering::Relaxed),
            rotate_calls: self.rotate_calls.load(Ordering::Relaxed),
            rotate_nanos: self.rotate_nanos.load(Ordering::Relaxed),
            pinned_hits: self.pinned_hits.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MetricSnapshot {
    pub windows_rotated: u64,
    pub flush_calls: u64,
    pub flush_nanos: u64,
    pub rotate_calls: u64,
    pub rotate_nanos: u64,
    pub pinned_hits: u64,
}

impl MetricSnapshot {
    pub fn flush_avg_nanos(&self) -> u64 {
        if self.flush_calls == 0 {
            return 0;
        }
        self.flush_nanos / self.flush_calls
    }

    pub fn rotate_avg_nanos(&self) -> u64 {
        if self.rotate_calls == 0 {
            return 0;
        }
        self.rotate_nanos / self.rotate_calls
    }
}
""",
    "docs/ingest.md": """# Ingest 服务

`ingest` 负责把上游的流式事件聚合进「窗口」，并在窗口过期后回收资源。
线上有两套实现：新路径 `crates/ingest/src/window.rs`，老路径
`crates/ingest/src/legacy.rs`（仍有线上流量，暂不迁移）。

## 窗口生命周期

1. 上游连接建立时调用 `WindowState::open`，得到一个自增窗口 id。
2. 每次心跳调用 `Window::touch`，刷新 `last_seen_ms`。
3. 轮转线程定期扫描，空闲超过 TTL 的窗口被回收并计入 `rotated_total`。
4. 被 `pin` 的窗口不参与淘汰：上层用它承载尚未落盘的审计批次。

## TTL 参数

| 参数 | 取值 | 说明 |
| --- | --- | --- |
| `WINDOW_TTL_MS` | 30_000 | 窗口空闲多久后回收 |
| `ROTATE_TICK_MS` | 5_000 | 轮转线程扫描间隔 |
| `MAX_WINDOWS` | 4 | 同时活跃窗口上限（有界，故用 `SmallVec`） |

TTL 的取值依据：上游心跳间隔 15 秒，客户端允许丢两次心跳，因此 TTL 取心跳间隔的
整数倍。改大 TTL 会直接抬高常驻内存，容器内存上限 512MB 时没有余量。

## 待办

1. `rotate_windows` 的骨架落在 `window.rs`，尚未接进 `flush`。
2. 轮转回收要区分 pinned 窗口，否则审计批次会丢。
3. 指标计数从 `flush` 与轮转线程收敛到 `rotate_windows` 内部。
4. 本文档 TTL 表随常量一起更新。
5. 轮转路径补 `tracing` 日志（`debug` 级，带窗口 id 与空闲时长）。

## 基准

见 `bench/results.md`。当前基线是 18_240 行/秒，p99 8.7ms；
最近一次优化后达到 24_910 行/秒，p99 3.2ms。
""",
    "bench/results.md": """# Ingest 基准结果（脚本 `bench/run.sh`）

环境：x86_64，16 vCPU，64GB，Linux 6.18；上游模拟器单进程喂事件。
每组跑 30 秒，取中位数；行 = 一条上游事件。

# 基线（优化前）

| 场景 | 行/秒 | p50 | p99 | max |
| --- | --- | --- | --- | --- |
| 单窗口 1 路 | 18_240 | 4.1ms | 8.7ms | 21.3ms |
| 单窗口 4 路 | 17_980 | 4.4ms | 9.2ms | 24.8ms |
| 混跑 8 路 | 16_310 | 5.8ms | 12.4ms | 33.1ms |

# 优化后

| 场景 | 行/秒 | p50 | p99 | max |
| --- | --- | --- | --- | --- |
| 单窗口 1 路 | 24_910 | 1.6ms | 3.2ms | 9.4ms |
| 单窗口 4 路 | 23_640 | 1.8ms | 3.9ms | 11.7ms |
| 混跑 8 路 | 21_070 | 2.3ms | 5.1ms | 15.2ms |

基准不进入 CI：CI 只跑功能测试，基准在夜间 `nightly-ingest-audit` 里跑。
""",
    "crates/ingest/tests/window_rotation.rs": """//! 窗口轮转的用例骨架（尚未实现）。

#[test]
fn expired_windows_are_reclaimed() {}

#[test]
fn pinned_windows_survive_rotation() {}

#[test]
fn rotation_reports_reclaimed_count() {}
""",
}

AUDIT_FILES = (
    "crates/ingest/src/window.rs",
    "crates/ingest/src/legacy.rs",
    "crates/ingest/src/metrics.rs",
    "crates/ingest/src/lib.rs",
)

AUDIT_TEMPLATES = (
    "阈值 {value} 未引用常量，建议改为引用符号",
    "注释缺少单位说明（毫秒与秒混用）",
    "热路径出现 clone()，建议改为借用",
    "错误分支只打日志、没有返回 Err",
    "缺少 debug_assert! 校验不变量",
    "锁粒度偏大：整个遍历期间持锁",
    "减法未用 saturating_sub，存在下溢风险",
    "输出依赖 HashMap 迭代顺序，不可复现",
)


def audit_report() -> str:
    """ci/audit.sh 的产物：300 条告警，只有一条 error。

    确定性生成，保证同一份脚本每次得到同一份历史；warn 的阈值都落在 1k–21k，
    不会和 error 那条的 12_500 或题面里的 30k/45k/60k 混淆。
    """
    lines = [
        "# ingest 配置审计报告（ci/audit.sh 生成，勿手改）",
        "",
        "范围：crates/ingest/src 全部文件；规则集 aud-v3。",
        "结论：300 条告警，其中 1 条 error、299 条 warn。error 必须修，warn 按季度清理。",
        "",
    ]
    for index in range(1, 301):
        line_no = (index * 7) % 400 + 8
        path = AUDIT_FILES[index % len(AUDIT_FILES)]
        level = "error" if index == 137 else "warn"
        if index == 137:
            message = "硬编码阈值 12_500 未引用常量（WINDOW_TTL_MS）"
        else:
            value = 500 * (index % 40 + 2)
            message = AUDIT_TEMPLATES[index % len(AUDIT_TEMPLATES)].format(value=f"{value:_}")
        lines.append(f"- AUD-{index:04d} [{level}] {path}:{line_no} {message}")
    return "\n".join(lines)


def incident_review() -> str:
    """线上故障的时间线与复盘，含一条被否决的跨进程方案。"""
    lines = [
        "# 故障复盘：ingest 窗口回收误判（07-16）",
        "",
        "状态：已恢复；影响时段 21:40–22:07（27 分钟）。编写：值班组。",
        "",
        "## 时间线",
        "",
        "- 21:38 上游连接数从 8 升到 12，超过 MAX_WINDOWS 设计值，开始排队。",
        "- 21:39 轮转线程最后一次正常回收，日志 `rotate done reclaimed=0`。",
        "- 21:40 首个超时告警；`flush` 单次耗时 300ms 以上。",
    ]
    for step in range(64):
        minute = 21 * 60 + 41 + step // 2
        clock = f"{minute // 60:02d}:{minute % 60:02d}"
        rps = 22_150 - step * 200 if step < 32 else 15_760 + (step - 32) * 130
        p99 = 9 + step if step < 33 else 42 - (step - 33)
        lines.append(
            f"- {clock} rps={rps} p99={p99}ms 存活窗口={4 + step % 9} pinned={step % 5} "
            f"轮转耗时={step * 3 % 47}ms flush 耗时={8 + step % 23}ms"
        )
    lines += [
        "- 22:02 定位到两处独立缺陷，临时把轮转线程的回收动作摘除，服务恢复。",
        "- 22:07 下游审计表补齐三个批次，告警解除。",
        "",
        "## 现象",
        "",
        "上游仍在正常心跳，连接也没有断开，但窗口数量持续上涨，很快超过了设计上限 4。",
        "值班先怀疑上游重连风暴，抓包后发现连接是复用的，排除了这个方向。",
        "随后监控显示 `flush` 的调用次数没变、耗时却从 1ms 级涨到 300ms 级，",
        "而轮转线程的回收计数从那之后再没有增长，说明回收根本没有发生。",
        "",
        "## 排查过程",
        "",
        "1. 先看日志：轮转线程每 5 秒打一次 `rotate done`，故障期间这些日志只剩心跳，",
        "   回收数一直是 0。日志里没有 panic，也没有 io 错误，说明它是在等锁。",
        "2. 再看锁：`flush` 与轮转线程用的是同一把 `Mutex<WindowState>`，`flush` 在持锁期间",
        "   遍历全部窗口并逐条写盘，窗口越多持锁越久，形成正反馈。",
        "3. 然后看淘汰条件：`flush` 里只判断 `idle_ms > WINDOW_TTL_MS`，没有排除 pinned，",
        "   而 pinned 正是承载未落盘审计批次的窗口。这个缺陷在正常时被 TTL 掩盖，",
        "   一旦积压超过 45 秒就会集中爆发。",
        "4. 最后看计数：`rotate_windows` 与 `flush` 各自 `fetch_add`，"
        "`rotated_total` 被重复累加，告警阈值因此偏低，掩盖了前 3 分钟的问题。",
        "5. 复现方式：把 TTL 临时调到 3 秒、并发写 12 条流，2 分钟内必现；"
        "单独跑 `window_rotation` 的三个用例全是空壳，所以单元测试完全没有拦住。",
        "",
        "## 为什么没被测试拦住",
        "",
        "`crates/ingest/tests/window_rotation.rs` 里的三个用例只有函数名、没有断言，",
        "跑起来一律通过。集成测试只验证了「窗口能开、能 touch」，没有覆盖回收与 pinned 交互。",
        "复盘结论：骨架用例必须先补断言再合入，否则等于没有测试。",
        "",
        "## 根因",
        "",
        "1. `flush` 与轮转线程共用同一把 `Mutex<WindowState>`，持锁遍历全部窗口，",
        "   轮转线程长期拿不到锁，过期窗口越积越多。",
        "2. 到期判定只看 `idle_ms`，没有排除 pinned 窗口，`flush` 把它们当成可回收，",
        "   审计批次因此在落盘前被清掉。",
        "3. `rotate_windows` 与 `flush` 各自计数，`rotated_total` 被加了两次，告警阈值失真。",
        "",
        "## 影响",
        "",
        "- 丢弃事件 1_204 条，集中在 21:41–21:44。",
        "- p99 峰值 42ms；吞吐从 22_150 行/秒掉到 15_760 行/秒。",
        "- 下游审计表缺 3 个批次，已手工补录并核对。",
        "",
        "## 候选方案与结论",
        "",
        "| 方案 | 结论 | 理由 |",
        "| --- | --- | --- |",
        "| 把窗口状态迁到 Redis，多进程共享 | 否决 | 跨进程一致性与 RTT 都不可接受 |",
        "| 缩小锁粒度，flush 按分片持锁 | 采纳 | 改动局限于 window.rs，不需要新依赖 |",
        "| 轮转线程独占回收职责 | 采纳 | 与第 3 步计划一致：计数收敛到 rotate_windows |",
        "| 增大 TTL 掩盖问题 | 否决 | 内存上限固定，TTL 已是上限 |",
        "",
        "## 后续动作",
        "",
        "1. 回收职责只保留在 `rotate_windows`，`flush` 不再判过期。",
        "2. pinned 窗口在两条路径上都要跳过，并补用例覆盖。",
        "3. 指标归属收敛后，告警阈值按新口径重算。",
        "4. 轮转路径补 `tracing` 日志，带上窗口 id 与空闲时长。",
    ]
    return "\n".join(lines)


# 长文放进工作区当文件读：REPL 逐行喂输入，多行粘贴会被拆成几百条 prompt。
SCENARIO["bench/audit-report.md"] = audit_report()
SCENARIO["bench/incident-review.md"] = incident_review()


TRANSCRIPT: list[str] = [
    "先熟悉代码。读 crates/ingest/src/window.rs 的 1-80 行，然后用一行告诉我 WINDOW_TTL_MS 现在的取值（带下划线原样）。",
    "把 TTL 从 30 秒改成 45 秒：常量写成 45_000，行内注释写「上游心跳 15 秒，留 3 倍余量」。只改 crates/ingest/src/window.rs 一个文件，绝对不要碰 crates/ingest/src/legacy.rs，那里跑着线上老路径。改完只回一句确认。",
    "记下设计决定，不用改代码：新增 pub fn rotate_windows(state: &mut WindowState, now_ms: u64) -> usize，返回本轮回收到期的窗口数；窗口容器换成 SmallVec<[Window; 4]>，不要 HashMap<u64, Window>——同时活跃窗口有界（上限 4），热路径点查用散列表是浪费。回一句收到。",
    "错误处理统一 thiserror：在 crates/ingest/src/lib.rs 里保留 IngestError::Expired { id: u64, age_ms: u64 }，不要引入 anyhow。然后把 rotate_windows 的骨架写进 window.rs：按 now_ms 回收空闲超过 TTL 的非 pinned 窗口，返回回收数；先不要接进 flush()。写完回一句确认。",
    "补充背景，这是一次设计评审的结论，请记住：我们否决了用 lru crate 的 LRU 缓存来管理窗口，原因是 pinned 窗口会违反淘汰顺序；容器只用 SmallVec。另外进程内存上限是 512MB，所以 TTL 45 秒就是上限，不能再往上加。收尾计划共 5 步：第 1 步改 TTL 常量，第 2 步加 rotate_windows 骨架，第 3 步把 metrics 计数移到 rotate_windows 内部，第 4 步更新 docs/ingest.md 的 TTL 表，第 5 步给轮转路径加 tracing 日志。回一句「已记住」。",
    "读 bench/results.md 全文，用两行分别回答：优化前和优化后的吞吐（行/秒，带下划线原样），以及优化前和优化后的 p99。不要展开。",
    "读 bench/audit-report.md 全文，读完只回一行：唯一一条 error 级规则的编号，以及它报出的硬编码阈值。",
    "再读 crates/ingest/src/metrics.rs 和 crates/ingest/tests/window_rotation.rs，然后回答：flush 耗时指标记录在哪个结构里？一行；pinned 窗口为什么不能回收？一行。",
    "读 bench/incident-review.md 全文，然后回答三点：根因第一条、一共丢弃多少条事件、p99 峰值是多少。",
    "顺带问个和任务无关的：nvme 的队列深度怎么看？给一行命令就行，不要执行。",
    "团队纪律记一下：全量测试预算 1 分钟，禁止在测试里做 bench；提交信息用 conventional commit，scope 写 ingest；注释和文档用中文。回一句收到。",
    "另外确认一下：CI 缓存是每次都重建吗？只回「会」或「不会」。",
    "用 bash 跑 wc -l crates/ingest/src/window.rs，把行数告诉我。",
    "先停在这里，等我下一步指令。",
]

MCQ_MARK = "下面是一组关于本次 ingest 任务的单选题"
CONTINUITY_MARK = "继续推进这个任务"
ACTION_MARK = "把第 4 步现在做完"

# 单选题：题干 + 正确项。顺序即题号，新增题目直接往后追加。
MCQ_QUESTIONS: list[tuple[str, str]] = [
    ("窗口 TTL 常量 WINDOW_TTL_MS 现在是多少？A) 30_000 B) 45_000 C) 60_000 D) 15_000。", "B"),
    (
        "为什么 TTL 取这个值？A) 上游心跳 5 秒，留 6 倍余量 B) 上游心跳 15 秒，留 3 倍余量 "
        "C) 上游心跳 30 秒，留 1.5 倍余量 D) 没有依据，随便取的。",
        "B",
    ),
    ("新增的窗口回收函数叫什么？A) rotate_windows B) sweep_windows C) evict_windows D) refresh_windows。", "A"),
    (
        "rotate_windows 返回什么？A) 空 B) 本轮回收的窗口数（usize） C) 被回收的窗口列表 "
        "D) bool 表示是否还有窗口。",
        "B",
    ),
    (
        "窗口容器最终用哪种结构？A) HashMap<u64, Window> B) BTreeMap<u64, Window> "
        "C) SmallVec<[Window; 4]> D) VecDeque<Window>。",
        "C",
    ),
    (
        "错误类型怎么定义？A) anyhow::Error B) thiserror 的 IngestError::Expired { id, age_ms } "
        "C) 直接 panic D) std::io::Error。",
        "B",
    ),
    (
        "哪个文件绝对不要改？A) crates/ingest/src/window.rs B) crates/ingest/src/legacy.rs "
        "C) docs/ingest.md D) crates/ingest/tests/window_rotation.rs。",
        "B",
    ),
    ("哪个方案被否决了？A) 用 lru crate 的 LRU 缓存 B) 用 SmallVec C) 用 thiserror D) 新增 rotate_windows。", "A"),
    ("否决它的核心理由？A) 性能不够 B) pinned 窗口会违反淘汰顺序 C) 依赖太多 D) 不支持 no_std。", "B"),
    ("进程内存上限是多少？A) 128MB B) 512MB C) 1GB D) 2GB。", "B"),
    ("优化后的吞吐是多少？A) 18_240 行/秒 B) 24_910 行/秒 C) 31_500 行/秒 D) 9_870 行/秒。", "B"),
    ("优化后的 p99 延迟是多少？A) 8.7ms B) 3.2ms C) 1.4ms D) 12.0ms。", "B"),
    (
        "收尾计划第 3 步是什么？A) 把 rotate_windows 接进 flush() B) 把 metrics 计数移到 "
        "rotate_windows 内部 C) 更新 docs/ingest.md 的 TTL 表 D) 加 tracing 日志。",
        "B",
    ),
    ("全量测试的时间预算是多少？A) 1 分钟 B) 5 分钟 C) 10 分钟 D) 不限制。", "A"),
    ("审计报告里唯一一条 error 级规则报出的硬编码阈值是多少？A) 12_500 B) 30_000 C) 45_000 D) 60_000。", "A"),
    ("那次故障一共丢弃了多少条事件？A) 120 B) 1_204 C) 4_512 D) 12_500。", "B"),
    ("故障期间 p99 峰值是多少？A) 8.7ms B) 12.4ms C) 42ms D) 118ms。", "C"),
    (
        "复盘里被否决的跨进程方案是哪个？A) 把窗口状态迁到 Redis B) 用 SmallVec 取代 HashMap "
        "C) 用 thiserror 统一错误 D) 新增 rotate_windows。",
        "A",
    ),
]

MCQ_KEY = {
    f"P{index}": answer
    for index, (_, answer) in enumerate(MCQ_QUESTIONS, 1)
}

PROBES: list[str] = [
    f"{MCQ_MARK}。只回答，不要解释，输出 {len(MCQ_KEY)} 行，每行格式严格为 `P<编号>: <字母>`。"
    + "".join(
        f"P{index}. {question}" for index, (question, _) in enumerate(MCQ_QUESTIONS, 1)
    ),
    "继续推进这个任务。用 3 句话回答：现在进行到哪一步、下一步的具体动作（点名函数或文件）、以及之后第 4 步和第 5 步。",
    "把第 4 步现在做完。",
]

# 18 个预埋事实；正则按 checkpoint 里可能出现的写法放宽。
FACT_CHECKS: list[tuple[str, str]] = [
    ("TTL=45_000", r"45[_ ,]?000|45\s*秒|45s"),
    ("心跳15s×3", r"15\s*(?:秒|s|seconds?)[^\n]{0,60}(?:3|三)\s*(?:倍|x|times)"),
    ("rotate_windows", r"rotate_windows"),
    ("返回usize", r"usize"),
    ("SmallVec", r"SmallVec"),
    ("thiserror/Expired", r"thiserror|IngestError::Expired"),
    ("legacy.rs", r"legacy\.rs"),
    ("否决lru", r"lru"),
    ("pinned", r"pinned"),
    ("512MB", r"512\s*MB"),
    ("吞吐24_910", r"24[_ ,]?910"),
    ("p99 3.2ms", r"3\.2\s*[/、,]\s*3\.9|3\.2\s*ms"),
    ("第3步metrics", r"metrics"),
    ("测试1分钟", r"1\s*(?:分钟|minute|min\b)|60\s*(?:秒|s\b)"),
    ("错误阈值12_500", r"12[_ ,]?500"),
    ("丢事件1_204", r"1[_ ,]?204"),
    ("故障p99 42ms", r"42\s*ms"),
    ("否决Redis", r"[Rr]edis"),
]

# 第 4 步是否真正落地，与连贯性同权计分。
ACTION_SCORE = 4

CONTINUITY_CHECKS: list[tuple[str, str]] = [
    ("进度", r"第\s*3\s*步|第三步|step\s*3"),
    ("下一步", r"metrics[^\n]{0,80}rotate_windows|rotate_windows[^\n]{0,80}metrics"),
    ("第4步", r"docs/ingest\.md"),
    ("第5步", r"tracing"),
]

MAX_SCORE = len(FACT_CHECKS) + len(MCQ_KEY) + len(CONTINUITY_CHECKS) + ACTION_SCORE


# ---------------------------------------------------------------------------
# 运行与解析
# ---------------------------------------------------------------------------


def fail(message: str) -> "NoReturn":
    print(f"错误：{message}", file=sys.stderr)
    raise SystemExit(2)


def write_scenario(work: Path) -> None:
    for relative, content in SCENARIO.items():
        path = work / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")


def run_repl(
    binary: Path,
    home: Path,
    work: Path,
    session_dir: Path,
    overlay: Path,
    script: str,
    timeout: int,
    log_path: Path,
    resume: str | None = None,
) -> str:
    command = [
        str(binary),
        "--repl",
        "--hide-thinking",
        "--cwd",
        str(work),
        "--session-dir",
        str(session_dir),
        "--config",
        str(overlay),
    ]
    if resume:
        command += ["--resume", resume]
    environment = dict(os.environ)
    environment["HOME"] = str(home)
    environment.pop("MOC_LOG", None)
    try:
        result = subprocess.run(
            command,
            input=script,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=environment,
        )
    except subprocess.TimeoutExpired:
        fail(f"`{binary}` 在 {timeout}s 内没有结束（见 {log_path}）")
    output = result.stdout + "\n" + result.stderr
    log_path.write_text(output, encoding="utf-8")
    return output


def session_file(session_dir: Path) -> Path:
    files = sorted(session_dir.rglob("*.jsonl"))
    if not files:
        fail(f"会话目录里没有 jsonl：{session_dir}")
    return files[0]


def entries(path: Path) -> list[dict]:
    out = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out


def keyed_messages(items: list[dict]) -> tuple[list[dict], list[dict]]:
    """拆出消息流与 compaction 记录。"""
    messages = []
    compactions = []
    for entry in items:
        if entry.get("type") == "compaction":
            compactions.append(entry)
            continue
        if entry.get("type") != "message":
            continue
        message = entry.get("message") or {}
        texts = []
        tools = []
        for part in message.get("content") or []:
            if "Text" in part:
                texts.append(part["Text"])
            elif "ToolCall" in part:
                tools.append(part["ToolCall"].get("name", ""))
        messages.append(
            {
                "role": message.get("role"),
                "text": "".join(texts),
                "tools": tools,
            }
        )
    return messages, compactions


def window_after(messages: list[dict], marker: str) -> tuple[list[dict], list[dict]]:
    """返回位于 marker 用户消息之后、下一条用户消息之前的所有消息。"""
    for index, message in enumerate(messages):
        if message["role"] == "User" and marker in message["text"]:
            tail = messages[index + 1 :]
            cut = []
            for item in tail:
                if item["role"] == "User":
                    break
                cut.append(item)
            return [messages[index]], cut
    return [], []


def parse_mcq(text: str) -> dict[str, str]:
    found = re.findall(r"P(\d+)\s*[:：]\s*([A-Da-d])", text or "")
    return {f"P{key}": value.upper() for key, value in found}


def score_facts(summary: str) -> tuple[int, list[str]]:
    missing = [name for name, pattern in FACT_CHECKS if not re.search(pattern, summary, re.I)]
    return len(FACT_CHECKS) - len(missing), missing


def score_continuity(answer: str) -> tuple[int, list[str]]:
    missing = [name for name, pattern in CONTINUITY_CHECKS if not re.search(pattern, answer, re.I)]
    return len(CONTINUITY_CHECKS) - len(missing), missing


def check_action(work: Path) -> bool:
    doc = work / "docs" / "ingest.md"
    if not doc.exists():
        return False
    text = doc.read_text(encoding="utf-8")
    return "45_000" in text and "30_000" not in text


def run_method(
    name: str,
    binary: Path,
    root: Path,
    home: Path,
    work: Path,
    base_sessions: Path,
    base_id: str,
    overlay_template: str,
    generations: int,
    timeout: int,
) -> dict:
    sessions = root / f"sessions-{name}"
    if sessions.exists():
        shutil.rmtree(sessions)
    shutil.copytree(base_sessions, sessions)
    if work.exists():
        shutil.rmtree(work)
    shutil.copytree(root / "work-base", work)
    overlay = root / f"overlay-{name}.toml"
    overlay.write_text(
        overlay_template.replace("{method}", name), encoding="utf-8"
    )
    script = "/compact\n" * generations + "\n".join(PROBES) + "\n/quit\n"
    output = run_repl(
        binary,
        home,
        work,
        sessions,
        overlay,
        script,
        timeout,
        root / f"log-{name}.txt",
        resume=base_id,
    )
    file = session_file(sessions)
    messages, compactions = keyed_messages(entries(file))
    summaries = [item.get("summary", "") for item in compactions]
    if not summaries:
        fail(f"{name}：没有产生 compaction（日志：{root / f'log-{name}.txt'}）\n{output[-2000:]}")

    final = summaries[-1]
    facts, missing_facts = score_facts(final)
    _, mcq_window = window_after(messages, MCQ_MARK)
    mcq_answer = "".join(item["text"] for item in mcq_window if item["role"] == "Assistant")
    mcq = parse_mcq(mcq_answer)
    correct = sum(1 for key, value in MCQ_KEY.items() if mcq.get(key) == value)
    retrieval = sum(len(item["tools"]) for item in mcq_window if item["role"] == "Assistant")

    _, continuity_window = window_after(messages, CONTINUITY_MARK)
    continuity_answer = "".join(
        item["text"] for item in continuity_window if item["role"] == "Assistant"
    )
    continuity, missing_steps = score_continuity(continuity_answer)

    _, action_window = window_after(messages, ACTION_MARK)
    action_ok = check_action(work)
    action_tools = sum(len(item["tools"]) for item in action_window if item["role"] == "Assistant")

    return {
        "method": name,
        "summaries": summaries,
        "facts": facts,
        "missing_facts": missing_facts,
        "mcq": correct,
        "mcq_total": len(MCQ_KEY),
        "continuity": continuity,
        "missing_steps": missing_steps,
        "retrieval": retrieval,
        "action_ok": action_ok,
        "action_tools": action_tools,
        "tokens": [
            (item.get("tokens_before"), item.get("tokens_after")) for item in compactions
        ],
        "collapsed": min(len(item) for item in summaries) < MIN_CHECKPOINT_CHARS,
        "score": facts + correct + continuity + (ACTION_SCORE if action_ok else 0),
    }


# ---------------------------------------------------------------------------
# 长会话模式：每轮压缩之间穿插一步真实工作，逐轮量化计划记忆
# ---------------------------------------------------------------------------

LONG_PLAN_STEPS = 5
LONG_CHINESE_NUMERALS = "一二三四五六七八九"
LONG_WINDOW = "crates/ingest/src/window.rs"
LONG_DOC = "docs/ingest.md"


# 只记录计划与决策，不落地代码：计划的每一步都留到后续轮次执行。
LONG_BASE: list[str] = [
    "先熟悉代码：读 crates/ingest/src/window.rs 的 1-80 行，然后用一行告诉我 WINDOW_TTL_MS 现在的取值（带下划线原样）。",
    "读 bench/results.md 全文，用两行分别回答：优化前和优化后的吞吐（行/秒，带下划线原样），以及优化前和优化后的 p99。不要展开。",
    "读 bench/audit-report.md 全文，读完只回一行：唯一一条 error 级规则的编号，以及它报出的硬编码阈值。",
    "再读 crates/ingest/src/metrics.rs 和 crates/ingest/tests/window_rotation.rs，然后回答：flush 耗时指标记录在哪个结构里？一行；pinned 窗口为什么不能回收？一行。",
    "读 bench/incident-review.md 全文，然后回答三点：根因第一条、一共丢弃多少条事件、p99 峰值是多少。",
    "补充背景，这是一次设计评审的结论，请记住：TTL 定为 45 秒（常量写 45_000，上游心跳 15 秒留 3 倍余量）；"
    "新增 pub fn rotate_windows(state: &mut WindowState, now_ms: u64) -> usize，返回本轮回收到期的窗口数，"
    "窗口容器换成 SmallVec<[Window; 4]>，不要 HashMap<u64, Window>；错误统一 thiserror，保留 "
    "IngestError::Expired { id: u64, age_ms: u64 }，不要引入 anyhow；绝对不要碰 crates/ingest/src/legacy.rs。"
    "另外两条硬约束：我们否决了用 lru crate 的 LRU 缓存来管理窗口，原因是 pinned 窗口会违反淘汰顺序；"
    "进程内存上限 512MB，TTL 45 秒就是上限，不能再往上加。"
    "收尾计划共 5 步：第 1 步改 TTL 常量，第 2 步加 rotate_windows 骨架，"
    "第 3 步把 metrics 计数移进 rotate_windows（签名加 metrics: &Metrics 参数），"
    "第 4 步更新 docs/ingest.md 的 TTL 表，"
    "第 5 步给轮转路径加 tracing 日志。回一句「已记住」。",
    "顺带问个和任务无关的：nvme 的队列深度怎么看？给一行命令就行，不要执行。",
    "团队纪律记一下：全量测试预算 1 分钟，禁止在测试里做 bench；提交信息用 conventional commit，"
    "scope 写 ingest；注释和文档用中文。回一句收到。",
    "另外确认一下：CI 缓存是每次都重建吗？只回「会」或「不会」。",
    "先停在这里，等我下一步指令。",
]


def window_text(work: Path) -> str:
    path = work / LONG_WINDOW
    return path.read_text(encoding="utf-8") if path.exists() else ""


def doc_text(work: Path) -> str:
    path = work / LONG_DOC
    return path.read_text(encoding="utf-8") if path.exists() else ""


def step_landed(work: Path) -> set[int]:
    """每一步是否真正落地，以工作区内容为准，不看模型自称完成了什么。"""
    window = window_text(work)
    doc = doc_text(work)
    flags = {
        1: "45_000" in window and "30_000" not in window,
        2: "fn rotate_windows" in window,
        3: "record_rotated" in window,
        4: "45_000" in doc,
        5: "tracing::" in window,
    }
    return {step for step, landed in flags.items() if landed}


def plan_truth(work: Path) -> tuple[set[int], int | None]:
    """当前应完成哪些步、下一步是哪一步；都以工作区状态为准。"""
    done = step_landed(work)
    pending = [step for step in range(1, LONG_PLAN_STEPS + 1) if step not in done]
    return done, (pending[0] if pending else None)


LONG_WORK: list[str] = [
    "执行计划第 1 步：把 crates/ingest/src/window.rs 里的 WINDOW_TTL_MS 从 30_000 改成 45_000，"
    "行内注释写「上游心跳 15 秒，留 3 倍余量」。只改这一个常量，绝对不要碰 crates/ingest/src/legacy.rs。"
    "改完只回一行确认。",
    "执行计划第 2 步：在 crates/ingest/src/window.rs 里把 WindowState::windows 换成 "
    "SmallVec<[Window; 4]>，并新增 pub fn rotate_windows(state: &mut WindowState, now_ms: u64) -> usize 的骨架："
    "按 now_ms 回收空闲超过 TTL 的非 pinned 窗口，返回回收数；先不要接进 flush()。改完回一行确认。",
    "执行计划第 3 步：把 crates/ingest/src/window.rs 的 rotate_windows 签名改成 "
    "pub fn rotate_windows(state: &mut WindowState, now_ms: u64, metrics: &Metrics) -> usize，"
    "在回收循环里调用 metrics.record_rotated(reclaimed as u64)；flush() 的过期计数保持不动。"
    "只改这一个文件，改完回一行确认。",
    "执行计划第 4 步：更新 docs/ingest.md 的 TTL 表，把 WINDOW_TTL_MS 那一行的 30_000 改成 45_000，"
    "并在说明里注明「45 秒 = 上游心跳 15 秒 × 3」。改完回一行确认。",
    "执行计划第 5 步：给 rotate_windows 的回收路径加 tracing::debug! 日志，带上窗口 id 与空闲时长。"
    "改完回一行确认。",
    "用 bash 跑 wc -l crates/ingest/src/window.rs 和 crates/ingest/src/metrics.rs，只报告两条行数，不要做别的。",
    "按团队规范写一条 conventional commit 信息（scope 写 ingest），一句话概括这次改动；"
    "不要执行 git，只回那一行。",
]


def long_probe(round_index: int) -> str:
    """每轮压缩后的进度自查：模拟开发者“我现在做到哪了”。

    REPL 逐行喂输入，多行探针会被拆成多条 prompt，必须压成一行。
    """
    return (
        f"【R{round_index}】这不是新任务，不要调用任何工具。用一行回答当前进度，"
        "格式严格为「已完成: <步号，逗号分隔，没有写 无>；正在做: <一句话>；"
        "接下来: <步号与动作，没有写 无>」。"
    )


# 计划条目的写法：中文 `第 N 步`、英文 `Step N` 都算。
STEP_PATTERN = re.compile(rf"第\s*([{LONG_CHINESE_NUMERALS}1-9])\s*步|steps?\s*(\d+)", re.I)
# 区间写法：`第 1–4 步`、`steps 1–4` 要展开成逐个步号。
STEP_RANGE = re.compile(
    rf"第\s*([{LONG_CHINESE_NUMERALS}1-9])\s*步?\s*[–—~至到-]\s*第?\s*([{LONG_CHINESE_NUMERALS}1-9])\s*步"
    rf"|steps?\s*(\d+)\s*[–—~至到-]\s*(\d+)",
    re.I,
)


def step_value(token: str) -> int:
    return int(token) if token.isdigit() else LONG_CHINESE_NUMERALS.index(token) + 1


def step_numbers(text: str) -> list[int]:
    """从文本里取步号：`第 3 步`、`第三步`、`Step 3` 或裸的 `1, 2`。"""
    found = [step_value(match.group(1) or match.group(2)) for match in STEP_PATTERN.finditer(text)]
    if found:
        return found
    found = [int(match.group(1)) for match in re.finditer(r"(?<![\d.])([1-9])(?![\d.])", text)]
    if found:
        return found
    # 裸的中文步号（“一、二”）只在完全没有阿拉伯数字时兜底。
    return [LONG_CHINESE_NUMERALS.index(ch) + 1 for ch in text if ch in LONG_CHINESE_NUMERALS]


def mentioned_steps(text: str) -> set[int]:
    """明确提到的步号集合（不含裸数字兜底），区间会展开。"""
    out: set[int] = set()
    for match in STEP_RANGE.finditer(text):
        tokens = [group for group in match.groups() if group]
        low, high = step_value(tokens[0]), step_value(tokens[1])
        out.update(range(min(low, high), max(low, high) + 1))
    out.update(step_numbers(text))
    return {step for step in out if 1 <= step <= LONG_PLAN_STEPS}


def plan_steps_present(summary: str) -> int:
    """计划条目还剩几条：五个步号至少各出现一次才算数。"""
    return len(mentioned_steps(summary))


def parse_progress(answer: str) -> tuple[set[int], list[int], str]:
    """解析进度自查：已完成步号集合、接下来步号、正在做原文。

    三个字段可能被模型写成一行（用 `；`）或三行，两种都要认。
    """
    done: set[int] = set()
    following: list[int] = []
    doing = ""
    for match in re.finditer(r"(已完成|正在做|接下来)\s*[:：]\s*([^\n；;]*)", answer):
        label, body = match.group(1), match.group(2).strip()
        if label == "已完成":
            done = set(step_numbers(body))
        elif label == "接下来":
            following = step_numbers(body)
        else:
            doing = body
    return done, following, doing


def run_long_round(
    binary: Path,
    home: Path,
    work: Path,
    sessions: Path,
    overlay: Path,
    session_id: str,
    round_index: int,
    work_turn: str | None,
    timeout: int,
    log_path: Path,
) -> None:
    # REPL 的 stdin 也喂给 ask 工具：模型一旦提问，控制行会被当成回答吃掉。
    script = (work_turn + "（直接执行，不要向我提问。）\n") if work_turn else ""
    script += "/compact\n" + long_probe(round_index) + "\n/quit\n"
    run_repl(binary, home, work, sessions, overlay, script, timeout, log_path, resume=session_id)


def run_long_method(
    name: str,
    binary: Path,
    root: Path,
    home: Path,
    work: Path,
    base_sessions: Path,
    base_id: str,
    overlay_template: str,
    rounds: int,
    timeout: int,
) -> dict:
    sessions = root / f"sessions-{name}"
    if sessions.exists():
        shutil.rmtree(sessions)
    shutil.copytree(base_sessions, sessions)
    # 恢复会话时 cwd 由会话 header 决定，`--cwd` 不再生效：所有方法共用同一个
    # 工作目录，开跑前重置到 base 状态，两个方法看到的起点完全一致。
    if work.exists():
        shutil.rmtree(work)
    shutil.copytree(root / "work-base", work)
    overlay = root / f"overlay-{name}.toml"
    overlay.write_text(overlay_template.replace("{method}", name), encoding="utf-8")

    rounds_data: list[dict] = []
    for round_index in range(1, rounds + 1):
        work_turn = LONG_WORK[round_index - 2] if 2 <= round_index <= len(LONG_WORK) + 1 else None
        log_path = root / f"log-{name}-r{round_index}.txt"
        run_long_round(
            binary,
            home,
            work,
            sessions,
            overlay,
            base_id,
            round_index,
            work_turn,
            timeout,
            log_path,
        )
        messages, compactions = keyed_messages(entries(session_file(sessions)))
        if len(compactions) < round_index:
            fail(
                f"{name}：第 {round_index} 轮没有产生 compaction（见 {log_path}）；"
                "常见原因是模型调用了 ask，把 /compact 当成了回答"
            )
        compaction = compactions[round_index - 1]
        summary = compaction.get("summary", "")
        done, next_step = plan_truth(work)
        present = plan_steps_present(summary)
        facts, _ = score_facts(summary)
        _, window = window_after(messages, f"【R{round_index}】")
        answer = "".join(item["text"] for item in window if item["role"] == "Assistant")
        answered_done, following, doing = parse_progress(answer)
        if next_step is None:
            next_ok = not following
        else:
            next_ok = following[:1] == [next_step]
        rounds_data.append(
            {
                "round": round_index,
                "chars": len(summary),
                "tokens_before": compaction.get("tokens_before"),
                "tokens_after": compaction.get("tokens_after"),
                "facts": facts,
                "done": done,
                "present": present,
                "answered_done": answered_done,
                "answered_next": following[:1],
                "answered_doing": doing,
                "done_ok": answered_done == done,
                "next_ok": next_ok,
                "expected_next": next_step,
                "tools": sum(len(item["tools"]) for item in window if item["role"] == "Assistant"),
            }
        )

    return {
        "method": name,
        "rounds": rounds_data,
        "final_steps": len(step_landed(work)),
    }


def render_long_table(results: list[dict]) -> str:
    headers = ["方法", "轮次", "token", "字符", "计划", "事实", "自查已完成", "自查下一步", "工具"]
    rows = []
    for result in results:
        for item in result["rounds"]:
            done_text = "无" if not item["answered_done"] else ",".join(
                str(step) for step in sorted(item["answered_done"])
            )
            truth_text = "无" if not item["done"] else ",".join(
                str(step) for step in sorted(item["done"])
            )
            rows.append(
                [
                    result["method"],
                    f"R{item['round']}",
                    f"{item['tokens_before']} → {item['tokens_after']}",
                    str(item["chars"]),
                    f"{item['present']}/{LONG_PLAN_STEPS}",
                    f"{item['facts']}/{len(FACT_CHECKS)}",
                    ("✓ " if item["done_ok"] else "✗ ") + done_text + f"（应 {truth_text}）",
                    "✓" if item["next_ok"] else f"✗ {item['answered_next'] or '无'}",
                    str(item["tools"]),
                ]
            )
    widths = [max(len(headers[i]), *(len(row[i]) for row in rows)) for i in range(len(headers))]
    lines = [
        "| " + " | ".join(headers[i].ljust(widths[i]) for i in range(len(headers))) + " |",
        "| " + " | ".join("-" * widths[i] for i in range(len(headers))) + " |",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row[i].ljust(widths[i]) for i in range(len(headers))) + " |")
    return "\n".join(lines)


def run_long_session(
    binary: Path,
    root: Path,
    home: Path,
    work: Path,
    overlay_text: str,
    methods: list[str],
    rounds: int,
    timeout: int,
) -> int:
    write_scenario(work)
    base_sessions = root / "sessions-base"
    base_overlay = root / "overlay-base.toml"
    base_overlay.write_text(overlay_text.replace("{method}", "moc"), encoding="utf-8")
    script = "\n".join(LONG_BASE) + "\n/quit\n"
    run_repl(
        binary,
        home,
        work,
        base_sessions,
        base_overlay,
        script,
        timeout,
        root / "log-base.txt",
    )
    base_entries = entries(session_file(base_sessions))
    base_id = next(
        (item.get("session_id") for item in base_entries if item.get("type") == "header"), None
    )
    if not base_id:
        fail("长会话 base 阶段没有生成会话")
    if step_landed(work):
        fail(f"base 阶段不该落地任何步骤，检查 {root / 'log-base.txt'}")
    shutil.copytree(work, root / "work-base")
    print(f"长会话 base：{base_id}（{len(LONG_BASE)} 条用户消息，计划 5 步全部未落地）")

    results = []
    for method in methods:
        print(f"\n== 运行 {method}（{rounds} 轮压缩）==")
        results.append(
            run_long_method(
                method,
                binary,
                root,
                home,
                work,
                base_sessions,
                base_id,
                overlay_text,
                rounds,
                timeout,
            )
        )

    print("\n" + render_long_table(results))
    print()
    for result in results:
        items = result["rounds"]
        size_text = " → ".join(str(item["chars"]) for item in items)
        facts = sum(item["facts"] for item in items) / len(items)
        print(
            f"- {result['method']}：终态落地 {result['final_steps']}/5 步；"
            f"checkpoint 字符 {size_text}；事实均值 {facts:.1f}/{len(FACT_CHECKS)}；"
            f"计划步号 {sum(item['present'] for item in items)}/{len(items) * LONG_PLAN_STEPS}；"
            f"进度回答 已完成 {sum(1 for item in items if item['done_ok'])}/{len(items)}、"
            f"下一步 {sum(1 for item in items if item['next_ok'])}/{len(items)}"
        )
        for item in items:
            print(
                f"    R{item['round']} 自查：已完成 {sorted(item['answered_done']) or '无'} | "
                f"正在做「{item['answered_doing'][:60]}」 | 下一步 {item['answered_next'] or '无'}"
            )
    print("\n注意：每轮都只跑一个样本，模型抽样会让分数波动。")
    return 0


IMPORT_PROBES = [
    "这段历史最早的阶段在做什么？请点名至少 3 个具体对象（文件、函数或类型名），2 句话以内，不要执行工具，不要只给概要。",
    "这段历史里已经修改过哪些文件？逐行列出路径；没有改过就写「无」。",
    "在最后一条用户指令之前，任务进行到哪一步、当时在改什么？请点名至少 3 个具体的文件或函数，3 句话以内，不要执行工具。",
]
# 每个探针最多核对多少个锚点，避免长会话的召回率被噪声稀释。
IMPORT_ANCHOR_LIMIT = 40
_TOKEN = re.compile(r"[A-Za-z0-9_.:/~@+%-]+")
# 工具调用的 JSON 字段名不是工程事实，出题时排除。
_STOP = {
    "new_string",
    "old_string",
    "path",
    "content",
    "command",
    "arguments",
    "file_path",
    "replace_all",
    "edits",
    "oldText",
    "newText",
    "true",
    "false",
    "null",
}


def salient(text: str, numbers: bool = True) -> list[str]:
    """给导入会话出题用的锚点抽取：路径、标识符，可选数字。

    真实会话里的裸数字经常是行号、会话 id 或 artifact 编号，召回价值低，
    导入模式的出题默认关闭数字，只考文件/函数/类型这类可验证的记忆。
    """
    anchors: list[str] = []
    seen: set[str] = set()
    for raw in _TOKEN.findall(text):
        token = raw.strip(".,;:+%")
        if len(token) < 3 or token in seen or token in _STOP:
            continue
        digits = sum(character.isdigit() for character in token)
        number = all(character.isdigit() or character in "_.," for character in token)
        numeric = numbers and number and (digits >= 3 or ("." in token and digits >= 2))
        named = token[0].isalpha() and (
            "_" in token
            or any(a.islower() and b.isupper() for a, b in zip(token, token[1:]))
        )
        path = token[0].isalpha() and "/" in token and ("." in token or token.count("/") >= 2)
        if not (numeric or named or path):
            continue
        seen.add(token)
        anchors.append(token)
        if len(anchors) >= IMPORT_ANCHOR_LIMIT:
            break
    return anchors


def recall(answer: str, anchors: list[str]) -> tuple[int, int]:
    if not anchors:
        return 0, 0
    return sum(1 for anchor in anchors if anchor in answer), len(anchors)


def file_recall(answer: str, files: list[str]) -> tuple[int, int]:
    if not files:
        return 0, 0
    hits = 0
    for path in files:
        tail = "/".join(path.split("/")[-2:])
        name = path.rsplit("/", 1)[-1]
        if path in answer or tail in answer or (len(name) >= 6 and name in answer):
            hits += 1
    return hits, len(files)


def import_omp_session(path: Path, root: Path, selector: str, max_tokens: int) -> tuple[Path, str, dict]:
    """把 omp 会话的 message 条目转成 moc 的会话文件；只读原文件。"""
    raw = [
        json.loads(line)
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    work = root / "work"
    work.mkdir(parents=True, exist_ok=True)
    session_id = "ses_" + hashlib.sha1(str(path).encode()).hexdigest()[:12]
    out: list[dict] = []
    parent: str | None = None

    def push(payload: dict) -> None:
        nonlocal parent
        entry = {"id": f"{len(out) + 1:08x}", "parent_id": parent, "at_ms": len(out) * 1000}
        entry.update(payload)
        out.append(entry)
        parent = entry["id"]

    push(
        {
            "type": "header",
            "version": 1,
            "session_id": session_id,
            "cwd": str(work),
            "model": selector,
            "mode": "normal",
            # 导入真实会话只做问答：workspace 权限下写入被拒、bash 走 bwrap 沙箱。
            "permission": "workspace",
            "parent_session_id": None,
            "title": "omp import",
            "created_at_ms": 0,
        }
    )
    converted: list[dict] = []
    for item in raw:
        if item.get("type") != "message":
            continue
        message = item.get("message") or {}
        role = message.get("role")
        text = "".join(
            block.get("text", "")
            for block in message.get("content") or []
            if isinstance(block, dict) and block.get("type") == "text"
        )
        if role == "toolResult":
            converted.append(
                {
                    "role": "Tool",
                    "text": text,
                    "parts": [
                        {
                            "ToolResult": {
                                "tool_call_id": message.get("toolCallId", ""),
                                "content": text,
                                "is_error": bool(message.get("isError")),
                            }
                        }
                    ],
                    "calls": [],
                    "result": (message.get("toolCallId", ""), bool(message.get("isError"))),
                    "chars": len(text),
                }
            )
        elif role in ("user", "developer", "assistant"):
            parts = []
            call_rows = []
            for block in message.get("content") or []:
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "text" and block.get("text"):
                    parts.append({"Text": block["text"]})
                elif block.get("type") == "toolCall" and role == "assistant":
                    arguments = block.get("arguments") or {}
                    if isinstance(arguments, dict):
                        arguments = json.dumps(arguments, ensure_ascii=False)
                    parts.append(
                        {
                            "ToolCall": {
                                "id": block.get("id", ""),
                                "name": block.get("name", ""),
                                "arguments": arguments,
                            }
                        }
                    )
                    call_rows.append(
                        (block.get("id", ""), block.get("name", ""), block.get("arguments") or {})
                    )
            if not parts:
                continue
            moc_role = {"user": "User", "developer": "Developer", "assistant": "Assistant"}[role]
            converted.append(
                {
                    "role": moc_role,
                    "text": text,
                    "parts": parts,
                    "calls": call_rows,
                    "result": None,
                    "chars": len(text) + sum(len(part.get("ToolCall", {}).get("arguments", "")) for part in parts),
                }
            )
        else:
            continue

    # 只保留最近 max_tokens 估算内的消息；开头不能是孤立工具结果。
    budget = max_tokens * 3.5 if max_tokens > 0 else float("inf")
    total = 0
    start = len(converted)
    while start > 0 and total + converted[start - 1]["chars"] <= budget:
        start -= 1
        total += converted[start]["chars"]
    while start < len(converted) and converted[start]["role"] == "Tool":
        start += 1
    kept = converted[start:]
    # 工具结果必须紧跟持有对应调用的 assistant 消息：omp 的流里结果可能乱序、
    # 也可能来自被截断的更早调用。这里按"调用 → 结果"重新配对，孤儿结果丢弃，
    # 无结果的调用从消息里移除，保证 moc 的请求校验能通过。
    results_by_id = {
        record["result"][0]: record for record in kept if record["role"] == "Tool"
    }
    emitted_results: list[tuple[str, bool]] = []
    emitted_calls: list[tuple[str, str, dict]] = []
    for record in kept:
        if record["role"] == "Tool":
            continue
        parts = [part for part in record["parts"] if "ToolCall" not in part]
        paired = [call for call in record["calls"] if call[0] in results_by_id]
        for call in paired:
            arguments = call[2]
            if not isinstance(arguments, str):
                arguments = json.dumps(arguments, ensure_ascii=False)
            parts.append(
                {
                    "ToolCall": {
                        "id": call[0],
                        "name": call[1],
                        "arguments": arguments,
                    }
                }
            )
            emitted_calls.append(call)
        if not parts:
            continue
        push({"type": "message", "message": {"role": record["role"], "content": parts}})
        for call in paired:
            result = results_by_id.pop(call[0])
            emitted_results.append(result["result"])
            push(
                {
                    "type": "message",
                    "message": {"role": "Tool", "content": result["parts"]},
                }
            )

    user_texts = [record["text"] for record in kept if record["role"] == "User"]
    # moc 压缩后会保留最后一条用户指令及其回复，因此"必须靠摘要记住"的是它之前的部分。
    # 出题只看丢弃区：头部 12 条正文衡量长程记忆，尾部 12 条衡量当前状态。
    # 工具调用参数是 JSON 字段与片段，不代表"记住了什么"，不参与出题。
    last_user = max(
        (index for index, record in enumerate(kept) if record["role"] == "User"),
        default=len(kept),
    )
    discarded = kept[:last_user] if last_user > 0 else kept
    dialog_blobs = [
        record["text"]
        for record in discarded
        if record["role"] in ("User", "Assistant") and record["text"].strip()
    ]
    calls = emitted_calls
    results = emitted_results
    successful = {call_id for call_id, is_error in results if not is_error}
    files = sorted(
        {
            arguments.get("path")
            for call_id, name, arguments in calls
            if name in ("edit", "write") and call_id in successful and arguments.get("path")
        }
    )
    truth = {
        "early": salient(" ".join(dialog_blobs[:24]), numbers=False),
        "near": salient(" ".join(dialog_blobs[-24:]), numbers=False),
        "files": files,
        "kept": len(kept),
        "discarded": len(discarded),
        "tokens": round(total / 3.5),
        "user_turns": len(user_texts),
    }
    session_dir = root / "sessions-import"
    session_dir.mkdir(parents=True, exist_ok=True)
    target = session_dir / f"{len(out)}_{session_id}.jsonl"
    target.write_text(
        "\n".join(json.dumps(entry, ensure_ascii=False) for entry in out) + "\n",
        encoding="utf-8",
    )
    return session_dir, session_id, truth


def run_import_method(
    name: str,
    binary: Path,
    root: Path,
    home: Path,
    work: Path,
    imported_dir: Path,
    base_id: str,
    overlay_template: str,
    generations: int,
    timeout: int,
    truth: dict,
) -> dict:
    sessions = root / f"sessions-{name}"
    if sessions.exists():
        shutil.rmtree(sessions)
    shutil.copytree(imported_dir, sessions)
    overlay = root / f"overlay-{name}.toml"
    overlay.write_text(overlay_template.replace("{method}", name), encoding="utf-8")
    script = "/compact\n" * generations + "\n".join(IMPORT_PROBES) + "\n/quit\n"
    output = run_repl(
        binary,
        home,
        work,
        sessions,
        overlay,
        script,
        timeout,
        root / f"log-{name}.txt",
        resume=base_id,
    )
    messages, compactions = keyed_messages(entries(session_file(sessions)))
    summaries = [item.get("summary", "") for item in compactions]
    if not summaries:
        fail(f"{name}：没有产生 compaction（日志：{root / f'log-{name}.txt'}）\n{output[-2000:]}")

    answers = []
    retrieval = 0
    for mark in IMPORT_PROBES:
        _, window = window_after(messages, mark)
        answers.append("".join(item["text"] for item in window if item["role"] == "Assistant"))
        retrieval += sum(len(item["tools"]) for item in window if item["role"] == "Assistant")
    early_hits, early_total = recall(answers[0], truth["early"])
    file_hits, file_total = file_recall(answers[1], truth["files"])
    near_hits, near_total = recall(answers[2], truth["near"])
    # 摘要本身记住了多少：与模型怎么措辞无关，衡量压缩保真度。
    summary_early = recall(summaries[-1], truth["early"])
    summary_near = recall(summaries[-1], truth["near"])
    summary_files = file_recall(summaries[-1], truth["files"])

    def ratio(hits: int, total: int) -> float:
        return hits / total if total else 0.0

    # 排名用摘要保真度：它不受"回答是不是啰嗦"影响。
    score = (
        ratio(*summary_early)
        + ratio(*summary_near)
        + ratio(*summary_files)
    ) / 3.0
    return {
        "method": name,
        "summaries": summaries,
        "early": (early_hits, early_total),
        "near": (near_hits, near_total),
        "files": (file_hits, file_total),
        "summary_early": summary_early,
        "summary_near": summary_near,
        "summary_files": summary_files,
        "retrieval": retrieval,
        "tokens": [
            (item.get("tokens_before"), item.get("tokens_after")) for item in compactions
        ],
        "collapsed": min(len(item) for item in summaries) < MIN_CHECKPOINT_CHARS,
        "score": round(score * 100),
        "answers": answers,
    }


def render_import_table(results: list[dict]) -> str:
    headers = ["方法", "代次", "摘要字符", "摘要记忆 早/近/文件", "回答召回 早/近/文件", "检索", "综合%", "token"]
    rows = []
    for result in results:
        sizes = " → ".join(str(len(item)) for item in result["summaries"])
        # 跨代统计：第一次压缩前 → 最后一次压缩后，避免只看末代把 30k→6k 报成 6k→6k。
        tokens = (result["tokens"][0][0], result["tokens"][-1][1])

        def percent(pair: tuple[int, int]) -> str:
            hits, total = pair
            return f"{round(hits / total * 100)}%" if total else "n/a"

        rows.append(
            [
                result["method"],
                str(len(result["summaries"])),
                sizes,
                "/".join(percent(result[key]) for key in ("summary_early", "summary_near", "summary_files")),
                "/".join(percent(result[key]) for key in ("early", "near", "files")),
                str(result["retrieval"]),
                f"{result['score']}%",
                f"{tokens[0]} → {tokens[1]}",
            ]
        )
    widths = [max(len(headers[i]), *(len(row[i]) for row in rows)) for i in range(len(headers))]
    lines = [
        "| " + " | ".join(headers[i].ljust(widths[i]) for i in range(len(headers))) + " |",
        "| " + " | ".join("-" * widths[i] for i in range(len(headers))) + " |",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row[i].ljust(widths[i]) for i in range(len(headers))) + " |")
    return "\n".join(lines)


def render_table(results: list[dict]) -> str:
    headers = ["方法", "代次", "摘要字符", "事实", "单选", "连贯", "检索", "第4步", "得分", "token"]
    rows = []
    for result in results:
        sizes = " → ".join(str(len(item)) for item in result["summaries"])
        # 跨代统计：第一次压缩前 → 最后一次压缩后，避免只看末代把 30k→6k 报成 6k→6k。
        tokens = (result["tokens"][0][0], result["tokens"][-1][1])
        rows.append(
            [
                result["method"],
                str(len(result["summaries"])),
                sizes,
                f"{result['facts']}/{len(FACT_CHECKS)}",
                f"{result['mcq']}/{result['mcq_total']}",
                f"{result['continuity']}/{len(CONTINUITY_CHECKS)}",
                str(result["retrieval"]),
                "✓" if result["action_ok"] else "✗",
                f"{result['score']}/{MAX_SCORE}",
                f"{tokens[0]} → {tokens[1]}",
            ]
        )
    widths = [max(len(headers[i]), *(len(row[i]) for row in rows)) for i in range(len(headers))]
    lines = [
        "| " + " | ".join(headers[i].ljust(widths[i]) for i in range(len(headers))) + " |",
        "| " + " | ".join("-" * widths[i] for i in range(len(headers))) + " |",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row[i].ljust(widths[i]) for i in range(len(headers))) + " |")
    return "\n".join(lines)


def build_overlay(selector: str, keep_recent: int, max_output: int) -> str:
    """生成基准覆盖层：关掉无关子系统，固定压缩策略，并按需收紧输出预留。"""
    provider, _, model = selector.partition("/")
    model = model.split(":")[0]
    text = (
        "[models.roles]\n"
        f'default = "{selector}"\n\n'
        "[mcp]\nenabled = false\n\n"
        "[context_files]\nenabled = false\n\n"
        "[skills]\nenabled = false\n\n"
        "[web_search]\nenabled = false\n\n"
        "[openviking]\nrecall = false\ncapture = false\nskills = false\n\n"
        "[compaction]\n"
        "auto = false\n"
        "auto_continue = false\n"
        'method_order = ["soft"]\n'
        'soft_method = "{method}"\n'
        f"keep_recent_tokens = {keep_recent}\n"
        f"protect_recent_tool_tokens = {keep_recent}\n"
    )
    if max_output > 0 and provider and model:
        # 输出预留会直接从压缩输入预算里扣；真实长会话动辄几十万 token，
        # 384k 的 max_output 会把预算压到十几万，必须按基准需要收紧。
        text += (
            f'\n[providers."{provider}".models."{model}"]\n'
            f"max_output_tokens = {max_output}\n"
        )
    return text


def main() -> int:
    parser = argparse.ArgumentParser(
        description="soft 压缩的可复现对比基准（真实模型 + 固定剧本 + 真实工具）。",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "示例：\n"
            "  scripts/soft-compaction-bench.py --model <provider>/<model> --methods moc,grok_build\n"
            "  scripts/soft-compaction-bench.py --methods moc --generations 1\n"
            "  scripts/soft-compaction-bench.py --methods moc,grok_build --rounds 8\n"
        ),
    )
    parser.add_argument("--model", default=None, help="模型 selector；默认取用户配置的 models.roles.default")
    parser.add_argument(
        "--methods",
        default="moc,grok_build",
        help="soft 摘要实现，逗号分隔（moc | grok_build），多个时输出对比表",
    )
    parser.add_argument("--generations", type=int, default=2, help="连续压缩次数，默认 2")
    parser.add_argument(
        "--rounds",
        type=int,
        default=0,
        help="长会话模式：每轮压缩之间执行一步真实工作，逐轮量化计划记忆（1-8）；0 表示关闭",
    )
    parser.add_argument(
        "--keep-recent-tokens",
        type=int,
        default=0,
        help="保留最近尾部 token 数；0 表示纯摘要对比（默认）",
    )
    parser.add_argument("--bin", default=str(DEFAULT_BIN), help="moc 可执行文件，默认 target/debug/moc")
    parser.add_argument(
        "--max-output-tokens",
        type=int,
        default=32768,
        help="基准里覆盖模型的 max_output_tokens，避免输出预留吃掉压缩输入预算；0 表示不改",
    )
    parser.add_argument(
        "--session",
        default=None,
        help="导入 omp 会话 jsonl（只读）做压缩对比；不给则使用内置合成剧本",
    )
    parser.add_argument(
        "--session-max-tokens",
        type=int,
        default=160000,
        help="导入时只保留最近的这么多 token 估算量（0 表示全部导入），默认 160k",
    )
    parser.add_argument("--timeout", type=int, default=900, help="单次 moc 运行的超时秒数")
    parser.add_argument("--keep", action="store_true", help="保留临时目录并打印路径")
    parser.add_argument(
        "--show-anchors",
        action="store_true",
        help="导入会话时打印派生出的出题锚点（默认只给命中率，避免泄露会话内容）",
    )
    parser.add_argument("--config", default=str(DEFAULT_CONFIG), help="用户配置路径（只读复制）")
    args = parser.parse_args()

    if args.generations < 1:
        fail("--generations 至少为 1")
    if args.rounds and not 1 <= args.rounds <= len(LONG_WORK) + 1:
        fail(f"--rounds 取值范围 1-{len(LONG_WORK) + 1}")
    if args.rounds and args.session:
        fail("--rounds 与 --session 不能同时使用")
    methods = [item.strip() for item in args.methods.split(",") if item.strip()]
    unknown = [item for item in methods if item not in {"moc", "grok_build"}]
    if unknown:
        fail(f"未知的 soft 实现：{', '.join(unknown)}（可选 moc、grok_build）")

    binary = Path(args.bin).expanduser().resolve()
    if not binary.exists():
        fail(f"找不到 {binary}；先运行 `cargo build -p moc-cli --bin moc`")
    config = Path(args.config).expanduser()
    if not config.exists():
        fail(f"找不到用户配置 {config}")

    selector = args.model or default_model(config)
    if not selector:
        fail("用户配置里没有 models.roles.default，请用 --model 指定")

    root = Path(tempfile.mkdtemp(prefix="moc-soft-bench-"))
    home = root / "home"
    work = root / "work"
    (home / ".moc").mkdir(parents=True)
    work.mkdir()
    shutil.copy2(config, home / ".moc" / "config.toml")

    overlay_text = build_overlay(selector, args.keep_recent_tokens, args.max_output_tokens)
    base_overlay = root / "overlay-base.toml"
    base_overlay.write_text(overlay_text.replace("{method}", "moc"), encoding="utf-8")

    print(f"模型：{selector}")
    if args.rounds:
        print(f"方法：{', '.join(methods)}    长会话轮次：{args.rounds}")
    else:
        print(f"方法：{', '.join(methods)}    代次：{args.generations}")
    print(f"临时目录：{root}")

    if args.rounds:
        code = run_long_session(
            binary, root, home, work, overlay_text, methods, args.rounds, args.timeout
        )
        if args.keep:
            print(f"\n临时目录已保留：{root}")
            print("提示：home/.moc/config.toml 是你配置的副本（可能含密钥），排查完请删除：")
            print(f"  rm -rf {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)
        return code


    if args.session:
        source = Path(args.session).expanduser()
        if not source.exists():
            fail(f"找不到会话文件 {source}")
        imported_dir, import_id, truth = import_omp_session(
            source, root, selector, args.session_max_tokens
        )
        print(
            f"导入会话：{import_id}（保留 {truth['kept']} 条消息 / {truth['user_turns']} 条用户消息，"
            f"约 {truth['tokens'] // 1000}k token；早期锚点 {len(truth['early'])}，近端 {len(truth['near'])}，"
            f"改动文件 {len(truth['files'])}）"
        )
        if args.show_anchors:
            print(f"早期锚点：{', '.join(truth['early'])}")
            print(f"近端锚点：{', '.join(truth['near'])}")
            print(f"改动文件：{', '.join(truth['files'])}")
        results = []
        for method in methods:
            print(f"\n== 运行 {method} ==")
            results.append(
                run_import_method(
                    method,
                    binary,
                    root,
                    home,
                    work,
                    imported_dir,
                    import_id,
                    overlay_text,
                    args.generations,
                    args.timeout,
                    truth,
                )
            )
        print("\n" + render_import_table(results))
        print()
        for result in results:
            sizes = " → ".join(str(len(item)) for item in result["summaries"])
            early, near, files = result["early"], result["near"], result["files"]
            summary_early, summary_near, summary_files = (
                result["summary_early"],
                result["summary_near"],
                result["summary_files"],
            )
            print(
                f"- {result['method']}：checkpoint {sizes} 字符；"
                f"摘要记忆 早 {summary_early[0]}/{summary_early[1]}、近 {summary_near[0]}/{summary_near[1]}、"
                f"文件 {summary_files[0]}/{summary_files[1]}；"
                f"回答召回 早 {early[0]}/{early[1]}、近 {near[0]}/{near[1]}、"
                f"文件 {files[0]}/{files[1]}；检索 {result['retrieval']} 次"
            )
            if result["collapsed"]:
                print("  ⚠ 出现退化 checkpoint（短于最短长度）")
        if len(results) > 1:
            alive = [item for item in results if not item["collapsed"]]
            if alive:
                top = max(item["score"] for item in alive)
                winners = [item["method"] for item in alive if item["score"] == top]
                verdict = "并列最高" if len(winners) > 1 else "得分最高"
                print(f"\n综合：{'、'.join(winners)} {verdict}（综合召回 {top}%；退化一票否决）。")
            else:
                print("\n综合：所有方法都产生了退化 checkpoint。")
            print("注意：每个方法只跑一个样本，模型抽样会让分数波动；需要方差就多跑几次。")
        collapsed = [item["method"] for item in results if item["collapsed"]]
        if args.keep:
            print(f"\n临时目录已保留：{root}")
            print("提示：home/.moc/config.toml 是你配置的副本（可能含密钥），排查完请删除：")
            print(f"  rm -rf {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)
        if collapsed:
            print(f"\n失败：{', '.join(collapsed)} 产生了退化 checkpoint", file=sys.stderr)
            return 1
        return 0

    write_scenario(work)
    base_sessions = root / "sessions-base"
    transcript = "\n".join(TRANSCRIPT) + "\n/quit\n"
    run_repl(
        binary,
        home,
        work,
        base_sessions,
        base_overlay,
        transcript,
        args.timeout,
        root / "log-transcript.txt",
    )
    base_file = session_file(base_sessions)
    base_entries = entries(base_file)
    base_id = next(
        (item.get("session_id") for item in base_entries if item.get("type") == "header"),
        None,
    )
    if not base_id:
        fail("transcript 阶段没有生成会话")

    window_rs = (work / "crates/ingest/src/window.rs").read_text(encoding="utf-8")
    if "45_000" not in window_rs or "rotate_windows" not in window_rs:
        fail(
            "transcript 阶段没有完成预期改动（TTL 与 rotate_windows），"
            f"检查 {root / 'log-transcript.txt'}"
        )
    shutil.copytree(work, root / "work-base")
    messages, _ = keyed_messages(base_entries)
    user_prompts = [item for item in messages if item["role"] == "User"]
    print(f"会话：{base_id}（{len(base_entries)} 条记录，{len(user_prompts)} 条用户消息）")

    results = []
    for method in methods:
        print(f"\n== 运行 {method} ==")
        results.append(
            run_method(
                method,
                binary,
                root,
                home,
                work,
                base_sessions,
                base_id,
                overlay_text,
                args.generations,
                args.timeout,
            )
        )

    print("\n" + render_table(results))
    print()
    for result in results:
        sizes = " → ".join(str(len(item)) for item in result["summaries"])
        print(f"- {result['method']}：checkpoint {sizes} 字符")
        if result["missing_facts"]:
            print(f"  未覆盖事实：{', '.join(result['missing_facts'])}")
        if result["missing_steps"]:
            print(f"  连贯性缺项：{', '.join(result['missing_steps'])}")
        print(f"  第 4 步落地：{'是' if result['action_ok'] else '否'}（工具调用 {result['action_tools']} 次）")
        if result["collapsed"]:
            print("  ⚠ 出现退化 checkpoint（短于最短长度）")

    if len(results) > 1:
        alive = [item for item in results if not item["collapsed"]]
        if not alive:
            print("\n综合：所有方法都产生了退化 checkpoint。")
        else:
            top = max(item["score"] for item in alive)
            winners = [item["method"] for item in alive if item["score"] == top]
            verdict = "并列最高" if len(winners) > 1 else "得分最高"
            print(
                f"\n综合：{'、'.join(winners)} {verdict}（{top}/{MAX_SCORE} 分；"
                "事实、单选、连贯性与第 4 步落地合计，退化一票否决）。"
            )
        print("注意：每个方法只跑一个样本，模型抽样会让分数波动；需要方差就多跑几次。")

    collapsed = [result["method"] for result in results if result["collapsed"]]
    if args.keep:
        print(f"\n临时目录已保留：{root}")
        print("提示：home/.moc/config.toml 是你配置的副本（可能含密钥），排查完请删除：")
        print(f"  rm -rf {root}")
    else:
        shutil.rmtree(root, ignore_errors=True)
    if collapsed:
        print(f"\n失败：{', '.join(collapsed)} 产生了退化 checkpoint", file=sys.stderr)
        return 1
    return 0


def default_model(config: Path) -> str | None:
    """从用户配置里读默认模型 selector（不依赖 tomllib，避免 Python 版本下限）。"""
    pattern = re.compile(r"^\s*default\s*=\s*\"([^\"]+)\"")
    in_roles = False
    for line in config.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_roles = stripped == "[models.roles]"
            continue
        if in_roles:
            match = pattern.match(line)
            if match:
                return match.group(1)
    return None


if __name__ == "__main__":
    raise SystemExit(main())

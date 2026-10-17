#!/usr/bin/env python3
"""soft 压缩的可复现对比基准。

同一份真实会话（固定剧本 + 真实模型 + 真实工具）分别用不同的 soft 摘要实现压缩，
然后测量「压缩后还记得多少」与「是否像没压缩过」：

  - 事实保真：14 个预埋事实在最终 checkpoint 里的命中数；
  - 探针正确率：14 道单选题，答案唯一；
  - 连贯性：当前进度、下一步、第 4/5 步是否与既定计划一致（0-4）；
  - 检索回填：回答探针时被迫回查 history:// 或磁盘的工具调用数；
  - 体积：每代 checkpoint 字符数、tokens_before/after；
  - 退化：checkpoint 短于最短长度视为塌缩（脚本以退出码 1 报告）。

用法：
    scripts/soft-compaction-bench.py --model <provider>/<model> --methods moc,grok_build
    scripts/soft-compaction-bench.py --methods moc --generations 1
    scripts/soft-compaction-bench.py --model p/m --methods grok_build --keep

依赖：Python 3.8+、仓库根已构建的 `target/debug/moc`，以及可用的 `~/.moc/config.toml`
（模型与供应商配置只读复制到临时 HOME，脚本不会写你的会话目录）。

默认 `--generations 2`：连续压缩两次，专门抓「第二次压缩把上下文丢成一句话」这类问题。
"""

from __future__ import annotations

import argparse
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
    "Cargo.toml": """[workspace]
members = ["crates/ingest"]
resolver = "2"
""",
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

TRANSCRIPT: list[str] = [
    "先熟悉代码。读 crates/ingest/src/window.rs 的 1-80 行，然后用一行告诉我 WINDOW_TTL_MS 现在的取值（带下划线原样）。",
    "把 TTL 从 30 秒改成 45 秒：常量写成 45_000，行内注释写「上游心跳 15 秒，留 3 倍余量」。只改 crates/ingest/src/window.rs 一个文件，绝对不要碰 crates/ingest/src/legacy.rs，那里跑着线上老路径。改完只回一句确认。",
    "记下设计决定，不用改代码：新增 pub fn rotate_windows(state: &mut WindowState, now_ms: u64) -> usize，返回本轮回收到期的窗口数；窗口容器换成 SmallVec<[Window; 4]>，不要 HashMap<u64, Window>——同时活跃窗口有界（上限 4），热路径点查用散列表是浪费。回一句收到。",
    "错误处理统一 thiserror：在 crates/ingest/src/lib.rs 里保留 IngestError::Expired { id: u64, age_ms: u64 }，不要引入 anyhow。然后把 rotate_windows 的骨架写进 window.rs：按 now_ms 回收空闲超过 TTL 的非 pinned 窗口，返回回收数；先不要接进 flush()。写完回一句确认。",
    "补充背景，这是一次设计评审的结论，请记住：我们否决了用 lru crate 的 LRU 缓存来管理窗口，原因是 pinned 窗口会违反淘汰顺序；容器只用 SmallVec。另外进程内存上限是 512MB，所以 TTL 45 秒就是上限，不能再往上加。收尾计划共 5 步：第 1 步改 TTL 常量，第 2 步加 rotate_windows 骨架，第 3 步把 metrics 计数移到 rotate_windows 内部，第 4 步更新 docs/ingest.md 的 TTL 表，第 5 步给轮转路径加 tracing 日志。回一句「已记住」。",
    "读 bench/results.md 全文，用两行分别回答：优化前和优化后的吞吐（行/秒，带下划线原样），以及优化前和优化后的 p99。不要展开。",
    "再读 crates/ingest/src/metrics.rs 和 crates/ingest/tests/window_rotation.rs，然后回答：flush 耗时指标记录在哪个结构里？一行；pinned 窗口为什么不能回收？一行。",
    "团队纪律记一下：全量测试预算 1 分钟，禁止在测试里做 bench；提交信息用 conventional commit，scope 写 ingest；注释和文档用中文。回一句收到。",
    "用 bash 跑 wc -l crates/ingest/src/window.rs，把行数告诉我。",
    "先停在这里，等我下一步指令。",
]

MCQ_MARK = "下面是一组关于本次 ingest 任务的单选题"
CONTINUITY_MARK = "继续推进这个任务"
ACTION_MARK = "把第 4 步现在做完"

PROBES: list[str] = [
    "下面是一组关于本次 ingest 任务的单选题。只回答，不要解释，输出 14 行，每行格式严格为 `P<编号>: <字母>`。"
    "P1. 窗口 TTL 常量 WINDOW_TTL_MS 现在是多少？A) 30_000 B) 45_000 C) 60_000 D) 15_000。"
    "P2. 为什么 TTL 取这个值？A) 上游心跳 5 秒，留 6 倍余量 B) 上游心跳 15 秒，留 3 倍余量 C) 上游心跳 30 秒，留 1.5 倍余量 D) 没有依据，随便取的。"
    "P3. 新增的窗口回收函数叫什么？A) rotate_windows B) sweep_windows C) evict_windows D) refresh_windows。"
    "P4. rotate_windows 返回什么？A) 空 B) 本轮回收的窗口数（usize） C) 被回收的窗口列表 D) bool 表示是否还有窗口。"
    "P5. 窗口容器最终用哪种结构？A) HashMap<u64, Window> B) BTreeMap<u64, Window> C) SmallVec<[Window; 4]> D) VecDeque<Window>。"
    "P6. 错误类型怎么定义？A) anyhow::Error B) thiserror 的 IngestError::Expired { id, age_ms } C) 直接 panic D) std::io::Error。"
    "P7. 哪个文件绝对不要改？A) crates/ingest/src/window.rs B) crates/ingest/src/legacy.rs C) docs/ingest.md D) crates/ingest/tests/window_rotation.rs。"
    "P8. 哪个方案被否决了？A) 用 lru crate 的 LRU 缓存 B) 用 SmallVec C) 用 thiserror D) 新增 rotate_windows。"
    "P9. 否决它的核心理由？A) 性能不够 B) pinned 窗口会违反淘汰顺序 C) 依赖太多 D) 不支持 no_std。"
    "P10. 进程内存上限是多少？A) 128MB B) 512MB C) 1GB D) 2GB。"
    "P11. 优化后的吞吐是多少？A) 18_240 行/秒 B) 24_910 行/秒 C) 31_500 行/秒 D) 9_870 行/秒。"
    "P12. 优化后的 p99 延迟是多少？A) 8.7ms B) 3.2ms C) 1.4ms D) 12.0ms。"
    "P13. 收尾计划第 3 步是什么？A) 把 rotate_windows 接进 flush() B) 把 metrics 计数移到 rotate_windows 内部 C) 更新 docs/ingest.md 的 TTL 表 D) 加 tracing 日志。"
    "P14. 全量测试的时间预算是多少？A) 1 分钟 B) 5 分钟 C) 10 分钟 D) 不限制。",
    "继续推进这个任务。用 3 句话回答：现在进行到哪一步、下一步的具体动作（点名函数或文件）、以及之后第 4 步和第 5 步。",
    "把第 4 步现在做完。",
]

MCQ_KEY = {
    "P1": "B",
    "P2": "B",
    "P3": "A",
    "P4": "B",
    "P5": "C",
    "P6": "B",
    "P7": "B",
    "P8": "A",
    "P9": "B",
    "P10": "B",
    "P11": "B",
    "P12": "B",
    "P13": "B",
    "P14": "A",
}

# 14 个预埋事实；正则按 checkpoint 里可能出现的写法放宽。
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


def render_table(results: list[dict]) -> str:
    headers = ["方法", "代次", "摘要字符", "事实", "单选", "连贯", "检索", "第4步", "得分", "token"]
    rows = []
    for result in results:
        sizes = " → ".join(str(len(item)) for item in result["summaries"])
        tokens = result["tokens"][-1]
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


def main() -> int:
    parser = argparse.ArgumentParser(
        description="soft 压缩的可复现对比基准（真实模型 + 固定剧本 + 真实工具）。",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "示例：\n"
            "  scripts/soft-compaction-bench.py --model <provider>/<model> --methods moc,grok_build\n"
            "  scripts/soft-compaction-bench.py --methods moc --generations 1\n"
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
        "--keep-recent-tokens",
        type=int,
        default=0,
        help="保留最近尾部 token 数；0 表示纯摘要对比（默认）",
    )
    parser.add_argument("--bin", default=str(DEFAULT_BIN), help="moc 可执行文件，默认 target/debug/moc")
    parser.add_argument("--timeout", type=int, default=900, help="单次 moc 运行的超时秒数")
    parser.add_argument("--keep", action="store_true", help="保留临时目录并打印路径")
    parser.add_argument("--config", default=str(DEFAULT_CONFIG), help="用户配置路径（只读复制）")
    args = parser.parse_args()

    if args.generations < 1:
        fail("--generations 至少为 1")
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

    overlay_text = (
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
        f"keep_recent_tokens = {args.keep_recent_tokens}\n"
        f"protect_recent_tool_tokens = {args.keep_recent_tokens}\n"
    )
    base_overlay = root / "overlay-base.toml"
    base_overlay.write_text(overlay_text.replace("{method}", "moc"), encoding="utf-8")

    print(f"模型：{selector}")
    print(f"方法：{', '.join(methods)}    代次：{args.generations}")
    print(f"临时目录：{root}")

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

# moc Rust 内存 profile（2026-11-05）

## 结论

空会话的约 30 MiB RSS 主要是程序代码与只读映射，并非空界面分配了 30 MiB 堆；实际稳定堆约 **0.95 MiB**。巨大会话确实有可观的数据结构成本：10 万条未压缩正文约 **196.93 MiB**，此外存在两套历史条目结构。上下文压缩后正文已转入文件映射，剩余百 MiB 主要是元数据和归档句柄。

最值得做的是：**减少 Core 常驻的重复历史投影、压缩常驻条目元数据、让 TUI 巨型消息按可见范围排版、改变 Web 静态资源整包常驻策略。** 改线程数、调用 malloc_trim 或简单 shrink_to_fit 都不足以解决主要问题。

本轮只做实际 profiling 和隔离诊断实验，没有修改产品行为。上一轮的 Web 首页修复保留不动。

## 方法与口径

- 基于提交 `5267efd` 加上一轮两行 Web 首页修复，复用上一轮隔离的合成会话。
- 原 release 程序复制到临时目录，使用真实 120×40 PTY。`/proc/PID/smaps` 用原程序采集；另以 `cargo rustc --release -p moc-cli --bin moc -- -C strip=none -C debuginfo=1` 构建带符号程序供 heaptrack 1.5.0 分配调用栈分析。
- heaptrack 会增加 RSS、运行时间和分配器扰动，**不把其进程 RSS 当正常程序 RSS**。稳定堆按正文就绪后 0.3–1.3 秒的时间线读取；峰值使用同一时刻的 folded stack，可加总，避免把各调用点不同时刻峰值相加。
- SIGTERM 由 moc 正常处理，会释放大部分会话状态。heaptrack 的退出 `leaked` 字段不能代表运行时驻留，更不能仅凭这个字段判定泄漏。
- `mallinfo2`、`malloc_trim` 使用临时 LD_PRELOAD 探针，在专用线程执行；只影响本次独立进程。该实验与正常 RSS、heaptrack 分开运行。
- 独立 Rust 探针复用 `SessionStore::read_state`，测量真实 `size_of`、Vec 长度/容量以及 shrink 前后 RSS。临时 example 已从源码中移除，副本在附件。
- 本轮 profile 每场景一次，属于内存构成诊断；上一轮性能中位数来自三次运行。代码页驻留、ASLR、线程时序和分配器状态会产生少量 RSS 差异。
- `MiB = 2^20 bytes`。heaptrack 文本默认十进制 M/K，本文已转换，原始输出保留原单位。

## 1. 空会话为什么接近 30 MiB

一次未插桩 smaps 样本总 RSS **31.11 MiB**，其他诊断轮约 29.6–30.4 MiB；与上一轮约 28 MiB 属于相同量级。

| 映射类别 | RSS MiB | 含义 |
|---|---:|---|
| 可执行代码映射 `r-xp` | 19.69 | 原生程序指令，文件支撑，可共享/回收 |
| 程序只读映射 | 4.50 | ELF 只读区域；不能把它全部归为某一嵌入资源 |
| 程序重定位及可写部分 | 1.67 | 其中约 1.64 MiB 为重定位后匿名脏页 |
| 其他匿名映射 | 1.66 | 分配器 arena、线程已触碰的栈等 |
| 主 heap 映射 | 0.96 | 仅 `[heap]` 不是所有 Rust 堆 |
| 共享库等 | 2.51 | 包含 libc 等的代码及数据映射 |
| 主线程栈 | 0.11 | 其他线程栈包括在匿名映射内 |

**实际稳定 malloc 堆为 998,358 bytes，即 0.952 MiB；峰值约 1.13 MiB。** 最大单项是 ratatui 双缓冲：`120 × 40 × 48 × 2 = 460,800 bytes`，约 **0.439 MiB**。其他包括约 53 KiB TLS trust anchors、32 KiB tracing slab、约 24 KiB Tokio 工作队列，以及配置、协议和运行时对象。

原程序磁盘体积约 69 MiB，其中 `.text` 约 47.9 MiB，只有一部分页进入 RSS。虚拟内存中数百 MiB 的匿名保留地址也不等于实际物理占用。Rust 没有 GC 堆，不代表代码映射、线程和界面缓冲为零。

**降低空闲基线的方向**是代码体积/布局与功能链接方式，而不是全项目查 clone。可对比 LTO、codegen-units、opt-level=s/z 或可选前端构建，但尚未实测收益；优化体积可能牺牲运行速度或构建时间，也不能把整段代码映射视作可直接省掉。TUI 并未解压 Web 资源包，不能把 Web 资源整包常驻算在 TUI 堆上。

## 2. 10 万条未压缩：正文之外还有什么

原程序正常 RSS 样本约 **346.39 MiB**；heaptrack 稳定堆 **305.09 MiB**，同一时刻峰值堆 **316.29 MiB**。

峰值分配的主要组成：

| 分配来源 | 申请的字节数 MiB | 说明 |
|---|---:|---|
| 反序列化后的消息正文 String | 196.93 | 10 万条约 2 KiB 正文；内容只有一份主存储，并非再复制了几份完整正文 |
| `Vec<Entry>` | 47.00 | 每项 376 字节，capacity 131,072 |
| 展示历史数组（`TranscriptEntry`） | 34.00 | 每项 272 字节，capacity 131,072 |
| `Vec<ContentPart>` | 7.63 | 10 万个单块消息，每个 ContentPart 80 字节 |
| MessageContent 的 Arc 存储 | 3.81 | 约 40 字节 × 10 万 |
| 消息 ID → 位置的 HashMap 表 | 4.13 | 不含另外分配的字符串键正文 |
| 其余 | 22.77 | ID、索引、上下文统计临时结构、TUI 与运行时等 |

优化编译会合并相同代码，heaptrack 的 272 字节数组扩容帧显示为 `AgentEntry`；不能据此判定加载了 10 万个 agent。独立 `size_of` 及 SessionState 两个数组的 len/capacity，确认该 34 MiB 对应 transcript 存储。

类型探针：

```text
Entry=376
TranscriptEntry=272
Message=16
MessageContent=8
ContentPart=80
entries len=100001 capacity=131072
transcript len=100000 capacity=131072
```

两组数组有效条目的内联数据约 **61.80 MiB**，申请容量是 **81 MiB**。这是正文共享以后依然存在的重复元数据成本。只读查看一页历史，却在 Core 回放并保留全历史状态，是百 MiB 的根本来源之一。

证据：[SessionState](../crates/moc-core/src/session/state.rs#L90)、[持久化 Entry](../crates/moc-core/src/session/entries.rs#L73)、[展示 TranscriptEntry](../crates/moc-protocol/src/methods/session.rs#L724)、[加载回放](../crates/moc-core/src/session/store.rs#L618)。

## 3. 压缩过的 10 万条为何仍有百 MiB

原程序 RSS 样本 **137.45 MiB**；稳定堆 **111.27 MiB**、峰值堆 **112.21 MiB**。

| 主要结构 | 申请容量 MiB |
|---|---:|
| Entry 数组 | 47.00 |
| TranscriptEntry 数组 | 34.00 |
| 99,980 个 Archive 对象 | 10.68 |
| 对应 Arc<Storage> | 3.81 |
| 消息位置哈希表 | 4.13 |
| 其余 ID、索引、少量上下文及界面 | 12.59 |

归档映射合计虚拟大小约 **204.68 MiB**，当时仅约 **0.109 MiB RSS**。正文归档有效，没有发现它重新全量进入物理内存。

但每条归档正文仍有 `Archive`：Bytes、OnceLock、两个 ID Vec，再包 Box 和 Arc。即使是没有工具调用的普通文字，两个空 Vec 和解析槽也占内联结构空间。profile 实测每条 112 字节 Archive 加约 40 字节 Arc 分配，合计约 **14.49 MiB / 10 万条**。

可降低方向：归档文件由批次统一持有，消息保存批次编号与 offset/len；把工具 ID 和解析缓存放进只对需要它们的消息创建的冷数据结构。需要保证归档字节存活、并发访问及工具调用关联语义。证据：[Archive 布局](../crates/moc-llm/src/content.rs#L20)、[from_archive](../crates/moc-llm/src/content.rs#L63)。

## 4. 单条 8 MiB 回复：TUI 排版放大

正常 RSS 样本 **159.25 MiB**；稳定堆 **106.93 MiB**、峰值堆 **131.79 MiB**。该合成消息含大量重复标题、段落和 Rust fenced code，不代表任意 8 MiB 纯文本都会达到同样开销。

同一峰值时刻的主要分配：

| 来源 | MiB |
|---|---:|
| pulldown-cmark 解析树 | 24.00 |
| ratatui Line 数组 | 24.00 |
| `push_code_row` 两条主要 Span 分配路径 | 22.22 |
| `flush_inline` 两条主要 Span 分配路径 | 13.33 |
| Core 正文（含其他 99 条） | 8.20 |
| TUI 正文副本的一条路径 | 8.00 |
| 行/正文拼装的一条路径 | 7.00 |
| 代码行填充的一条路径 | 5.72 |
| 其余 | 19.32 |

本次全过程约 **1,628 万次分配**，约 **583 万次临时分配**。插桩有明显时间开销，不能用这轮耗时替代上一轮正常运行耗时。

按消息块懒渲染仍会在某个巨型消息进入视口时，为该消息生成完整 `Vec<Line<'static>>` 及各行 Span。于是“视口只有 40 行”不等于只分配 40 行。

应优先研究块内分页/分段布局、索引加可见行缓存，保留完整原文与复制能力；不建议用静默截断内容换内存。仅消除一份 8 MiB 文本副本无法解决约 100 MiB 的排版放大。证据：[assistant_layout](../crates/moc-tui/src/render/scrollback.rs#L1240)、[render_markdown_with](../crates/moc-tui/src/render/markdown.rs#L123)、[push_code_row](../crates/moc-tui/src/render/markdown.rs#L1297)。

## 5. Web 后端额外固定内存

这里分析 Rust Web 服务，不包含 Chromium。

未请求页面时 RSS **19.32 MiB**，仅请求一次 `/` 后变为 **41.05 MiB**。heaptrack 显示：

- zstd 整包解压临时 Vec capacity 约 **16 MiB**。
- 切分后的各静态资源 body 合计约 **15.91 MiB**，由静态 `LazyLock` 长期持有。
- 临时整包与独立 body 曾同时存在，总堆峰值约 **32.27 MiB**；临时 Vec 随后释放，不能说它永远保留两份。
- 仅请求首页也解压全部资源。浏览器实际请求 zstd 响应还会为已访问资源保存额外压缩缓存。

降低方向：构建期按资源生成可独立读取的压缩数据、按需解压加有界缓存，或直接服务预压缩表示并为不支持的客户端按需解码。代价是压缩率、CPU、缓存策略和协商行为，需要保持单文件分发要求。证据：[静态资源实现](../crates/moc-web/src/assets.rs#L18)、[整包解压](../crates/moc-web/src/assets.rs#L73)。

## 6. 已实测的“简单优化”效果

### malloc_trim

在正常界面就绪后，诊断线程调用一次 glibc `malloc_trim(0)`：

| 场景 | 调用前 RSS MiB | 调用后 RSS MiB | 减少 MiB |
|---|---:|---:|---:|
| 空会话 | 29.56 | 29.52 | 0.04 |
| 100 条 | 37.62 | 36.86 | 0.76 |
| 10 万条未压缩 | 348.88 | 342.79 | 6.09 |
| 10 万条定期压缩 | 136.85 | 136.16 | 0.69 |
| 单条 8 MiB | 159.63 | 159.54 | 0.09 |

它不是解决主要占用的方法；不能据分配器报告有 free 字节就断言这些页都可返还内核。

### shrink_to_fit

独立回放探针对两组历史 Vec 调用 shrink_to_fit：

- 未压缩 10 万条：malloc 管理的 mmap 从 89,272,576 降至 69,132,576 bytes，申请容量减少约 **19.21 MiB**。
- **RSS 前后均为 317,812 KiB**，基本没有降低。
- 压缩场景同样：RSS 前后均为 106,340 KiB。

原因是 Vec 容量尾部没有被触碰，未必已占用物理页。不能将“申请容量减少 19 MiB”报告为“RSS 降低 19 MiB”。

### arena 与 Tokio 线程数

一次对照：`MALLOC_ARENA_MAX=1` 对空会话、压缩会话只减少约 0.8/2.9 MiB，超长消息约 9.5 MiB；线程数改为 1 使空会话线程从 15 降为 4，RSS 约少 1.3 MiB。它们不是几十到上百 MiB 的主要来源。单 arena 有锁竞争代价，少 worker 有并发能力代价；未做吞吐验证，不建议直接作为产品默认值。

## 7. 建议优先级及收益边界

1. **Core 历史单一存储 + 紧凑索引。** 优先避免为每个历史消息同时常驻 Entry 和完整 TranscriptEntry。展示层按页投影，存储索引使用可证明密集的内部整数位置。10 万条 TranscriptEntry 有效内联载荷本身约 **25.94 MiB**，可以作为结构级优化空间参考；还须保留索引、ID 与访问成本，实际 RSS 收益不能等同全部载荷。
2. **Entry 常见变体与冷附加字段分离。** 376 字节条目包含各种可选信息/最大变体尺寸。只在确实存在 tool、bash、agent 等数据时保存相应冷记录，并维持序列化形状。不要无脑对每条消息加 Box 导致额外分配；需要比较布局和分配次数。
3. **归档描述符集中存储。** 针对已实测的约 14.49 MiB 归档句柄成本做批次共享和紧凑描述。冷数据保留多种场景边界，不能针对纯文本基准删掉工具信息。
4. **TUI 巨型消息块内懒排版。** 这是超长单条场景最明显的杠杆，目标是让 Line/Span 缓存跟可见范围相关。必须验证滚动锚点、终端宽度变化、代码块、表格、复制和选区。
5. **Web 静态资源按需驻留。** 约 16 MiB 的常驻资源成本有明确证据，应按资源访问频率和压缩方式设计，避免每次请求重新解压整包。
6. **最后再调空会话构建体积。** 真正空闲堆不到 1 MiB；只有先证明构建选项减少了运行时驻留代码页，才能说空闲内存变小。不要拿磁盘二进制体积变化代替 RSS 数据。

以上是测量后的可行方案，尚未实现产品级优化，也不承诺未经验证的收益倍数。

## 复现附件与完整性

附件保留：heaptrack 原始 `.zst`、demangled 调用栈报告、峰值 folded stacks、massif 时间线、原程序 smaps、mallinfo/trim 前后结果、尺寸探针与启动脚本。原语料可由上一轮生成器重建。

关键命令：

```bash
cargo rustc --release -p moc-cli --bin moc -- -C strip=none -C debuginfo=1
python /tmp/moc-memory-20261105/profile.py --mode rss
python /tmp/moc-memory-20261105/profile.py --mode heaptrack
heaptrack_print /tmp/moc-memory-20261105/heaptrack-default-huge.zst \
  -p 0 -a 0 -T 0 --flamegraph-cost-type peak -F huge-peak.folded
python /tmp/moc-memory-20261105/web_profile.py
```

脚本使用临时目录中的 `moc-original` / `moc-profile`，跨机器复现需按脚本路径复制对应构建；诊断结束已恢复标准 `cargo build --release --bin moc`，没有将 profiler 嵌入产品。

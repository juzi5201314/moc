# moc 二进制体积优化记录（2026-11-05）

## 问题与边界

用户目标是去掉不需要的内容、降低程序体积，并尽量减少空会话内存；明确不接受单 codegen unit 等显著拖慢构建的方案。

启动 RSS 与二进制体积有关，但不是线性关系。只有实际触碰的文件页进入进程 RSS。之前代码页回收实验的个位数 MiB 是另一个方向；本轮不把代码页回收加入产品。

## 基线为什么大

提交 `5267efd`，加前轮 Web 首页修复。原 release：72,175,304 bytes（68.83 MiB），已经 strip 符号，不能再靠删除调试信息取得主要收益。

| ELF 部分 | 大小 MiB |
|---|---:|
| `.text` 原生机器码 | 47.89 |
| 主 `.rodata` | 8.50 |
| `.eh_frame`、`.eh_frame_hdr`、异常表 | 8.21 |
| 动态重定位表 | 2.17 |
| 重定位后只读数据及其他 | 约 2.06 |

带符号构建的 llvm-nm 近似归类显示：moc_core 约 6.14 MiB、serde/serde_json 相关约 6.69 MiB、Tokio 约 2.19 MiB、moc_tui 约 1.38 MiB、jsonschema 约 1.24 MiB、moc_protocol 约 1.15 MiB、rmcp 约 0.91 MiB。另有大量显示为 `core`、`alloc` 的泛型、drop glue 和异步实例化，不能误解为标准库固定需要这么大。

同名大型异步状态机存在多个编译实例，如 `run_prompt_locked`、`agents::spawn`、`SessionTable::get`；同名不保证机器码完全相同。多前端、网络/TLS、JS 执行、解析、高亮、字体/图形等实际功能也在同一程序中。

不采用 `panic=abort`：工具执行使用 `catch_unwind` 隔离 panic，改 abort 会把工具异常变成整进程退出。异常展开部分不是无条件可删的垃圾。

## 确认并实施的依赖裁剪

### TUI 高亮

原来直接依赖 syntect 的 `default-fancy`，同时 two-face 默认启用 `regex-onig`。syntect 源码明确在二者同时开启时使用 Oniguruma，fancy-regex 后端不生效。

应用只使用 two-face 的预编译 syntax/theme dump，没有调用 syntect 的默认语法/主题加载、HTML 输出、YAML/Plist 语法与主题导入。

保留 `parsing`、`dump-load`、`regex-onig`，保留原有 two-face 与实际 Oniguruma 后端。注意 `parsing` 自身仍依赖 `dump-create`，没有声称去掉这个传递特性。

### MCP

生产 moc 是 MCP 客户端，测试中才构造 MCP server。关闭 rmcp 默认的 server/macros 特性，保留 client、子进程传输、HTTP 传输；dev-dependencies 显式启用 server。

锁文件删除 6 个包：fancy-regex 0.16.2、plist、quick-xml、yaml-rust、linked-hash-map、rmcp-macros。其他功能所需版本与依赖保留。例如 jsonschema 使用的 fancy-regex 0.19.2 仍保留。

这些裁剪减少编译范围，但不保证删除的每个包此前都有大量代码进入最终二进制，链接器本来就能去掉未引用函数。

## 构建方案的取舍

- `ThinLTO + codegen-units=1 + opt-level=s`：体积 41.66 MiB，约少 39.5%；用户明确排除单 codegen unit，因此不采用。实测空会话 RSS 仅从约 27.22 降到 26.53 MiB，巨型回复排版 CPU 有变慢迹象。
- 默认 codegen 并行度 + ThinLTO + 默认 opt-level=3：本轮构建 7 分 09 秒，产物约 71 MiB，比基线还大，因此不采用。
- 仅裁依赖、普通默认构建：72,183,464 bytes（68.84 MiB），比基线多 8,160 bytes（约 0.01%），没有可宣称的最终体积收益。
- 裁依赖 + opt-level=2：73,688,712 bytes（70.28 MiB），比基线更大，不采用。

构建时间记录：依赖裁剪触发的部分重建 176.34 秒；opt-level=2 变更触发广泛重建 422.13 秒；默认并行 ThinLTO 首次构建 429 秒；单 codegen 实验 258 秒。**这些重建范围、缓存状态和并行负载不同，不能据此量化日常增量构建退化比例。** 曾准备只重编 CLI 的对照，但命令缺少 `-p moc-cli` 被 Cargo 拒绝，没有有效测量结果；因 ThinLTO 已同时出现体积增加和构建成本，未继续投入该方案。最终默认构建缓存命中耗时 1.66 秒，也不能拿来和全量重建直接比较。

最终保留原来的 `[profile.release] strip = true`，没有新增任何 release 参数。单 codegen unit 按用户约束明确排除，ThinLTO 和 opt-level=2 因产物没有收益也排除。

两个 LTO 实验均通过环境变量运行，没有写进 Cargo 配置。也没有添加 target-cpu=native、panic=abort 或全局强制链接器选项。

## 最终验证与结论

MCP 34 个已有测试全部通过，包括握手、列举/调用、图片、MRTR、并发和重连；TUI 高亮 1 个、diff 3 个、Markdown 25 个测试全部通过。没有改业务源代码或新增为实现凑数的测试。

最终依赖裁剪版空会话三次中位数：RSS **26.91 MiB**，输入回显 **18.11ms**，就绪 **2310.01ms**；基线对应 **27.22 MiB / 18.09ms / 2325.06ms**。差异很小且有环境噪声，不把它称为显著内存改善。

结论：可删除的 feature 确实存在，已裁掉；但它们不是最终机器码的大头，链接器已经去掉了多数无引用代码。本轮在用户要求的构建时间约束内，没有找到显著减少二进制/RSS的低风险配置组合。要进一步明显缩小，需有针对性地减少大型泛型/异步链路的重复实例化、拆分并按需加载冷功能，或研究启动工作集。不能为了体积删掉当前真实使用的 TLS、MCP、解析、高亮、图片和前端能力。

## 测量方法

- 每种产物复制为独立文件，避免构建覆盖正在测量的程序。
- PTY 120×40，使用上一轮隔离的空、100 条、10 万条和单条 8 MiB Markdown 会话；每种场景三次。
- RSS、CPU 及回显探针复用前轮脚本；含约 2.3 秒 PTY 能力协商等待，不能将其全部归为实际启动计算时间。
- 同一共享开发机器，内存分配器、页缓存和后台负载可产生噪声。只在构建结束后进行正式候选运行测量。
- 部分废弃方案测量尾部与下一轮构建有重叠，不据此声称精确性能退化百分比；该方案主要依据用户构建约束排除。
- 所有生成语料与测量结果保留在独立附件，未读取真实用户会话。

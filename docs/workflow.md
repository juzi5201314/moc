# workflow（动态多 actor 编排）

moc 的动态 workflow 让模型把一个多阶段协作过程写成一段 TypeScript 脚本，由确定性引擎驱动执行：phase 编排、role/actor 的 ask 调用、world.run 命令、hole 缺口填补、artifact 报告——全部带 journal 与审批。

## 是什么、何时用

单次提示解决不了的**结构化多轮任务**：需要 planner/reviewer 循环、需要中间产物落盘、需要在敏感命令前审批、需要失败后确定性续跑（零 token 重放已完成部分）。脚本即编排；每次 ask 是一个独立子代理会话，上下文干净。

## 激活与零泄漏

`/workflow [指令]`（TUI/REPL/Web 斜杠命令或 `session/workflow_activate`）按会话持久化激活。未激活的会话**完全不变**：9 个 workflow 工具不出现在声明表、prompt 里没有 workflow 文本、`workflow-authoring` skill 既不在 `<skills>` 目录也不能被模型加载、子代理与 workflow actor 一律不可见——请求级字节与 `enabled=false` 的会话逐字节一致。停用的唯一例外是 `/skill:` 手动加载路径（保留给用户自主行为）。

激活后模型可见 `workflow-authoring` skill（脚本 facade 的**权威契约**：每个 facade 签名、site 词法、诊断码表都写在 skill 里，此处不重复）。

普通子代理与 workflow actor 是两条独立入口：`spawn_agent(role, task)` 只能选择配置中启用的固定角色，不能定义提示词、工具或模型。仅激活 workflow 后，负责编写脚本的主代理才能在脚本内声明动态 actor；这些定义只属于当前会话的当前 run，不注册到普通角色目录，也不会覆盖 Explorer/custom。

`AgentPersona.tools?: readonly string[]` 指定该 actor 的工具允许列表，例如 `agent("reader", {system: "只读调查", model: "provider/model", tools: ["read", "grep", "find"]})`。省略时沿用受控默认工具面；`yield` 始终保留，actor 自身不能再创建代理或取得 workflow 编排工具。提示、工具与所属 run 在创建时冻结；模型仍支持原有审批/运行期 role 覆盖。

动态 actor 的取消令牌绑定 run；run 结束、停止或失败会终止其余活动 actor。后续 ask 只能由所属 driver 投递，同一 run 的 actor 可通信；普通 `agent/message`、跨 run 消息以及直接 `session/prompt`/`session/retry` 不得将其唤醒。resume/amend 仍由 workflow 负责重放或建立新 run 的子会话，不把前驱 actor 变成可全局复用的角色。

## 审批窗（TUI / REPL / WebUI 一致）

`create_workflow`/`amend_workflow`/`fill_workflow_hole` 走到需要审批的命令集时弹出审批窗：

- **编排图**：phase 按最长路径分层，节点显示 phase 名、role 名（fan-out 标 `×N`）、ask/world.run 计数、hole 标识；实线=data 边、虚线=order 边、回边单独标注；宽度不足退化纵向列表。
- **role 模型选择**：每个 role 一个模型选择器（含思考强度），默认值来自审批载荷 `defaults.role_models`（会话模型或脚本字面量）；`model(...)` 引用的 `ModelRef` 重映射表逐项可选。批准时 decision 携带 `role_models`+`model_ref_map` 覆盖。
- **命令集**：脚本里全部 `world.run` 命令；`hole_fill` 只列新增命令。
- **脚本全文**：语法高亮查看（hole_fill 只显示新增命令）。
- 批准/拒绝（拒绝可带反馈回传给模型）。

## run 面板

`/workflows` 打开 run 列表 → run 面板：同一张图按 phase/role 状态实时着色（pending/running/done/failed/cancelled + 在飞/完成 ask 数 + token 用量）；role 行可运行中改模型（`session/workflow_set_role_model`，之后的 ask 用新模型）；reports、artifacts、log、hole 状态、停止/续跑。

**artifacts**：file/markdown 可查看全文；chart 渲染为图（TUI 文本图、web Mermaid）；table 按 `key` 去重替换；metrics 取每字段最新值；board 按 `status` 分列、按 `key` 去重移动卡片；同一 artifact_id 多次声明为版本，可切换查看。

## journal、resume、amend

- **journal**：`(site_id, ordinal)` 唯一键的确定性记录，`runs/<run_id>/journal.jsonl` 落盘。
- **resume**（`session/workflow_resume`）：重放 journal，已完成 ask/world.run **零模型请求**重放；输入 hash 不符报 `errored{InputHashMismatch}`。
- **amend**：分叉——未变 ask 零请求导入（`SeedFrom`：把前驱 actor 子会话 transcript 截断复制到导入边界，生成**新**子会话；前驱 run 的审计记录字节不变），首个变化 ask 及之后正常执行。
- **hole**：`hole("name")` 暂停 run、打开 hole 通知主会话；`fill_workflow_hole` 填代码（含新 world.run 命令时触发 `hole_fill` 审批）后注入执行、分支恢复。

## 保存与 args

`save_workflow` 存脚本到 `<session>/workflows/saved/`；`list_saved_workflows` 枚举；`args` 字段随 run 保存，`create_workflow(args)`/`amend_workflow(args)` 透传进脚本上下文。

## world.run

`world.run(argv)` **不经 shell**：固定 argv 直 exec（Workspace 模式经 bwrap argv 包裹、Full 模式直接 exec），资源限额照常生效；参数含 `$(...)`、`;`、空格、引号原样传递。命令在审批窗中列明。

## 并发与用量

`create_workflow.max_concurrency` 钳到 `[1, max_concurrency_ceiling]`；actor 间按序调度。每 ask 的 token（input/output/cache）与费用从 actor 子会话实际 usage 累积到 role 与 run，进 run 快照与 `Usage` 事件。

## 配置（`[workflow]`，config/schema.rs）

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `enabled` | `true` | 总开关；false 时激活报明确错误 |
| `max_concurrency_ceiling` | `16` | 在飞 ask 上限天花板 |
| `world_run_timeout_ms` | `300000` | world.run 默认超时，opts.timeoutMs 覆盖 |
| `file_read_bytes` | `1048576` | files.read 单文件上限 |
| `log_bytes` | `8192` | 单条 log 上限 |
| `ask_max_repairs` | `3` | ask 结构化交付修复重试上限 |

## 存储布局

`<session>/workflows/`：`saved/`（保存的脚本）、`runs/<run_id>/`（`run.json` 快照、`journal.jsonl`、`script.ts`、`artifacts/`）。

## 诊断码（以 workflow-authoring skill 为准）

`E_WORKFLOW_SKILL`（未加载 skill）、`PHASE_REQUIRED`、`INPUT_HASH_MISMATCH`、actor/ask/world/artifact/files 各域错误码——完整表见内置 skill 文档。

## 与 zcode 的差异

- 无完整 TS 类型检查：**oxc 解析 + 静态诊断**（编译器在 analyze/lower 阶段报错）。
- TUI chart 为**文本图**（sparkline + 数值表）；web 侧为 Mermaid `xychart-beta`/`quadrantChart`。

## 测试

workflow 集成测试需要真实 worker 进程：`cargo test -p moc-core --test integration workflow_flow`。测试支持库按 `MOC_TEST_BIN` → `CARGO_BIN_EXE_moc` → `target/debug/moc` → 自动 `cargo build -p moc-cli` 的顺序解析二进制，常规开发目录下无需手动设置环境变量。

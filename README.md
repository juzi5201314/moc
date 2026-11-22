# moc

用 Rust 编写的 AI 编码助手。默认提供终端界面（TUI），无交互终端时自动退回行式模式；也内置浏览器界面（`moc web`）、编辑器接入（`moc acp`）和脚本批处理模式（`--print`）。

## 主要特性

- **多种使用方式**：终端 TUI、浏览器界面（[docs/web.md](docs/web.md)）、Zed / Neovim 等编辑器接入（[docs/acp.md](docs/acp.md)）与非交互命令行，共用同一套配置和会话数据。
- **多供应商接入**：支持 OpenAI、Anthropic 及兼容 OpenAI 协议的网关，按供应商和模型灵活配置请求参数（[docs/api-parameters.md](docs/api-parameters.md)）。
- **内置代码与联网工具**：文件读写、精准编辑（附结构化改动预览）、全文搜索、文件查找、Shell 命令与 `web_fetch` 网页抓取（无需密钥，自动转为结构化 Markdown）。
- **权限沙盒与资源上限**：提供 `full`、`workspace`、`workspace-only` 三档路径与网络隔离（[docs/sandbox.md](docs/sandbox.md)），并可独立限制命令执行的 CPU 与内存上限（[docs/limits.md](docs/limits.md)）。
- **图片自动预处理**：统一压缩当前与历史图片并按请求体预算分配空间，避免静默丢图或超限失败（[docs/images.md](docs/images.md)）。
- **计划模式与目标模式**：`/plan` 先起草并逐段批注审阅计划再动手；`/goal` 设定持久目标，自动多轮推进直到完成或遇阻。
- **子代理与动态工作流**：可派生独立子代理并行处理子任务；通过 `/workflow` 按需开启多阶段编排、审批与断点续跑（[docs/workflow.md](docs/workflow.md)）。
- **上下文管理与原地回退**：随时查看发给模型的真实上下文构成，支持多种压缩策略（[docs/compaction.md](docs/compaction.md)）及原地回退/前滚消息，且不产生分叉会话。
- **旁路问答**：主任务运行时用 `/btw 问题` 在侧边随手追问，不污染主上下文，也不打断当前任务。
- **多窗口安全协作**：同一会话可被多个终端或浏览器同时打开；同一时刻仅允许一个窗口编辑，其余窗口自动以只读模式打开。
- **MCP 与本地插件**：支持外部 MCP 服务器（含交互式表单提问）与本地 JavaScript 插件，可扩展工具、斜杠命令、钩子与提示词（[docs/plugins.md](docs/plugins.md)）。
- **循环保护与自动重试**：自动识别无进展循环与重复输出，网络波动或限流时按策略自动退避重试（[docs/http-errors.md](docs/http-errors.md)）。

## 安装

需要 Rust 1.88+ 以及 Node.js ≥ 22 与 `pnpm`（用于编译期构建内嵌 Web 前端；Linux 还需 `nasm`）：

```bash
cargo build --release
```

产物位于 `target/release/moc`（单二进制分发），也可通过 `cargo install --path crates/moc-cli` 安装到全局。

命令解释器选择：Unix 上使用 `bash` 或 `zsh`（`fish` / `nushell` 等自动回退到 `bash`）；Windows 按 Git Bash → PowerShell → cmd 依次回退。

## 快速开始

1. 启动终端界面：
   ```bash
   ./target/release/moc
   ```

2. 首次运行会引导配置模型。按 `Ctrl+G` 或输入 `/config` 打开设置：
   - 添加供应商，填入接口地址与 API 密钥；
   - 按 `F` 拉取模型列表并勾选常用模型；
   - 按 `Ctrl+P` 选择当前使用的模型（TUI 与 Web 共享最近一次手动选择的模型）。

3. 在底部输入框输入需求并按 `Enter` 发送。随时按 `Ctrl+H` 查看完整快捷键。

## 命令行用法

```bash
moc                           # 启动终端界面（默认）
moc web                       # 启动浏览器界面（默认 127.0.0.1:7432，占用则改用随机端口）
moc acp                       # 作为 ACP 代理供 Zed 等编辑器接入
moc --print "问题"            # 一次性非交互输出
moc --list                    # 查看历史会话
moc --resume <id>             # 恢复指定会话
```

行式模式下，日志输出到 `stderr`，`stdout` 只保留回答正文；需要排查问题时设置 `RUST_LOG=debug`。

### 脚本与 CI 模式（`--print`）

- `--output-format text|json|stream-json`：`text` 仅输出最终答案（默认）；`json` 在结束时输出完整汇总（会话 ID、最终答案、变更文件、工具统计、用量、耗时与错误）；`stream-json` 逐行输出过程事件（NDJSON），末行为同一汇总对象。
- `--max-turns N`：限制模型最大请求轮数（默认不限）。
- `--allow-tools a,b`：自动放行指定工具的权限申请（逗号分隔，可重复；`all` 放行全部），其余申请按拒绝处理。

退出码：`0` 成功；`1` 一般错误（模型/网络等）；`2` 存在被拒绝的工具权限申请；`3` 达到 `--max-turns` 上限。

## 常用交互

| 操作 | 快捷键 / 命令 |
|---|---|
| 发送 / 换行 / 停止生成 | `Enter` / `Shift+Enter` / `Esc` |
| 打开设置 | `Ctrl+G` 或 `/config` |
| 切换模型与思考等级 | `Ctrl+P` 或 `/model`（`Tab` / `←→` 调整思考等级） |
| 切换思考 / 工具显示详细度 | `Ctrl+T` / `Ctrl+O` |
| 查看发给模型的上下文明细 | `/context`（或点击顶部进度条） |
| 查看当前可用工具 | `/tools` |
| 查看工具统计与压缩次数 / 会话信息 | `/debuginfo` / `/session` |
| 导出会话为 Markdown | `/export [path]`（自动脱敏常见密钥，不覆盖已有文件） |
| 计划模式 / 持久目标 / 旁路问答 | `/plan` / `/goal` / `/btw 问题` |
| 压缩或清理技能目录缓存 | `/compact-skills [compact\|force\|cleanup [all]]` |
| 复制选中文本 | `Ctrl+C` / 右键 / `y` |

更多终端交互细节见 [docs/tui.md](docs/tui.md)，浏览器端细节见 [docs/web.md](docs/web.md)。

## 配置文件

- 全局配置：`~/.moc/config.toml`
- 项目覆盖：工作目录下的 `.moc/config.toml`
- 模型选择记录：`~/.moc/model-selection.toml`（TUI 与 Web 共用；恢复旧会话时保持该会话原模型）
- 界面偏好：`~/.moc/tui.toml`（TUI）、`~/.moc/web.toml`（Web）

## 开发与仓库结构

```bash
cargo test --workspace --locked   # 运行全量测试
cargo build                       # 调试构建
```

上下文压缩策略见 [docs/compaction.md](docs/compaction.md)，压缩实验脚本见 [docs/compaction-lab.md](docs/compaction-lab.md)。

### Workspace 目录

- `crates/moc-core` — 核心引擎：会话管理、提示调度、工具执行、沙盒与循环保护
- `crates/moc-llm` — 模型接口传输与编解码（OpenAI / Anthropic / 兼容网关）
- `crates/moc-protocol` — 前后端 JSON-RPC 协议与 schema
- `crates/moc-mcp` — MCP 客户端
- `crates/moc-plugin` — 本地插件系统（目录发现、QuickJS 运行时与宿主桥）
- `crates/moc-ptc-protocol` / `crates/moc-ptc-worker` — 代码执行模式（PTC）协议与内嵌 worker
- `crates/moc-tui` / `crates/moc-web` / `crates/moc-acp` — 终端、浏览器、ACP 三套前端
- `crates/moc-cli` — 命令行总入口

### 模块划分约定

文件按共同演化的领域职责拆分，不为凑短文件引入额外抽象层；一般实现文件控制在 200–600 行，超过 1,000 行时审视职责边界。

| 领域 | 目录与职责 |
|---|---|
| TUI 事件与状态 | `moc-tui/src/event/` 分管输入、分页、协议更新与模态交互；`app/` 按领域组织状态方法 |
| TUI 渲染与设置 | `render/scrollback.rs` 与子模块共享高度估算与绘制；`render/markdown/` 与 `settings/` 按职责拆分 |
| 核心服务与提示 | `moc-core/src/service/` 拆分协议入口与提示生命周期、重试、授权及批次执行 |
| 会话存储 | `moc-core/src/session/` 分管生命周期、历史分页、回退、持久化、编辑锁与投影 |
| 工作流与插件 | `workflow/` 分离编译分析、调度与运行时；`moc-plugin/` 分离发现、激活、钩子与调用 |

维护边界约束：优先使用最小可见性；拆分时不改变锁作用域、取消清理、事件与落盘顺序；测试验证真实行为而非扫描源码字面量；变更通过 `cargo test --workspace --locked` 验收。

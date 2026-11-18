# moc

Rust 编写的终端 AI 编码助手。默认启动 TUI，无终端时自动降级为行式交互；`moc web` 提供浏览器前端，`moc acp` 以 ACP agent 身份供 Zed 等编辑器接入，`--print` 支持一次性非交互输出，方便脚本与 CI 集成。

## 特性

- **多前端**：TUI（默认）、浏览器（`moc web`）、ACP（`moc acp`，见 [docs/acp.md](docs/acp.md)）与行式交互按需切换；每个前端进程内嵌一个 core
- **图片输入**：统一压缩当前与历史图片，按完整请求预算分配空间；超限明确报错，524 附带请求体和图片诊断，见 [图片发送规则](docs/images.md)
- **多进程安全**：同一会话可以被多个进程打开，写租约保证同一时刻只有一个写者，其余前端只读降级
- **多供应商**：OpenAI、Anthropic 及兼容 OpenAI 协议的网关均可接入
- **代码工具**：`read` / `edit` / `write` / `bash` / `grep` / `find`，编辑前带结构化 diff 预览与确认
- **资源限制**：可为 agent 的 bash 单独限制 CPU 与内存，编译占满机器时可以接受变慢、尽量不失败；默认关闭，见 [docs/limits.md](docs/limits.md)
- **联网工具**：`web_fetch`（本地抓取，HTML 转 Markdown；无需密钥，GitHub/docs.rs/MDN/arXiv/npm/PyPI 结构化）
- **计划模式**：让模型先写执行计划，审阅后再逐步执行，避免盲目改动
- **子代理**：可 spawn 独立子会话处理并行任务，共享工作目录与工具权限
- **上下文管理**：实时查看上下文占用与真正送进模型的正文，支持压缩与消息原地回退（截断当前会话文件，可前滚接回；同一会话不产生分叉）
- **旁路问答**：TUI 中 `/btw 问题` 打开独立 dock，可连续追问；每会话仅内存保留 20 轮，不调用工具、不写主上下文或输入历史，`Alt+B` 切换面板
- **MCP 支持**：可接入外部 MCP 服务器扩展工具集；支持 2026-07-28 的 MRTR（server 可在 `tools/call` 中经 elicitation 表单/URL 向用户提问）
- **插件**：本地目录里的 JavaScript 插件（内嵌 QuickJS）可注册工具、斜杠命令、提示词贡献与生命周期钩子（会话、提示、模型、压缩、权限、工作区、后台任务与子代理），见 [docs/plugins.md](docs/plugins.md)
- **workflow**：`/workflow` 激活的动态多 actor 编排——确定性引擎、phase/role/ask/world.run、审批窗与 run 面板、journal/resume/amend、hole 填补与 artifacts，未激活会话零泄漏，见 [docs/workflow.md](docs/workflow.md)
- **循环保护**：自动检测无进展循环与重复内容，防止 token 浪费

## 安装

```bash
cargo build --release
```

需要 Rust 1.88+（brush-parser 的 MSRV）。构建产物为 `target/release/moc`；PTC Host 已内嵌在同一个二进制中，首次 `execute` 时由 `moc` 自身以隐藏 worker 模式启动一个长期子进程。`cargo install --path crates/moc-cli` 只安装 `moc`。

shell 选择：Unix 上只接受 bash/zsh（fish/nushell 等回退 bash）；Windows 按 Git Bash → PowerShell → cmd 回退。实际解释器会写进系统提示的 `<env>`，命令始终在对应语法下执行。

## 快速开始

1. 启动 TUI：
   ```bash
   ./target/release/moc
   ```

2. 首次运行会提示配置模型。按 `Ctrl+G`（或输入 `/config`）进入设置：
   - 添加供应商（如 OpenAI），填入 API 地址与密钥
   - 按 `F` 获取模型列表，勾选需要的模型
   - 返回模型选择器选择本次使用的模型，之后 Web 和 TUI 都会记住它

3. 在底部输入框输入需求，按 `Enter` 发送。TUI 中按 `Ctrl+H` 查看完整快捷键帮助。

## 用法

```bash
moc                           # 启动 TUI（默认）
moc web                       # 启动浏览器前端（默认 127.0.0.1:7432，占用则改用随机端口）
moc acp                       # 以 ACP agent 身份服务 stdio（Zed 等编辑器接入）
moc --print "问题"            # 一次性非交互输出
moc --list                    # 查看会话列表
moc --resume <id>             # 恢复历史会话
```

行式前端的日志写 stderr，stdout 只输出应答正文。需要调试时设置 `RUST_LOG=debug`。

`--print` 面向脚本与 CI，可搭配：

- `--output-format text|json|stream-json`：`text` 只打最终答案（默认）；`json` 结束时输出一个汇总对象（会话 id、最终答案、变更文件、工具调用汇总、token 用量、耗时与错误）；`stream-json` 把每条会话更新输出为一行 NDJSON，末行是同一个汇总对象
- `--max-turns N`：限制单次提示的模型请求轮数；达到上限后不再发起新请求（默认无限制）
- `--allow-tools a,b`：自动批准列出的工具的权限申请（逗号分隔，可重复；`all` 放行全部），未列出的申请按拒绝处理

退出码：`0` 成功；`1` 模型/网络等一般错误；`2` 存在被权限拒绝的工具调用；`3` 超出 `--max-turns` 轮次上限。
浏览器前端的架构、令牌与跨进程写租约见 [docs/web.md](docs/web.md)；ACP 协议覆盖与编辑器配置见 [docs/acp.md](docs/acp.md)。

## 常用交互

| 操作 | 按键 / 命令 |
|---|---|
| 发送消息 | `Enter` |
| 换行 | `Shift+Enter` |
| 停止当前生成 | `Esc` |
| 打开配置 | `Ctrl+G` 或 `/config` |
| 切换模型与思考等级 | `Ctrl+P` 或 `/model`（`Tab` 或 `←→` 就地切换思考等级）；`/config` 的模型角色字段用同一个选择器 |
| 查看上下文内容 | `/context`（顶部进度条点击打开占用明细） |
| 查看实际可用工具 | `/tools`（TUI、WebUI 弹窗展示当前会话下一轮请求发送的工具） |
| 查看工具调用统计与压缩次数 | `/debuginfo`（TUI、Web、行式 CLI） |
| 查看当前会话身份与存储大小 | `/session`（TUI、Web、行式 CLI） |
| 导出会话为 Markdown | `/export [path]`（TUI、Web、行式 CLI；工具折叠、常见密钥脱敏，Web 在大小允许时同时下载，不覆盖已有文件） |
| 压缩技能目录 | `/compact-skills` 或 `/compact-skills compact`（仅缺失/失效缓存）、`/compact-skills force`（全部重压） |
| 清理技能缓存 | `/compact-skills cleanup`（删除来源已不存在的记录）、`/compact-skills cleanup all`（全部清空） |
| 切换思考显示档位 | `Ctrl+T` |
| 切换工具显示档位 | `Ctrl+O` |
| 复制选中文本 | `Ctrl+C` / 右键 / `y`（有选区时，OSC52） |
| 计划模式 | `/plan` |
| 会话目标 | `/goal`（设目标后自动续跑直到达成；`/goal pause`/`resume`/`clear` 控制） |

完整快捷键与 TUI 行为见 [docs/tui.md](docs/tui.md)。

## 项目配置

- 全局配置：`~/.moc/config.toml`
- 项目覆盖：工作目录下 `.moc/config.toml`
- 模型选择：`~/.moc/model-selection.toml`（Web、TUI 共用最近主动选择；恢复会话保持自身模型）
- TUI 偏好：`~/.moc/tui.toml`（仅界面，不发送给模型）
- API 参数：TUI 与 WebUI 共用按协议分组的配置入口，支持供应商默认值与模型覆盖，见 [API 请求参数配置](docs/api-parameters.md)

## 开发

```bash
cargo test          # 运行测试
cargo build         # 调试构建
```

上下文压缩的优先级、保留边界与诊断日志见 [压缩规则](docs/compaction.md)。真实模型与语料的多轮测试、请求录制和离线重放见 [实验使用说明](docs/compaction-lab.md)。

## Workspace 结构

- `crates/moc-core` — Agent 内核：会话、提示调度、工具执行、循环保护
- `crates/moc-plugin` — 本地插件系统：目录发现、QuickJS 运行时、宿主桥与 `moc` 模块类型声明
- `crates/moc-ptc-protocol` — PTC 宿主与 worker 共用的 JSON 协议和 prelude
- `crates/moc-ptc-worker` — 内嵌于 `moc` 的 QuickJS worker 执行循环
- `crates/moc-protocol` — JSON-RPC 协议层
- `crates/moc-llm` — LLM 传输与编解码
- `crates/moc-tui` — 终端 TUI 前端
- `crates/moc-web` — 浏览器前端（axum + WebSocket 桥）
- `crates/moc-acp` — ACP agent 前端（stdio/NDJSON，官方 `agent-client-protocol` SDK）
- `crates/moc-cli` — 命令行入口与内嵌 PTC worker 入口

### 模块导航与拆分约定

文件按共同变化的领域职责拆分，不为缩短行数新增 crate、动态分派或运行时消息层。
普通实现以约 200–600 行为参考，超过 1,000 行时检查职责是否混合；行数不是硬性设计目标。
测试按行为主题组织；协议生成物、声明式表格和跨领域集成测试不机械套用实现文件阈值。

| 领域 | 入口与实现归属 |
|---|---|
| TUI 事件 | `moc-tui/src/event.rs` 保留启动、事件循环；`event/` 分管输入、历史分页、协议更新、计划、代理、命令与模态交互 |
| 模态键盘 | `event/modal_key.rs` 路由；子模块处理各类模态，栈上的延迟动作在模态借用结束后按原顺序执行 |
| 应用状态 | `app.rs` 保留状态所有权；`app/` 按转写、会话、输入、选择器、进度与显示操作组织固有方法 |
| 转写渲染 | `render/scrollback.rs` 维护视口与缓存；子模块负责批次/树索引、消息、工具内容及布局；估高与实际渲染共享实现 |
| Markdown / 设置 | `render/markdown/` 按解析事件、行内排版、媒体与表格拆分；`settings/` 分离导航、编辑、持久化和渲染 |
| Core 服务 | `service.rs` 保留共享状态与入口；`service/protocol.rs` 是唯一 `AgentService` 实现，委派会话、配置、插件、资源与工作流操作 |
| 提示执行 | `service/prompt/` 分离生命周期、输入提交、重试、动态提醒、执行循环与工具授权/批次执行 |
| 会话 | `session/` 按生命周期、历史分页、回退、持久化、配置、租约与投影组织；`SessionTable` 仍是状态所有者 |
| 工作流 | `workflow/compile/analyze/` 分离遍历、站点、静态检查、依赖追踪与计划装配；引擎和运行时分别组织调度、请求、审批、执行与 journal |
| 插件 / LLM | 插件宿主区分发现、激活、钩子和调用；Anthropic 编解码区分请求、响应及流式状态机 |

维护这些边界时遵守以下约束：

- 生产模块显式导入依赖；内部协作优先私有或 `pub(super)`，跨领域契约才使用 `pub(crate)` / `pub`。
- 移动实现不改变数据所有权、锁作用域、取消清理、事件发送及持久化顺序，不通过新增 clone、锁或通道解决拆分后的访问问题。
- 模态、布局和缓存的关联不变量随实现一起移动，避免一处改估高、另一处忘改绘制。
- `#[cfg(test)] mod tests;` 的测试文件可按主题继续拆分；行为断言不能因换目录而削弱。
- 不用扫描源码字面量或固定 helper 名称代替行为测试；渲染正确性通过 buffer、交互与真实终端验证。
- 验证使用 `cargo test --workspace --locked`；性能比较使用相同 release 配置、相同输入、交替运行的前后版本，不能把文件变短等同于运行更快。

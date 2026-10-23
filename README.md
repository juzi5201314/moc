# moc

Rust 编写的终端 AI 编码助手。默认启动 TUI，无终端时自动降级为行式交互；`moc web` 提供浏览器前端，`--print` 支持一次性非交互输出，方便脚本与 CI 集成。

## 特性

- **多前端**：TUI（默认）、浏览器（`moc web`）与行式交互按需切换；每个前端进程内嵌一个 core
- **多进程安全**：同一会话可以被多个进程打开，写租约保证同一时刻只有一个写者，其余前端只读降级
- **多供应商**：OpenAI、Anthropic 及兼容 OpenAI 协议的网关均可接入
- **代码工具**：`read` / `edit` / `write` / `bash` / `grep` / `find`，编辑前带结构化 diff 预览与确认
- **资源限制**：可为 agent 的 bash 单独限制 CPU 与内存，编译占满机器时可以接受变慢、尽量不失败；默认关闭，见 [docs/limits.md](docs/limits.md)
- **联网工具**：`web_search`（Tavily）与 `web_fetch`（本地抓取，HTML 转 Markdown；无需密钥，GitHub/docs.rs/MDN/arXiv/npm/PyPI 结构化）
- **计划模式**：让模型先写执行计划，审阅后再逐步执行，避免盲目改动
- **子代理**：可 spawn 独立子会话处理并行任务，共享工作目录与工具权限
- **上下文管理**：实时查看上下文占用与真正送进模型的正文，支持压缩与消息回退分叉（独立会话，出身只对用户可见）
- **MCP 支持**：可接入外部 MCP 服务器扩展工具集
- **插件**：本地目录里的 JavaScript 插件（内嵌 QuickJS）可注册工具、斜杠命令、工具钩子与提示词贡献，见 [docs/plugins.md](docs/plugins.md)
- **循环保护**：自动检测无进展循环与重复内容，防止 token 浪费

## 安装

```bash
cargo build --release
```

需要 Rust 1.88+（brush-parser 的 MSRV）。构建产物为 `target/release/moc`；PTC Host 已内嵌在同一个二进制中，首次 `run_code` 时由 `moc` 自身以隐藏 worker 模式启动一个长期子进程。`cargo install --path crates/moc-cli` 只安装 `moc`。

shell 选择：Unix 上只接受 bash/zsh（fish/nushell 等回退 bash）；Windows 按 Git Bash → PowerShell → cmd 回退。实际解释器会写进系统提示的 `<env>`，命令始终在对应语法下执行。

## 快速开始

1. 启动 TUI：
   ```bash
   ./target/release/moc
   ```

2. 首次运行会提示配置模型。按 `Ctrl+G`（或输入 `/config`）进入设置：
   - 添加供应商（如 OpenAI），填入 API 地址与密钥
   - 按 `F` 获取模型列表，勾选需要的模型
   - 指定主模型

3. 在底部输入框输入需求，按 `Enter` 发送。TUI 中按 `Ctrl+H` 查看完整快捷键帮助。

## 用法

```bash
moc                           # 启动 TUI（默认）
moc web                       # 启动浏览器前端（默认 127.0.0.1:7432）
moc --print "问题"            # 一次性非交互输出
moc --list                    # 查看会话列表
moc --resume <id>             # 恢复历史会话
```

行式前端的日志写 stderr，stdout 只输出应答正文。需要调试时设置 `RUST_LOG=debug`。
浏览器前端的架构、令牌与跨进程写租约见 [docs/web.md](docs/web.md)。

## 常用交互

| 操作 | 按键 / 命令 |
|---|---|
| 发送消息 | `Enter` |
| 换行 | `Shift+Enter` |
| 停止当前生成 | `Esc` |
| 打开配置 | `Ctrl+G` 或 `/config` |
| 切换模型与思考等级 | `Ctrl+P` 或 `/model`（`Tab` 或 `←→` 就地切换思考等级）；`/config` 的模型角色字段用同一个选择器 |
| 查看上下文内容 | `/context`（顶部进度条点击打开占用明细） |
| 切换思考显示档位 | `Ctrl+T` |
| 切换工具显示档位 | `Ctrl+O` |
| 复制选中文本 | `Ctrl+C` / 右键 / `y`（有选区时，OSC52） |
| 计划模式 | `/mode plan` |

完整快捷键与 TUI 行为见 [docs/tui.md](docs/tui.md)。

## 项目配置

- 全局配置：`~/.moc/config.toml`
- 项目覆盖：工作目录下 `.moc/config.toml`
- TUI 偏好：`~/.moc/tui.toml`（仅界面，不发送给模型）

## 开发

```bash
cargo test          # 运行测试
cargo build         # 调试构建
```

## Workspace 结构

- `crates/moc-core` — Agent 内核：会话、提示调度、工具执行、循环保护
- `crates/moc-plugin` — 本地插件系统：目录发现、QuickJS 运行时、宿主桥与 `moc` 模块类型声明
- `crates/moc-ptc-protocol` — PTC 宿主与 worker 共用的 JSON 协议和 prelude
- `crates/moc-ptc-worker` — 内嵌于 `moc` 的 QuickJS worker 执行循环
- `crates/moc-protocol` — JSON-RPC 协议层
- `crates/moc-llm` — LLM 传输与编解码
- `crates/moc-tui` — 终端 TUI 前端
- `crates/moc-web` — 浏览器前端（axum + WebSocket 桥）
- `crates/moc-cli` — 命令行入口与内嵌 PTC worker 入口

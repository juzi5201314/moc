# Web 前端使用说明

`moc web` 启动一个 core 进程 + 浏览器前端，并自动用系统默认浏览器打开 WebUI。
它和 `moc`（TUI）是平级的两个入口，不引入 daemon，也不做前后端分离的进程间转发：

```bash
moc web                       # 默认 127.0.0.1:7432，随机令牌，自动打开浏览器
moc web --no-open             # 不自动打开浏览器（SSH / 无图形会话时也会自动跳过）
moc web --port 8080           # 指定端口
moc web --token my-secret     # 固定令牌，方便脚本
moc web --cwd ~/project       # 指定工作目录（沿用 CLI 的 --cwd）
```

启动后终端会打印带令牌的 URL：

```
moc web 已启动：http://127.0.0.1:7432/?token=3f2a...
```

没自动弹出时手动打开即可。令牌缺失或错误时 WebSocket 握手返回 401。

## 能力

- 左侧栏按工作区（cwd）分组列出会话，子代理挂在父会话下；收件箱汇总待处理的权限、提问和计划
- 顶部标签页并行打开多个会话，右侧面板：计划 / Todo / 变更 / 子代理 / 上下文 / 旁路（BTW）
- 流式正文与思维链、工具卡片（结构化 diff，统一 / 分栏）、Markdown 与代码高亮
- Composer：插队 / 队列、`!` bash、斜杠命令、资源补全、图片输入、模型 / 思考等级 / 模式 / 权限切换
- 权限与提问卡片、计划审阅、回退分叉、用量、插件、设置（全局 / 项目配置）
- 目录选择器（`fs/list_dir`）添加工作区；亮 / 暗主题默认跟随系统
- 断线自动重连，重连后恢复已打开的标签

## 构建

前端在 `crates/moc-web/web`（pnpm + React + Vite），`build.rs` 在编译期把 `web/dist`
嵌入二进制，分发仍只需一个可执行文件：

```bash
pnpm --dir crates/moc-web/web install
pnpm --dir crates/moc-web/web build
cargo build --release --bin moc
```

没有 `web/dist` 时 `cargo build` 仍能通过，但只嵌入一页「Web 前端未构建」提示；
设 `MOC_WEB_REQUIRE_DIST=1` 则直接构建失败（release CI 使用）。

开发时一个终端跑 `moc web --token dev --no-open`，另一个跑 `pnpm --dir crates/moc-web/web dev`，
打开 `http://localhost:5173/?token=dev`（Vite 把 `/ws` 代理到 7432）。

## 连接模型：一个前端一个 core

每个前端进程内嵌一个 core：

| 命令 | 进程形态 |
|---|---|
| `moc` | TUI + core |
| `moc web` | Web 服务 + core |
| `moc --print` / `moc --repl` | 行式前端 + core |

浏览器标签页不是独立进程：同一个 `moc web` 下的所有标签页共享同一个 core 和同一张会话表。

- **同一 core 的多标签页**：同一会话可以被多个标签页同时打开；写互斥由 core 内的提示锁负责，第二个提示会收到 `SESSION_BUSY`。
- **跨 core 进程**：每个会话有一把跨进程写租约（Unix `flock` / Windows 命名互斥体），同一时刻只允许一个 core 进程持有。

## 跨进程写租约

写租约在以下时机获取：

- `session/new`（新会话）
- `session/load` 且 `read_only = false`
- 第一次发送提示（兜底）

租约持有到 `session/unload`（TUI 的 `/close`、Web 的「释放会话」）或**进程退出**；
内核在进程死亡时自动释放，不会留下需要手工清理的陈旧锁。

另一个进程以写模式打开同一会话时，core 返回 `-32015`，并在 `data.lock` 里带上持有者：

```json
{"lock":{"pid":12345,"frontend":"tui","process_name":"moc","acquired_at_ms":1760000000000}}
```

前端据此降级为**只读打开**：可以看历史，不能发提示或改会话状态。TUI 在状态栏显示
「只读」，Web 在标题旁显示「只读 · tui」并在输入区提示持有者。

`session/unlock` 只释放本进程持有的租约；别人持有时返回 `-32015`，不支持跨进程强夺。
要接管会话，请让持有者执行 `/close`、点「释放会话」，或退出那个进程。

只读打开不取租约，因此不会阻塞写者，也不会因为写者退出而失效；重新加载即可拿到写租约。

## 安全边界

- 只绑定回环地址（`127.0.0.1` / `::1`），非回环地址直接拒绝启动
- 访问令牌走 URL query，WebSocket 握手时校验；比较使用定长比较
- 浏览器带 `Origin` 时只接受本机来源，防 DNS rebinding 与跨站页面连入
- 令牌会出现在终端输出与浏览器地址栏；共享机器上建议用 `--token` 显式指定并妥善保管

## 与 TUI 的差异

| 能力 | TUI | Web |
|---|---|---|
| 流式正文 / 思维链 | 完整 | 完整 |
| 工具调用卡片 | 完整（diff、图表等） | 完整（结构化 diff、按工具摘要；无图表） |
| 图片输入 | 支持 | 支持（当前模型声明 image 输入时） |
| 计划审阅 / 回退 / 上下文窗 | 完整 | 完整 |
| 多会话 | 单会话 | 标签页并行，同 core 共享，写互斥 |
| git 分支显示 | 支持 | 不支持（协议未提供） |

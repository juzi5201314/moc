# moc

Rust 编写的小型编码 agent：moc-core（Agent 内核）通过 moc-protocol（JSON-RPC 2.0，
内嵌 duplex 传输）同时驱动终端 TUI 与行式 REPL 两个前端。

## Workspace 结构

- `crates/moc-core` — Agent 内核：会话、turn 调度、hashline 锚点编辑、工具执行
- `crates/moc-protocol` — JSON-RPC 协议层：方法定义、会话更新事件、结构化 diff DTO
- `crates/moc-llm` — LLM 传输与编解码（openai_chat / openai_responses / anthropic_messages）
- `crates/moc-cli` — 入口：参数解析、会话派发（默认 TUI / `--repl` 行式）
- `crates/moc-tui` — 终端交互前端（ratatui + crossterm）

## 构建与运行

```bash
cargo build
./target/debug/moc                 # 交互模式：默认进入 TUI
./target/debug/moc --repl          # 行式 REPL
./target/debug/moc --print "问题"  # 非交互一次性输出
./target/debug/moc --list          # 会话列表
./target/debug/moc --resume <id>   # 恢复历史会话（TUI / REPL 均可）
```

无 TTY（管道 / 脚本）时自动降级为行式 REPL。

## TUI

布局自上而下：转写区（markdown 渲染、工具调用盒、diff、todo 树）→
圆角输入框（斜杠补全、历史、粘贴 chip、图片 chip）→ statusline。
美术基调：GrokNight 深色主题（#141414 底、TokyoNight 强调色），
diff 红绿底、OneHalfDark 代码高亮，启动时按终端能力从 truecolor 量化到 256/16 色。

支持鼠标：滚轮滚动、点击标题折叠/展开工具块与思考块、拖选文本后 `y` 复制。

### 快捷键

| 按键 | 作用 |
| --- | --- |
| `Enter` / `Alt+Enter` | 补全菜单确认 / 发送；Alt+Enter 在输入框换行 |
| `↑` / `↓` | 补全菜单中选择命令；否则浏览输入历史 / 多行移动光标 |
| `Tab` | 补全菜单确认；否则切换滚动区 / 输入框焦点 |
| `Shift+Tab` | 输入框聚焦时循环 Normal → Ask → Plan；滚动区聚焦时切回输入框 |
| `Ctrl+T` | 全局展开 / 收起思考内容 |
| `Ctrl+O` | 全局展开 / 收起工具调用 |
| `Ctrl+V` | 粘贴剪贴板图片（wl-paste / xclip） |
| `Ctrl+M` | 模型选择菜单 |
| `Ctrl+R` | 会话恢复菜单 |
| `y` | 复制拖选内容（OSC52 + 本地剪贴板） |
| `Esc` | 清除选择 / 清空输入 / 关闭模态 |
| `Ctrl+C` | 轮次运行中取消；空闲时双击退出 |

斜杠命令：`/help` `/new` `/model` `/resume` `/mode` `/compact` `/quit`。

### statusline

默认渲染包含：模型 · 模式 › 上下文进度条 › 工作目录 › git 分支 › todo 进度 ‖
输入/输出 token、tok/s、轮次计时、费用（需模型 pricing 配置）。

可在 `~/.moc/config.toml` 用模板自定义（占位符按语义着色）：

```toml
[tui]
statusline = "{model} * {mode} -> {workdir}"
```

可用占位符：`{model}` `{mode}` `{context}` `{context_percent}` `{workdir}`
`{branch}` `{todo}` `{tokens_in}` `{tokens_out}` `{tok_s}` `{turn_elapsed}` `{cost}`。

## 工具调用 diff

edit / write 执行时由 core 生成结构化 diff（LCS 对齐、上下文 3 行、
超限截断标记），经 `ToolFinished` 与权限请求携带到前端；权限弹窗内直接
预览将要产生的变更。

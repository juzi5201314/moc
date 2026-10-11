# moc

Rust 编写的小型编码 agent：moc-core（Agent 内核）通过 moc-protocol（JSON-RPC 2.0，
内嵌 duplex 传输）同时驱动终端 TUI 与行式 REPL 两个前端。

## Workspace 结构

- `crates/moc-core` — Agent 内核：会话、提示调度、hashline 锚点编辑、工具执行
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

首次启动不会预置供应商、模型或密钥。TUI 自动打开 `/config`：添加供应商、填写 API 地址与认证，按 `F` 获取模型，勾选需要的模型后添加；也可以手动添加模型。改动即刻生效，无需手动保存。主模型必须明确配置，其他模型角色默认沿用主模型。

全局内核配置位于 `$MOC_HOME/config.toml`（默认 `~/.moc/config.toml`），项目覆盖位于工作目录的 `.moc/config.toml`。TUI 偏好独立保存在 `$MOC_HOME/tui.toml`，不会发送给模型。字段、配置来源、缓存、保留及压缩策略见 [内核配置说明](crates/moc-core/README.md)。

## TUI

布局自上而下：顶部信息条（品牌 · git 分支 · 工作目录 ｜ 上下文进度条 · 费用）→
转写区（markdown 渲染、工具调用盒、diff、todo 树）→ 圆角输入框（斜杠补全、历史、
粘贴 chip、图片 chip；多行输入自动长高，下边框左端是帮助键提示、右端是
模型 · 模式）→ statusline。
内容左右各留 2 格边距、块之间以留白分隔：用户消息是保留圆角边框且背景与转写区一致的面板（末行右侧
给本地时间），助手回复带「耗时 + 时间」尾注，思维链展开成引文带（左竖线 + 抬升底色）。
美术基调：GrokNight 深色主题（#141414 底、TokyoNight 强调色），
diff 红绿底、OneHalfDark 代码高亮，启动时按终端能力从 truecolor 量化到 256/16 色。

支持鼠标：滚轮滚动、点击标题折叠/展开工具块与思考块、拖选文本后 `y` 复制；点击输入框下边框右端的模式名会就地弹出模式菜单（图例本身样式不变），点条目即切换会话模式，`↑↓` + `Enter`、`Esc` 同样可用。

### 配置与运行中输入

`/config` 或 `Ctrl+G` 打开设置页，包含模型、供应商、指令、技能、工具、上下文、会话、MCP、OpenViking、界面和高级设置。供应商与模型按分组展示，`←→` 或 `Enter` 展开／收起；`↑↓` 选择，`Enter` 就地编辑单行字段，多行内容在弹出的小窗里编辑并用 `Ctrl+Enter` 应用。`Space` 切换开关，`Delete` 删除整组，`R` 恢复默认值，`F2` 切换全局／项目，`F4` 显示高级项，`/` 搜索。**每次修改都会立刻写入配置文件并让内核生效，不需要手动保存**；`Ctrl+Z` 撤回，`Ctrl+Y`（或 `Ctrl+Shift+Z`）重做，撤回记录只存在于当前进程内。密钥默认隐藏，`F6` 临时显示；写入前会校验配置及外部修改冲突。

运行中 `Enter` 会把消息作为补充要求交给正在执行的模型；底部 statusline 显示 `Working`、`Thinking`、`Responding`、`Running`、`Waiting` 或 `Stopping`，输入框边框显示 `Esc Stop`。可在“界面”页改为“先停下再重新提示”，即停止当前回答后发送新消息。`Ctrl+Enter` 把消息排到待发送队列（显示在输入框右上方），点击可重新编辑，点击 `×` 取消。队列按会话隔离，在当前回答结束后依次发送；设置页或弹窗打开时暂停派发，关闭后自动继续。

输入框为空且当前没有运行中的提示时连续按两次 `Esc`，会打开消息回退选择器。`↑↓` 在当前用户消息的请求、模型回复和工具结果之间移动，`←→` 切换用户消息，`Enter` 会创建一个保留原会话的回退分支并将上下文切换到选中位置；选中用户消息时会把原消息恢复到输入框，供修改后重新发送。

“界面”页可配置快捷键、思考与工具输出的默认展开状态、完成／需要你回答时的通知。通知只在终端失焦时发送，通过 OSC 9 交给终端呈现；操作系统通知支持取决于终端。鼠标交互自动启用，没有捕获开关。

上游回显的模型 id 或推理强度与本地请求不一致时（网关降级、别名映射、静默降低强度等），转写区右上角会弹出一条告警胶囊，形如 `model a → b ｜ effort max → high`：复用右下角一次性提示的样式，但改用橙色强调，8 秒后自动消失，同一次提示内再次不一致只替换不堆叠，新提示开始时清除。只有上游确实回显了对应字段才会告警，网关不作声不产生任何提示。

`Ctrl+Enter`、`Ctrl+M` 这类组合键需要终端支持区分；不支持时 `Ctrl+M` 等同于 `Enter`，切换会话模式请用 `Alt+M`（键位可在设置页重绑）。

模式菜单与 `/mode` 现在支持标准、计划、PTC 和 `(clm)` 实验模式。切换到计划模式会把会话模型切到 `[models.roles].plan` 指定的模型（设为 `none` 则沿用当前会话模型），退出计划模式（批准计划或手动切出）恢复进入前的模型。`/clm-experimental` 可直接启用 CLM；也可用 `--mode clm-experimental` 创建新会话。CLM 模式会在 TUI 图例显示 `(clm)`，并在首次请求后锁定本会话模式，不能再切换回其它模式；原始会话日志仍完整保留，模型维护的是独立实时上下文视图。

### 快捷键

| 按键 | 作用 |
| --- | --- |
| `Enter` / `Ctrl+Enter` | 发送或补充当前提示 / 加入后续队列 |
| `Shift+Enter` / `Alt+Enter` | 输入框换行 |
| `↑` / `↓` | 补全菜单中选择命令；否则浏览输入历史 / 多行移动光标 |
| `Tab` | 补全菜单确认；否则切换滚动区 / 输入框焦点 |
| `Shift+Tab` / `Alt+M` | 切换思考等级 / 切换会话模式 |
| `Ctrl+T` | 全局展开 / 收起思考内容 |
| `Ctrl+H` | Help 面板（键位可配置，重绑后表内同步） |
| `Ctrl+O` | 全局展开 / 收起工具调用 |
| `Ctrl+V` | 粘贴剪贴板图片或文本（图片优先） |
| `Ctrl+P` | 模型选择菜单 |
| `Ctrl+R` | 会话恢复菜单 |
| `Ctrl+G` | 打开配置 |
| `Alt+↑` | 编辑队首消息 |
| `y` | 复制拖选内容（OSC52 + 本地剪贴板） |
| `Esc` | 运行中停止当前提示；空闲时清除选区或输入，空输入框双击打开消息回退，或关闭模态 |
| `Ctrl+C` | Cancel while running; press again to exit |

斜杠命令包括 `/help` `/new` `/model` `/resume` `/mode` `/clm-experimental` `/ask` `/compact` `/usage` `/config` `/quit`；`/usage` 展示当前会话各模型的调用次数、输入／输出 token、缓存命中率、思考 token、TPS 与激活时间，并附全部模型总和。输入框以 `!` 开头的内容直接执行 Bash 并把命令及结果写入 agent 上下文，以 `!!` 开头则执行后只在界面显示，不写入 agent 上下文。

### 剪贴板

`Ctrl+V` 粘贴剪贴板里的图片，没有图片时粘贴文本；Linux 下图片需要 `wl-paste`
（Wayland）或 `xclip`（X11），WSL 直接读 Windows 剪贴板，Windows Terminal 里按
`Ctrl+V` 即可，无需额外配置。`y` 复制拖选内容到终端与系统剪贴板（OSC52）。

### statusline

内置渲染是「左侧 todo 进度 + 右侧统计」两段：`☑ 2/9 … ⇅ 输入 › 输出 › tok/s ›
提示计时 › 费用`；模型与模式在输入框下边框右端，工作目录/分支/上下文进度在顶部
信息条（费用需要模型 pricing 配置）。

可在 `~/.moc/tui.toml` 用模板自定义（占位符按语义着色）：

```toml
statusline = "{model} * {mode} -> {workdir}"
```

可用占位符：`{model}` `{mode}` `{context}` `{context_percent}` `{workdir}`
`{branch}` `{todo}` `{tokens_in}` `{tokens_out}` `{tok_s}` `{prompt_elapsed}` `{cost}`。

### 上下文占用窗

`/context` 或点击顶部信息条右端的上下文进度条打开。窗内按 `system`、
`project`、`skills`、`tools`、`messages`
逐块列出估算 token 与占已用上下文的比例，每块的色标与下方色条上对应那段同色。
色条按模型上下文窗口定标：已用部分落在左侧，右侧 `─` 是剩余窗口，空会话整条
都是空窗口；色条下方标注各分块右界的累积用量，空间不够时只保留挤得下的标注，
「已用总量」与右端窗口上限优先。token 一律按 k 显示（`9030 → 9.03k`，非零但
不足 10 token 记 `<0.01k`），全 TUI 同一套写法。

## 工具调用 diff

edit / write 执行时由 core 生成结构化 diff（LCS 对齐、上下文 3 行、
超限截断标记），经 `ToolFinished` 与权限请求携带到前端；权限弹窗内直接
预览将要产生的变更。

`edit` 保留单文件 `{path, edits}` 格式，也支持 `{files: [{path, edits}, ...]}`。后者按文件顺序串行应用，每个文件仍必须先由 `read` 提供当前 hashline 锚点；全部成功后回传合并 diff。

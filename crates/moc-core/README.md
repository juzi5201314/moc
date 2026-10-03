# moc-core

`moc-core` 是编码代理内核：通过 `moc-llm` 调用上游模型，通过 `moc-protocol::AgentService` 向前端提供会话、prompt、取消、计划和压缩服务。`moc-cli` 是内嵌该内核的 REPL，不维护第二套代理循环。

## 配置与启动

全局配置为 `$MOC_HOME/config.toml`，默认 `~/.moc/config.toml`。启用 `sources.project_config` 时，会话还会合并其工作目录中的 `.moc/config.toml`，最后应用按顺序传入的 `--config` 覆盖层。表递归合并，标量与数组整体替换。供应商和主模型没有产品预设；TUI 首次启动自动引导配置。

下面是配置结构示例，端点、模型和密钥环境变量必须替换为实际供应商设置：

```toml
[models.roles]
default = "provider/model"
smol = "inherit"
slow = "inherit"
plan = "inherit"

[providers.provider]
api = "openai_chat"
base_url = "https://your-provider.example"
api_key = "env:MOC_API_KEY"

[defaults.models."*-reasoner"]
context_window = 500000
max_output_tokens = 128000
input = ["text", "image"]

[providers.provider.models."example-reasoner"]
reasoning = true
thinking_levels = ["low", "medium", "high", "xhigh", "max"]

[providers.provider.models.model]
context_window = 200000
max_output_tokens = 32000
max_request_bytes = 52428800
input = ["text"]

[tools]
approval_mode = "prompt"

```

`base_url` 会自动补齐 `/v1`；末尾加 `#` 可跳过该处理。`api` 也支持 `openai_responses` 和 `anthropic_messages`。启用推理后，未配置 `thinking_levels` 时默认可选 `low`、`medium`、`high`、`xhigh`、`max`；Anthropic 使用 adaptive thinking 与 `output_config.effort`，不会发送旧式 `budget_tokens`。

`[defaults.models]` 的键支持 glob 匹配，匹配结果按模式具体度合并，具体模型字段优先。例如 `"*-reasoner"` 会匹配 `provider/example-reasoner`；以上名称仅演示配置结构，不代表内置模型。模型的 `max_output_tokens` 是模型自身上限，不再受跨模型的全局输出上限截断。
支持 `input = ["image"]` 的模型会接收所有图片输入形式。输入文本中的独立 `@path/to/image.png`、`@~/image.png` 标记会由 core 读取本地文件并转成 Base64；不再需要配置 `compat.image_url`。Responses 协议中，模型 ID 末段以 `gpt-` 开头时默认启用远程压缩，可用模型字段 `remote_compaction = false` 关闭。

完整工具、上下文与模型兼容配置见 `src/config/defaults.toml`、`src/config/schema.rs` 和 `src/config/model.rs`。不要把供应商密钥提交到仓库。

供应商兼容项放在 `[providers.<provider>.models.<model>.compat]`：

- `supports_prompt_cache_key = false`：Chat/Responses 请求不发送缓存键及相关缓存选项，适用于拒绝扩展字段的上游；不关闭 Anthropic 的消息缓存断点。默认 `true`。
- `requires_reasoning_content_for_all_assistant_turns = true`：回传所有 assistant 思维链，并为缺失的历史补显式空块；不只针对工具调用。默认 `false`。
- `requires_reasoning_content_for_tool_calls` 与 `requires_assistant_content_for_tool_calls`：分别补齐工具调用历史中的推理字段和正文；显式空字符串不会被编码器转换成缺失字段。

供应商的 `connect_timeout_secs` 只限制 DNS/TCP/TLS 建连，默认 20 秒。`timeout_secs` 默认不设置，不限制首包等待或流式空闲；仅用户显式设置时才限制非流式整请求以及流式首包等待和后续空闲。

core 的供应商重试默认是最多 10 次尝试、初始退避 0.5 秒、最长退避 300 秒，并开启抖动。`retry` 表仅覆盖指定字段；连接池会保留客户端注入的选项并按建连超时复用。

### 配置界面与来源

TUI `/config` 按选项卡分区，供应商与模型用可折叠分组展示；单行字段就地编辑，多行内容在弹出的小窗里编辑。每次修改立刻写入并生效：写入前做完整 schema 校验与外部变更检测，再原子写入目标文件；TUI 在内存里保留本次进程的改动历史，可用 `Ctrl+Z` 撤回、`Ctrl+Y` 重做。TUI 偏好单独保存到 `tui.toml`，不属于内核配置。模型发现调用所选协议的 `/models` 端点，处理分页并去重；结果默认不勾选，只导入用户选择的模型。发现接口通常不包含完整上下文窗口、输入能力与定价，添加后需核对这些字段。

来源开关分散在各自页面：指令页控制项目配置文件与用户／项目指令文件，技能页控制 MOC／`.agents` 技能目录，MCP 页控制用户／项目 MCP 配置文件。技能默认启用所有发现项，同名按来源优先级遮蔽，不重复加载，也没有允许名单。已有的 `skills.disable` 用于显式禁用。

技能目录的常驻预算按模型上下文窗口动态计算：取上下文窗口的 `skills.catalog_budget_percent`（默认 1%）与 `skills.catalog_budget_min_tokens`（默认 8000 token）两者的较大者；超出预算的条目降级为短语，名字始终可见。

### 请求、进程与输出预算

- `providers.<id>.max_concurrency`：同一供应商跨会话共享并发限额，`0` 表示不限；取消等待不会占用名额。模型生成和压缩请求使用同一限额。
- `providers.<id>.cache`：`auto`（默认）、`short`、`long`、`off`。自动模式按协议使用缓存，不发额外预热请求；Anthropic 缓存系统前缀及近期历史，OpenAI 兼容协议使用稳定会话路由键而不强塞缓存断点。长保留需要上游支持：Anthropic 为一小时，OpenAI 为 `24h`。`supports_prompt_cache_key=false` 可关闭不兼容的 OpenAI 扩展字段。`smol = "inherit"` 时软压缩沿用主请求的 system/tools 前缀和缓存路由。
- 模型的 `max_request_bytes` 控制单次 JSON 请求体，留空为 50 MiB。达到高水位后，core 只在请求副本中按旧到新淘汰图片，压缩和会话原始历史不被改写；图片 token 按固定视觉估算，不按 Base64 字节计量。
- `tools.job.max_running=4`：限制后台进程数量；`overflow="error"` 默认直接向模型返回超限错误，`"wait"` 等待空闲名额，可取消。前台命令不占后台名额。
- `tools.output`：所有工具（包括 MCP）共用首尾输出预算（各 20 KiB）和读写文件容量上限（100 MiB），超出预算的完整文本自动归档；工具本身的分页、搜索和专项限制仍在各自分组。

### 上下文压缩与解释

`compaction.threshold` 表示开始压缩的触发阈值，默认 `{ mode = "percent", value = 85 }`；固定 token 可写为 `{ mode = "tokens", value = 120000 }`。百分比只乘上下文窗口，不扣除模型 `max_output_tokens`。自动压缩完成还必须进入触发阈值的 80% recovery band；例如 200k 窗口的 170k 触发线要求压缩后不高于 136k，避免在临界线附近反复压缩。上下文溢出恢复只要求压缩后重新放入可用窗口，再继续请求。

上下文计量优先使用最近一次上游完整 input/output usage 作为精确锚点，仅对锚点后的新消息本地估算；同时以当前完整会话、系统提示和工具定义的本地估算作为安全下限。图片按固定视觉 token 估算，不把 Base64 当作文本。压缩改变历史后会清除旧锚点并重新估算。上下文详情按系统提示、指令文件、技能、工具定义及会话历史展示占用分解；供应商 usage 可用时，内部触发使用 provider 锚点与本地估算的较大值。

### 会话保留与工作区记忆

`session.retention` 默认启用：30 天后归档会话，保护全局最新 20 个、每个工作区最新 10 个以及已加载／锁定会话；未引用产物保留 30 天，后台日志保留 14 天。在 `/config` 的会话页按 `G` 预览，再明确确认执行。会话归档为 gzip，仍能列出和恢复；历史、压缩归档及恢复备份引用的产物不会被删除。压缩前的历史会写入 `history://<session>/compaction/INDEX.md` 分段索引，模型可通过 `read` 回读被摘要省略的精确细节。保留策略与上下文压缩互不替代。

OpenViking 页集中管理服务配置和工作区绑定。按 `W` 查看知识域，`Enter` 将旧路径迁移到新目录并保持同一身份；目录不存在只标记，不自动清理。`Delete` 需要明确确认：共享知识域只解除当前路径绑定，最后一个绑定才删除远程记忆。非 Git 工作区同样支持持久身份，目录改名后应使用迁移操作而不是创建新知识域。



```sh
cargo run -p moc-cli -- --models
cargo run -p moc-cli -- --cwd /path/to/workspace
cargo run -p moc-cli -- --print "解释当前项目的入口"
cargo run -p moc-cli -- --resume SESSION_ID
```

交互命令包括 `/mode`、`/plan`、`/approve execute|compact|new|reject`、`/compact`、`/undo`、`/new`、`/sessions`、`/model` 和 `/quit`。Ctrl+C 取消当前提示；TUI 输入框为空时双击 `Esc` 可通过消息回退选择器创建保留原会话的分支并重建上下文。`--print` 不输出流式通知，只打印最后一条 assistant 正文。

## 嵌入接口

- `Core::new(CoreConfig)` 加载配置并建立会话目录，返回 `Arc<Core>`；允许尚未配置模型，以便前端完成首次引导，创建会话时再要求可用主模型。
- `CoreConfig` 包含 `home`、`cwd`、`overlays`、`session_dir` 与可注入的 `moc_llm::Client`。
- 将 `Core` 交给 `moc_protocol::Server`，通过协议客户端调用会话和提示方法。
- 前端退出时调用 `Core::shutdown().await`，等待取消收尾并回收后台进程。
- `resolve_models` 提供配置中的模型列表，不进行网络探测。

## 行为与安全边界

- 默认授权模式是 `yolo`；上述示例显式使用 `prompt`。deny 规则始终生效，未声明确认能力的前端不能默许需确认的工具。
- ask/plan 不注册 edit/write，但 bash 不是操作系统沙箱。工作区检测会报告只读模式违规，不回滚用户可能同时进行的修改。
- hashline 编辑只能引用 read 实际展示的完整行。未服务、漂移或过期范围拒绝执行，并附当前锚点；重启后必须重新建立许可。
- 文件写入使用原子替换。撤销拒绝覆盖编辑之后的外部修改；跨文件 move 分别记录两个文件的撤销，需要两次 `/undo` 恢复两者。
- 取消会保存已经收到的 assistant 内容并补齐未执行调用的工具结果。后台任务继续运行，直至完成、被 kill、会话卸载或 core 退出。
- 会话使用追加式 JSONL，逐条 flush；`session/new` 只登记内存，首次写入 entry 时才创建 transcript 文件，因此从未产生对话内容的空会话不会落盘、也不会出现在 `session/list` 中。压缩与历史重写使用临时文件加 rename。它不是每条记录都 fsync 的事务数据库。
- 压缩依次尝试 remote、shake、soft；原生载荷只用于兼容模型，换模型或载荷失效时恢复日志视图。`auto_continue=false` 会在提示内压缩完成后停止继续请求模型。

## 验证

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

测试使用固定本地上游，真实执行协议连接、文件工具、子进程、PTY 和 CLI。真实供应商验收需要自行提供配置和凭据。
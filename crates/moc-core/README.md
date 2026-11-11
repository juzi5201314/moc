# moc-core

`moc-core` 是编码代理内核：通过 `moc-llm` 调用上游模型，通过 `moc-protocol::AgentService` 向前端提供会话、prompt、取消、计划和压缩服务。`moc-cli` 是内嵌该内核的 REPL，不维护第二套代理循环。

## 配置与启动

全局配置为 `$MOC_HOME/config.toml`，默认 `~/.moc/config.toml`。`sources.project_config` 默认关闭；启用后会话还会合并其工作目录中的 `.moc/config.toml`——等于信任该仓库：项目配置可改写全局设置并触发其中的 `!命令`。之后按顺序应用传入的 `--config` 覆盖层。表递归合并，标量与数组整体替换。供应商和主模型没有产品预设；TUI 首次启动自动引导配置。

下面是配置结构示例，端点、模型和密钥环境变量必须替换为实际供应商设置：

```toml
[models.roles]
explore = "inherit"

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

[sandbox]
default_mode = "full"
network = true
on_denied = "ask"
allow_unsandboxed = true
overlay_home = true
extra_read_paths = []
extra_write_paths = []
deny_read_paths = []
deny_write_paths = []

```

`[models.roles]` 仅保留 `explore`，默认 `inherit` 表示沿用父会话当前模型。用户主动选择的完整模型 selector（含思考等级、Fusion 组合）由 core 保存到 `~/.moc/model-selection.toml`，Web、TUI 和交互式 CLI 共用。新会话使用最近选择，恢复会话使用其自身模型；子代理、后台任务与 `--print --model` 不更新这份记忆。首次使用或记住的模型已不可用时，需要主动选择，非交互调用传入 `--model`。

计划模式的进入、退出和批准执行均不改变模型。计划期间手动切换的模型会继续保留。旧全局 `models.roles.default` 只在尚无模型记忆时迁移一次；旧 `default`、`plan` 配置读取时移除，保存设置时清除，项目配置不再覆盖日常模型选择。

`base_url` 会自动补齐 `/v1`；末尾加 `#` 可跳过该处理。`api` 也支持 `openai_responses` 和 `anthropic_messages`。启用推理后，未配置 `thinking_levels` 时默认可选 `low`、`medium`、`high`、`xhigh`、`max`；列表第一项为默认等级，`off` 始终可用。也可写入自定义等级（小写字母、数字和 `-_.`，最长 32 个字符），请求时原样发送，或经 `compat.thinking.effort_map` 改写。Anthropic 使用 adaptive thinking 与 `output_config.effort`，不会发送旧式 `budget_tokens`。

`[defaults.models]` 的键支持 glob 匹配，匹配结果按模式具体度合并，具体模型字段优先。例如 `"*-reasoner"` 会匹配 `provider/example-reasoner`；以上名称仅演示配置结构，不代表内置模型。模型的 `max_output_tokens` 是模型自身上限，不再受跨模型的全局输出上限截断。
支持 `input = ["image"]` 的模型会接收所有图片输入形式。输入文本中的独立 `@path/to/image.png`、`@~/image.png` 标记会由 core 读取本地文件并转成 Base64；不再需要配置 `compat.image_url`。
模型字段 `fast_mode = true` 声明该模型接受 `service_tier = "fast"`（默认 `false`）。只有这类模型在模型选择器里显示 Fast Mode 开关；开关是会话级状态（`session/set_fast_mode`，写入会话 JSONL），默认关闭，新会话沿用上一个会话的选择。开关打开且当前模型支持时，主请求、压缩与 `/btw` 请求体都带 `service_tier = "fast"`；切到不支持的模型时开关保留但不生效。

完整工具、上下文与模型兼容配置见 `src/config/defaults.toml`、`src/config/schema.rs` 和 `src/config/model.rs`。不要把供应商密钥提交到仓库。

供应商兼容项放在 `[providers.<provider>.models.<model>.compat]`：

- `supports_prompt_cache_key = false`：Chat/Responses 请求不发送缓存键及相关缓存选项，适用于拒绝扩展字段的上游；不关闭 Anthropic 的消息缓存断点。默认 `true`。
- `requires_reasoning_content_for_all_assistant_turns = true`：回传所有 assistant 思维链，并为缺失的历史补显式空块；不只针对工具调用。默认 `false`。
- `requires_reasoning_content_for_tool_calls` 与 `requires_assistant_content_for_tool_calls`：分别补齐工具调用历史中的推理字段和正文；显式空字符串不会被编码器转换成缺失字段。
- `instruction_role = "user"`：把规定性指令片段（工具政策、PTC 运行时、仓库规则、技能目录、子代理契约与追加指令）改以 user 角色下发，供对 system 角色遵循度不足的上游使用；默认 `"system"`，行为与不配置完全一致。配套的 `instruction_system`（默认 `["intro", "mcp servers", "environment"]`）列出仍留在 system 消息里的描述性片段，留空表示整条指令都用 user；可用片段名（即正文窗里的标题）：`intro`、`tool policy`、`ptc runtime`、`mcp servers`、`repo rules`、`skills`、`plugins`、`subagent`、`environment`、`append`，拼错会在配置加载期报错。分组只改变消息边界与角色：片段正文、顺序与 token 占用不变，Anthropic 下 user 组会与首条用户消息合并。

供应商的 `connect_timeout_secs` 只限制 DNS/TCP/TLS 建连，默认 20 秒。`timeout_secs` 默认不设置，不限制首包等待或流式空闲；仅用户显式设置时才限制非流式整请求以及流式首包等待和后续空闲。

core 的供应商重试默认是最多 10 次尝试、初始退避 0.5 秒、最长退避 300 秒，并开启抖动。`retry` 表仅覆盖指定字段；连接池会保留客户端注入的选项并按建连超时复用。

### 配置界面与来源

TUI `/config` 按选项卡分区，供应商与模型用可折叠分组展示；单行字段就地编辑，多行内容在弹出的小窗里编辑。每次修改立刻写入并生效：写入前做完整 schema 校验与外部变更检测，再原子写入目标文件；TUI 在内存里保留本次进程的改动历史，可用 `Ctrl+Z` 撤回、`Ctrl+Y` 重做。TUI 偏好单独保存到 `tui.toml`，不属于内核配置。模型发现调用所选协议的 `/models` 端点，处理分页并去重；结果默认不勾选，只导入用户选择的模型。发现接口通常不包含完整上下文窗口、输入能力与定价，添加后需核对这些字段。

来源开关分散在各自页面：指令页控制项目配置文件与用户／项目指令文件，技能页控制 MOC／`.agents` 技能目录，MCP 页控制用户／项目 MCP 配置文件（项目级 `.mcp.json` / `mcp.json` 默认不加载，`[mcp] project_config` 开启即信任该仓库，其中声明的命令会被启动）。技能默认启用所有发现项，同名按来源优先级遮蔽，不重复加载，也没有允许名单。已有的 `skills.disable` 用于显式禁用。指令文件里的 `@路径` 导入在同一指令根内展开：相对导入按来源文件所在目录解析，`~/` 与绝对路径也必须落在根内；单文件上限 1 MiB，每个指令文件的导入合计上限 4 MiB，目录、设备、FIFO 与超限文件跳过并告警，代码围栏内的 `@` 不展开，导入深度默认 5 层（`context_files.import_depth`），最终注入另受 `context_files.max_tokens`（默认 16000）约束。

技能正文经 `skill` 工具按需读取，引用与脚本只列出路径、不预读。目录的常驻预算取上下文窗口的 `skills.catalog_budget_percent`（默认 1%）与 `skills.catalog_budget_min_tokens`（默认 8000 token）两者较大值。有缓存的短触发提示完整保留；未缓存描述按最大最小公平份额分配额外预算，短项的剩余额度让给长项，不再因排序靠后退化为只有名字。若预算连全部最小发现提示都容纳不下，则保留发现信息并在目录注明预算不足，而不是隐藏技能。

#### 手动压缩技能目录

TUI、Web 与行式 CLI 均支持：

| 命令 | 范围与行为 |
|---|---|
| `/compact-skills` 或 `/compact-skills compact` | 当前项目可见、允许模型调用的技能，仅生成缺失或输入已变化的摘要；补全中的 `compact` 是默认入口 |
| `/compact-skills force` | 同一范围全部重压，不删除其他项目的缓存 |
| `/compact-skills cleanup` | 清理全局缓存中来源 `SKILL.md` 已确认不存在的记录；路径无法访问时保留并报告 |
| `/compact-skills cleanup all` | 原子清空全部摘要缓存，不修改技能文件 |

压缩使用当前会话模型及思考等级；Web 空白草稿使用当前选择，直接调用 `skills/compact`，不创建真实会话。请求只有英文压缩提示与技能发现元数据，不包含当前聊天、项目指令、工具或技能正文。摘要目标是简短英文触发提示，硬上限为单行 160 字符；它不是技能正文的替代品。没有待处理项和两个 cleanup 命令都不调用模型。

执行期间占用前台 working，拒绝发送、排队与 steering；TUI 可用 Esc/Ctrl+C、Web 可用停止按钮、行式 CLI 可用 Ctrl+C 取消。TUI 与 Web 使用消息滚动区中的**行内临时状态块**：开始时立即显示，在同一位置更新真实批次进度（已生成并校验数量/总数、缓存跳过数），全部校验后进入原子提交阶段；完成后原位显示压缩/跳过/清理数量及目录 token 预估前后值（也可能增加）。失败或取消也更新同一块。状态不弹窗、不需要关闭、不自动消失，会自然随之后的消息滚动；Web 草稿首次发消息时保留本地显示，但不把它放入发给模型的 input。所有状态不写入 core 聊天历史、上下文或用量事件，空白会话不因此落盘；重新加载或离开草稿后临时显示不保留。

缓存位于 `$MOC_HOME/cache/skill-descriptions.json`。按真实来源路径定位，并核对名称、description、when_to_use、globs/paths 与压缩提示版本的输入指纹；正文变化或模型切换不自动使摘要失效，换模型重压使用 `force`。缓存记录生成模型和时间，不保存密钥。

维护操作持有跨进程独占写锁，普通读取仍可使用旧快照；模型结果先全部在内存校验，提交前复核来源未变化，再同步临时文件并原子替换快照。任何批次失败、输出缺项/重复/超限、来源变化或提交前取消，都不发布本次结果；已经发生的模型调用费用无法回滚。成功后刷新目录索引，下一次请求使用新目录；没有自动后台压缩。

### 请求、进程与输出预算

- `providers.<id>.max_concurrency`：同一供应商跨会话共享并发限额，`0` 表示不限；取消等待不会占用名额。模型生成和压缩请求使用同一限额。
- `providers.<id>.cache`：`auto`（默认）、`short`、`long`、`off`。自动模式按协议使用缓存，不发额外预热请求；Anthropic 的断点落在最后一条含静态前缀的指令消息内的静态末块、指令边界（指令以 system 下发时上提为「系统提示 + 工具定义」的最小缓存单元）与最后一条会进入历史的消息上——本轮追加的提醒不落位（它不进历史，落上去也永远不会命中），静态块断点让同一模板在不同工作目录间复用；OpenAI 兼容协议使用稳定会话路由键而不强塞缓存断点。长保留需要上游支持：Anthropic 为一小时，OpenAI 为 `24h`。`supports_prompt_cache_key=false` 可关闭不兼容的 OpenAI 扩展字段。上下文压缩固定使用当前会话模型；`explore` 角色已供子代理任务选模。
- 模型的 `max_request_bytes` 控制最终 JSON 请求体，留空为 50 MiB。供应商发送边界统一压缩当前、历史及工具图片；内联图片 Base64 共享至多 8 MiB 且不超过请求剩余额度，单图目标 500 KiB。不能在尺寸/质量下限内满足预算时本地报错，不静默删图；原始会话不改写。图片 token 仍按视觉估算，不按 Base64 字节计量。完整策略与 524 诊断见 [图片发送规则](../../docs/images.md)。
- `tools.max_concurrent_calls`：标准模型工具批次中只读工具的同时执行上限；写入工具、bash、询问、计划、todo、skill 与 `run_code` 仍作为顺序屏障。默认 16。
- `tools.todo`：`reminders=true` 时，距上次 todo 变更累计 `reminders_after=12` 次工具调用、且仍有未完成项，就在下一轮注入一次提醒；前端以 `TodoReminder` 占位行展示，每个提示最多 `reminders_max=3` 次。`init` 同时收到 `list` 与 `items` 时以 `list` 为准，缺 `op` 的载荷按无歧义形态推断。
- `tools.job.max_running=4`：限制后台进程数量；`overflow="error"` 默认直接向模型返回超限错误，`"wait"` 等待空闲名额，可取消。前台命令不占后台名额。
- `tools.output`：所有工具（包括 MCP）共用首尾输出预算（各 20 KiB）和读写文件容量上限（100 MiB），超出预算的完整文本自动归档；工具本身的分页、搜索和专项限制仍在各自分组。
- `find` 与 `grep` 默认忽略隐藏项与 `.git/`：需要时显式传 `hidden=true`，两个工具语义一致。

### 子代理

普通模式只允许 `spawn_agent({role: "explorer", task: "…"})` 或批量 `tasks: [{role, task, title?}]` 派发**已注册且启用的固定角色**；`context` 只是共享任务正文。内置可选角色只有 Explorer，custom 由用户安装的插件在加载时注册。主代理不能传 `model`、`preset`、系统提示、工具集或 `output_schema` 来定义/覆盖角色。`agent/list` 同时返回可派发的 `roles: [{name, description}]`；系统提示的 `<agent-roles>` 与原生/PTC 工具声明使用同一角色目录。

`[models.roles].explore` 配置 Explorer 的固定模型：`inherit` 继承父会话真实模型，`none` 禁用该角色；TUI/Web settings 都可选择。没有其它启用 custom 角色时，目录为空、`spawn_agent` 从原生工具表和 PTC 声明中消失，直接 RPC 创建同样拒绝。省略 role 不会回退到通用代理；`sidekick` 不能通过普通 spawn 创建。

Explorer 沿用工具拒绝列表：禁用 `edit`、`write`、`spawn_agent`、`stop_agent`、`wait_agent`、`plan`、`exit_plan`、`clm_context`、`ask`，保留 shell、MCP 与插件工具用于调查，仍受原有引擎、模式和会话权限约束。角色提示要求禁止任何工作区写入，返回调查结论与文件行号证据；这不是额外的 shell/MCP 只读沙盒。原生调用与 PTC 嵌套调用使用同一角色工具策略。

子代理从任务文本开始，不复制父 transcript；任务保留为用户消息，不再提升成能覆盖角色约束的系统指令。固定角色的系统提示与工具策略在创建时冻结，父／兄弟名册作为尾部提醒注入。Fusion 仍保留原有专用 lead/sidekick 通道；workflow 动态 actor 通过内部入口创建，不加入普通角色目录。

父代理给出完整任务后继续独立工作或等待交付，后续消息仅用于必要澄清或解除阻塞。`message_agent.target` 接受代理 ID、`"parent"` 或 `"all"`；运行中消息在安全边界排队注入，不取消当前请求，空闲固定角色可被唤醒。每个目标返回 `injected`／`woken`／`failed` 回执。`wait_agent` 默认等待 180 秒，结果自动通知，窗口超时不构成取消理由。workflow actor 的任务消息限于所属 run 的 driver 或同一 run 的 actor。

子代理用 `yield {report}` 交付最终结果并结束本轮；缺失时最多提醒三次，仍未交付则回退到最后一条 assistant 消息。结果在子代理进入终态时自动作为 notice 投进父会话，父模型下一轮即可看到预览（正文上限 500 字符，超出提示用 `wait_agent` 取全文）。

子代理使用持久子会话 transcript；core 重启后历史条目显示 `idle`，固定角色可继续通过 `message_agent` 交互。workflow actor 的执行权属于所属会话的活动 run，结束后不能通过 `session/prompt`、`session/retry` 或普通 `agent/message` 独立恢复，应使用 workflow 的 resume/amend。TUI `/agents`（`Ctrl+N`）唤出子代理侧栏，点击卡片聚焦子会话、`Esc` 或双击 `←` 返回主会话、`Ctrl+S` 停止聚焦中的代理；REPL 只提供列表。Web 手动创建入口必须选择服务器返回的固定角色，目录为空时禁用。

子代理的 transcript 与 sidecar 落在**父会话的 sidecar 目录里**（`<会话目录>/<父会话 ID>/<时间戳>_<子会话 ID>.jsonl`），因此代理树与目录树同构：会话列表与按 ID 查找会穿过会话目录发现子代理，删除父会话一并删除整棵子树。兄弟会话目录（`jobs`／`artifacts`／`plans`／`compaction`）里的文件不算会话。

子代理的正文契约与运行时纪律分两层：`<subagent>` 块声明身份（你是子代理、父代理接收你的最终结果）、交付方式（`yield` 或任务不可能）与通信纪律（`message_agent` 只用于提问、阻塞与决策，进度和完成报告一律走 `yield`）；父／兄弟名册作为尾部提醒每轮注入，兄弟状态变化不会让系统前缀缓存失效。

“子代理”身份由会话头的 `parent_session_id` 确定，普通固定角色、Fusion sidekick 与 workflow actor 均经过统一子会话创建层。消息回退分叉与审批计划的新会话执行不是子代理：它们没有父链接，只记录 `forked_from`，模型侧仍是普通会话，没有子代理契约、名册或 `yield`。

### Web 搜索与抓取

`web_search` 查询互联网并返回答案与编号来源；`web_fetch` 抓取 1–20 个 URL 的正文（markdown）。两者都是只读工具，在标准、问答与计划模式可用，但开关独立：

- `web_search` 需要 `[web_search] api_key`，未配置时从工具定义隐藏。查询支持 `site:host`、`-site:host`、`after:YYYY-MM-DD`、`before:YYYY-MM-DD`、引号短语与 `-term`；带时间过滤的查询零结果时自动去掉时间过滤重试一次。每条来源摘要按 240 字符截断。
- `web_fetch` 只受 `[web_fetch] enabled` 控制，不需要密钥。HTML 用 htmd 转 Markdown；`query` 传入时每页只返回相关度最高的 3 个约 800 字符块，省略则返回整页；结果头给出 final URL、HTTP 状态、Content-Type、method 与截断说明。失败项随成功正文一同返回，全部失败仍算成功，便于模型更换地址重试。

`[web_fetch]` 默认 `timeout_secs=30`、`max_bytes=52428800`（50 MiB，单页原始响应）、`max_chars=500000`（单页转换后正文）。一次调用最多 20 个 URL，并发 4 个，整批读取上限 100 MiB；原始响应超限、正文超限都会截断并标记。工具输出超过统一首尾预算（默认 40 KiB）时自动归档到 `artifact://artifacts/<id>-output.txt`，模型可用 `read` 的 `offset`/`limit` 精确回读长文。

站点感知：GitHub issue/PR/README/blob、docs.rs rustdoc JSON、MDN `index.json`、arXiv Atom API、npm registry、PyPI JSON。站点 API 失败、限流或格式变化时回退通用 HTML 抓取；GitHub 匿名 API 限流 60 次/小时，环境中有 `GITHUB_TOKEN` 或 `GH_TOKEN` 时自动携带；docs.rs rustdoc JSON 缓存在会话 sidecar。只接受 `http(s)` 且拒绝含内嵌凭据的 URL，允许 localhost 与私网地址；不做 JS 渲染、PDF/Office 转换与 Stack Exchange。

`[web_search]` 当前只有 Tavily 一个供应商：`api_key` 支持字面量、`env:变量名` 或 `!命令`（与供应商密钥同语法），`base_url` 留空使用官方 `https://api.tavily.com`，自定义代理直接按 `{base}/search` 拼接，`timeout_secs` 默认 30。鉴权失败、限流（429/432/433）与超时分别以 `E_WEB_AUTH`、`E_WEB_RATE_LIMIT`、`E_TIMEOUT` 返回，限流提示附带 `Retry-After`。

### 代码编排（PTC）

`run_code` 在标准、问答、计划和 PTC 模式均可作为代码入口；PTC 模式的原生工具面仅保留它。Core 按需启动一个长期 `moc --ptc-worker` Host，多个逻辑 execution 通过 ID 复用同一进程；每次 execution 都创建全新的 QuickJS Runtime/Context，因此全局变量、Promise 和堆不会跨 execution 泄漏。Host 与 core 通过一行一条 JSON 的 RPC 推进脚本；工具和 MCP 调用由本次提示创建的目录按权限执行，不会把完整 MCP schema 注入模型，也不会跨项目共享目录。脚本应先用 `catalog.search()` / `catalog.describe()` 发现 MCP，再通过 `tools[name](args)` 调用。

脚本工具结果固定为 `{content, summary, structuredContent, isError, status, artifact}`；结构化 MCP 数据保留原始 JSON，文本预算只影响模型展示。失败会 reject 带 `code`、`message`、`result` 的错误。`state.store/load/remove/keys` 提供会话级 JSON 状态，每次成功写入立即追加到 JSONL，重启和回退按日志重建，普通 JavaScript 全局变量不会跨次保留。

PTC 相关限制位于 `[tools.ptc]`：`memory_limit_bytes`、`max_output_bytes`、`max_result_bytes`、`max_pending_calls`、`max_total_calls`、`max_concurrent_calls`、`max_concurrent_runs`、`host_idle_timeout_secs`、`max_state_bytes` 以及默认/最大超时。写工具和需要授权的调用是并发屏障；脚本返回前必须等待所有调用。同步 JS 循环由 QuickJS interrupt watchdog 中断，Core deadline 覆盖完整工具等待；取消若超过宽限期无响应会重启整个 Host，并让所有活跃 execution 收到 `E_PTC_WORKER`。worker 不提供文件、网络、进程、模块加载或定时器 API，工作区修改仍由宿主工具的现有授权与事务策略控制。

PTC Host 入口已编译进 `moc`，首次 `run_code` 时将当前 `moc` 作为子进程并传入隐藏参数；因此发布和安装只需要一个 `moc` 文件。Host 崩溃、协议损坏或无法在取消宽限期内停止时，core 会让所有活跃 execution 收到 `E_PTC_WORKER`，随后下一次 execution 懒启动新 Host；空闲 Host 按 `host_idle_timeout_secs` 回收。QuickJS 是原生 C 构建（rquickjs），Windows x86_64 MSVC 支持仍是 experimental，跨平台发布需要在该平台构建。Host 只通过 stdio 通信，后续可以把它放进 OS 沙箱（bwrap 等）作为统一边界。

### 插件

插件是本地目录里的 JavaScript（ES 模块），在 core 进程内用 QuickJS 执行（每个插件一个专用线程与 Runtime，与 PTC 的独立 worker 不同）。发现来源按优先级为项目 `.moc/plugins`（cwd 向上到 git root）、`[plugins] dirs`、`$MOC_HOME/plugins`；**发现即启用**，因此项目级来源等于信任该仓库，`[plugins] project=false` 或 `disable` 是退出口。完整契约见 [docs/plugins.md](../../docs/plugins.md)，类型声明在 `crates/moc-plugin/js/moc.d.ts`。

插件可注册四类能力：工具（`modes` 取 `default`/`ask`/`plan`，默认三种全开；`parallel` 默认 `false` 作为批次屏障）、斜杠命令、工具钩子与提示词贡献。插件工具按声明的 `modes` 决定原生可见性（`default` 覆盖 CLM 实验引擎，PTC 不放行）；`run_code` 的工具目录不列出插件工具，`run_code` 与插件内部都不能调用插件工具（`E_PLUGIN_RECURSION`）。

`beforeToolCall` 只对模型发起的调用生效，返回值可 `deny`（模型收到 `E_PLUGIN_DENIED`）或改写 `arguments`；改写发生在沙盒判定、bash 规则与授权之前，多个插件按加载顺序串联。`afterToolCall` 是观察型，失败只记日志。钩子自身抛错或超时按 fail-open 处理，不会阻断工具执行。

插件提示词汇总进系统提示的 `plugins` 片段（`PIECE_PLUGINS`），按插件名排序拼接并受 `[plugins] max_prompt_chars` 限制；该片段每轮重建，不属于稳定前缀。`moc.tools.call()` 是插件获得文件/搜索能力的唯一通道，只放行 `read`、`grep`、`find`、`web_search`、`web_fetch`。

调用是串行的（同一插件排队）；`load_timeout_ms` 约束模块求值与 `activate`，`call_timeout_ms` 到点会中断 JS 并把该插件在本进程内禁用（`/plugins` 与 `plugin/list` 显示原因）；插件等宿主工具超时只让本次调用失败。`run`/命令/钩子/`prompt`/`activate` 返回的 Promise 会被 QuickJS 任务队列驱动到落定，永远不落定的报 `E_PLUGIN_STATE`。内存上限是单个 Runtime 的 `memory_limit_bytes`。插件目录内容或配置变化后宿主在下一个提示/命令重建。QuickJS 崩溃会带走整个进程——插件不是沙盒，是「你信任的本地代码」。

### 上下文压缩与解释

`compaction.method` 单选压缩实现，默认 `"moc"`：先用 shake 裁剪旧工具结果，腾出的空间不足时再用 soft 生成摘要并保留最近对话。选择 `"grok_build"` 时直接进入 Grok Build 摘要流程：阈值前后台摘要历史前缀，触发压缩时结合该摘要和新增尾部生成 9 段摘要，再整体替换历史，只保留最后真实用户查询、摘要与运行时提醒。Grok Build 不执行 shake，也不使用 `keep_recent_tokens`、`protect_recent_tool_tokens` 或 shake 参数。

新增的 `"eocc"` 是 NVIDIA《You don’t need long context》启发的实验算法：保留有界任务原文，把历史改写成面向剩余工作的六段交接状态，不做 shake 或预火，不保留普通近期尾部；尚未完成的工具组仍保留，避免结果失去调用所有者。三种算法并存，默认及原有两种算法不变。它们均归档压缩前历史，并提供 `history://compaction/INDEX.md` 回读指引；Grok Build 和 EoCC 用独立提醒携带运行状态。EoCC 的预算、配置示例与论文结论边界见 [上下文压缩说明](../../docs/compaction.md#eocc实验性交接状态)。

手动 `/compact <指令>` 会直接进入所选实现的摘要步骤；不带指令时按该实现的默认流程执行。自动压缩及上下文溢出恢复也使用同一 `method`。

旧压缩配置不兼容：删除 `compaction.method_order`、`compaction.soft_method`、整个 `[compaction.remote]` 表和模型字段 `remote_compaction`，改用 `[compaction]` 下的 `method = "moc"` 或 `method = "grok_build"`。这些旧键会触发未知字段错误，不自动迁移。旧会话记录中的 `native` 字段只接受解析并忽略；原生载荷不再回放。旧 remote checkpoint 的文本仅是文件操作占位说明，不能替代原生摘要，因此加载时作废其截断边界，从最近一次 reset 后恢复原始历史，再按当前压缩配置处理。

`compaction.threshold` 表示开始压缩的触发阈值，默认 `{ mode = "percent", value = 85 }`；固定 token 可写为 `{ mode = "tokens", value = 120000 }`。百分比只乘上下文窗口，不扣除模型 `max_output_tokens`。自动压缩完成还必须进入触发阈值的 80% recovery band；例如 200k 窗口的 170k 触发线要求压缩后不高于 136k，避免在临界线附近反复压缩。上下文溢出恢复只要求压缩后重新放入可用窗口，再继续请求。

上下文计量优先使用最近一次上游完整 input/output usage 作为精确锚点，仅对锚点后的新消息本地估算；同时以当前完整会话、系统提示和工具定义的本地估算作为安全下限。图片按固定视觉 token 估算，不把 Base64 当作文本。压缩改变历史后会清除旧锚点并重新估算。上下文详情按 `system`、`AGENTS.md (p+g)`、`skills`、`tools`、`messages` 展示占用分解，插件贡献提示词时额外显示 `plugins`；供应商 usage 可用时，内部触发使用 provider 锚点与本地估算的较大值。

WebUI 与 TUI 共用 `session/context` 的分类口径：`AGENTS.md (p+g)` 合计项目（project）与全局（global）规则及其导入内容；`tools` 包含工具定义/schema、tool policy、PTC 使用说明、工具声明和系统前缀中的 MCP 服务器目录；`system` 保留身份、环境、自定义系统提示词及代理协作指令。`skills` 统计常驻技能目录，`plugins` 只统计插件贡献的提示词；会话历史、工具调用与结果、按需加载的技能正文，以及执行过程中注入的运行时提醒（含 PTC MCP 工具索引）都归 `messages`。分类只改变占用展示，不改变模型收到的提示词或上下文总量。

`session/context_content` 将同一快照的用量与来源分类一起返回；`session/context_read` 只读取指定分类的目录或单条正文，不暴露请求 JSON 树，也不通过父节点拼接全文。所有工具结果及手动 Bash 输出不可读取；用户输入超过 2,000 个 Unicode 字符或 80 行时整条正文不可读取。隐藏项只保留来源、原因与用量，正文不进入查看器的内存／临时文件缓存；原模型请求与统计保持不变。来源由日志元数据标记，不由用户正文或界面显示名称推断。可见长正文按 UTF-8 边界分页，连接级快照的过期、关闭与隔离规则不变。

`session/set_model` 在切换前按目标模型窗口与它在当前压缩配置下的触发线检查上下文：装得下就直接切换，装不下则按参数 `overflow` 处理——`switch` 直接切换并把缺口回报给前端，`compact` 先切换再由新模型压缩上下文，`ask` 不切换、只回报缺口让前端询问用户。压缩在切换后由新模型执行，已有文本摘要仍可跨模型使用。压缩失败不回退切换，失败原因随结果返回。

### CLM 实验引擎

使用 `--engine clm` 或 `/clm-experimental` 启用。模型可通过 `clm_context` 工具读取、替换或追加实时工作上下文；不可变 JSONL transcript 作为完整审计源保留。TUI 状态栏显示 `(clm)`，会话第一次请求后引擎锁定，不能切换回其它引擎。

### 会话保留

`session.compress` 默认关闭；打开后新建会话写 `.jsonl.zst`（独立 zstd 帧拼接），已有会话保持原格式，读取自动识别两种格式。

`session.retention` 默认启用：未被引用的产物保留 30 天，后台日志保留 14 天；在 `/config` 的会话页按 `G` 预览，再明确确认执行。历史、压缩归档及恢复备份引用的产物不会被删除。压缩前的历史会写入 `history://compaction/INDEX.md` 分段索引，模型可通过 `read` 回读被摘要省略的精确细节。保留策略与上下文压缩互不替代。

```sh
cargo run -p moc-cli -- --models
cargo run -p moc-cli -- --cwd /path/to/workspace
cargo run -p moc-cli -- --print "解释当前项目的入口"
cargo run -p moc-cli -- --resume SESSION_ID
```

交互命令包括 `/help`、`/quit`（`/exit` 同义）、`/cancel`、`/engine`、`/autonomy`、`/clm-experimental`、`/permission full|workspace|workspace-only|clear`、`/plan`、`/ask`、`/approve execute|compact|new|reject`、`/agents`、`/compact`、`/usage`、`/new`、`/sessions` 和 `/model`。`/usage` 展示当前会话各模型的调用次数、输入／输出 token、缓存命中率、思考 token、TPS 与激活时间，并附全部模型总和。输入框以 `!` 开头的内容直接执行 Bash 并把命令及结果写入 agent 上下文，以 `!!` 开头则执行后只在界面显示，不写入 agent 上下文；两者都不触发模型请求。`workspace` / `workspace-only` 要求 core 启动时探测到可用的 OS sandbox runner（Linux `bwrap`、macOS `sandbox-exec`）；原生 Windows 只提供 `full`，WSL2 走 Linux 实现。包管理器写 `$HOME` 缓存由 Linux HOME overlay 或 workspace-only 合成 HOME 兜住，不需要枚举缓存目录；细则见「行为与安全边界」。

## 嵌入接口

- `Core::new(CoreConfig)` 加载配置并建立会话目录，返回 `Arc<Core>`；允许尚未配置模型，以便前端完成首次引导，创建会话时再要求可用主模型。
- `Core::new_with_sandbox_support(CoreConfig, SandboxSupport)` 用于测试或嵌入方注入沙盒能力；`Core::sandbox_support()` 返回当前平台可用模式。
- `CoreConfig` 包含 `home`、`cwd`、`overlays`、`session_dir` 与可注入的 `moc_llm::Client`。
- 将 `Core` 交给 `moc_protocol::Server`，通过协议客户端调用会话和提示方法。
- 前端退出时调用 `Core::shutdown().await`，等待取消收尾并回收后台进程。
- `resolve_models` 提供配置中的模型列表，不进行网络探测。

## 行为与安全边界

- `full` 是 yolo：bash 不做文件系统/网络隔离，但危险命令拒绝规则始终生效。
- `[tools.bash].allow_patterns` 已移除，旧配置中的该字段会触发未知字段错误；删除即可。非 full 命令不再有“批准后以 full 执行”的旁路。
- 非 full 模式下一律先跑 OS 沙盒：`workspace` 以只读方式挂载宿主根，工作区、会话 scratch、`[sandbox].extra_write_paths` 可写，`$HOME` 在 Linux 上是会话级 copy-on-write 层；`workspace-only` 只挂载运行时只读系统路径、工作区、scratch 与合成 HOME，宿主 HOME 不存在。`[sandbox].network=false` 时 Linux 使用独立网络命名空间，macOS 拒绝网络。没有白名单旁路：命令失败后由模型带 `sandbox_permissions` + `justification` 申请更宽模式，用户可允许一次、本会话始终允许该命令/该前缀或拒绝；路径越界同理，可授权单次或本会话路径。授权只存在内存，切换模式或 `/permission clear` 即清空。`read` / `find` / `grep` 与 `write` / `edit` 共用同一套路径策略，执行阶段还会复校验一次。
- `workspace` 仍可读宿主文件（除 `[sandbox].deny_read_paths`），不是保密边界；`workspace-only` 才隐藏宿主 HOME。MCP、`web_fetch`、`web_search` 与 PTC worker 在 core 进程执行，不受 OS 沙盒约束。
- ask/plan 不注册 edit/write；bash 仍受当前会话 permission 控制。工作区检测会报告只读模式违规，不回滚用户可能同时进行的修改。
- 计划模式进入、退出、批准均不修改模型，也不保存或恢复旧模型。`session/new` 的 `plan_active` 只决定模式。
- `plan` 保存/修订会话计划文件与 Draft entry（`markdown` 全量写草稿、`edits` 锚点增量修订，对已评审版可播种新草稿）；`exit_plan({})` 提交工作草稿触发审阅并结束当前模型轮次，不退出计划模式。前三种审批执行动作均沿用当前会话模型，其中 `ExecuteCompacted` 的摘要、预算和执行请求均使用该模型；压缩失败保持计划待审且不提前切换模式。新会话执行将计划标记为 Approved，源会话仍保持计划模式，新会话继承当前模型开始 Normal 执行（独立会话，只在会话头记出身，不建立父子链接）。CLI 的 `/approve execute|compact|new|reject` 继续可用。
- 文件写入使用原子替换。撤销拒绝覆盖编辑之后的外部修改；跨文件 move 分别记录两个文件的撤销，需要两次 `/undo` 恢复两者。覆盖超过读写容量上限（`tools.output.max_file_bytes`，默认 100 MiB）的旧文件时不产出 diff、也不记录撤销，工具结果里会说明。撤销栈每个文件最多保留 8 份、全会话合计 64 MiB，超限淘汰最旧记录，被淘汰的快照不能恢复。
- 取消会保存已经收到的 assistant 内容并补齐未执行调用的工具结果。后台任务继续运行，直至完成、被 kill、会话卸载或 core 退出。
- 会话使用追加式 JSONL，逐条 flush；`session/new` 只登记内存，首条消息 entry 才把 header 与已缓冲的簿记条目一并落盘，因此只改过配置、从未产生对话内容的空会话不会落盘、也不会出现在 `session/list` 中。压缩与历史重写使用临时文件加 rename。它不是每条记录都 fsync 的事务数据库。
- `compaction.method` 可选 `moc`（shake 不足再 soft）、`grok_build`（Grok Build 两遍摘要）或 `eocc`（实验性有界交接状态）；三者都提供历史归档回读指引。`auto_continue=false` 会在提示内压缩完成后停止继续请求模型。

### 循环保护

开箱即用，不需要单独配置。`[limits].loop_detection=true` 同时启用流内和跨轮检测：

- 思考／正文：4096 个 Unicode 字符的滚动窗口检测精确周期；有界段落指纹检测中英文高度相似内容，低信息增量规则仅用于思考。代码围栏、常见代码行、数字和单字符填充不作为正文循环证据。
- 工具参数：识别多个 JSON 顶层值重复拼接；合法 JSON 的重复数组元素和字符串内容不按循环停止，仍受参数字节上限约束。
- 工具轮次：保留最近 32 个批次指纹，比较规范化参数、完整未截断结果和读取目标版本；检测长度 1～4 的短周期。无变化周期连续出现 5 次时纠偏一次，之后再次形成周期直接结束。提交变更和未完成的正常等待打断周期证据；已结束任务的反复等待不豁免。

命中保护会释放模型响应连接、取消提示所属子代理、嵌套操作与后台进程组，停止后续模型请求；不会终止其他提示或手动创建的后台任务。已提交的写入保留；正在提交的写入按事务出口完整结束。尚未提交的正文、思考、签名、加密 reasoning 和工具调用不会进入回放；TUI 移除对应生成预览。CLI `--print` 返回非零退出码。协议结束原因是 `loop_detected`，不是 `length`／用户取消，不能触发自动续写、压缩恢复或重试。

提示本身没有轮数、字节、token 与调用次数上限：模型可以连续工作任意多轮，单个请求的生成阶段也没有本地硬时限。唯一保留的时限是连接与重试：`[limits].connect_deadline_secs`（默认 900 秒）约束流式请求收到响应头之前、非流式请求的整个请求。单次请求超过 `[limits].request_warn_secs`（默认 900 秒）只推一条通知，前端显示警告后继续等待：

| 字段 | 默认值 | 含义 |
|---|---:|---|
| `max_length_continues` | 2 | 回复因长度上限中断后自动续写的次数 |
| `loop_detection` | true | 思考、正文与工具参数里的重复内容识别 |
| `connect_deadline_secs` | 900 | 连接与重试硬时限；模型生成阶段不受限制 |
| `request_warn_secs` | 900 | 单次请求耗时警告阈值；只发通知，不终止请求 |

`connect_deadline_secs` 与 `request_warn_secs` 必须为正数，连接与重试时限不得超过 7 天。子代理共享父提示的循环保护状态：父提示命中循环会取消所属子代理。检测并不保证零误报、零漏报。关闭客户端连接是否立即停止供应商计算，取决于供应商取消语义。


## 验证

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

测试使用固定本地上游，真实执行协议连接、文件工具、子进程、PTY 和 CLI。mock 上游用 `MockServer::builder().start()` 创建专属服务器，不要用走全局对象池的 `MockServer::start()`：并发用例共用池化实例会把 mock 串台。真实供应商验收需要自行提供配置和凭据。
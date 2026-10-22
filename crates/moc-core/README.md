# moc-core

`moc-core` 是编码代理内核：通过 `moc-llm` 调用上游模型，通过 `moc-protocol::AgentService` 向前端提供会话、prompt、取消、计划和压缩服务。`moc-cli` 是内嵌该内核的 REPL，不维护第二套代理循环。

## 配置与启动

全局配置为 `$MOC_HOME/config.toml`，默认 `~/.moc/config.toml`。`sources.project_config` 默认关闭；启用后会话还会合并其工作目录中的 `.moc/config.toml`——等于信任该仓库：项目配置可改写全局设置并触发其中的 `!命令`。之后按顺序应用传入的 `--config` 覆盖层。表递归合并，标量与数组整体替换。供应商和主模型没有产品预设；TUI 首次启动自动引导配置。

下面是配置结构示例，端点、模型和密钥环境变量必须替换为实际供应商设置：

```toml
[models.roles]
default = "provider/model"
explore = "inherit"
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

`[models.roles]` 的 `explore` 与 `plan` 默认 `inherit`（沿用主模型）。切换到计划模式时，会话模型应用 `plan` 指定的模型；`plan = "none"` 表示不切换。手动切出计划模式恢复进入前的模型；批准计划执行则使用 `session/plan_approve.execution_model` 指定的模型，字段缺省时使用审批发生时的当前会话模型。模式与模型变化均记入 transcript，卸载重载后保持一致。

`base_url` 会自动补齐 `/v1`；末尾加 `#` 可跳过该处理。`api` 也支持 `openai_responses` 和 `anthropic_messages`。启用推理后，未配置 `thinking_levels` 时默认可选 `low`、`medium`、`high`、`xhigh`、`max`；Anthropic 使用 adaptive thinking 与 `output_config.effort`，不会发送旧式 `budget_tokens`。

`[defaults.models]` 的键支持 glob 匹配，匹配结果按模式具体度合并，具体模型字段优先。例如 `"*-reasoner"` 会匹配 `provider/example-reasoner`；以上名称仅演示配置结构，不代表内置模型。模型的 `max_output_tokens` 是模型自身上限，不再受跨模型的全局输出上限截断。
支持 `input = ["image"]` 的模型会接收所有图片输入形式。输入文本中的独立 `@path/to/image.png`、`@~/image.png` 标记会由 core 读取本地文件并转成 Base64；不再需要配置 `compat.image_url`。Responses 协议中，模型 ID 末段以 `gpt-` 开头时默认启用远程压缩，可用模型字段 `remote_compaction = false` 关闭。

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

技能目录的常驻预算按模型上下文窗口动态计算：取上下文窗口的 `skills.catalog_budget_percent`（默认 1%）与 `skills.catalog_budget_min_tokens`（默认 8000 token）两者的较大者；超出预算的条目降级为短语，名字始终可见。

### 请求、进程与输出预算

- `providers.<id>.max_concurrency`：同一供应商跨会话共享并发限额，`0` 表示不限；取消等待不会占用名额。模型生成和压缩请求使用同一限额。
- `providers.<id>.cache`：`auto`（默认）、`short`、`long`、`off`。自动模式按协议使用缓存，不发额外预热请求；Anthropic 的断点落在最后一条含静态前缀的指令消息内的静态末块、指令边界（指令以 system 下发时上提为「系统提示 + 工具定义」的最小缓存单元）与最后一条会进入历史的消息上——本轮追加的提醒不落位（它不进历史，落上去也永远不会命中），静态块断点让同一模板在不同工作目录间复用；OpenAI 兼容协议使用稳定会话路由键而不强塞缓存断点。长保留需要上游支持：Anthropic 为一小时，OpenAI 为 `24h`。`supports_prompt_cache_key=false` 可关闭不兼容的 OpenAI 扩展字段。上下文压缩固定使用当前会话模型；`explore` 角色已供子代理任务选模。
- 模型的 `max_request_bytes` 控制单次 JSON 请求体，留空为 50 MiB。配置值原样生效（校验只要求大于 0）：设得比触发余量还小时图片会被全部淘汰。达到高水位后，core 只在请求副本中按旧到新淘汰图片，压缩和会话原始历史不被改写；图片 token 按固定视觉估算，不按 Base64 字节计量。
- `tools.max_concurrent_calls`：标准模型工具批次中只读工具的同时执行上限；写入工具、bash、询问、计划、todo、skill 与 `run_code` 仍作为顺序屏障。默认 16。
- `tools.todo`：`reminders=true` 时，距上次 todo 变更累计 `reminders_after=12` 次工具调用、且仍有未完成项，就在下一轮注入一次提醒；前端以 `TodoReminder` 占位行展示，每个提示最多 `reminders_max=3` 次。`init` 同时收到 `list` 与 `items` 时以 `list` 为准，缺 `op` 的载荷按无歧义形态推断。
- `tools.job.max_running=4`：限制后台进程数量；`overflow="error"` 默认直接向模型返回超限错误，`"wait"` 等待空闲名额，可取消。前台命令不占后台名额。
- `tools.output`：所有工具（包括 MCP）共用首尾输出预算（各 20 KiB）和读写文件容量上限（100 MiB），超出预算的完整文本自动归档；工具本身的分页、搜索和专项限制仍在各自分组。
- `find` 与 `grep` 默认忽略隐藏项与 `.git/`：需要时显式传 `hidden=true`，两个工具语义一致。

### 子代理

模型可通过 `spawn_agent` 创建独立上下文的直接子代理，并用 `list_agents`、`message_agent`、`stop_agent`、`wait_agent` 管理。`model` 仅接受 `inherit`、`default`、`explore`、`plan`；`inherit` 使用父会话当前模型，角色从 `[models.roles]` 解析。子代理从任务文本开始，不复制父 transcript；任务原文同时写入其系统提示的 `<subagent>` 块（权威顺序高于通用仓库规则），可通信的父／兄弟名册则以尾部提醒注入、不进系统前缀。父子共享工作目录、工具、MCP 与继承的会话权限；子代理不继承父会话的沙盒路径/命令授权，需要时重新向用户申请。子代理不可创建、停止或等待其它子代理。

`message_agent` 是唯一的通信入口，`target` 接受代理 ID、`"parent"`（仅子代理）或 `"all"`（顶层是全部运行中的直接子代理，子代理是全部运行中的兄弟）；运行中的目标在下一次安全边界收到消息，空闲目标被唤醒新回合，工具为每个目标返回 `injected`／`woken`／`failed` 回执。`wait_agent` 支持批量：省略 `agent_ids` 即等待全部运行中的直接子代理，任一个进入终态、或消息到达、或窗口（默认 180 秒）到期即返回快照，运行期间以 `tool_progress` 推送进度行。

子代理用 `yield {report}` 交付最终结果并结束本轮；缺失时最多提醒三次，仍未交付则回退到最后一条 assistant 消息。结果在子代理进入终态时自动作为 notice 投进父会话，父模型下一轮即可看到预览（正文上限 500 字符，超出提示用 `wait_agent` 取全文）。

子代理使用持久子会话 transcript；core 重启后历史条目状态显示为 `idle`，可继续用 `message_agent` 唤醒。TUI 的人类面只有一个子代理命令 `/agents`（等同 `Ctrl+N`）：名册里 `Enter` 把主 TUI 聚焦到该子代理的会话（转写与输入框都属于它，直接打字即可交流），`Esc` 或双击 `←` 返回主会话，`Ctrl+S` 停止选中的子代理；创建子代理由主模型决定。REPL 只提供 `/agents` 列表。

子代理的 transcript 与 sidecar 落在**父会话的 sidecar 目录里**（`<会话目录>/<父会话 ID>/<时间戳>_<子会话 ID>.jsonl`），因此代理树与目录树同构：会话列表与按 ID 查找会穿过会话目录发现子代理，删除父会话一并删除整棵子树。兄弟会话目录（`jobs`／`artifacts`／`plans`／`compaction`）里的文件不算会话。

子代理的正文契约与运行时纪律分两层：`<subagent>` 块声明身份（你是子代理、父代理接收你的最终结果）、交付方式（`yield` 或任务不可能）与通信纪律（`message_agent` 只用于提问、阻塞与决策，进度和完成报告一律走 `yield`）；父／兄弟名册作为尾部提醒每轮注入，兄弟状态变化不会让系统前缀缓存失效。

"子代理"身份的唯一判据是会话头的 `parent_session_id`，而它只由 `spawn_agent` 写入。消息回退分叉与审批计划的新会话执行都不是子代理：它们是不带父链接的独立会话，只在会话头记 `forked_from`（纯用户侧出身，`/resume` 列表与回退提示里可见），模型侧与普通会话完全一致——没有 `<subagent>` 块、没有父／兄弟名册、没有 `yield`，也不继承源会话的循环保护状态。

### Web 搜索与抓取

`web_search` 查询互联网并返回答案与编号来源；`web_fetch` 抓取 1–20 个 URL 的正文（markdown）。两者都是只读工具，在标准、问答与计划模式可用，但开关独立：

- `web_search` 需要 `[web_search] api_key`，未配置时从工具定义隐藏。查询支持 `site:host`、`-site:host`、`after:YYYY-MM-DD`、`before:YYYY-MM-DD`、引号短语与 `-term`；带时间过滤的查询零结果时自动去掉时间过滤重试一次。每条来源摘要按 240 字符截断。
- `web_fetch` 只受 `[web_fetch] enabled` 控制，不需要密钥。HTML 用 htmd 转 Markdown；`query` 传入时每页只返回相关度最高的 3 个约 800 字符块，省略则返回整页；结果头给出 final URL、HTTP 状态、Content-Type、method 与截断说明。失败项随成功正文一同返回，全部失败仍算成功，便于模型更换地址重试。

`[web_fetch]` 默认 `timeout_secs=30`、`max_bytes=52428800`（50 MiB，单页原始响应）、`max_chars=500000`（单页转换后正文）。一次调用最多 20 个 URL，并发 4 个，整批读取上限 100 MiB；原始响应超限、正文超限都会截断并标记。工具输出超过统一首尾预算（默认 40 KiB）时自动归档到 `artifact://artifacts/<id>-output.txt`，模型可用 `read` 的 `offset`/`limit` 精确回读长文。

站点感知：GitHub issue/PR/README/blob、docs.rs rustdoc JSON、MDN `index.json`、arXiv Atom API、npm registry、PyPI JSON。站点 API 失败、限流或格式变化时回退通用 HTML 抓取；GitHub 匿名 API 限流 60 次/小时，环境中有 `GITHUB_TOKEN` 或 `GH_TOKEN` 时自动携带；docs.rs rustdoc JSON 缓存在会话 sidecar。只接受 `http(s)` 且拒绝含内嵌凭据的 URL，允许 localhost 与私网地址；不做 JS 渲染、PDF/Office 转换与 Stack Exchange。

`[web_search]` 当前只有 Tavily 一个供应商：`api_key` 支持字面量、`env:变量名` 或 `!命令`（与供应商密钥同语法），`base_url` 留空使用官方 `https://api.tavily.com`，自定义代理直接按 `{base}/search` 拼接，`timeout_secs` 默认 30。鉴权失败、限流（429/432/433）与超时分别以 `E_WEB_AUTH`、`E_WEB_RATE_LIMIT`、`E_TIMEOUT` 返回，限流提示附带 `Retry-After`。

### 代码编排（PTC）

`run_code` 在标准、问答、计划和 PTC 模式均可作为代码入口；PTC 模式的原生工具面仅保留它。脚本运行在独立的 `moc-ptc-worker` 进程中（QuickJS-NG/rquickjs 原生执行，无 WASM、无 JIT），worker 与 core 通过一行一条 JSON 的 RPC 推进脚本；工具和 MCP 调用由本次提示创建的目录按权限执行，不会把完整 MCP schema 注入模型，也不会跨项目共享目录。脚本应先用 `catalog.search()` / `catalog.describe()` 发现 MCP，再通过 `tools[name](args)` 调用。

脚本工具结果固定为 `{content, summary, structuredContent, isError, status, artifact}`；结构化 MCP 数据保留原始 JSON，文本预算只影响模型展示。失败会 reject 带 `code`、`message`、`result` 的错误。`state.store/load/remove/keys` 提供会话级 JSON 状态，每次成功写入立即追加到 JSONL，重启和回退按日志重建，普通 JavaScript 全局变量不会跨次保留。

PTC 相关限制位于 `[tools.ptc]`：`memory_limit_bytes`、`max_output_bytes`、`max_result_bytes`、`max_pending_calls`、`max_concurrent_calls`、`max_state_bytes` 以及默认/最大超时。写工具和需要授权的调用是并发屏障；脚本返回前必须等待所有调用。worker 不提供文件、网络、进程、模块加载或定时器 API，工作区修改仍由宿主工具的现有授权与事务策略控制。

`moc-ptc-worker` 随 `moc-cli` 包一起构建并安装，`moc` 启动时查找同目录的 worker 二进制；找不到时 `run_code` 返回 `E_PTC_WORKER`。worker 崩溃或提前退出只影响本次脚本，core 收到 `E_PTC_WORKER` 并继续；超时或取消会终止子进程。QuickJS 是原生 C 构建（rquickjs），Windows x86_64 MSVC 支持仍是 experimental，跨平台发布需要在该平台构建。worker 只通过 stdio 通信，后续可以把它放进 OS 沙箱（bwrap 等）作为统一边界。

### 插件

插件是本地目录里的 JavaScript（ES 模块），在 core 进程内用 QuickJS 执行（每个插件一个专用线程与 Runtime，与 PTC 的独立 worker 不同）。发现来源按优先级为项目 `.moc/plugins`（cwd 向上到 git root）、`[plugins] dirs`、`$MOC_HOME/plugins`；**发现即启用**，因此项目级来源等于信任该仓库，`[plugins] project=false` 或 `disable` 是退出口。完整契约见 [docs/plugins.md](../../docs/plugins.md)，类型声明在 `crates/moc-plugin/js/moc.d.ts`。

插件可注册四类能力：工具（`tier` 取 `read`/`write`/`exec`，默认 `read`；`parallel` 默认 `false` 作为批次屏障）、斜杠命令、工具钩子与提示词贡献。插件工具在模型侧与 MCP 工具同属扩展工具：按风险等级参与模式白名单（标准模式全部放行，问答/计划只放行 `read`，PTC 不放行），`run_code` 与插件内部都不能调用插件工具（`E_PLUGIN_RECURSION`）。

`beforeToolCall` 只对模型发起的调用生效，返回值可 `deny`（模型收到 `E_PLUGIN_DENIED`）或改写 `arguments`；改写发生在沙盒判定、bash 规则与授权之前，多个插件按加载顺序串联。`afterToolCall` 是观察型，失败只记日志。钩子自身抛错或超时按 fail-open 处理，不会阻断工具执行。

插件提示词汇总进系统提示的 `plugins` 片段（`PIECE_PLUGINS`），按插件名排序拼接并受 `[plugins] max_prompt_chars` 限制；该片段每轮重建，不属于稳定前缀。`moc.tools.call()` 允许插件调用宿主只读内置工具，是插件获得文件/搜索能力的唯一通道。

调用是串行的（同一插件排队）；`call_timeout_ms` 到点会中断 JS 并把该插件在本进程内禁用（`/plugins` 与 `plugin/list` 显示原因），内存上限是单个 Runtime 的 `memory_limit_bytes`。KV 存在 `$MOC_HOME/plugin-data/<插件名>/kv.json`，按插件名跨项目共享。QuickJS 崩溃会带走整个进程——插件不是沙盒，是「你信任的本地代码」。

### 上下文压缩与解释

`compaction.threshold` 表示开始压缩的触发阈值，默认 `{ mode = "percent", value = 85 }`；固定 token 可写为 `{ mode = "tokens", value = 120000 }`。百分比只乘上下文窗口，不扣除模型 `max_output_tokens`。自动压缩完成还必须进入触发阈值的 80% recovery band；例如 200k 窗口的 170k 触发线要求压缩后不高于 136k，避免在临界线附近反复压缩。上下文溢出恢复只要求压缩后重新放入可用窗口，再继续请求。

上下文计量优先使用最近一次上游完整 input/output usage 作为精确锚点，仅对锚点后的新消息本地估算；同时以当前完整会话、系统提示和工具定义的本地估算作为安全下限。图片按固定视觉 token 估算，不把 Base64 当作文本。压缩改变历史后会清除旧锚点并重新估算。上下文详情按 `system`、`project`、`skills`、`tools`、`messages` 等固定分块展示占用分解；供应商 usage 可用时，内部触发使用 provider 锚点与本地估算的较大值。

`session/set_model` 在切换前按目标模型窗口与它在当前压缩配置下的触发线检查上下文：装得下就直接切换，装不下则按参数 `overflow` 处理——`switch` 直接切换并把缺口回报给前端，`compact` 先切换再由新模型压缩上下文，`ask` 不切换、只回报缺口让前端询问用户。压缩必须发生在切换之后：`ModelChange` 会作废旧模型的原生载荷与远程摘要，checkpoint 只能由新模型产出。压缩失败不回退切换，失败原因随结果返回。

### CLM 实验模式

使用 `--mode clm-experimental` 或 `/clm-experimental` 启用。模型可通过 `clm_context` 工具读取、替换或追加实时工作上下文；不可变 JSONL transcript 作为完整审计源保留。TUI 状态栏显示 `(clm)`，会话第一次请求后模式锁定，不能切换回其它模式。

### 会话保留与工作区记忆

`session.compress` 默认关闭；打开后新建会话写 `.jsonl.zst`（独立 zstd 帧拼接），已有会话保持原格式，读取自动识别两种格式。

`session.retention` 默认启用：未被引用的产物保留 30 天，后台日志保留 14 天；在 `/config` 的会话页按 `G` 预览，再明确确认执行。历史、压缩归档及恢复备份引用的产物不会被删除。压缩前的历史会写入 `history://compaction/INDEX.md` 分段索引，模型可通过 `read` 回读被摘要省略的精确细节。保留策略与上下文压缩互不替代。

OpenViking 页集中管理服务配置和工作区绑定。按 `W` 查看知识域，`Enter` 将旧路径迁移到新目录并保持同一身份；目录不存在只标记，不自动清理。`Delete` 需要明确确认：共享知识域只解除当前路径绑定，最后一个绑定才删除远程记忆。非 Git 工作区同样支持持久身份，目录改名后应使用迁移操作而不是创建新知识域。



```sh
cargo run -p moc-cli -- --models
cargo run -p moc-cli -- --cwd /path/to/workspace
cargo run -p moc-cli -- --print "解释当前项目的入口"
cargo run -p moc-cli -- --resume SESSION_ID
```

交互命令包括 `/help`、`/quit`（`/exit` 同义）、`/cancel`、`/mode`、`/clm-experimental`、`/permission full|workspace|workspace-only|clear`、`/plan`、`/ask`、`/approve execute|compact|new|reject`、`/agents`、`/compact`、`/usage`、`/new`、`/sessions` 和 `/model`。`/usage` 展示当前会话各模型的调用次数、输入／输出 token、缓存命中率、思考 token、TPS 与激活时间，并附全部模型总和。输入框以 `!` 开头的内容直接执行 Bash 并把命令及结果写入 agent 上下文，以 `!!` 开头则执行后只在界面显示，不写入 agent 上下文；两者都不触发模型请求。`workspace` / `workspace-only` 要求 core 启动时探测到可用的 OS sandbox runner（Linux `bwrap`、macOS `sandbox-exec`）；原生 Windows 只提供 `full`，WSL2 走 Linux 实现。包管理器写 `$HOME` 缓存由 Linux HOME overlay 或 workspace-only 合成 HOME 兜住，不需要枚举缓存目录；细则见「行为与安全边界」。

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
- 计划模式进入时应用 `models.roles.plan`（`none` 时沿用当前模型），手动退出时恢复进入前的模型；审批退出改用选定执行模型。`session/new` 直接以计划模式创建会话同样应用计划角色模型。
- `write_plan` 只保存会话计划文件和 Submitted entry；`exit_plan({})` 触发审阅并结束当前模型轮次，不退出计划模式。前三种审批执行动作使用同一执行模型，其中 `ExecuteCompacted` 的摘要、预算和执行请求均使用该模型；压缩失败保持计划待审且不提前切换模式或模型。新会话执行将计划标记为 Approved，源会话仍保持计划模式，新会话以选定模型开始 Normal 执行（独立会话，只在会话头记出身，不建立父子链接）。CLI 的 `/approve execute|compact|new|reject` 继续可用，未指定模型时沿用当前会话模型。
- 文件写入使用原子替换。撤销拒绝覆盖编辑之后的外部修改；跨文件 move 分别记录两个文件的撤销，需要两次 `/undo` 恢复两者。覆盖超过读写容量上限（`tools.output.max_file_bytes`，默认 100 MiB）的旧文件时不产出 diff、也不记录撤销，工具结果里会说明。撤销栈每个文件最多保留 8 份、全会话合计 64 MiB，超限淘汰最旧记录，被淘汰的快照不能恢复。
- 取消会保存已经收到的 assistant 内容并补齐未执行调用的工具结果。后台任务继续运行，直至完成、被 kill、会话卸载或 core 退出。
- 会话使用追加式 JSONL，逐条 flush；`session/new` 只登记内存，首次写入 entry 时才创建 transcript 文件，因此从未产生对话内容的空会话不会落盘、也不会出现在 `session/list` 中。压缩与历史重写使用临时文件加 rename。它不是每条记录都 fsync 的事务数据库。
- 压缩依次尝试 remote、shake、soft；原生载荷只用于兼容模型，换模型或载荷失效时恢复日志视图。`auto_continue=false` 会在提示内压缩完成后停止继续请求模型。

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
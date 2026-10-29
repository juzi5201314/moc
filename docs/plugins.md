# 插件系统

moc 的插件是**本地目录里的 JavaScript（ES 模块）**，在 core 进程内用 QuickJS 执行。一个插件可以注册四类能力：

- 工具（对模型可见，等同原生工具）
- 斜杠命令（`/名字`，在人机交互界面里执行）
- 工具钩子（`beforeToolCall` / `afterToolCall`）
- 提示词贡献（每轮注入 `<plugins>` 片段）

没有安装命令、没有注册表：把目录放进去就会被发现，改动即时生效（下一个提示或下一次命令）。

## 目录布局

```text
<root>/<name>/plugin.json     # 清单
<root>/<name>/index.js        # 入口（清单的 main 可改名）
<root>/<name>/...             # 插件自己的其它模块、资源
```

`<root>` 按以下顺序扫描，**同名插件按优先级先到先得**：

| 优先级 | 来源 | 说明 |
|---|---|---|
| 项目 | `<cwd>/.moc/plugins`，逐级向上直到 git root | 每个祖先目录都扫，越近的越好 |
| 配置 | `[plugins] dirs` 里的每个目录 | 支持 `~` |
| 用户 | `$MOC_HOME/plugins` | 跨项目可用 |

以 `.` 开头的目录被跳过；符号链接会先规范化再去重，同一个目录不会被加载两次。

**凡是发现到的插件都自动启用**——项目级目录等于「这个仓库的代码会被执行」。克隆不可信仓库前先确认，或临时关掉：

```toml
[plugins]
project = false      # 不扫项目目录
disable = ["noisy"]  # 按插件名禁用
```

## 清单 `plugin.json`

```json
{
  "name": "hello",
  "version": "0.1.0",
  "description": "示例插件",
  "main": "index.js"
}
```

- `name`：必填，1–64 位小写字母/数字/连字符，且以字母开头。它就是 `/plugins` 里的名字。
- `main`：可选，默认 `index.js`；必须是插件目录内的 `.js`/`.mjs`，不能逃出目录。
- 未知字段直接报错，不做猜测。

## 入口模块

ES 模块，`export default` 一个对象：

```js
import { log, session, tools } from "moc";

export default {
  name: "hello",
  version: "0.1.0",

  // 可选：模块求值完成后调用一次；可异步（宿主会等 Promise 落定）。
  // 加载发生在会话之外，所以没有会话上下文：session() 与 tools.call() 不可用。
  async activate() {
    log.info("hello 启用");
  },
  // 可选：进程退出/插件卸载时调用
  deactivate() {},

  tools: {
    hello: {
      description: "向某人打招呼",
      parameters: {
        type: "object",
        properties: { name: { type: "string", description: "称呼" } },
        required: ["name"],
        additionalProperties: false,
      },
      modes: ["default", "ask", "plan"], // 默认三种全开；收窄成 ["default"] 也行
      parallel: false,    // 默认 false：作为批次屏障
      async run(input, ctx) {
        return `你好，${input.name}`;
      },
    },
  },

  commands: {
    hello: {
      description: "打印一次问候",
      usage: "[称呼]",
      run(args, ctx) {
        return `hello ${args.trim() || "world"}`;
      },
    },
  },

  hooks: {
    beforeToolCall(event, ctx) {
      // 返回 { deny } 拒绝本次调用；返回 { arguments } 改写参数
      if (event.name === "bash") return { deny: "本仓库不允许 shell" };
      return undefined;
    },
    afterToolCall(event, ctx) {
      log.debug("工具完成", event.name, event.isError);
    },
  },

  prompt(ctx) {
    return "本仓库的提交信息用中文。";
  },
};
```

### 工具

| 字段 | 说明 |
|---|---|
| `description` | 必填，非空。会作为工具描述进模型提示，按提示词规范写：只讲这个工具自己的用法。 |
| `parameters` | 必填，JSON Schema 对象（`type: "object"`）。原样进模型提示；调用时按其中的 `type`、`required`、`properties`、`additionalProperties`、`items`、`enum` 校验参数。 |
| `modes` | 可选，取值 `default`/`ask`/`plan` 的数组，默认三种全开。取值是会话的执行模式词表（`default` 指无模式限制的常规提示），决定该工具何时允许调用；工具表本身只随引擎变化、不随执行模式裁剪。`default` 同时覆盖 CLM 实验引擎，PTC 不开放插件工具。空数组或未知模式会加载失败。 |
| `parallel` | 默认 `false`。为 `true` 时允许与其它只读工具并发，否则整批作为屏障。 |
| `run(input, ctx)` | 必填。第一个参数是按 schema 校验过的对象（不满足时模型收到 `E_PLUGIN_ARGUMENTS`），第二个是会话快照。 |

工具声明只接受上表中的字段，未知字段会加载失败。`modes` 省略时三种全开，显式传入 `null`、非数组或非字符串元素会加载失败。

`modes` 只约束调用放行，不授予额外能力；插件的宿主能力由 `tools.call` 白名单决定。`run_code` 的工具目录不列出插件工具，也不能调用它们。

工具名全局唯一：与内置工具、MCP 工具（`mcp__` 前缀）、其它插件工具撞名会让该工具被忽略并告警（内置优先）。工具名只允许字母、数字、下划线、连字符。

返回值按下面的规则映射成工具结果：

- `string` → 正文
- `{ content, structuredContent?, isError? }` → 富结果（结构化内容交给宿主呈现）
- 其它 JSON → `JSON.stringify` 后作为正文
- `undefined` / `null` → 空正文

抛异常视为**插件故障**：宿主停用该插件（本进程内）、通知用户，后续调用一律得到 `E_PLUGIN_DISABLED`。
要表达「这次调用失败，但插件本身没问题」，返回 `{ content: "…", isError: true }`，不要 throw。

### 斜杠命令

命令名与插件名同规则，最终就是 `/名字`。`run(args, ctx)` 的 `args` 是命令名之后的原始文本（未分词）。返回值同样按上面的规则映射，作为一条提示写进转写。

命令没有单独的「失败」通道：参数不对就把说明作为文本返回，别用 throw——throw 一律按插件故障处理（停用并通知）。

命令名会进入前端补全表；与内置命令同名时内置优先。

### 钩子

钩子按插件加载顺序串联：后一个看到前一个改写后的值。抛异常或超时只停用出错插件并通知，其余插件与核心操作继续。

**只有模型发起的工具调用会触发工具钩子**：`run_code` 里的子调用、插件内部的工具调用都不重入。

**能改变行为的钩子**（只消费下表中的字段；字段类型不对就忽略并告警，不会停用插件）：

| 钩子 | 返回 | 应用点 |
|---|---|---|
| `beforeToolCall` | `{ deny?, arguments? }` | 沙盒判定、bash 规则与授权之前 |
| `messageReceived` | `{ text? }` | 入站消息写入 transcript 之前；用户输入、steer、代理消息共用 |
| `afterToolCall` | `{ content?, structuredContent? }` | 结果归档、落库、回灌模型与推前端之前 |
| `sessionStart` | `{ messages?: [{ role?: "system"\|"user", text }] }` | 会话创建后、第一条用户消息之前落库 |
| `permissionRequested` | `{ option? }` | 询问用户之前；只能选本次提供的一次性选项（`allow_once` / `reject_once`） |
| `agentStarted` | `{ task? }` | 子代理首轮提示之前 |

**纯观察钩子**（返回值忽略）：

| 钩子 | 时机 | 载荷要点 |
|---|---|---|
| `beforePrompt` | 输入已落库、首次模型请求前 | `promptId, firstPrompt, model, mode, permission, source, text, images` |
| `afterPrompt` | 提示成功 / 失败 / 取消后 | `promptId, finishReason, usage, durationMs, loopDetected, error` |
| `sessionEnd` | 删除 / 卸载前 | `sessionId, reason(deleted\|unloaded)` |
| `sessionConfigChanged` | 引擎 / 自主程度 / 计划态 / 模型 / 权限切换后 | `field, from, to` |
| `beforeModelCall` | 每次上游请求尝试前 | `promptId, attempt, provider, model, effort, messages, tools, contextTokens, maxOutputTokens` |
| `afterModelCall` | 每次尝试落定 | `promptId, attempt, finishReason, usage, firstTokenMs, durationMs, mismatch, error` |
| `beforeCompaction` | 压缩入口 | `reason(threshold\|overflow\|manual\|pre_approve\|switch), instructions, model` |
| `afterCompaction` | 压缩终态 | `reason, ok, method, tokensBefore, tokensAfter, full, durationMs, error` |
| `permissionResolved` | 权限答复后 | `promptId, requestId, option(或 null)` |
| `workspaceChanged` | 工作区 diff 非空 | `violation, changes[]` |
| `planDecided` | 计划批准 / 拒绝后 | `planId, action` |
| `agentFinished` | 子代理终态 | `agentId, parentSessionId, status, finishReason, lastMessage` |
| `jobFinished` | 后台任务结束 | `jobId, command, exitCode, logPath` |
| `userCommand` | 用户手动命令执行完 | `command, durationMs, isError, status, content` |

为什么有些钩子只能观察：

- `beforePrompt` / `afterPrompt`：消息已落库、请求已组装，改写在 `messageReceived` 处完成；提示结果在 `afterPrompt` 时已经定型。
  `/retry` 继续上下文时没有新消息：`beforePrompt` 照常触发（`text` 为空串、`images` 为 `0`、`source` 仍为 `user`），`messageReceived` 不会触发；它剪掉的失败回合只影响会话记录，插件看到的是剪完后的上下文。
- `beforeModelCall` / `afterModelCall`：请求的组装、token 预算与缓存断点归 core；每轮动态文本用 `prompt(ctx)`（系统提示片段）表达。响应是流式发出的，事后无法改写。
- `beforeCompaction` / `afterCompaction`：压缩策略由阈值与 `compaction.method_order` 决定；注入 `instructions` 会隐式把压缩切到 soft 路径，语义变化太大。
- `sessionEnd`：删除是用户的明确意图，插件不应否决。
- `permissionResolved` / `workspaceChanged` / `planDecided` / `agentFinished` / `jobFinished` / `userCommand`：动作已经提交到 transcript 或前端，再改会导致两处不一致。

`sessionStart` 注入的消息会进入 transcript 与模型上下文；Web 前端创建会话后会拉取转写，因此立即可见；TUI 新建会话时不拉历史，注入内容在下次加载会话时显示。

`"1" → "2"`：新增了上述生命周期钩子，原有钩子契约不变。

### 提示词

`prompt(ctx)` 返回字符串（或字符串数组）时，内容放进系统提示的 `<plugins>` 片段，按插件名排序拼接，总长度受 `[plugins] max_prompt_chars` 限制（超出按字符截断）。返回空串等于没有贡献。

这条片段每轮都会重建，**不属于稳定前缀**，不要在这里写每次都变的内容（时间戳、随机数），否则会破坏上游缓存。

## `moc` 模块

| 导出 | 说明 |
|---|---|
| `version` | 宿主 API 版本（当前 `"2"`）。 |
| `session()` | 当前会话快照 `{ sessionId, cwd, mode, permission, model, isSubagent }`；`mode` 是混合词表——执行模式非默认时报模式名（`ask`/`plan`），否则报引擎名（`standard`/`ptc`/`clm_experimental`）。在 `activate`/`deactivate` 里调用会抛 `E_PLUGIN_STATE`（那时没有会话）。 |
| `log.debug/info/warn/error(...)` | 写 moc 日志，`RUST_LOG=moc_plugin=debug` 可见。 |
| `tools.call(name, args)` | 调用宿主工具，返回 `{ content, structuredContent, isError }`。 |
| `config.register/get/set` | 注册配置声明、读取有效值、按安装来源写回本插件配置。 |
| `session.state` / `session().state` / `ctx.state` | 会话内按插件隔离的同步持久化 KV。 |

`tools.call` 的边界：

- 只在工具的 `run` 里可用；命令、钩子、`prompt`、`activate` 里调用会抛 `E_PLUGIN_TOOLS_UNAVAILABLE`
- 只放行白名单里的只读工具：`read`、`grep`、`find`、`web_search`、`web_fetch`；写、shell、`job`、`ask`、`todo`、`run_code`、MCP 工具与插件工具一律拒绝
- 不能调自身或其它插件工具，也不能从 `run_code` 里调用插件工具（`E_PLUGIN_RECURSION`）

## 插件持久化配置

配置字段在模块顶层注册，模块求值结束后冻结。`activate` 可以读取、写入已注册配置，但不能再注册字段：

```js
import { config, session } from "moc";

config.register("prefix", {
  type: "text",
  label: "计数前缀",
  description: "显示在计数结果前面的文本。",
  default: "count",
});
config.register("style", {
  type: "choice",
  choices: ["short", "full"],
  default: "short",
});

export default {
  commands: {
    count: {
      description: "显示会话计数",
      run() {
        session.state.count = (session.state.count ?? 0) + 1;
        return `${config.get("prefix")}: ${session.state.count}`;
      },
    },
  },
};
```

对应配置只保存在本插件的命名空间，不会与 moc 核心字段混在一起：

```toml
[plugins."my-plugin".config]
prefix = "次数"
style = "full"
```

### 注册与读写

- `config.register(key, definition)`：键为 1–128 UTF-8 字节且不能包含控制字符，不能重复注册。声明接受 `type`、`label`、`description`、`default`、`choices`，未知字段报错。
- `type` 可为 `bool`、`integer`、`number`、`text`、`secret`、`multiline`、`strings`、`object`、`choice`。`choices` 仅用于 `choice`，必须为非空、不重复的字符串数组。默认值必须满足字段类型且能无损表示为 TOML；不支持 `null` 或超出有符号 64 位范围的整数。
- `config.get(key)`：返回合并后的有效值，其次为声明默认值；两者都没有则返回 `undefined`。未注册键报错。默认值仅参与读取与 UI 展示，不会自动写入配置文件。
- `config.set(key, value)`：同步写入安装来源所属文件，只能写本插件、已注册的键。`config.set(key, null)` 删除该层显式值，重新使用高优先级覆盖、继承值或默认值；不是把 `null` 写进 TOML。
- 项目 `.moc/plugins` 中的插件写回**安装所在项目**的 `.moc/config.toml`；祖先项目发现的插件写回那个祖先项目，而非当前子目录。用户插件和 `[plugins] dirs` 中的插件写回 `$MOC_HOME/config.toml`。符号链接不改变发现来源的归属。
- 读取遵循全局 → 安装来源项目（如果不同）→ 当前项目 → CLI 覆盖的顺序，因此 `set` 写入来源层后，`get` 仍可能返回更高优先级覆盖的值。配置字符串不额外执行 `env:` 或 `!命令`。
- TUI `/config` 自动显示“插件”分组及声明的标题、说明、默认值和枚举。`secret` 只在界面遮罩，磁盘仍为明文。

启动以及每次调用前都会校验整个插件配置，包括未知键。类型错误、未知键、无效默认值或写盘失败只禁用该插件，核心配置与其它插件继续工作。配置出错时仍保留已取得的字段声明，便于在设置页修复；插件移除后保留其配置，不要求插件在场才能加载 moc。

## 会话持久化 state

`session.state`、`session().state` 与回调参数 `ctx.state` 是同一能力的三种入口，数据按 **session ID + 插件名** 隔离：

```js
const state = session().state;
state.count = (state.count ?? 0) + 1;
state.preferences = { colors: ["blue"], enabled: true };
const value = state.preferences;        // 深冻结的读取快照
state.preferences = { ...value, enabled: false }; // 修改后整值赋回
Object.keys(state);                    // 枚举已保存的键
"count" in state;                      // 是否存在（null 与缺失不同）
delete state.count;
```

- 赋值同步持久化，成功返回后才更新本次回调的读取缓存；写入失败不覆盖旧文件。读取不存在的键返回 `undefined`，赋 `null` 则保存 JSON null。`delete` 删除键。
- 键为 1–128 UTF-8 字节，支持包括 `__proto__` 在内的普通字符串；state 代理使用空原型，不支持修改原型、`defineProperty` 或冻结代理本身。
- 值仅接受无损 JSON：拒绝 `undefined`、函数、Symbol、BigInt、NaN/Infinity、稀疏数组、访问器、循环引用以及 Date 等自定义原型对象，不能靠 `JSON.stringify` 静默省略或转换。即使插件捕获非法写入异常，调用结束时仍会禁用该插件。
- 每个回调首次读取时加载一次命名空间快照，点查与枚举共用本次快照；下次回调重新读取。返回的对象与数组深冻结，深层修改不会自动保存，必须整值赋回。
- `activate`/`deactivate` 没有会话，不能访问 state。缓存的旧 state/ctx 句柄只允许在所属 session 的回调中使用，跨 session 使用会报 `E_PLUGIN_STATE`，不会偷偷改写另一个会话。
- state 独立于聊天历史：同一 session 的清空、压缩、恢复不会清除；删除 session 才随 sidecar 删除。回退会创建新 session ID，新会话、分叉及子代理默认没有原会话的插件 state。
- 各插件的数据分文件隔离；单个 state 文件损坏不会使聊天 transcript 无法加载，也不会影响其它插件。卸载插件保留数据，重新安装同名插件可继续使用。

## 执行模型与边界

- **每个插件一个专用线程**，线程里建独立的 QuickJS `Runtime`。插件之间不共享全局状态，一个插件的死循环不会卡住另一个（各自有 `call_timeout_ms`）。
- 同一个插件的调用**串行**：工具、命令、钩子、`prompt` 排队执行。插件里不需要自己加锁。
- **加载超时**：`load_timeout_ms` 覆盖模块求值与 `activate`。到点仍未完成时中断 JS 并返回 `E_PLUGIN_TIMEOUT`，该插件按加载失败列出，不会拖住启动它的提示。
- **调用超时**：JS 自身跑到 `call_timeout_ms` 时宿主置中断标志，QuickJS 在 JS 执行期间中断，返回 `E_PLUGIN_TIMEOUT`；该插件在**本进程内**被禁用，`/plugins` 与 `plugin/list` 会显示原因，`/plugins reload` 或重启 moc 恢复。这样一次卡死不会让后续每个调用都再等一轮超时。
- **借宿主工具时超时**：插件线程在等 `tools.call` 的宿主工具，同样受同一个 `call_timeout_ms` 约束（宿主工具本身慢不会被无限等待）；这条路径只让本次调用失败，不禁用插件。
- **失败与恢复**：运行期任何异常（工具、命令、钩子、`prompt`、配置校验）都会让该插件在**本进程内停用**并通知一次：模型下一轮收到一条插件提醒，前端显示停用原因与 JS 调用栈。停用后的调用返回 `E_PLUGIN_DISABLED`（带首次失败原因）。恢复方式：TUI/REPL 执行 `/plugins reload`（协议 `plugin/reload`）、改动插件文件、保存配置或重启 moc。
- **内存**：`memory_limit_bytes`（默认 64 MiB）是单个 Runtime 的上限，超限抛 `E_PLUGIN_MEMORY`。
- **模块解析**：只允许 `moc` 这个虚拟模块与插件目录内的相对路径（`./x.js` / `../y.mjs`）。裸模块名与 `node:` 内置模块一律拒绝；没有 `node_modules`，也没有文件、网络、进程、定时器 API——需要这些能力就通过 `tools.call` 借宿主的工具。
- **崩溃**：QuickJS 是嵌入式的，C 层崩溃会带走整个 moc 进程。这是嵌入执行换来的代价：插件不是沙盒，是「你信任的本地代码」。
- 同步 API：插件里的 `tools.call` 直接返回。`run`、命令、钩子、`prompt`、`activate` 可以返回 Promise，宿主会驱动 QuickJS 的任务队列等到落定；没有定时器与事件循环，所以永远不落定的 Promise 会报 `E_PLUGIN_STATE`，不会静默变成 `{}`。

## 配置

```toml
[plugins]
enabled = true
user = true          # 扫描 $MOC_HOME/plugins
project = true       # 扫描 <cwd>/.moc/plugins（开启即信任仓库）
dirs = []            # 额外目录，支持 ~；相对路径按工作目录解析
disable = []         # 按插件名禁用
max_plugins = 32
memory_limit_bytes = 67108864
load_timeout_ms = 5000    # 模块求值 + activate
call_timeout_ms = 5000    # 单次工具/命令/钩子/prompt
max_prompt_chars = 4000
```

改动何时生效：插件目录里的发现结果、`plugin.json` 与 `.js`/`.mjs` 内容（含子目录里的模块）变化，会在下一次提示或 `plugin/list` 时重建宿主；保存配置同样重建（`disable`、超时、内存上限）。被停用的插件可用 `/plugins reload` 立即恢复，不必重启 moc。

## 查看与调试

- TUI：`/plugins` 列出插件、来源目录、注册的工具与命令、失败原因；插件命令也在补全表里。
- TUI / REPL：`/plugins reload` 重新发现并加载插件。插件停用时转写里会留一条提示（含 JS 栈），TUI 状态栏同时给出告警。
- Web：插件停用会在状态条上提示原因，并出现「重载插件」按钮。
- `/context` 里的系统提示片段会多出 `plugins` 一块，内容是本次实际注入的插件提示词。
- REPL：`/plugins` 同上。
- 协议：`plugin/list`、`plugin/reload`、`plugin/run_command`（见 `crates/moc-protocol/src/methods/plugin.rs`）。
- 常见错误码：

| 码 | 含义 |
|---|---|
| `E_PLUGIN_LOAD` | 读不到入口文件 |
| `E_PLUGIN_IMPORT` | 非法的模块说明符或逃出插件目录 |
| `E_PLUGIN_DESCRIPTOR` | `export default` 形状不对（未知字段、缺 `run`、名字非法等） |
| `E_PLUGIN_CONFIG` | 配置声明、未知键、类型、默认值或写入失败 |
| `E_PLUGIN_VALUE` | 持久化值不能无损序列化为 JSON |
| `E_PLUGIN_THREAD` / `E_PLUGIN_RUNTIME` | 线程或 Runtime 起不来 |
| `E_PLUGIN_MEMORY` | 超过 `memory_limit_bytes` |
| `E_PLUGIN_TIMEOUT` / `E_PLUGIN_INTERRUPTED` | 超时或被打断 |
| `E_PLUGIN_DISABLED` | 插件已在本进程内停用；`/plugins reload` 或重启后恢复 |
| `E_PLUGIN_STATE` | 没有会话上下文却调用会话 API，或 Promise 永远不会落定 |
| `E_PLUGIN_ARGUMENTS` | 工具参数不符合 `parameters` 里的 schema |
| `E_PLUGIN_TOOLS_UNAVAILABLE` | 在不允许的地方调 `tools.call`，或调用了白名单外的工具 |
| `E_PLUGIN_DENIED` | `beforeToolCall` 拒绝了本次调用 |
| `E_PLUGIN_RECURSION` | 试图嵌套调用插件工具 |

## 示例

`examples/plugins/hello` 是完整可用的最小插件，把它整个目录拷到 `<仓库>/.moc/plugins/hello` 即可：

```bash
mkdir -p .moc/plugins
cp -r examples/plugins/hello .moc/plugins/
```

之后模型会看到 `hello` 工具，`/hello` 可以直接执行，`/plugins` 里能看到它。

编辑器里想要类型提示，在插件目录放一份 `jsconfig.json` 并在 `index.js` 顶部写 `/** @type {import("./moc").Plugin} */`，再把 `crates/moc-plugin/js/moc.d.ts` 拷成插件目录下的 `moc.d.ts` 即可（示例里已经这么做）。

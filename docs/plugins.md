# 插件系统

moc 的插件是放在本地目录中的 **JavaScript（ES 模块）**，在进程内由嵌入式 QuickJS 运行。无需安装命令或远程注册表，把目录放进约定路径就会自动发现并生效。

插件可以扩展五类能力：

- **工具**：向模型提供自定义工具，与内置工具地位相同；
- **斜杠命令**：注册 `/命令名`，供用户在终端、Web 或行式界面调用；
- **生命周期钩子**：拦截工具调用、消息入站、权限询问或观察会话事件；
- **提示词片段**：按轮注入 `<plugins>` 系统提示片段；
- **固定子代理角色**：注册可由主代理派生的专用只读/受限子代理角色。

## 发现规则与安全提示

目录结构：

```text
<root>/<name>/plugin.json     # 插件清单
<root>/<name>/index.js        # 入口模块（可在清单中用 main 指定其他文件）
```

按以下优先级顺序扫描，**同名插件先到先得**：

| 优先级 | 来源目录 | 说明 |
|---|---|---|
| 1（最高） | `<cwd>/.moc/plugins`（逐级向上至 Git 根目录） | 离当前工作目录越近优先级越高 |
| 2 | 配置文件 `[plugins].dirs` 指定的目录 | 支持 `~` 展开 |
| 3 | `$MOC_HOME/plugins` | 跨项目全局可用 |

以 `.` 开头的目录会被跳过，符号链接先规范化再去重。

> **安全提醒**：发现到的插件会自动加载执行。启用项目级插件意味着运行该仓库内的代码；克隆不可信仓库前请先确认，或在配置中关闭：
> ```toml
> [plugins]
> project = false      # 不扫描项目目录
> disable = ["noisy"]  # 按名称禁用特定插件
> ```

## 清单文件 `plugin.json`

```json
{
  "name": "hello",
  "version": "0.1.0",
  "description": "示例插件",
  "main": "index.js"
}
```

- `name`：必填，1–64 位，以小写字母开头，仅含小写字母、数字和连字符（显示在 `/plugins` 中）。
- `main`：可选，默认 `index.js`，必须位于插件目录内且后缀为 `.js` / `.mjs`。
- 不接受未知字段，写错会直接报加载错误。

## 入口模块示例

入口文件需 `export default` 一个描述对象：

```js
import { log, session, tools } from "moc";

export default {
  name: "hello",
  version: "0.1.0",

  // 模块加载后调用一次（支持 async）；此时无会话上下文，不可调用 session() 或 tools.call()
  async activate() {
    log.info("hello 已启用");
  },
  deactivate() {},

  tools: {
    hello: {
      description: "Greet a user by name.",
      parameters: {
        type: "object",
        properties: { name: { type: "string", description: "Display name." } },
        required: ["name"],
        additionalProperties: false,
      },
      modes: ["default", "ask", "plan"], // 可选，默认三者全开
      parallel: false,                   // 可选，默认 false（非只读/串行）
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
      if (event.name === "bash") return { deny: "本仓库禁止执行 Shell 命令" };
    },
    afterToolCall(event, ctx) {
      log.debug("工具完成", event.name, event.isError);
    },
  },

  prompt(ctx) {
    return "本仓库的提交信息使用中文。";
  },
};
```

> 参考 `examples/plugins/hello`，复制到 `.moc/plugins/hello` 即可试用。把 `crates/moc-plugin/js/moc.d.ts` 复制到插件目录并配合 `jsconfig.json`，可在编辑器获得完整类型补全。

## 各类扩展能力

### 1. 工具（`tools`）

| 字段 | 说明 |
|---|---|
| `description` | 必填非空，直接作为工具说明发给模型（建议用简洁准确的英语）。 |
| `parameters` | 必填，JSON Schema（`type: "object"`），调用前自动按 `type`/`required`/`properties`/`additionalProperties`/`items`/`enum` 校验。 |
| `modes` | 可选，`default` / `ask` / `plan` 的子集，默认全开；决定哪些执行模式下允许调用。 |
| `parallel` | 默认 `false`；设为 `true` 时允许与其他只读工具并行执行。 |
| `run(input, ctx)` | 必填，接收校验后的入参和会话快照。 |

规则要点：
- 工具名全局唯一（仅允许字母、数字、下划线、连字符）；与内置工具或 MCP 工具撞名时内置优先并告警。
- 在代码执行引擎（PTC）中，插件工具可通过 `tools.<name>(args)` 或 `tools["name"](args)` 调用（连字符或数字开头的名称用方括号；需 `await` 调用结果而非 `tools` 对象本身）。
- 返回值映射：`string` 直接作为正文；`{ content, structuredContent?, isError? }` 作为富结果；其他 JSON 值序列化为文本；`null`/`undefined` 为空正文。
- **区分业务失败与插件故障**：业务报错请返回 `{ content: "...", isError: true }`；直接 `throw` 异常会被判定为插件故障，导致该插件在当前进程中被自动停用。

### 2. 斜杠命令（`commands`）

命令名规则与插件名一致，注册后自动进入前端补全表（与内置命令同名时内置优先）。`run(args, ctx)` 的 `args` 是命令后的原始字符串；返回值按与工具相同的规则显示在对话区。参数错误请直接返回提示文本，不要 `throw`。

### 3. 生命周期钩子（`hooks`）

钩子按插件加载顺序依次执行，后一个插件能看到前一个的改写结果。模型直发与经 `execute` 发起的工具调用都会触发 `beforeToolCall` / `afterToolCall`（插件内部调用 `tools.call` 不重入钩子）。

**可改写行为的钩子**（字段类型不对时忽略并告警，不因此停用插件）：

| 钩子 | 可返回字段 | 生效时机 |
|---|---|---|
| `beforeToolCall` | `{ deny?, arguments? }` | 沙盒判定、危险命令检查与授权弹窗之前 |
| `afterToolCall` | `{ content?, structuredContent? }` | 结果归档、落盘与回传模型之前 |
| `messageReceived` | `{ text? }` | 用户输入、插队消息或代理消息写入历史之前 |
| `sessionStart` | `{ messages?: [{ role?: "system"\|"user", text }] }` | 新会话创建后、首条用户消息之前写入历史 |
| `permissionRequested` | `{ option? }` | 询问用户之前；仅限选取本次提供的一次性选项（`allow_once` / `reject_once`） |
| `agentStarted` | `{ task? }` | 子代理首轮提示之前 |

**只读观察钩子**（返回值忽略）：

| 钩子 | 时机 | 主要载荷字段 |
|---|---|---|
| `beforePrompt` / `afterPrompt` | 提示发送前 / 结束后 | `promptId`, `model`, `mode`, `text`, `finishReason`, `usage`, `durationMs`, `error` |
| `sessionEnd` | 会话删除或卸载前 | `sessionId`, `reason` (`deleted` \| `unloaded`) |
| `sessionConfigChanged` | 引擎/自主程度/计划态/模型/权限变更后 | `field`, `from`, `to` |
| `beforeModelCall` / `afterModelCall` | 每次模型请求前后 | `promptId`, `attempt`, `provider`, `model`, `effort`, `usage`, `firstTokenMs`, `mismatch`, `error` |
| `beforeCompaction` / `afterCompaction` | 上下文压缩前后 | `reason`, `method`, `tokensBefore`, `tokensAfter`, `ok`, `error` |
| `permissionResolved` / `planDecided` | 权限答复后 / 计划审批后 | `requestId`, `option` / `planId`, `action` |
| `agentFinished` / `jobFinished` / `userCommand` | 子代理结束 / 后台任务结束 / 手动 `!` 命令结束 | 对应 ID、状态、耗时与结果摘要 |

### 4. 提示词片段（`prompt`）

`prompt(ctx)` 返回字符串或字符串数组，按插件名排序拼入系统提示的 `<plugins>` 段，总长度受 `[plugins].max_prompt_chars` 限制。由于每轮都会重建，请勿在其中放时间戳、随机数等每轮变化的内容，以免破坏上游提示缓存。

### 5. 固定子代理角色（`agents.registerPreset`）

可在模块加载阶段注册固定子代理角色（运行期回调不可注册；`explorer` 与 `sidekick` 为保留名）：

```js
import { agents } from "moc";

agents.registerPreset({
  name: "docs-reader",
  description: "只读检索项目文档并返回文件位置，不负责修改",
  model: "inherit", // 可指定模型，或设 "none" 禁用
  systemPrompt: "Read-only research; cite exact files and line numbers; refuse edits.",
  tools: { allow: ["read", "grep", "find"] }, // allow 与 deny 互斥；自动保留 yield
});

export default {};
```

角色定义在创建子代理时冻结，工具策略只能收窄而不能超出当前会话权限。

## `moc` 内置模块 API

| 导出 | 说明 |
|---|---|
| `version` | 宿主 API 版本（当前 `"2"`） |
| `session()` | 当前会话快照 `{ sessionId, cwd, mode, permission, model, isSubagent }`；在 `activate`/`deactivate` 调用报 `E_PLUGIN_STATE` |
| `log.debug/info/warn/error(...)` | 写入 moc 日志（`RUST_LOG=moc_plugin=debug` 可见） |
| `tools.call(name, args)` | 仅可在工具的 `run` 中调用，白名单限于只读内置工具 `read`、`grep`、`find`、`web_fetch` |
| `config.register/get/set` | 声明、读取与写回本插件的持久化配置 |
| `session.state` / `ctx.state` | 按「会话 ID + 插件名」隔离的同步持久化键值存储 |
| `agents.registerPreset` | 模块加载阶段注册固定子代理角色 |

### 插件持久化配置（`config`）

在模块顶层调用 `config.register(key, def)` 注册字段（模块加载完成后冻结），配置自动归类到 `[plugins."<name>".config]`，并在 TUI `/config` 的「插件」分类下自动生成设置项。

- 支持类型：`bool`、`integer`、`number`、`text`、`secret`（仅界面遮罩，磁盘仍明文）、`multiline`、`strings`、`object`、`choice`（配非空去重 `choices`）。
- `config.set(key, value)` 写回插件安装来源对应的配置文件（项目插件写该项目 `.moc/config.toml`，用户插件写 `$MOC_HOME/config.toml`）；传 `null` 删除当前层覆盖以恢复继承或默认值。

### 会话持久化状态（`session.state`）

- 赋值即同步落盘，写入失败不破坏旧数据；支持 `delete`、`in` 与 `Object.keys()`。
- 读取的对象与数组均为深冻结快照，修改嵌套字段后需整值重新赋回。
- 值必须能无损序列化为 JSON（拒绝 `undefined`、函数、`Symbol`、`BigInt`、`NaN`/`Infinity`、稀疏数组、循环引用与 `Date` 等自定义原型对象）。
- 数据与对话历史分文件存放：同一会话的原地回退、清空、压缩和恢复都会保留插件 `state`；删除会话时随会话附属目录一并清理；新建会话和子代理拥有独立的 `state`，不继承原会话。

## 运行隔离、配置与排错

- **独立线程与超时**：每个插件独占一个线程与 QuickJS 运行时，自身调用串行执行，互不阻塞。加载超时（`load_timeout_ms`）或调用超时（`call_timeout_ms`）会中断脚本并将出错插件在本进程内停用；内存超限（`memory_limit_bytes`，默认 64 MiB）同样报错隔离。
- **模块限制**：仅允许导入 `moc` 与插件目录内相对路径模块，不提供 `node_modules`、网络、文件或定时器 API（需要只读文件/网页能力请走 `tools.call`）。
- **热重载与恢复**：修改插件文件或保存配置后，在下一次提示或查询插件列表时自动重载；插件运行出错被停用后，执行 `/plugins reload`（Web 点击状态条「重载插件」）即可恢复，无需重启 moc。

```toml
[plugins]
enabled = true
user = true                   # 扫描 $MOC_HOME/plugins
project = true                # 扫描 <cwd>/.moc/plugins
dirs = []                     # 额外插件目录
disable = []                  # 禁用的插件名
max_plugins = 32
memory_limit_bytes = 67108864 # 单个插件内存上限（64 MiB）
load_timeout_ms = 5000        # 加载与 activate 超时
call_timeout_ms = 5000        # 单次调用超时
max_prompt_chars = 4000       # 插件提示词总长度上限
```

常见错误码：

| 错误码 | 含义 |
|---|---|
| `E_PLUGIN_LOAD` / `E_PLUGIN_IMPORT` | 入口读取失败，或模块路径非法/越出插件目录 |
| `E_PLUGIN_DESCRIPTOR` | `export default` 结构非法（未知字段、缺 `run`、名称不合规等） |
| `E_PLUGIN_CONFIG` / `E_PLUGIN_VALUE` | 配置校验或写盘失败 / `state` 值无法无损序列化为 JSON |
| `E_PLUGIN_MEMORY` / `E_PLUGIN_TIMEOUT` / `E_PLUGIN_INTERRUPTED` | 内存超限、超时或被取消 |
| `E_PLUGIN_DISABLED` | 插件因先前故障已在本进程停用（执行 `/plugins reload` 恢复） |
| `E_PLUGIN_STATE` | 无会话上下文却访问会话 API，或返回永不落定的 Promise |
| `E_PLUGIN_ARGUMENTS` | 模型传入的工具参数不符合 `parameters` Schema |
| `E_PLUGIN_TOOLS_UNAVAILABLE` | 在禁止位置或超出只读白名单调用 `tools.call` |
| `E_PLUGIN_DENIED` | 工具调用被 `beforeToolCall` 钩子拒绝 |

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

- `name`：必填，1–64 位小写字母/数字/连字符，且以字母开头。它就是 KV 的命名空间与 `/plugins` 里的名字。
- `main`：可选，默认 `index.js`；必须是插件目录内的 `.js`/`.mjs`，不能逃出目录。
- 未知字段直接报错，不做猜测。

## 入口模块

ES 模块，`export default` 一个对象：

```js
import { kv, log, session, tools } from "moc";

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
      tier: "read",       // read（默认）| write | exec
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
| `parameters` | 必填，JSON Schema 对象（`type: "object"`）。原样进模型提示。 |
| `tier` | `read`（默认）/`write`/`exec`。决定只读模式（问答、计划）是否可见。 |
| `parallel` | 默认 `false`。为 `true` 时允许与其它只读工具并发，否则整批作为屏障。 |
| `run(input, ctx)` | 必填。第一个参数是按 schema 校验过的对象，第二个是会话快照。 |

工具名全局唯一：与内置工具、MCP 工具（`mcp__` 前缀）、其它插件工具撞名会让该工具被忽略并告警。工具名只允许字母、数字、下划线、连字符。

返回值按下面的规则映射成工具结果：

- `string` → 正文
- `{ content, structuredContent?, isError? }` → 富结果（结构化内容交给宿主呈现）
- 其它 JSON → `JSON.stringify` 后作为正文
- `undefined` / `null` → 空正文

### 斜杠命令

命令名与插件名同规则，最终就是 `/名字`。`run(args, ctx)` 的 `args` 是命令名之后的原始文本（未分词）。返回值同样按上面的规则映射，作为一条提示写进转写。

命令名会进入前端补全表；与内置命令同名时内置优先。

### 钩子

只有**模型发起的**工具调用会触发钩子：`run_code` 里的子调用、插件内部的工具调用都不重入。

`beforeToolCall(event, ctx)`：

- `event = { name, arguments }`
- 返回 `{ deny: "原因" }` → 本次调用被拒绝，模型收到 `E_PLUGIN_DENIED`（含插件名与原因）
- 返回 `{ arguments: {...} }` → 用新参数替换，**改写发生在沙盒判定、bash 规则与授权之前**
- 返回 `undefined`/`null` → 不改
- 抛异常或超时 → fail-open，只记日志，调用继续

多个插件按加载顺序串联，后一个看到的 `arguments` 是前一个改写后的结果；任一插件拒绝即中止。

`afterToolCall(event, ctx)`：观察型。`event = { name, arguments, content, isError }`，返回值被忽略，失败只记日志。

### 提示词

`prompt(ctx)` 返回字符串（或字符串数组）时，内容放进系统提示的 `<plugins>` 片段，按插件名排序拼接，总长度受 `[plugins] max_prompt_chars` 限制（超出按字符截断）。返回空串等于没有贡献。

这条片段每轮都会重建，**不属于稳定前缀**，不要在这里写每次都变的内容（时间戳、随机数），否则会破坏上游缓存。

## `moc` 模块

| 导出 | 说明 |
|---|---|
| `version` | 宿主 API 版本（当前 `"1"`）。 |
| `session()` | 当前会话快照 `{ sessionId, cwd, mode, permission, model, isSubagent }`。在 `activate`/`deactivate` 里调用会抛 `E_PLUGIN_STATE`（那时没有会话）。 |
| `kv.get(name)` / `set` / `delete` / `keys()` / `clear()` | 插件级持久化，值必须是 JSON 可序列化。存盘在 `$MOC_HOME/plugin-data/<插件名>/kv.json`，**按插件名共享，跨项目**，总字节数受 `[plugins] max_kv_bytes` 限制。 |
| `log.debug/info/warn/error(...)` | 写 moc 日志，`RUST_LOG=moc_plugin=debug` 可见。 |
| `tools.call(name, args)` | 调用宿主**只读内置工具**，返回 `{ content, structuredContent, isError }`。 |

`tools.call` 的边界：

- 只在工具的 `run` 里可用；命令、钩子、`prompt`、`activate` 里调用会抛 `E_PLUGIN_TOOLS_UNAVAILABLE`
- 只能调 `tier = read` 的内置工具（如 `read`、`grep`、`find`）；写、shell、`run_code`、MCP 工具与插件工具一律拒绝
- 不能调自身或其它插件工具，也不能从 `run_code` 里调用插件工具（`E_PLUGIN_RECURSION`）

## 执行模型与边界

- **每个插件一个专用线程**，线程里建独立的 QuickJS `Runtime`。插件之间不共享全局状态，一个插件的死循环不会卡住另一个（各自有 `call_timeout_ms`）。
- 同一个插件的调用**串行**：工具、命令、钩子、`prompt` 排队执行。插件里不需要自己加锁。
- **超时/取消**：到 `call_timeout_ms` 时宿主置中断标志，QuickJS 在 JS 执行期间中断，返回 `E_PLUGIN_TIMEOUT`；该插件在**本进程内**被禁用，`/plugins` 与 `plugin/list` 会显示原因，重启 moc 恢复。这样一次卡死不会让后续每个调用都再等一轮超时。
- **内存**：`memory_limit_bytes`（默认 64 MiB）是单个 Runtime 的上限，超限抛 `E_PLUGIN_MEMORY`。
- **模块解析**：只允许 `moc` 这个虚拟模块与插件目录内的相对路径（`./x.js` / `../y.mjs`）。裸模块名与 `node:` 内置模块一律拒绝；没有 `node_modules`，也没有文件、网络、进程、定时器 API——需要这些能力就通过 `tools.call` 借宿主的工具。
- **崩溃**：QuickJS 是嵌入式的，C 层崩溃会带走整个 moc 进程。这是嵌入执行换来的代价：插件不是沙盒，是「你信任的本地代码」。
- 同步 API：插件里的一切都是同步的（`kv`、`tools.call` 都直接返回）。`run`/`prompt`/钩子可以返回 Promise，宿主会等待其落定。

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
max_kv_bytes = 1048576
max_prompt_chars = 4000
```

## 查看与调试

- TUI：`/plugins` 列出插件、来源目录、注册的工具与命令、失败原因；插件命令也在补全表里。
- `/context` 里的系统提示片段会多出 `plugins` 一块，内容是本次实际注入的插件提示词。
- REPL：`/plugins` 同上。
- 协议：`plugin/list`、`plugin/run_command`（见 `crates/moc-protocol/src/methods/plugin.rs`）。
- 常见错误码：

| 码 | 含义 |
|---|---|
| `E_PLUGIN_LOAD` | 读不到入口文件 |
| `E_PLUGIN_IMPORT` | 非法的模块说明符或逃出插件目录 |
| `E_PLUGIN_DESCRIPTOR` | `export default` 形状不对（未知字段、缺 `run`、名字非法等） |
| `E_PLUGIN_THREAD` / `E_PLUGIN_RUNTIME` | 线程或 Runtime 起不来 |
| `E_PLUGIN_MEMORY` | 超过 `memory_limit_bytes` |
| `E_PLUGIN_TIMEOUT` / `E_PLUGIN_INTERRUPTED` | 超时或被打断 |
| `E_PLUGIN_TOOLS_UNAVAILABLE` | 在不允许的地方调 `tools.call`，或调用了非只读工具 |
| `E_PLUGIN_DENIED` | `beforeToolCall` 拒绝了本次调用 |
| `E_PLUGIN_RECURSION` | 试图嵌套调用插件工具 |
| `E_PLUGIN_KV_KEY` / `E_PLUGIN_KV_LIMIT` | KV 键非法或超出配额 |

## 示例

`examples/plugins/hello` 是完整可用的最小插件，把它整个目录拷到 `<仓库>/.moc/plugins/hello` 即可：

```bash
mkdir -p .moc/plugins
cp -r examples/plugins/hello .moc/plugins/
```

之后模型会看到 `hello` 工具，`/hello` 可以直接执行，`/plugins` 里能看到它。

编辑器里想要类型提示，在插件目录放一份 `jsconfig.json` 并在 `index.js` 顶部写 `/** @type {import("./moc").Plugin} */`，再把 `crates/moc-plugin/js/moc.d.ts` 拷成插件目录下的 `moc.d.ts` 即可（示例里已经这么做）。

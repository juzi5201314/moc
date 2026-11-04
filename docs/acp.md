# ACP 前端使用说明

`moc acp` 把 moc 以 ACP（Agent Client Protocol）agent 的身份跑在 stdio 上（NDJSON，
一行一条 JSON-RPC），供 Zed、Neovim 等编辑器内置 agent 面板直接接入——与
`codex acp`、`gemini --acp` 同一角色。

```bash
moc acp                       # 服务 stdio 直到客户端断开（stdin EOF）
moc acp --model <selector>    # 新会话的默认模型
moc acp --engine ptc          # 新会话的引擎
moc acp --plan                # 新会话直接进入 plan 模式
moc acp --cwd ~/project       # 会话默认工作目录（编辑器一般每次请求都带 cwd）
moc acp --config extra.toml   # 追加配置覆盖层（与 TUI 共用同一套配置）
```

stdout 只走 ACP 报文；日志写 stderr，`RUST_LOG=debug moc acp` 即可调试而不污染协议。

## 编辑器接入（Zed 示例）

`settings.json`：

```json
{
  "agent_servers": {
    "moc": {
      "command": "/path/to/moc",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

之后在 agent 面板的 「+ / New Thread」 里选择 moc。

## 与 moc 的关系

`moc acp` 内部照旧起一个进程内 core + 协议客户端——它和 TUI、web 是平级的第三种
前端，不是第二个内核。因此：

- **会话互通**：ACP 会话 ID 就是 moc 的 `ses_*`。编辑器里建的会话可以用
  `moc --resume <id>`、TUI、web 打开继续聊，反过来也成立。
- **持久化、权限、工具、压缩、计划全部复用**：编辑器看到的 diff 确认走
  `session/request_permission`，`ask` 工具走 ACP elicitation 表单。
- **写租约照旧生效**：同一会话被 TUI 和编辑器同时打开时，只有一个写者。

## 协议覆盖

| ACP | moc 侧行为 |
|---|---|
| `initialize` | 声明能力：`loadSession`、`sessionCapabilities`(list/resume/close/delete)、`promptCapabilities`(image+embeddedContext) |
| `session/new` | `session/new`；响应附带 modes 与模型 `configOptions`（来自 `config/get`） |
| `session/load` | `session/load`；响应后按序重放 transcript 为 `user_message_chunk`/`agent_message_chunk`/`agent_thought_chunk`/`tool_call*` |
| `session/resume` | `session/load`（`max_entries=0`），不重放历史 |
| `session/list` | `session/list`，cwd 过滤 + 游标分页 |
| `session/close` / `session/delete` | `session/unload` / `session/delete` |
| `session/prompt` | `session/prompt`；文本与图片输入转成 moc `InputPart` |
| `session/cancel` | `session/cancel`——prompt 响应挂在独立任务里，dispatch 循环随时能收 cancel |
| `session/set_mode` | `default`/`dont_ask`/`unattended` → `session/set_autonomy`；`plan` → `session/plan_mode` |
| `session/set_config_option` | `model` 一项 → `session/set_model`（溢出动作直接 Switch） |
| `session/request_permission`（agent→client） | moc 权限询问，选项映射 allow_once/allow_always/reject_once |
| `elicit`（agent→client） | moc `ask` 工具；客户端无 form elicitation 能力时返回 MethodNotFound 由 core 降级 |
| `session/update` | `AssistantDelta`→文本/思维 chunk，`ToolStarted/Finished/Progress`→`tool_call*`，diff→`diff` 内容块，todo/plan→`plan`，用量→`usage` |

## 明确不支持

- **客户端下挂 MCP**：`mcpCapabilities.http/sse=false`，`session/new` 里附带的
  `mcpServers` 会被忽略（记一条警告日志）。要扩工具请用 moc 自己的 MCP/插件配置。
- **`authenticate`**：`authMethods` 为空——供应商密钥由 moc 自己的 `/config` 管。
- **`fs/*` 与 `terminal/*`**：moc 的 read/edit/bash 全部本地执行，不委托客户端，
  客户端是否声明这些能力不影响功能。
- ACP 语义下无意义的 CLI 参数会被拒绝：`PROMPT`、`--resume`、`--continue`、
  `--list`、`--models`、`--print`、`--repl`、`--detail`、`--hide-thinking` 以及
  web 专用选项。

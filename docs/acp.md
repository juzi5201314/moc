# 编辑器接入（ACP）使用说明

`moc acp` 通过标准输入输出（stdio / NDJSON）提供 ACP（Agent Client Protocol）代理服务，可直接接入 Zed、Neovim 等支持 ACP 的编辑器代理面板。

## 启动参数

```bash
moc acp                       # 启动 ACP 服务，直到客户端断开
moc acp --model <selector>    # 指定新会话的默认模型
moc acp --engine ptc          # 指定新会话的默认引擎
moc acp --plan                # 新会话默认进入计划模式
moc acp --cwd ~/project       # 指定默认工作目录（编辑器通常会在每次请求中附带目录）
moc acp --config extra.toml   # 追加配置文件覆盖层
```

`stdout` 专用于传输 ACP 协议报文，日志统一写入 `stderr`；需要排查问题时直接运行 `RUST_LOG=debug moc acp`，不会干扰协议通信。

## 在 Zed 中接入

在 Zed 的 `settings.json` 中添加：

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

保存后，在 Zed 代理面板点击「+ / New Thread」并选择 `moc` 即可使用。

## 与终端 / 浏览器端互通

`moc acp` 与 TUI、Web 使用同一套会话存储、配置、权限沙盒、工具与压缩机制：

- **会话完全互通**：编辑器中创建的会话（`ses_*`）可以直接在终端用 `moc --resume <id>` 或在浏览器中继续对话，反之亦然。
- **权限与提问复用**：编辑器内的改动确认与模型提问分别映射到 ACP 的权限确认与表单交互。
- **编辑占用一致**：同一会话同时在编辑器和 TUI/Web 中打开时，同一时刻只有一个窗口可写。

## 协议能力一览

| ACP 方法 | moc 对应行为 |
|---|---|
| `initialize` | 声明会话加载、列举、恢复、关闭、删除以及图片与内嵌上下文能力 |
| `session/new` | 创建新会话，并返回可用模式及模型配置项 |
| `session/load` / `session/resume` | 加载会话（`load` 按序重放完整历史，`resume` 不重放历史） |
| `session/list` / `close` / `delete` | 按工作目录分页列举、卸载或删除会话 |
| `session/prompt` / `session/cancel` | 提交文本与图片提示，或随时取消运行中的提示 |
| `session/set_mode` | 切换自主程度（`default` / `dont_ask` / `unattended`）或计划模式（`plan`） |
| `session/set_config_option` | 切换会话模型 |
| `session/request_permission` | 向编辑器请求工具执行或越界路径权限（允许一次 / 始终允许 / 拒绝） |
| `elicit` | 向编辑器弹出模型提问表单（客户端不支持时自动降级） |
| `session/update` | 推送流式正文、思考片段、工具状态、unified diff、待办/计划与用量统计 |

## 当前不支持项

- **编辑器透传的 MCP 服务器**：忽略 `session/new` 中附带的 `mcpServers`；扩展 MCP 请在 moc 自身配置中设置。
- **`authenticate`**：供应商与 API 密钥统一由 moc 的 `/config` 或配置文件管理。
- **委托客户端的 `fs/*` 与 `terminal/*`**：moc 的文件读写与命令执行均在本地完成，不依赖客户端提供这些能力。
- **非 ACP 专用 CLI 参数**：`PROMPT`、`--resume`、`--continue`、`--list`、`--models`、`--print`、`--repl`、`--detail`、`--hide-thinking` 以及 Web 专用参数在 `moc acp` 下会被直接拒绝。

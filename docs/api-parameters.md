# API 请求参数配置

在 TUI 和 Web 的「供应商与模型」设置页中，你可以按供应商所用协议直接配置模型请求参数：供应商层级作为默认值，展开具体模型后可单独覆盖。每个配置项的说明都附带原始字段名（例如 `reasoning.summary`），可直接按字段名搜索。

## 支持的协议与主要参数

| API 协议 | 可配置项概览 |
|---|---|
| `openai_chat` | 温度、核采样、惩罚项、随机种子、停止序列、思考强度、回复详细度、结构化输出（JSON Schema）、工具选择与并行调用、提示缓存、流式选项、服务档位、组织/项目请求头等 |
| `openai_responses` | 采样参数、思考摘要/历史范围/模式/强度、文本详细度与结构化输出、附加返回数据、超长输入截断策略、提示模板、缓存与图片压缩、工具选择与并行调用、服务档位、组织/项目请求头等 |
| `anthropic_messages` | 采样参数、停止序列、思考模式/预算/展示方式、思考强度与结构化输出、工具选择、服务容量/速度/处理区域、自动缓存，以及版本/实验特性/工作区请求头等 |

连接超时、请求超时、重试、并发、缓存策略、回复 token 上限及协议兼容开关均在同一页面中管理，并按当前所选协议自动过滤，不展示与该协议无关的字段。切换协议不会删掉旧协议已保存的字段，切回后仍可继续编辑。

> **说明**：这里只开放 moc 实际支持的多轮文本、图片与本地工具调用参数，不提供对 `model`、`messages`/`input`、`tools`、`stream` 等会话核心字段的静态覆写；具体参数是否被上游接受取决于你的模型、账号与网关，moc 不会悄悄降级不受支持的参数。

## 存储位置与优先级

- **供应商默认值**：保存在 `providers.<供应商>.extra_body` 与 `providers.<供应商>.headers`。
- **模型级覆盖**：保存在 `providers.<供应商>.models.<模型>.compat.extra_body` 与 `compat.headers`。
- **合并规则**：对象字段逐层合并，数组字段整体替换（例如模型只覆盖 `reasoning.mode` 时，供应商设置的 `reasoning.summary` 与 `reasoning.context` 依然保留）。清空或恢复「默认/继承」仅移除当前层级的覆盖。
- **JSON 编辑器**：页面仍保留「扩展请求体（JSON）」与完整请求头编辑器，用于填写供应商私有字段或复合配置；它与独立表单字段操作同一份数据。
- **思考强度优先级**：你在会话中手动选定的思考等级优先于 API 默认强度（`reasoning_effort` / `reasoning.effort` / `output_config.effort`）；会话选择 `off` 时会自动移除强度、预算与显示字段。
- **默认体验保留**：未显式覆盖时，moc 默认开启 Responses 的 `reasoning.summary = "auto"`、Anthropic 的 `thinking.display = "summarized"` 及 Chat 流式的 `stream_options.include_usage = true`（关闭用量统计会导致 token 与费用统计缺失）。会话中的 Fast Mode 仍优先写入 `service_tier = "fast"`。

## 配置示例

```toml
[providers.example]
api = "openai_responses"
base_url = "https://your-provider.example/v1"
api_key = "env:MODEL_API_KEY"

[providers.example.extra_body.reasoning]
summary = "detailed"
context = "all_turns"
effort = "medium"

[providers.example.extra_body.text]
verbosity = "low"

[providers.example.models.my-model]
reasoning = true
max_output_tokens = 32000

[providers.example.models.my-model.compat.extra_body.reasoning]
mode = "pro"

[providers.example.models.my-model.compat.headers]
OpenAI-Project = "your-project-id"
```

按此配置，该模型请求会同时带上 `reasoning.summary`、`reasoning.context`、`reasoning.mode` 并默认使用 `medium` 思考强度；若你在会话中手动切换为 `example/my-model:high`，则以 `high` 为准。

停止序列与联合类型工具选择建议通过 JSON 编辑器填写（如 `stop = ["END,", "\n结束"]`），避免被误拆。

## 官方参数参考

- [OpenAI Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
- [OpenAI Responses](https://developers.openai.com/api/reference/resources/responses/methods/create)
- [Anthropic Messages](https://platform.claude.com/docs/en/api/messages/create)

> 开发者注：参数目录与协议可见范围分别维护在 `crates/moc-core/src/config/parameters.rs` 与 `crates/moc-core/src/config/catalog.rs`，TUI 与 Web 共用。

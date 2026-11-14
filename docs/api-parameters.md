# API 请求参数配置

TUI 与 WebUI 的「供应商与模型」页面共用 core 的字段目录，按供应商选择的 API 格式展示请求参数。供应商参数提供默认值；展开模型后可以单独覆盖。每个字段说明都包含原始字段名，例如 `reasoning.summary`，可以按字段名搜索。

## 覆盖范围

| API 格式 | 独立配置入口 |
|---|---|
| `openai_chat` | 采样温度、核采样、重复惩罚、随机种子、token 概率、候选数量、停止序列、思考强度、回复详细程度、输出格式与 JSON Schema、预期输出、工具选择与并行调用、缓存、流式选项、元数据、安全标识、留存、服务档位、审核、组织／项目请求头 |
| `openai_responses` | 采样、思考摘要／历史范围／模式／强度、文本详细程度与结构化输出、附加返回数据、超长输入策略、提示模板、缓存及诊断、图片压缩、工具选择与并行调用、流式混淆、元数据、安全标识、留存、服务档位、审核、访问计划、组织／项目请求头 |
| `anthropic_messages` | 采样、停止序列、思考模式／预算／返回方式、思考强度与结构化输出、工具选择、用户标识、服务容量／速度／处理区域、自动缓存与诊断、版本／实验功能／工作区／用户档案请求头 |

连接超时、请求超时、重试、并发、缓存策略、端点、模型 token 上限与协议兼容开关仍在同一页面。Chat 的 `max_tokens` / `max_completion_tokens` 通过「回复 token 上限」与「Chat 输出上限字段名」配置；Responses 的 `max_output_tokens` 和 Anthropic 的 `max_tokens` 使用相同的回复上限入口。

协议兼容项也按 API 过滤，不展示当前编码器不使用的字段：

- 仅 Chat 显示输出上限字段名、`developer` 角色、思考回传字段名／别名，以及 `compat.thinking` 的开关、强度字段名和等级映射。
- 停止序列兼容开关只在 Chat／Anthropic 显示；提示缓存键兼容开关只在 Chat／Responses 显示。
- `tool_choice` 的 `required` → `any` 改写选项仅在 Chat 显示；通用的历史回传、并行调用及结构化输出开关仍保留。

切换 API 后，参数入口与候选值随之更新；TUI 在保存回执到达前也会更新本地目录，不混入旧协议的字段。隐藏不会删除已存配置，切回原 API 后仍可编辑原值。跨 API 的 `defaults.models` 规则不按单个供应商过滤；扩展请求体与请求头 JSON 编辑器仍保留完整原始内容。

这里覆盖已实现的文本、图片与客户端函数调用流程，而不是启用上游的所有产品功能。会话生成的 `model`、`messages` / `input`、`system` / `instructions`、`tools`、`stream` 不提供静态覆盖入口；音频、服务端内置工具、后台任务、无输出的缓存预热、服务端会话／容器管理与自动压缩不在此范围内。Chat 旧版 `functions` / `function_call` 不替代 moc 使用的客户端工具协议。

具体参数是否可用仍取决于模型、账号与网关。配置页给出字段类型及常见限制，不会向上游探测功能，也不会静默降级不受支持的参数。

## 存储与优先级

- 供应商请求参数存于 `providers.<供应商>.extra_body`，请求头存于 `providers.<供应商>.headers`。
- 模型覆盖存于 `providers.<供应商>.models.<模型>.compat.extra_body` 和 `compat.headers`。
- 请求对象逐层合并，数组整体替换。例如模型只覆盖 `reasoning.mode` 时，供应商的 `reasoning.summary` 与 `reasoning.context` 仍保留。
- 清空独立参数、选择「默认／继承」或重置只删除当前范围的覆盖，不把页面默认值写入配置文件。TUI 的模型参数组、Web 的模型详情需要展开后编辑。
- 「扩展请求体（JSON）」与完整请求头编辑器保留，用于供应商自定义字段及一次性编辑复合配置；独立字段与 JSON 编辑器修改的是同一份数据。

API 的 `reasoning_effort`、`reasoning.effort` 或 `output_config.effort` 是默认思考强度；会话明确选择的等级优先。没有配置 API 默认强度时沿用可选思考等级的第一项。自定义强度需加入模型的 `thinking_levels`。Anthropic 的 `enabled` + `budget_tokens` 使用显式配置的 `output_config.effort`，不会自动追加会话等级；会话选择 `off` 时移除强度、预算与显示字段。

moc 保留未配置时的请求默认行为：Responses 请求 `reasoning.summary = "auto"`；Anthropic 请求 `thinking.display = "summarized"`；Chat 流式请求包含 `stream_options.include_usage = true`。明确配置会替代这些默认值。关闭用量统计可能使 token 和费用信息缺失。Responses 会按历史思考回传策略追加 `reasoning.encrypted_content`，以保持多轮推理连续性。

设置 `prompt_cache_key` 可覆盖默认会话缓存键。`cache = "off"` 或关闭缓存键兼容开关时不会发送对应缓存配置；关闭停止序列、工具选择、并行字段或结构化输出兼容开关时，不会让扩展请求体绕过这些开关。会话 Fast Mode 仍优先写入其既有的 `service_tier = "fast"`。

## 示例

以下名称只演示结构；模型 ID 与服务地址须替换为实际值，密钥通过环境变量提供。

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

此模型的默认请求同时保留 `reasoning.summary`、`reasoning.context`、`reasoning.mode`，使用 `medium` 思考强度；显式选择 `example/my-model:high` 后使用 `high`。`text.verbosity` 控制最终回复详细程度，与 `reasoning.summary` 的思考摘要粒度不同。

停止序列与工具选择的联合类型使用 JSON 编辑器。例如 `stop = ["END,", "\n结束"]` 不会按逗号或换行拆分；Web 的有符号整数编辑器拒绝超出 JavaScript 精确表示范围的整数，TUI 可编辑完整 `i64` 范围。

## 参数依据

- [OpenAI Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
- [OpenAI Responses](https://developers.openai.com/api/reference/resources/responses/methods/create)
- [Anthropic Messages](https://platform.claude.com/docs/en/api/messages/create)

维护请求参数时修改 `crates/moc-core/src/config/parameters.rs`，维护协议兼容项的可见范围时修改 `crates/moc-core/src/config/catalog.rs`；两套 UI 不各自维护 API 参数清单。协议字段类型变化后按项目生成流程更新 schema 与 Web 类型。

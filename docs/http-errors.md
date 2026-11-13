# 上游错误与自动重试

TUI、WebUI 和 CLI 复用 `moc-llm` 的 `ApiError` 显示，不需要各前端维护状态码表。

## 显示内容

错误依次显示 HTTP 状态码、中文含义、内部分类、上游消息、错误代码/请求 ID、Cloudflare Ray ID、Retry-After（上游提供时）、请求体诊断和针对性建议。

例如：

```text
HTTP 431 请求头过大 [Unknown]
上游消息：request header fields too large (request_id=req_example)
排查建议：检查附加请求头、Cookie 和重复鉴权字段；压缩图片请求体不能解决请求头超限。
```

`[Unknown]` 表示尚未识别具体错误类型，不表示不可恢复。没有明确永久拒绝证据时，按供应商重试预算继续尝试；431 等明确 HTTP 拒绝仍停止，显示层无需为此新增内部分类。

上游消息最多显示 512 个字符，控制字符替换为空格，HTML 错误页显示为简洁提示；`ApiError.message` 和 `raw_body` 保留接收到的原值（HTTP 响应读取仍受既有大小上限约束）。这避免页面源码挤满错误区域，同时不破坏程序排查所需原始数据。

## 覆盖范围

- 标准请求错误：400–418 中的已定义状态、421–426、428、429、431、451。
- 标准服务错误：500–508、已废弃的 510、511。
- 重定向和意外的信息/成功状态：给出接口路径、代理和响应内容的检查方向。
- Cloudflare 惯例：520–526、530；提示以“若由 Cloudflare 返回”为前提，不仅凭数值认定实际供应商。
- Anthropic 惯例：529 服务过载。
- Nginx 等非标准惯例：444、494–497、499；其中 444/499 可能只出现在代理日志或经中间层转译，不能断言客户端实际收到原生 Nginx 响应或用户主动取消。
- 419、440、527、598、599：明确标记非标准、定义可能因服务而异。
- 其他未知码：按数字所属范围说明“未识别的请求拒绝/服务端错误”等，不猜测精确原因。

如果上游已明确报告额度耗尽、上下文超限或内容策略拒绝，这些语义优先于笼统 HTTP 提示。例如 429 + `insufficient_quota` 显示检查账户额度，而不是只建议等待；400 + 上下文超限不会混同 HTTP 字节超限。

524 仍按统一超时策略处理，不区分是否含图片。请求大小、图片数量和等待时间仅作为诊断事实，不直接用于判定根因。重试达到配置上限时，仍由外层错误显示总尝试次数。

## 自动重试

原则是「明确不能原样恢复才停止」，不要求网关提供标准 HTTP 错误格式：

- `rate_limit_exceeded`、`rate_limit_error`、`token rate limit` 等归为限流；不与余额或账户额度耗尽混淆。
- 额度、余额、认证、权限、资源不存在、参数不支持、内容过滤等明确错误停止原样重试。明确的 HTTP 拒绝（如 400、401、402、403、404、413、422、431、451、501、505、511）不会被笼统限流消息覆盖。
- 408、429、过载、暂时性服务错误、传输中断、异常响应和未分类上游错误在预算内重试；409 等未明确永久失败的冲突同样允许尝试。
- 上下文超限、思考签名不匹配继续走专用恢复流程；本地请求构造失败、循环守卫与用户取消不进入通用重试。循环守卫原有的思考纠偏预算保持独立。

HTTP 200 的 JSON 错误体、首个有效内容之前的 SSE 错误、连接错误和生成中途的失败，共享一次逻辑生成的 `providers.<id>.retry.max_attempts`，不会各自重新计算次数。一个完整响应成功后，下一轮工具后续生成才开始新预算。默认配置不变：moc 最多尝试 10 次（含首次），0.5 秒起步，300 秒退避上限，开启抖动；独立使用 `moc-llm` 的默认值为 3 次和 20 秒上限。

退避采用指数增长与可配置抖动，`Retry-After` 整数秒数作为最短等待时间，流内错误也继承该响应头。既有连接／重试时限仍生效，取消会立即打断等待。HTTP 日期形式的 `Retry-After` 仍不解析。

### 流中失败

主会话丢弃失败尝试的正文、思考与未执行工具调用，不写入模型历史；保留已完成轮次和工具结果，并记录失败尝试已知的用量。不会重放已经执行的工具，但无法保证上游失败请求未计费。

`generation_retry` 通知前端撤销当前未提交预览，显示等待与尝试次数，不结束提示。TUI 和 WebUI 清除失败预览；行式 CLI 与 ACP 无法撤回已发送的文字，因此明确通知本次生成的先前输出作废，再展示重试结果。旁路问答使用同一预算规则，通过 `btw_update.waiting` 清空失败预览，不改动主会话。

直接使用 `Client::stream` 的调用方在首个有效内容之前自动获得重试；已经交付内容之后，不会由传输层悄悄重放。需要恢复的消费者应使用 `stream_with_budget`，撤销自己的失败预览，并通过同一 `RetryBudget` 计算下一次等待。

## 参考资料

- [IANA HTTP 状态码注册表](https://www.iana.org/assignments/http-status-codes/http-status-codes.xhtml)
- [Cloudflare 5xx 错误](https://developers.cloudflare.com/support/troubleshooting/http-status-codes/cloudflare-5xx-errors/)
- [Cloudflare 522](https://developers.cloudflare.com/support/troubleshooting/http-status-codes/cloudflare-5xx-errors/error-522/)
- [Cloudflare 524](https://developers.cloudflare.com/support/troubleshooting/http-status-codes/cloudflare-5xx-errors/error-524/)
- [Cloudflare 530](https://developers.cloudflare.com/support/troubleshooting/http-status-codes/cloudflare-5xx-errors/error-530/)
- [Anthropic API errors](https://platform.claude.com/docs/en/api/errors)
- [Nginx return 指令](https://nginx.org/en/docs/http/ngx_http_rewrite_module.html#return)
- [Nginx SSL 错误处理](https://nginx.org/en/docs/http/ngx_http_ssl_module.html#errors)

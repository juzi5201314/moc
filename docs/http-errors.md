# HTTP 错误显示

TUI、WebUI 和 CLI 复用 `moc-llm` 的 `ApiError` 显示，不需要各前端维护状态码表。

## 显示内容

错误依次显示 HTTP 状态码、中文含义、内部分类、上游消息、错误代码/请求 ID、Cloudflare Ray ID、Retry-After（上游提供时）、请求体诊断和针对性建议。

例如：

```text
HTTP 431 请求头过大 [Unknown]
上游消息：request header fields too large (request_id=req_example)
排查建议：检查附加请求头、Cookie 和重复鉴权字段；压缩图片请求体不能解决请求头超限。
```

`[Unknown]` 是内部分类，表示没有为这个状态定义特殊程序行为；中文说明仍解释这个 HTTP 状态。显示说明独立于分类和重试逻辑，本次扩充提示不改变哪些错误会自动重试。

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

## 参考资料

- [IANA HTTP 状态码注册表](https://www.iana.org/assignments/http-status-codes/http-status-codes.xhtml)
- [Cloudflare 5xx 错误](https://developers.cloudflare.com/support/troubleshooting/http-status-codes/cloudflare-5xx-errors/)
- [Cloudflare 522](https://developers.cloudflare.com/support/troubleshooting/http-status-codes/cloudflare-5xx-errors/error-522/)
- [Cloudflare 524](https://developers.cloudflare.com/support/troubleshooting/http-status-codes/cloudflare-5xx-errors/error-524/)
- [Cloudflare 530](https://developers.cloudflare.com/support/troubleshooting/http-status-codes/cloudflare-5xx-errors/error-530/)
- [Anthropic API errors](https://platform.claude.com/docs/en/api/errors)
- [Nginx return 指令](https://nginx.org/en/docs/http/ngx_http_rewrite_module.html#return)
- [Nginx SSL 错误处理](https://nginx.org/en/docs/http/ngx_http_ssl_module.html#errors)

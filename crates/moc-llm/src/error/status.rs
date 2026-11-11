//! 面向用户的状态码说明；独立于错误分类和自动重试策略。
//!
//! 标准码以 IANA HTTP Status Code Registry 为准；非标准码注明惯例来源，
//! 不把代理转述的状态码当成已确认的厂商或故障根因。
use super::ApiErrorKind;

pub(super) fn explanation(status: Option<u16>, kind: ApiErrorKind) -> (&'static str, &'static str) {
    // 明确的供应商错误类型比同一个 HTTP 状态的笼统描述更有用，例如 429 额度耗尽。
    match kind {
        ApiErrorKind::QuotaExceeded => {
            return (
                "额度耗尽",
                "检查供应商账户余额、项目额度和消费上限；单纯等待或重试通常不能恢复额度。",
            );
        }
        ApiErrorKind::ContextLength => {
            return (
                "上下文超限",
                "输入历史或输出预留超过模型上下文限制；请压缩历史或调整输出上限。这与 HTTP 请求体字节上限不同。",
            );
        }
        ApiErrorKind::ContentFilter => {
            return (
                "内容策略拒绝",
                "供应商明确报告内容策略限制；请查看上游说明并调整请求内容。",
            );
        }
        _ => {}
    }
    let Some(status) = status else {
        return match kind {
            ApiErrorKind::Auth => ("认证失败", "检查当前供应商的凭据及其有效期。"),
            ApiErrorKind::Permission => ("访问被拒绝", "检查账户、项目和模型访问权限。"),
            ApiErrorKind::Billing => ("计费受限", "检查账户余额、订阅和供应商计费状态。"),
            ApiErrorKind::RateLimit => {
                ("请求限流", "降低并发或请求频率，并参考上游建议的等待时间。")
            }
            ApiErrorKind::InvalidRequest => {
                ("请求无效", "根据上游错误消息检查模型、参数和协议兼容配置。")
            }
            ApiErrorKind::NotFound => ("资源不存在", "核对 API 地址、模型名称及部署名称。"),
            ApiErrorKind::RequestTooLarge => (
                "请求体过大",
                "减小请求体或按网关限制设置模型 max_request_bytes。",
            ),
            ApiErrorKind::Conflict => ("请求冲突", "检查并发请求及上游资源状态。"),
            ApiErrorKind::Overloaded => (
                "服务过载",
                "上游暂时没有足够容量；稍后重试或切换可用模型/线路。",
            ),
            ApiErrorKind::ServerError => ("上游服务错误", "结合请求 ID 检查供应商状态和服务日志。"),
            ApiErrorKind::Timeout => (
                "上游超时",
                "核对请求耗时和超时设置；超时本身不能证明请求过大。",
            ),
            _ => (
                "上游未分类错误",
                "上游未提供可识别的 HTTP 状态或错误类型；请保留原始错误和请求 ID 排查。",
            ),
        };
    };
    match status {
        300..=303 | 305..=308 => (
            "接口重定向",
            "本次未取得预期 API 响应；检查 base_url、接口路径及代理重定向配置。",
        ),
        304 => (
            "收到缓存响应",
            "模型生成接口未返回预期响应体；检查网关缓存与接口路径。",
        ),
        400 => (
            "请求参数错误",
            "根据上游消息检查参数、模型及协议兼容配置；不要仅凭 400 判断是图片或上下文问题。",
        ),
        401 => (
            "认证失败",
            "检查当前供应商 API Key/登录凭据是否有效，以及代理是否正确转发鉴权头。",
        ),
        402 => (
            "付款或计费受限",
            "供应商常用此码表示余额或付费权限不足；具体以账户状态和上游消息为准。",
        ),
        403 => (
            "访问被拒绝",
            "检查模型权限、账户/地区限制和网关访问规则；403 不一定表示 API Key 错误。",
        ),
        404 => (
            "接口或模型不存在",
            "核对 base_url、API 路径、模型 ID 或部署名称；部分服务也用 404 隐藏无权访问的资源。",
        ),
        405 => (
            "请求方法不支持",
            "检查 API 路径及代理配置，确认该端点允许 POST。",
        ),
        406 => (
            "响应格式无法满足",
            "检查 Accept 请求头及接口支持的响应格式。",
        ),
        407 => (
            "代理认证失败",
            "检查 HTTP 代理凭据；这是代理层认证，未必是模型 API Key 的问题。",
        ),
        408 => (
            "接收请求超时",
            "服务端未及时收完请求；检查上传链路和网关接收时限，不要与模型生成超时混淆。",
        ),
        409 => (
            "请求状态冲突",
            "检查并发操作、资源状态或幂等键，按上游说明解决冲突。",
        ),
        410 => (
            "资源已移除",
            "核对接口或模型是否停用，更新到供应商仍支持的地址和模型。",
        ),
        411 => (
            "缺少内容长度",
            "网关要求 Content-Length；检查代理是否改写请求体或传输方式。",
        ),
        412 => (
            "请求前置条件失败",
            "检查条件请求头和上游资源版本；原样重试通常不会改变条件。",
        ),
        413 => (
            "请求体过大",
            "上游明确拒绝了请求体大小；请按网关上限设置模型 max_request_bytes，并分批处理图片或压缩历史。",
        ),
        414 => (
            "请求地址过长",
            "检查 base_url 和查询参数，确认消息内容没有被代理拼进 URL。",
        ),
        415 => (
            "媒体类型不支持",
            "检查 Content-Type、请求编码与图片格式是否符合该接口要求。",
        ),
        416 => ("请求范围无效", "检查代理或接口附加的 Range 请求头。"),
        417 => (
            "请求期望无法满足",
            "检查 Expect 请求头以及代理对上传协商的支持。",
        ),
        418 => (
            "保留状态码",
            "IANA 将 418 标为未使用；实际可能是服务自定义响应，请以上游原文为准。",
        ),
        421 => (
            "请求被发往错误服务",
            "检查域名、Host/SNI 和反向代理路由配置。",
        ),
        422 => (
            "请求语义校验失败",
            "请求格式可识别，但内容或参数未通过校验；查看上游给出的具体字段原因。",
        ),
        423 => ("资源被锁定", "检查上游资源锁及并发操作状态。"),
        424 => ("依赖操作失败", "查看上游依赖服务或前置操作的失败原因。"),
        425 => (
            "拒绝过早发送的请求",
            "服务担心请求被重放；检查 TLS early data 与代理配置。",
        ),
        426 => ("需要升级协议", "检查接口要求的 HTTP/TLS 协议及代理支持。"),
        428 => (
            "缺少请求前置条件",
            "按接口要求设置条件请求头，核对资源版本。",
        ),
        429 => (
            "请求限流",
            "降低并发、请求频率或 token 用量，并参考 Retry-After；若上游明确报告额度耗尽，应先处理账户额度。",
        ),
        431 => (
            "请求头过大",
            "检查附加请求头、Cookie 和重复鉴权字段；压缩图片请求体不能解决请求头超限。",
        ),
        451 => (
            "法律原因限制访问",
            "服务因法律要求限制资源访问；以上游说明为准。",
        ),
        500 => (
            "上游内部错误",
            "上游处理失败；可按重试策略等待恢复，持续失败时提供请求 ID 给供应商排查。",
        ),
        501 => (
            "接口能力未实现",
            "服务不支持所需功能或方法；核对协议与端点，原样重试通常不能补齐能力。",
        ),
        502 => (
            "网关收到无效上游响应",
            "检查网关到供应商的连接、响应格式及供应商状态；不能直接归因于客户端输入。",
        ),
        503 => (
            "服务暂不可用",
            "可能是维护、过载或依赖故障；参考 Retry-After 和供应商状态。",
        ),
        504 => (
            "网关等待上游超时",
            "核对网关与源站的处理耗时和超时设置；状态码本身不能证明请求过大。",
        ),
        505 => (
            "HTTP 版本不支持",
            "检查网关支持的 HTTP 版本与协议协商配置。",
        ),
        506 => (
            "服务内容协商配置错误",
            "上游内容协商形成内部配置冲突，需要服务管理员检查。",
        ),
        507 => (
            "上游存储资源不足",
            "服务无法完成所需存储操作；联系供应商检查其资源，不代表本机磁盘已满。",
        ),
        508 => (
            "上游检测到处理循环",
            "检查服务或代理内部的循环依赖/路由，提供请求 ID 给管理员。",
        ),
        510 => (
            "已废弃的扩展状态码",
            "510 标准已废弃，旧服务可能仍使用；按上游说明检查接口扩展要求。",
        ),
        511 => (
            "网络访问需要认证",
            "检查网络入口或认证门户；通常不是模型 API 的认证问题。",
        ),
        // 以下含义是厂商惯例，其他代理可以重新定义，提示不宣称识别了真实供应商。
        419 | 440 => (
            "非标准认证/会话错误",
            "部分服务用此码表示会话过期或校验失败；含义未统一，请核对上游文档和登录状态。",
        ),
        444 => (
            "非标准连接拒绝",
            "Nginx 的 444 表示不发送响应就关闭连接；客户端收到数值 444 时可能经过网关转译，请检查代理访问规则。",
        ),
        494 => (
            "非标准请求头错误",
            "Nginx 常用此码表示请求头过大；其他服务可能另有定义，请查看网关日志。",
        ),
        495 | 496 => (
            "非标准客户端证书错误",
            "Nginx 常用 495/496 表示客户端证书校验失败/缺失；检查 mTLS 和代理配置，具体以上游为准。",
        ),
        497 => (
            "非标准 HTTP/HTTPS 不匹配",
            "Nginx 常用此码表示向 HTTPS 端口发送了 HTTP；核对 URL 协议与端口。",
        ),
        499 => (
            "非标准请求取消",
            "Nginx 常用此码记录客户端提前断开；收到该响应也可能是中间代理转译，不能据此认定用户主动取消。",
        ),
        520 => (
            "非标准源站响应错误",
            "若由 Cloudflare 返回，表示源站响应为空、未知或异常；结合 Ray ID 检查源站和代理日志。",
        ),
        521 => (
            "非标准源站连接被拒绝",
            "若由 Cloudflare 返回，表示源站拒绝连接；检查源站是否运行、端口和防火墙规则。",
        ),
        522 => (
            "非标准源站连接超时",
            "若由 Cloudflare 返回，表示连接握手或请求确认超时；检查源站可达性、负载和防火墙。",
        ),
        523 => (
            "非标准源站不可达",
            "若由 Cloudflare 返回，检查源站 IP、DNS 和网络路由。",
        ),
        524 => (
            "非标准网关读写超时",
            "Cloudflare 的 524 表示等待源站响应或向源站写入数据超时。不等于已确认请求体超限（超限通常为 413），不能仅凭请求含图片判断原因；请核对网关/源站日志及超时设置。",
        ),
        525 => (
            "非标准源站 TLS 握手失败",
            "若由 Cloudflare 返回，检查 Cloudflare 到源站的 TLS 协议、证书及监听配置；不是要求关闭客户端证书校验。",
        ),
        526 => (
            "非标准源站证书无效",
            "若由 Cloudflare 返回，检查源站证书的有效期、域名和证书链；应修复证书而非关闭校验。",
        ),
        527 => (
            "非标准代理错误",
            "部分代理使用此码，含义未统一；请保留上游原文和请求 ID，按该代理文档排查。",
        ),
        529 => (
            "非标准服务过载",
            "Anthropic 用此码表示服务过载；其他服务的定义可能不同，请结合上游消息和服务状态判断。",
        ),
        530 => (
            "非标准源站解析错误",
            "若由 Cloudflare 返回，检查响应正文中的 1xxx 细分错误码及源站域名解析；530 本身不足以确定具体原因。",
        ),
        598 | 599 => (
            "非标准网络超时",
            "部分代理用 598/599 表示读超时或连接超时，定义未统一；请以上游原文和代理日志为准。",
        ),
        // 已知码逐条列在前；未识别的状态按类别兜底，不重复列区间以免吞掉上面的具体含义。
        other => match other / 100 {
            1 => (
                "意外的信息响应",
                "未取得预期的最终 API 响应；检查代理协议处理。",
            ),
            2 => (
                "成功状态中包含错误",
                "HTTP 状态看似成功，但响应内容报告失败；请以上游错误正文为准。",
            ),
            3 => (
                "未识别的重定向响应",
                "检查 API 地址和代理路由，并保留原始状态码及响应正文。",
            ),
            4 => (
                "未识别的请求拒绝",
                "该状态码可能由供应商自定义；请以上游原文及文档为准，不能只凭数字确定根因。",
            ),
            5 => (
                "未识别的服务端错误",
                "该状态码可能由供应商或代理自定义；结合请求 ID 和服务日志排查，不假定具体故障或自动重试资格。",
            ),
            _ => (
                "非标准状态码",
                "此状态码不属于标准 HTTP 状态范围；请检查代理转译和供应商文档。",
            ),
        },
    }
}

/// 显示层收敛长消息和控制字符；ApiError 的 message/raw_body 始终保留原值。
pub(super) fn message_preview(message: &str) -> String {
    let trimmed = message.trim_start();
    if trimmed
        .get(..15)
        .is_some_and(|s| s.eq_ignore_ascii_case("<!doctype html>"))
        || trimmed
            .get(..5)
            .is_some_and(|s| s.eq_ignore_ascii_case("<html"))
    {
        return "上游返回 HTML 错误页，原始响应已保留；请结合状态码和请求 ID 排查".into();
    }
    let mut out = String::new();
    for (count, ch) in message.chars().enumerate() {
        if count == 512 {
            out.push('…');
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiError;

    #[test]
    fn statuses_have_specific_guidance_without_changing_retry_classification() {
        for (status, title, retryable) in [
            (401, "认证失败", false),
            (407, "代理认证失败", false),
            (413, "请求体过大", false),
            (431, "请求头过大", false),
            (499, "非标准请求取消", false),
            (502, "网关收到无效上游响应", true),
            (504, "网关等待上游超时", true),
            (521, "非标准源站连接被拒绝", false),
            (522, "非标准源站连接超时", false),
            (524, "非标准网关读写超时", true),
            (525, "非标准源站 TLS 握手失败", false),
            (526, "非标准源站证书无效", false),
            (529, "非标准服务过载", true),
            (530, "非标准源站解析错误", false),
            (599, "非标准网络超时", false),
            (555, "未识别的服务端错误", false),
            (471, "未识别的请求拒绝", false),
            (700, "非标准状态码", false),
        ] {
            let error = ApiError::new(
                Some(status),
                ApiErrorKind::from_status(status),
                "provider detail",
                "raw",
            );
            let display = error.to_string();
            assert!(display.contains(title), "{status}: {display}");
            assert!(display.contains("provider detail"));
            assert_eq!(error.is_retryable(), retryable, "{status}");
        }
    }

    #[test]
    fn explicit_semantics_override_generic_http_hints() {
        for (status, kind, title) in [
            (429, ApiErrorKind::QuotaExceeded, "额度耗尽"),
            (400, ApiErrorKind::ContextLength, "上下文超限"),
            (403, ApiErrorKind::ContentFilter, "内容策略拒绝"),
        ] {
            assert_eq!(explanation(Some(status), kind).0, title);
        }
        assert_eq!(explanation(None, ApiErrorKind::Overloaded).0, "服务过载");
    }

    #[test]
    fn display_bounds_plain_text_and_hides_html_without_mutating_original() {
        let text = "中".repeat(600) + "\u{1b}[31m";
        let error = ApiError::new(
            Some(502),
            ApiErrorKind::ServerError,
            text.clone(),
            text.clone(),
        );
        assert!(error.to_string().contains('…'));
        assert!(!error.to_string().contains('\u{1b}'));
        assert_eq!(error.message, text);
        assert_eq!(error.raw_body.as_ref(), text);
        assert_eq!(message_preview("错误\n\u{1b}[31m"), "错误  [31m");
        assert!(
            message_preview("  <!DOCTYPE html><html>bad gateway</html>").contains("HTML 错误页")
        );
        assert_eq!(message_preview("<invalid argument>"), "<invalid argument>");
    }
}

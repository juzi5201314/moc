//! 方法契约：一个方法 = 一个零尺寸标记类型 + 一对 Params/Result DTO。
//!
//! 三个方向标记 trait 决定方法能出现在连线的哪一侧：
//!
//! | trait | 发起方 | 处理方 | 线上类型 |
//! |---|---|---|---|
//! | [`ClientRequest`] | tui / webui / desktop | core（[`crate::AgentService`]） | 请求 |
//! | [`ServerRequest`] | core | 前端（[`crate::ClientBuilder::on_request`]） | 反向请求 |
//! | [`ServerNotification`] | core | 前端（[`crate::ClientBuilder::on_notification`]） | 通知 |
//!
//! 方向是**强约束**而不是文档约定：服务端与客户端都按自己实现的方向查表分发，
//! 方向不符的方法名在另一侧一律按 [`crate::RpcErrorCode::MethodNotFound`] 处理。

use serde::Serialize;
use serde::de::DeserializeOwned;

/// 一个协议方法。
pub trait Method {
    /// 线上方法名。
    const NAME: &'static str;
    /// 参数 DTO。
    type Params: Serialize + DeserializeOwned;
    /// 结果 DTO。
    type Result: Serialize + DeserializeOwned;

    /// 编码请求或通知参数；具体实现留在声明方法的 crate，避免前端重复单态化。
    fn encode_params(params: Self::Params) -> serde_json::Result<serde_json::Value>;
    /// 解码请求或通知参数，保留 DTO 的字段校验与未知变体处理。
    fn decode_params(value: serde_json::Value) -> serde_json::Result<Self::Params>;
    /// 解码响应 DTO；错误到 RPC 错误码的映射仍由调用边界负责。
    fn decode_result(value: serde_json::Value) -> serde_json::Result<Self::Result>;
}

// 不能改成 trait 默认方法：默认方法仍会在调用方实例化，重新复制整条 Serde 链。
macro_rules! json_codec {
    () => {
        #[inline(never)]
        fn encode_params(params: Self::Params) -> serde_json::Result<serde_json::Value> {
            serde_json::to_value(params)
        }

        #[inline(never)]
        fn decode_params(value: serde_json::Value) -> serde_json::Result<Self::Params> {
            serde_json::from_value(value)
        }

        #[inline(never)]
        fn decode_result(value: serde_json::Value) -> serde_json::Result<Self::Result> {
            serde_json::from_value(value)
        }
    };
}
pub(crate) use json_codec;

/// 客户端发起、服务端实现（core 实现 [`crate::AgentService`]）。
pub trait ClientRequest: Method {}

/// 服务端发起（反向请求）、客户端实现。
pub trait ServerRequest: Method {}

/// 服务端单向推送、客户端订阅。
pub trait ServerNotification: Method {}

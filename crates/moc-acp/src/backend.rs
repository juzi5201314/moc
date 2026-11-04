//! moc 调用接缝：生产实现是 [`ClientHandle`]，测试换成记录调用的桩。

use std::future::Future;

use moc_protocol::error::RpcCallError;
use moc_protocol::{ClientHandle, ClientRequest};

/// 调用 core 的最小接缝：只承载 `ClientHandle::call`，处理程序由此与连接解耦。
pub(crate) trait Backend: Clone + Send + Sync + 'static {
    fn call<M: ClientRequest>(
        &self,
        params: M::Params,
    ) -> impl Future<Output = Result<M::Result, RpcCallError>> + Send
    where
        M::Params: Send;
}

impl Backend for ClientHandle {
    async fn call<M: ClientRequest>(&self, params: M::Params) -> Result<M::Result, RpcCallError>
    where
        M::Params: Send,
    {
        ClientHandle::call::<M>(self, params).await
    }
}

#[cfg(test)]
pub(crate) mod stub {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// 按方法名排队返回预置结果，并记录每次调用的 (method, params)。
    #[derive(Clone, Default)]
    pub(crate) struct StubBackend {
        pub calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        pub results: Arc<Mutex<std::collections::VecDeque<(String, serde_json::Value)>>>,
    }

    impl StubBackend {
        pub fn push<M: ClientRequest>(&self, result: M::Result) -> &Self {
            self.results
                .lock()
                .unwrap()
                .push_back((M::NAME.to_owned(), serde_json::to_value(result).unwrap()));
            self
        }
    }

    impl Backend for StubBackend {
        async fn call<M: ClientRequest>(&self, params: M::Params) -> Result<M::Result, RpcCallError>
        where
            M::Params: Send,
        {
            let params = serde_json::to_value(params).unwrap();
            self.calls
                .lock()
                .unwrap()
                .push((M::NAME.to_owned(), params));
            let (name, value) = self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("StubBackend 没有为 {} 预备结果", M::NAME));
            assert_eq!(name, M::NAME, "桩结果与实际调用方法不符");
            Ok(serde_json::from_value(value).unwrap())
        }
    }
}

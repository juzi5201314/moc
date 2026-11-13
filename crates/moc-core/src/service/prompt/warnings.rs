use super::RequestWarning;
use crate::service::protocol_to_rpc;
use moc_protocol::RpcError;
use moc_protocol::methods::SessionUpdate;

pub(super) async fn emit_fatal(
    update: &moc_protocol::UpdateSink,
    session_id: &str,
    prompt_id: &str,
    message: &str,
) -> Result<(), RpcError> {
    update
        .emit(
            session_id.to_owned(),
            SessionUpdate::Error {
                prompt_id: prompt_id.to_owned(),
                message: message.to_owned(),
                retryable: false,
            },
        )
        .await
        .map_err(protocol_to_rpc)
}

impl RequestWarning {
    pub(super) fn spawn(
        update: &moc_protocol::UpdateSink,
        session_id: &str,
        prompt_id: &str,
        warn_secs: u64,
    ) -> Self {
        let update = update.clone();
        let session_id = session_id.to_owned();
        let prompt_id = prompt_id.to_owned();
        Self(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(warn_secs)).await;
            let _ = update
                .emit(
                    session_id,
                    SessionUpdate::GuardNotice {
                        prompt_id,
                        message: format!(
                            "本次请求已超过 {warn_secs} 秒，仍在等待模型响应，不会自动终止"
                        ),
                        terminal: false,
                    },
                )
                .await;
        }))
    }
}

impl Drop for RequestWarning {
    fn drop(&mut self) {
        self.0.abort();
    }
}

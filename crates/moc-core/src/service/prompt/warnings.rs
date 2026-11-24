use super::RequestWarning;
use crate::service::protocol_to_rpc;
use moc_protocol::RpcError;
use moc_protocol::methods::SessionUpdate;
use moc_i18n::t;
use std::time::Duration;

pub(super) async fn emit_request_retry(
    update: &moc_protocol::UpdateSink,
    session_id: &str,
    prompt_id: &str,
    attempt: u32,
    max: u32,
    delay: Duration,
    message: &str,
) -> Result<(), RpcError> {
    update
        .emit(
            session_id.to_owned(),
            SessionUpdate::GenerationRetry {
                prompt_id: prompt_id.to_owned(),
                message: t!(
                    "core.err.request_retry_notice",
                    secs = format!("{:.1}", delay.as_secs_f64()),
                    attempt = attempt,
                    max = max,
                    message = message,
                )
                .into_owned(),
                retry_in_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            },
        )
        .await
        .map_err(protocol_to_rpc)
}

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
                        message: t!("core.err.slow_request_warn", secs = warn_secs).into_owned(),
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

//! 历史页读取：小页直返，大页在私有文件重组，避免完整 JSON 字符串与消息同时常驻。
use moc_protocol::methods::{
    SessionHistory, SessionHistoryParams, SessionHistoryResult, SessionLoadPageParams,
    SessionLoadPageResult,
};
use moc_protocol::{ClientHandle, RpcCallError, RpcError, RpcErrorCode};
use tokio::io::AsyncWriteExt;

pub(crate) trait HistoryClient {
    async fn history_page(
        &self,
        page: SessionLoadPageParams,
    ) -> Result<SessionLoadPageResult, RpcCallError>;
}

struct Release {
    client: ClientHandle,
    id: Option<String>,
}
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(transfer_id) = self.id.take() {
            let client = self.client.clone();
            // 即使上层取消加载也主动释放；断线/进程退出还有服务端连接清理。
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = client
                        .call::<SessionHistory>(SessionHistoryParams::Close { transfer_id })
                        .await;
                });
            }
        }
    }
}

impl HistoryClient for ClientHandle {
    async fn history_page(
        &self,
        page: SessionLoadPageParams,
    ) -> Result<SessionLoadPageResult, RpcCallError> {
        let mut result = self
            .call::<SessionHistory>(SessionHistoryParams::Open { page })
            .await?;
        if let SessionHistoryResult::Page { page } = result {
            return Ok(*page);
        }
        let SessionHistoryResult::Chunk {
            ref transfer_id, ..
        } = result
        else {
            return Err(failed("历史传输未返回页面"));
        };
        let mut release = Release {
            client: self.clone(),
            id: Some(transfer_id.clone()),
        };
        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(failed)?
            .map_err(failed)?;
        let mut file = tokio::fs::File::from_std(file);
        let mut expected = 0u64;
        loop {
            let SessionHistoryResult::Chunk {
                transfer_id,
                offset,
                data,
                next_offset,
            } = result
            else {
                return Err(failed("历史传输中途改变结果类型"));
            };
            let next = offset
                .checked_add(data.len() as u64)
                .ok_or_else(|| failed("历史块偏移溢出"))?;
            if release.id.as_ref() != Some(&transfer_id)
                || offset != expected
                || next_offset.is_some_and(|value| value != next || value <= offset)
            {
                return Err(failed("历史块不连续"));
            }
            file.write_all(data.as_bytes()).await.map_err(failed)?;
            let Some(offset) = next_offset else {
                break;
            };
            expected = offset;
            result = self
                .call::<SessionHistory>(SessionHistoryParams::Read {
                    transfer_id,
                    offset,
                })
                .await?;
        }
        file.flush().await.map_err(failed)?;
        if let Some(transfer_id) = release.id.take() {
            let _ = self
                .call::<SessionHistory>(SessionHistoryParams::Close { transfer_id })
                .await;
        }
        let mut file = file.into_std().await;
        tokio::task::spawn_blocking(move || {
            use std::io::Seek;
            file.rewind().map_err(failed)?;
            serde_json::from_reader(std::io::BufReader::new(file)).map_err(failed)
        })
        .await
        .map_err(failed)?
    }
}

fn failed(error: impl std::fmt::Display) -> RpcCallError {
    RpcError::new(
        RpcErrorCode::InternalError.code(),
        format!("历史页读取失败：{error}"),
    )
    .into()
}

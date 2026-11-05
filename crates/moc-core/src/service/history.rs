//! 完整历史页的冻结传输；大 JSON 写入私有临时文件，线上每块大小固定。
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moc_protocol::methods::{SessionHistoryParams, SessionHistoryResult, SessionLoadPageResult};
use moc_protocol::{AgentService, RequestContext, RpcError, RpcErrorCode};

use super::{Core, CoreInner};

const INLINE: usize = 1024 * 1024;
const CHUNK: usize = 256 * 1024;
const TTL: Duration = Duration::from_secs(300);

const MAX_TRANSFERS: usize = 8;

pub(crate) struct Transfers {
    // 完整 UUID 标识冻结结果，连接号隔离读取权限。
    entries: HashMap<(u64, String), Arc<parking_lot::Mutex<Transfer>>>,
    connections: HashSet<u64>,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Default for Transfers {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            connections: HashSet::new(),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_TRANSFERS)),
        }
    }
}
struct Transfer {
    file: File,
    len: u64,
    touched: Instant,
    slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Transfers {
    fn get(
        &self,
        connection: u64,
        id: &str,
    ) -> Result<Arc<parking_lot::Mutex<Transfer>>, RpcError> {
        self.entries
            .get(&(connection, id.to_owned()))
            .cloned()
            .ok_or_else(|| invalid("历史传输已关闭或过期，请重新读取"))
    }
    fn close(&mut self, connection: u64, id: String) {
        self.entries.remove(&(connection, id));
    }
}

/// 小页只使用最多 INLINE 字节的暂存区；溢出后转移到文件，不先创建完整 JSON Vec。
#[derive(Default)]
struct Spool {
    memory: Vec<u8>,
    file: Option<File>,
    len: u64,
}
impl Write for Spool {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.file.is_none() && self.memory.len().saturating_add(bytes.len()) > INLINE {
            let mut file = tempfile::tempfile()?;
            file.write_all(&self.memory)?;
            self.memory = Vec::new();
            self.file = Some(file);
        }
        if let Some(file) = &mut self.file {
            file.write_all(bytes)?;
        } else {
            self.memory.extend_from_slice(bytes);
        }
        self.len += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()?;
        }
        Ok(())
    }
}

fn freeze(page: &SessionLoadPageResult) -> Result<Option<Transfer>, RpcError> {
    let mut writer = std::io::BufWriter::new(Spool::default());
    serde_json::to_writer(&mut writer, page).map_err(io_error)?;
    let spool = writer.into_inner().map_err(io_error)?;
    Ok(spool.file.map(|file| Transfer {
        file,
        len: spool.len,
        touched: Instant::now(),
        slot: None,
    }))
}
impl Transfer {
    fn read(&mut self, id: String, offset: u64) -> Result<SessionHistoryResult, RpcError> {
        if self.touched.elapsed() >= TTL {
            return Err(invalid("历史传输已过期，请重新读取"));
        }
        if offset > self.len {
            return Err(invalid("历史块偏移越界"));
        }
        self.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let mut bytes = vec![0; (self.len - offset).min(CHUNK as u64) as usize];
        self.file.read_exact(&mut bytes).map_err(io_error)?;
        match std::str::from_utf8(&bytes) {
            Ok(_) => {}
            Err(error) if error.error_len().is_none() => bytes.truncate(error.valid_up_to()),
            Err(_) => return Err(invalid("历史块偏移必须位于 UTF-8 边界")),
        }
        let next = offset + bytes.len() as u64;
        self.touched = Instant::now();
        Ok(SessionHistoryResult::Chunk {
            transfer_id: id,
            offset,
            data: String::from_utf8(bytes).expect("字节边界已经验证"),
            next_offset: (next < self.len).then_some(next),
        })
    }
}

pub(super) async fn handle(
    core: &Core,
    ctx: RequestContext,
    params: SessionHistoryParams,
) -> Result<SessionHistoryResult, RpcError> {
    let connection = ctx.connection_id();
    match params {
        SessionHistoryParams::Open { page } => {
            let slot = Arc::clone(&core.inner.history_transfers.lock().slots)
                .try_acquire_owned()
                .map_err(|_| {
                    RpcError::new(
                        RpcErrorCode::SessionBusy.code(),
                        "历史传输繁忙，请在当前读取完成后重试",
                    )
                })?;
            let page = core.session_load_page(ctx.clone(), page).await?;
            let prepared = tokio::task::spawn_blocking(move || {
                let Some(mut transfer) = freeze(&page)? else {
                    return Ok::<_, RpcError>((
                        SessionHistoryResult::Page {
                            page: Box::new(page),
                        },
                        None,
                    ));
                };
                // 在后台丢弃完整DTO，文件持有许可直到close/过期/断线。
                drop(page);
                transfer.slot = Some(slot);
                let id = uuid::Uuid::now_v7().to_string();
                let chunk = transfer.read(id.clone(), 0)?;
                Ok((chunk, Some((id, transfer))))
            })
            .await
            .map_err(io_error)??;
            let (chunk, transfer) = prepared;
            if ctx.cancel().is_cancelled() {
                return Err(invalid("历史读取已取消"));
            }
            let Some((id, transfer)) = transfer else {
                return Ok(chunk);
            };
            let fresh = {
                let mut transfers = core.inner.history_transfers.lock();
                transfers.entries.insert(
                    (connection, id),
                    Arc::new(parking_lot::Mutex::new(transfer)),
                );
                debug_assert!(transfers.entries.len() <= MAX_TRANSFERS);
                transfers.connections.insert(connection)
            };
            if fresh {
                watch(&core.inner, &ctx);
            }
            Ok(chunk)
        }
        SessionHistoryParams::Read {
            transfer_id,
            offset,
        } => {
            let transfer = core
                .inner
                .history_transfers
                .lock()
                .get(connection, &transfer_id)?;
            tokio::task::spawn_blocking(move || transfer.lock().read(transfer_id, offset))
                .await
                .map_err(io_error)?
        }
        SessionHistoryParams::Close { transfer_id } => {
            core.inner
                .history_transfers
                .lock()
                .close(connection, transfer_id);
            Ok(SessionHistoryResult::Closed)
        }
    }
}

fn watch(inner: &Arc<CoreInner>, ctx: &RequestContext) {
    let weak = Arc::downgrade(inner);
    let updates = ctx.updates();
    let shutdown = inner.shutdown.clone();
    let connection = ctx.connection_id();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = updates.closed() => break,
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let Some(inner) = weak.upgrade() else { return; };
                    // 文件读取很短；不等待正在读取的传输锁。
                    inner.history_transfers.lock().entries.retain(|_, transfer| {
                        transfer.try_lock().is_none_or(|value| value.touched.elapsed() < TTL)
                    });
                }
            }
        }
        if let Some(inner) = weak.upgrade() {
            let mut transfers = inner.history_transfers.lock();
            transfers
                .entries
                .retain(|(owner, _), _| *owner != connection);
            transfers.connections.remove(&connection);
        }
    });
}
fn invalid(message: &str) -> RpcError {
    RpcError::new(RpcErrorCode::InvalidParams.code(), message)
}
fn io_error(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        RpcErrorCode::InternalError.code(),
        format!("历史传输失败：{error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionTable, entries::SessionOrigin};
    use moc_protocol::{Message, SessionEngine, SessionPermission};

    #[tokio::test]
    async fn frozen_pages_round_trip_utf8_and_release_with_owner() {
        let root = tempfile::tempdir().unwrap();
        let table = SessionTable::new(root.path().join("sessions"), false);
        let session = table
            .create(
                "/tmp",
                "test/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .unwrap();
        let small = table.load_page(&session.id, None, 1, None).await.unwrap();
        assert!(freeze(&small).unwrap().is_none(), "小页无需文件或传输句柄");
        table
            .append_message(
                &session.id,
                Message::user_text("中文🙂\\\"\n".repeat(100_000)),
                None,
                None,
            )
            .await
            .unwrap();
        let mut page = table
            .load_pages(&session.id, None, 1, None, None, None, true)
            .await
            .unwrap();
        let expected = page.clone();
        let mut transfer = freeze(&page).unwrap().expect("大页溢出到文件");
        page.entries.clear();
        assert!(transfer.read("t".into(), transfer.len + 1).is_err());
        let first = transfer.read("t".into(), 0).unwrap();
        assert_eq!(
            transfer.read("t".into(), 0).unwrap(),
            first,
            "同偏移重读幂等"
        );
        let mut data = String::new();
        let mut offset = 0;
        let mut count = 0;
        loop {
            let chunk = transfer.read("t".into(), offset).unwrap();
            assert!(serde_json::to_vec(&chunk).unwrap().len() <= CHUNK * 2 + 256);
            let SessionHistoryResult::Chunk {
                data: text,
                next_offset,
                offset: actual,
                ..
            } = chunk
            else {
                panic!("应为数据块");
            };
            assert_eq!(actual, offset);
            assert!(text.len() <= CHUNK && !text.is_empty());
            data.push_str(&text);
            count += 1;
            let Some(next) = next_offset else {
                break;
            };
            assert_eq!(next, data.len() as u64);
            offset = next;
        }
        assert!(count > 1);
        assert_eq!(
            serde_json::from_str::<SessionLoadPageResult>(&data).unwrap(),
            expected,
            "冻结结果不受源页面变化影响，正文及字段必须无损"
        );
        let middle = data.find('中').unwrap() as u64 + 1;
        assert!(transfer.read("t".into(), middle).is_err());
        transfer.touched = Instant::now() - TTL;
        assert!(transfer.read("t".into(), 0).is_err(), "过期后不得继续读取");
        let mut transfers = Transfers::default();
        transfer.slot = Some(Arc::clone(&transfers.slots).try_acquire_owned().unwrap());
        assert_eq!(transfers.slots.available_permits(), MAX_TRANSFERS - 1);
        let transfer = Arc::new(parking_lot::Mutex::new(transfer));
        let weak = Arc::downgrade(&transfer);
        transfers.entries.insert((1, "t".into()), transfer);
        assert!(transfers.get(2, "t").is_err(), "其它连接不能读此传输");
        transfers.close(2, "t".into());
        assert!(weak.upgrade().is_some(), "其它连接不能释放此传输");
        transfers.close(1, "t".into());
        assert!(weak.upgrade().is_none(), "close释放最后一个文件句柄");
        assert_eq!(transfers.slots.available_permits(), MAX_TRANSFERS);
        transfers.close(1, "t".into());
    }
}

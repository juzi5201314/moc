//! 连接收尾测试：对端不读数据、处理器 panic 时，收尾都必须完成。
//!
//! 用可控假对端复现「连接活着但对端不再读取」：读半边只递一次请求，写半边接受
//! 固定字节数后永远不 ready。服务端的 `write_all` 因此永久挂起——收尾若不响应
//! 取消，`close()` / `wait()` 会跟着挂死。

mod support;

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use moc_protocol::frame::DEFAULT_MAX_FRAME_BYTES;
use moc_protocol::methods::{SessionPrompt, SessionUpdateMethod};
use moc_protocol::{ClientBuilder, FrameEncoder, Framing, RpcCallError, Server, ServerOptions};
use serde_json::json;
use support::{ScriptedService, client_info, local_client, prompt_params, request};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// ───────────────────────────── 假对端 ─────────────────────────────

/// 假对端：读半边一次性递完请求，写半边接受 `limit` 字节后永远不 ready。
#[derive(Clone)]
struct StalledPeer {
    inner: Arc<StalledInner>,
}

struct StalledInner {
    /// 预组好的请求帧与已递出的偏移。
    incoming: Mutex<(Vec<u8>, usize)>,
    /// 写半边已接受的字节数。
    accepted: AtomicUsize,
    /// 写半边最多接受多少字节。
    limit: usize,
}

impl StalledPeer {
    /// 构造：`requests` 是预组好的请求帧字节，`limit` 是写半边能接受的字节数。
    fn new(requests: Vec<u8>, limit: usize) -> Self {
        Self {
            inner: Arc::new(StalledInner {
                incoming: Mutex::new((requests, 0)),
                accepted: AtomicUsize::new(0),
                limit,
            }),
        }
    }

    /// 写半边是否已经用满额度（此后再写永远不 ready）。
    fn is_stalled(&self) -> bool {
        self.inner.accepted.load(Ordering::SeqCst) >= self.inner.limit
    }
}

impl AsyncRead for StalledPeer {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut incoming = self
            .inner
            .incoming
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (bytes, offset) = &mut *incoming;
        if *offset >= bytes.len() {
            // 对端活着但不再发数据：不 EOF、不 ready
            return Poll::Pending;
        }
        let size = (bytes.len() - *offset).min(buf.remaining());
        buf.put_slice(&bytes[*offset..*offset + size]);
        *offset += size;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for StalledPeer {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let accepted = self.inner.accepted.load(Ordering::SeqCst);
        if accepted >= self.inner.limit {
            // 对端不再读取：永远不 ready（真实世界里就是 TCP 窗口打满）
            return Poll::Pending;
        }
        let size = (self.inner.limit - accepted).min(buf.len());
        self.inner.accepted.store(accepted + size, Ordering::SeqCst);
        Poll::Ready(Ok(size))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// 组若干 `ping` 请求帧。
fn ping_frames(count: i64) -> Vec<u8> {
    let mut encoder = FrameEncoder::new(Framing::ContentLength);
    let mut frames = Vec::new();
    for id in 1..=count {
        let body = request(id, "ping", json!({}))
            .to_vec()
            .expect("报文可序列化");
        assert!(encoder.encode(&body, DEFAULT_MAX_FRAME_BYTES, &mut frames));
    }
    frames
}

#[tokio::test]
async fn teardown_completes_while_peer_stops_reading() {
    // 三条 ping 的响应远超写半边额度：写任务必然卡在第一条响应中间
    let peer = StalledPeer::new(ping_frames(3), 16);
    let server = Server::with_options(ScriptedService::new(), ServerOptions::default());
    let connection = server.spawn(peer.clone());

    let deadline = Instant::now() + Duration::from_secs(5);
    while !peer.is_stalled() {
        assert!(Instant::now() < deadline, "写任务没有被对端拖住");
        tokio::task::yield_now().await;
    }

    server.shutdown();
    tokio::time::timeout(Duration::from_secs(5), connection.wait())
        .await
        .expect("对端不读数据时收尾也必须完成");
    assert!(connection.is_closed());
}

// ───────────────────────────── 处理器 panic ─────────────────────────────

#[tokio::test]
async fn handler_panic_still_finishes_the_connection() {
    // 前端注册的通知处理器 panic 会沿读循环展开主任务：完成信号必须照样触发，
    // 否则 `close()` / `closed()` 永久挂起、写任务跟着泄漏。
    let builder = ClientBuilder::new(client_info()).on_notification::<SessionUpdateMethod, _, _>(
        |_params, _ctx| -> std::future::Ready<()> {
            panic!("处理器故意 panic，验证收尾兜底")
        },
    );
    let (_server, connection, handle, _) = local_client(ScriptedService::new(), builder).await;

    let prompt_handle = handle.clone();
    let prompt = tokio::spawn(async move {
        prompt_handle
            .call::<SessionPrompt>(prompt_params("ses_1", "触发更新"))
            .await
    });

    tokio::time::timeout(Duration::from_secs(5), handle.closed())
        .await
        .expect("处理器 panic 后前端连接必须收尾");
    assert!(handle.is_closed());
    let outcome = prompt.await.expect("任务正常结束");
    assert!(
        matches!(outcome, Err(RpcCallError::Closed)),
        "连接已死，在飞提示应当收到 Closed：{outcome:?}"
    );

    // 前端半边随写任务退出而关闭：服务端也要收尾
    tokio::time::timeout(Duration::from_secs(5), connection.wait())
        .await
        .expect("前端 panic 后服务端连接必须收尾");
}

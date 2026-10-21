//! 浏览器 WebSocket 与 core 传输层之间的双向桥。
//!
//! 每个 WebSocket 连接在 core 侧都有一条独立的 NDJSON 连接：`initialize`、
//! 提示注册表、在飞请求上限都是 per-connection 的，和 TUI 内嵌连接没有区别。
//! 多个标签页因此是多个前端连接共享同一个 core，而不是多路复用一条连接。

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use moc_core::Core;
use moc_protocol::transport;
use moc_protocol::{Server, ServerOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn serve(socket: WebSocket, server: Arc<Server<Core>>) {
    let (core_side, frontend_side) = transport::duplex(256 * 1024);
    let connection = server.spawn(core_side);
    let (mut sink, mut stream) = socket.split();
    let (mut reader, mut writer) = tokio::io::split(frontend_side);

    // 浏览器 → core：每条 WS 消息补一个换行，凑成一行 JSON。
    let to_core = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            let bytes = match message {
                Message::Text(text) => text.as_str().as_bytes().to_vec(),
                Message::Binary(bytes) => bytes.to_vec(),
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            if writer.write_all(&bytes).await.is_err() || writer.write_all(b"\n").await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    // core → 浏览器：NDJSON 一行一帧。
    let to_ws = tokio::spawn(async move {
        let mut lines = BufReader::new(&mut reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.is_empty() {
                continue;
            }
            if sink.send(Message::Text(line.into())).await.is_err() {
                break;
            }
        }
    });

    let _ = tokio::join!(to_core, to_ws);
    connection.close();
    connection.wait().await;
}

/// 桥接用的服务端选项：WS 是消息边界，NDJSON 让每一帧自带结尾。
pub fn options() -> ServerOptions {
    ServerOptions {
        framing: moc_protocol::Framing::Ndjson,
        ..ServerOptions::default()
    }
}

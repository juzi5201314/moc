//! 进程级 HTTP(S) 字节计数：`moc monitor` 的网络面板以此为准。
//!
//! 计数只覆盖经 reqwest 收发的响应体 / 请求体（不含 TLS 与头部开销）；
//! 其他 crate 的 reqwest 读取路径可以调用 [`record_rx`] / [`record_tx`]
//! 汇入同一对计数器。

use std::sync::atomic::{AtomicU64, Ordering};

/// 进程启动以来 HTTP(S) 响应体累计收字节。
pub static HTTP_RX_BYTES: AtomicU64 = AtomicU64::new(0);
/// 进程启动以来 HTTP(S) 请求体累计发字节。
pub static HTTP_TX_BYTES: AtomicU64 = AtomicU64::new(0);

/// 计入收到的 `bytes` 字节。
pub fn record_rx(bytes: u64) {
    HTTP_RX_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// 计入发出的 `bytes` 字节。
pub fn record_tx(bytes: u64) {
    HTTP_TX_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

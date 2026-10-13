//! 传输入口：内核只认 [`Transport`]，具体形态由调用方挑。
//!
//! | 入口 | 用途 |
//! |---|---|
//! | [`stdio`] | 前端以子进程方式拉起 core（webui / desktop 的常规接法） |
//! | [`duplex`] | TUI 内嵌 core：一对内存管道直连 [`crate::Server`] 与 [`crate::ClientBuilder`] |
//! | [`listen_tcp`] / [`connect_tcp`] | 跨进程、跨容器；**只允许绑定回环地址** |
//! | [`listen_unix`] / [`connect_unix`] | 同机跨进程（Unix 域套接字，带文件权限） |
//!
//! 传输层**不做鉴权**：拿到管道就等于拿到 core 的全部能力（含执行工具）。TCP 因此
//! 只应绑定回环；需要跨机访问时应当在外部（SSH 隧道、mTLS 代理）解决，或者后续
//! 在协议层加握手鉴权——本轮不做。
//!
//! Windows 下没有 Unix 域套接字入口，只有 stdio 与 TCP。

use std::io;
use std::net::SocketAddr;
use std::path::Path;

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::net::{TcpListener, TcpStream};

/// 传输只需要满足它：读写半边都会由连接内核拆开。
pub trait Transport: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T> Transport for T where T: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

/// 标准输入 + 标准输出组成的传输。
///
/// 前端以子进程方式拉起 core 时用这条：`stdout` 只承载协议报文，日志必须走
/// `stderr`（`tracing` 默认就是），否则会把帧流写坏。
pub fn stdio() -> impl Transport {
    tokio::io::join(tokio::io::stdin(), tokio::io::stdout())
}

/// 一对内存双向管道。
///
/// TUI 内嵌 core 的接法：`server.spawn(server_side)` 与
/// `ClientBuilder::connect(client_side)`，两端跑的是与跨进程完全相同的调用代码。
/// `buffer_bytes` 是单向缓冲上限，也是内存中背压的起点。
pub fn duplex(buffer_bytes: usize) -> (DuplexStream, DuplexStream) {
    tokio::io::duplex(buffer_bytes)
}

/// 连接 TCP 服务端。
pub async fn connect_tcp(addr: SocketAddr) -> io::Result<TcpStream> {
    TcpStream::connect(addr).await
}

/// 监听 TCP。只接受回环地址，避免未鉴权的执行协议暴露到局域网。
pub async fn listen_tcp(addr: SocketAddr) -> io::Result<TcpListener> {
    if !addr.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TCP transport only accepts loopback addresses",
        ));
    }
    tokio::net::TcpListener::bind(addr).await
}

/// 连接 Unix 域套接字。
#[cfg(unix)]
pub async fn connect_unix(path: impl AsRef<Path>) -> io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(path).await
}

/// 监听 Unix 域套接字，并把套接字文件权限收紧到 `0600`。
///
/// 套接字文件的权限决定谁能连上（core 会执行工具，权限比 TCP 端口更好把控），
/// 因此这里显式设置而不依赖进程 umask：设置失败即整体失败，绝不留一个权限随
/// 环境变化的套接字。
#[cfg(unix)]
pub async fn listen_unix(path: impl AsRef<Path>) -> io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;

    let path = path.as_ref();
    let listener = tokio::net::UnixListener::bind(path)?;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        // 清理掉权限不可信的套接字文件，否则下一次绑定会撞上它
        drop(listener);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tcp_listener_rejects_non_loopback_addresses() {
        let address = "0.0.0.0:0".parse().unwrap();
        let result = listen_tcp(address).await;
        assert!(matches!(
            result,
            Err(error) if error.kind() == io::ErrorKind::InvalidInput
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_listener_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时间")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("moc-unix-{stamp}"));
        std::fs::create_dir_all(&directory).expect("临时目录");
        let path = directory.join("core.sock");

        let listener = listen_unix(&path).await.expect("绑定成功");
        let mode = std::fs::metadata(&path)
            .expect("套接字文件存在")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "套接字必须只有属主可连");

        drop(listener);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&directory);
    }
}

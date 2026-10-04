use std::io;
use std::net::SocketAddr;

use tokio::sync::mpsc;

use teto_dpdk::TcpSocketOptions;

use crate::driver::AcceptItem;
use crate::runtime::{runtime_stopped, Cmd, TetoRuntime};
use crate::tcp_stream::TetoTcpStream;

/// Accepted connections waiting for [`TetoTcpListener::accept`]. When full,
/// the driver stops accepting and new connections wait in F-Stack's backlog.
const ACCEPT_QUEUE: usize = 1024;

/// An async TCP listener backed by F-Stack.
///
/// Mirrors the [`tokio::net::TcpListener`](https://docs.rs/tokio/1/tokio/net/struct.TcpListener.html)
/// API: create it from a [`TetoRuntime`] with [`bind`](Self::bind), then
/// [`accept`](Self::accept) connections as [`TetoTcpStream`]s. Dropping the
/// listener closes the listening socket; accepted streams keep working.
///
/// # Example
///
/// ```rust,no_run
/// use teto_tokio::{TetoRuntime, TetoTcpListener};
/// use teto_dpdk::config::{FStackConfig, TcpSocketOptions};
/// use tokio::io::{AsyncReadExt, AsyncWriteExt};
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let rt = TetoRuntime::start(FStackConfig::for_docker()).await?;
///     let opts = TcpSocketOptions::default().nodelay(true);
///     let mut listener = TetoTcpListener::bind(&rt, "0.0.0.0:8080".parse().unwrap(), opts).await?;
///
///     loop {
///         let (mut stream, peer) = listener.accept().await?;
///         tokio::spawn(async move {
///             let mut buf = [0u8; 4096];
///             loop {
///                 match stream.read(&mut buf).await {
///                     Ok(0) | Err(_) => break,
///                     Ok(n) => { let _ = stream.write_all(&buf[..n]).await; }
///                 }
///             }
///         });
///     }
/// }
/// ```
pub struct TetoTcpListener {
    accept_rx: mpsc::Receiver<AcceptItem>,
    local_addr: SocketAddr,
    rt: TetoRuntime,
}

impl std::fmt::Debug for TetoTcpListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TetoTcpListener").field("local_addr", &self.local_addr).finish()
    }
}

impl TetoTcpListener {
    /// Listen on `addr` (IPv4 only). `opts` are applied to every accepted
    /// connection. A failure (e.g. an address not configured in
    /// `config.ini`) leaves the runtime usable.
    pub async fn bind(rt: &TetoRuntime, addr: SocketAddr, opts: TcpSocketOptions) -> io::Result<Self> {
        crate::require_v4(addr)?;
        opts.validate()?;
        let (accept_tx, accept_rx) = mpsc::channel(ACCEPT_QUEUE);
        let local_addr = rt.call(|reply| Cmd::ListenTcp { addr, opts, accept_tx, reply }).await?;
        Ok(TetoTcpListener { accept_rx, local_addr, rt: rt.clone() })
    }

    /// Accept the next inbound connection.
    ///
    /// Per-connection failures (e.g. descriptor exhaustion) are returned as
    /// errors; the listener stays usable.
    pub async fn accept(&mut self) -> io::Result<(TetoTcpStream, SocketAddr)> {
        let connected = self.accept_rx.recv().await.ok_or_else(runtime_stopped)??;
        let peer = connected.peer;
        let stream = TetoTcpStream::from_connected(connected, self.rt.clone());
        stream.mark_accepted();
        Ok((stream, peer))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

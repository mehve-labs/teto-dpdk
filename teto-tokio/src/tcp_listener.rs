use std::io;
use std::net::SocketAddr;

use tokio::sync::{mpsc, oneshot};

use teto_dpdk::{FStack, FStackConfig, TcpSocketOptions};

use crate::tcp_driver::{AcceptItem, TcpDriver};
use crate::tcp_stream::TetoTcpStream;

/// Accepted connections waiting for [`TetoTcpListener::accept`]. When full,
/// the driver stops accepting and new connections wait in F-Stack's backlog.
const ACCEPT_QUEUE: usize = 1024;

/// An async TCP listener backed by F-Stack.
///
/// Mirrors the [`tokio::net::TcpListener`](https://docs.rs/tokio/1/tokio/net/struct.TcpListener.html) API. [`bind`](Self::bind)
/// initialises F-Stack on a dedicated thread and starts listening;
/// [`accept`](Self::accept) yields [`TetoTcpStream`]s.
///
/// # Example
///
/// ```rust,no_run
/// use teto_tokio::TetoTcpListener;
/// use teto_dpdk::config::{FStackConfig, TcpSocketOptions};
/// use tokio::io::{AsyncReadExt, AsyncWriteExt};
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let cfg = FStackConfig::for_docker();
///     let opts = TcpSocketOptions::default().nodelay(true);
///     let mut listener = TetoTcpListener::bind(cfg, "0.0.0.0:8080".parse().unwrap(), opts).await?;
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
    _thread: std::thread::JoinHandle<()>,
}

impl TetoTcpListener {
    /// Initialise F-Stack and start listening on `addr` (IPv4 only).
    ///
    /// Spawns a dedicated OS thread that owns the F-Stack poll loop. F-Stack
    /// can be initialised once per process, so this can be called once, and
    /// not together with [`TetoUdpSocket::bind`](crate::TetoUdpSocket::bind).
    /// Initialisation and bind failures are returned as errors. Because
    /// F-Stack can't be initialised twice, a bind failure (e.g. an address
    /// not configured in `config.ini`) can't be retried in the same process.
    ///
    /// The F-Stack thread exits once the listener and all its streams have
    /// been dropped; F-Stack can't be restarted afterwards.
    pub async fn bind(
        cfg: FStackConfig,
        addr: SocketAddr,
        opts: TcpSocketOptions,
    ) -> io::Result<Self> {
        crate::require_v4(addr)?;
        opts.validate()?;

        let (accept_tx, accept_rx) = mpsc::channel(ACCEPT_QUEUE);
        let (ready_tx, ready_rx) = oneshot::channel::<io::Result<SocketAddr>>();

        let thread = std::thread::Builder::new()
            .name("fstack-tcp".into())
            .spawn(move || {
                let started = FStack::init(&cfg)
                    .and_then(|fs| TcpDriver::new(&fs, addr, &opts, accept_tx).map(|d| (fs, d)));
                let (fs, mut driver) = match started {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(driver.local_addr()));
                // Runs until the listener and every stream are dropped (or a
                // tick panics; the panic then resumes here).
                let _ = fs.run(|| {
                    if !driver.tick() {
                        fs.stop();
                    }
                });
            })?;

        let local_addr = ready_rx.await.map_err(|_| {
            io::Error::other("F-Stack thread exited during initialisation")
        })??;

        Ok(TetoTcpListener { accept_rx, local_addr, _thread: thread })
    }

    /// Accept the next inbound connection.
    ///
    /// Per-connection failures (e.g. a socket option F-Stack rejected for this
    /// connection) are returned as errors; the listener stays usable.
    pub async fn accept(&mut self) -> io::Result<(TetoTcpStream, SocketAddr)> {
        match self.accept_rx.recv().await {
            Some(Ok((stream, peer))) => {
                stream.mark_accepted();
                Ok((stream, peer))
            }
            Some(Err(e)) => Err(e),
            None => Err(io::Error::new(io::ErrorKind::BrokenPipe, "F-Stack runtime stopped")),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

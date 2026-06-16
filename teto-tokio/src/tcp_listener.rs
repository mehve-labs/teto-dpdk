use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::collections::HashMap;

use tokio::sync::mpsc;

use teto_dpdk::config::{FStackConfig, TcpSocketOptions};
use teto_dpdk::fstack::ffi::{
    create_tcp_listener, init_fstack, run_fstack_tcp, set_tcp_tick_callback, FStackTcpListener,
};

use crate::runtime::{tcp_on_connect, tcp_on_data, tcp_on_disconnect, tcp_tick};
use crate::runtime::{TcpChannelHub, TCP_HUB};
use crate::tcp_stream::TetoTcpStream;

/// An async TCP listener backed by F-Stack.
///
/// Mirrors the [`tokio::net::TcpListener`] API. Call [`bind`](Self::bind) to
/// initialise the F-Stack runtime on a dedicated thread and start listening,
/// then [`accept`](Self::accept) to receive new connections as
/// [`TetoTcpStream`]s.
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
    accept_rx: mpsc::UnboundedReceiver<(TetoTcpStream, SocketAddr)>,
    local_addr: SocketAddr,
    _thread: std::thread::JoinHandle<()>,
}

impl TetoTcpListener {
    /// Initialise F-Stack and start listening on `addr`.
    ///
    /// This spawns a dedicated OS thread that owns the F-Stack event loop.
    /// The returned listener receives new connections via an async channel.
    ///
    /// Can only be called once per process (F-Stack is a singleton).
    pub async fn bind(
        cfg: FStackConfig,
        addr: SocketAddr,
        opts: TcpSocketOptions,
    ) -> io::Result<Self> {
        let (accept_tx, accept_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        TCP_HUB
            .set(TcpChannelHub {
                accept_tx,
                cmd_tx,
                cmd_rx: Mutex::new(cmd_rx),
                connections: Mutex::new(HashMap::new()),
                listener_ptr: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "F-Stack TCP runtime already initialised",
                )
            })?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let local_addr = addr;

        let thread = std::thread::Builder::new()
            .name("fstack-tcp".into())
            .spawn(move || {
                init_fstack(&cfg.config_args(), &cfg.eal_args());

                let ip = addr.ip().to_string();
                let port = addr.port();
                let ffi_opts = opts.to_ffi();

                let listener = create_tcp_listener(
                    &ip,
                    port,
                    &ffi_opts,
                    tcp_on_connect,
                    tcp_on_data,
                    tcp_on_disconnect,
                );

                let hub = TCP_HUB.get().unwrap();
                let raw = &*listener as *const FStackTcpListener;
                hub.listener_ptr
                    .store(raw as *mut (), Ordering::Release);

                set_tcp_tick_callback(tcp_tick);

                let _ = ready_tx.send(Ok(()));

                // Blocks forever — the listener UniquePtr stays alive on the stack.
                run_fstack_tcp(&listener);
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        ready_rx
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "F-Stack thread panicked during init")
            })?
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(TetoTcpListener {
            accept_rx,
            local_addr,
            _thread: thread,
        })
    }

    /// Accept the next inbound TCP connection.
    ///
    /// Returns the async stream and the peer's socket address.
    pub async fn accept(&mut self) -> io::Result<(TetoTcpStream, SocketAddr)> {
        self.accept_rx.recv().await.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "F-Stack runtime shut down",
            )
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

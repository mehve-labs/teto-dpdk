use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use tokio::sync::mpsc;

use teto_dpdk::config::FStackConfig;
use teto_dpdk::fstack::ffi::{
    create_udp_socket, init_fstack, run_fstack, set_udp_tick_callback, FStackUdpSocket,
};

use crate::runtime::{udp_on_packet, udp_tick};
use crate::runtime::{FStackCmd, UdpChannelHub, UDP_HUB};

/// An async UDP socket backed by F-Stack.
///
/// Mirrors the [`tokio::net::UdpSocket`] API. Call [`bind`](Self::bind) to
/// initialise the F-Stack runtime and bind the socket, then use
/// [`recv_from`](Self::recv_from) and [`send_to`](Self::send_to) for async I/O.
///
/// # Example
///
/// ```rust,no_run
/// use teto_tokio::TetoUdpSocket;
/// use teto_dpdk::config::FStackConfig;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let cfg = FStackConfig::for_docker();
///     let mut socket = TetoUdpSocket::bind(cfg, "0.0.0.0:8080".parse().unwrap()).await?;
///
///     let mut buf = [0u8; 65535];
///     loop {
///         let (n, peer) = socket.recv_from(&mut buf).await?;
///         socket.send_to(&buf[..n], peer).await?;
///     }
/// }
/// ```
pub struct TetoUdpSocket {
    recv_rx: mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
    cmd_tx: mpsc::UnboundedSender<FStackCmd>,
    local_addr: SocketAddr,
    _thread: std::thread::JoinHandle<()>,
}

impl TetoUdpSocket {
    /// Initialise F-Stack and bind a UDP socket on `addr`.
    ///
    /// Spawns a dedicated OS thread for the F-Stack event loop.
    /// Can only be called once per process (F-Stack is a singleton).
    pub async fn bind(cfg: FStackConfig, addr: SocketAddr) -> io::Result<Self> {
        let (recv_tx, recv_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        UDP_HUB
            .set(UdpChannelHub {
                recv_tx,
                cmd_tx: cmd_tx.clone(),
                cmd_rx: Mutex::new(cmd_rx),
                socket_ptr: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "F-Stack UDP runtime already initialised",
                )
            })?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let local_addr = addr;

        let thread = std::thread::Builder::new()
            .name("fstack-udp".into())
            .spawn(move || {
                init_fstack(&cfg.config_args(), &cfg.eal_args());

                let ip = addr.ip().to_string();
                let port = addr.port();

                let socket = create_udp_socket(&ip, port, udp_on_packet);

                let hub = UDP_HUB.get().unwrap();
                let raw = &*socket as *const FStackUdpSocket;
                hub.socket_ptr
                    .store(raw as *mut (), Ordering::Release);

                set_udp_tick_callback(udp_tick);

                let _ = ready_tx.send(Ok(()));

                run_fstack(&socket);
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        ready_rx
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "F-Stack thread panicked during init")
            })?
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(TetoUdpSocket {
            recv_rx,
            cmd_tx,
            local_addr,
            _thread: thread,
        })
    }

    /// Receive a datagram, returning the number of bytes read and the sender's
    /// address. If the datagram is larger than `buf`, excess bytes are dropped.
    pub async fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (data, addr) = self.recv_rx.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "F-Stack runtime shut down")
        })?;

        let n = std::cmp::min(data.len(), buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok((n, addr))
    }

    /// Send a datagram to `addr`. Returns the number of bytes sent.
    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let len = buf.len();
        self.cmd_tx
            .send(FStackCmd::UdpSend {
                data: buf.to_vec(),
                addr,
            })
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "F-Stack runtime shut down")
            })?;
        Ok(len)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

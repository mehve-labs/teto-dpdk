use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};

use teto_dpdk::net::UdpSocket;

use crate::conn::lock;
use crate::runtime::{runtime_stopped, Cmd, TetoRuntime};
use crate::require_v4;

/// Datagrams buffered in each direction between tokio and the F-Stack thread.
const QUEUE: usize = 1024;
/// Datagrams handled per direction per poll iteration.
const BATCH: usize = 256;
const MAX_DATAGRAM: usize = 65535;
/// Receive buffers are carved out of blocks this size, one allocation per block.
const RX_BLOCK: usize = 1024 * 1024;

type RxItem = io::Result<(Bytes, SocketAddr)>;

#[derive(Default)]
struct Shared {
    /// First send error since the last `send_to` call.
    send_error: Mutex<Option<io::Error>>,
}

/// The F-Stack-thread ends of a socket's channels, sent with the bind command.
pub(crate) struct UdpParts {
    rx_tx: mpsc::Sender<RxItem>,
    tx_rx: mpsc::Receiver<(Bytes, SocketAddrV4)>,
    shared: Arc<Shared>,
}

/// An async UDP socket backed by F-Stack.
///
/// Mirrors the [`tokio::net::UdpSocket`](https://docs.rs/tokio/1/tokio/net/struct.UdpSocket.html)
/// API: create it from a [`TetoRuntime`] with [`bind`](Self::bind), then use
/// [`recv_from`](Self::recv_from) and [`send_to`](Self::send_to).
///
/// # Example
///
/// ```rust,no_run
/// use teto_tokio::{TetoRuntime, TetoUdpSocket};
/// use teto_dpdk::config::FStackConfig;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let rt = TetoRuntime::start(FStackConfig::for_docker()).await?;
///     let socket = TetoUdpSocket::bind(&rt, "0.0.0.0:8080".parse().unwrap()).await?;
///
///     let mut buf = [0u8; 65535];
///     loop {
///         let (n, peer) = socket.recv_from(&mut buf).await?;
///         socket.send_to(&buf[..n], peer).await?;
///     }
/// }
/// ```
pub struct TetoUdpSocket {
    rx: tokio::sync::Mutex<mpsc::Receiver<RxItem>>,
    tx: mpsc::Sender<(Bytes, SocketAddrV4)>,
    shared: Arc<Shared>,
    local_addr: SocketAddr,
    _rt: TetoRuntime,
}

impl std::fmt::Debug for TetoUdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TetoUdpSocket").field("local_addr", &self.local_addr).finish()
    }
}

impl TetoUdpSocket {
    /// Bind a UDP socket on `addr` (IPv4 only). Datagrams queued with
    /// `send_to` are still sent after the socket is dropped.
    pub async fn bind(rt: &TetoRuntime, addr: SocketAddr) -> io::Result<Self> {
        require_v4(addr)?;
        let (rx_tx, rx_rx) = mpsc::channel(QUEUE);
        let (tx_tx, tx_rx) = mpsc::channel(QUEUE);
        let shared = Arc::new(Shared::default());
        let parts = UdpParts { rx_tx, tx_rx, shared: shared.clone() };
        let local_addr = rt.call(|reply| Cmd::BindUdp { addr, parts, reply }).await?;
        Ok(TetoUdpSocket {
            rx: tokio::sync::Mutex::new(rx_rx),
            tx: tx_tx,
            shared,
            local_addr,
            _rt: rt.clone(),
        })
    }

    /// Receive a datagram, returning the number of bytes copied into `buf`
    /// and the sender's address. If the datagram is larger than `buf`, the
    /// excess is discarded. Receive errors reported by F-Stack are returned.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let item = self.rx.lock().await.recv().await;
        let (data, addr) = item.ok_or_else(runtime_stopped)??;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok((n, addr))
    }

    /// Queue a datagram to `addr` (IPv4 only), waiting while the send queue is
    /// full. Returns `buf.len()` once queued.
    ///
    /// Sending happens on the F-Stack thread, so a failure is reported by the
    /// next `send_to` call (similar to how kernel sockets report ICMP errors).
    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let addr = require_v4(addr)?;
        if let Some(e) = lock(&self.shared.send_error).take() {
            return Err(e);
        }
        self.tx
            .send((Bytes::copy_from_slice(buf), addr))
            .await
            .map_err(|_| runtime_stopped())?;
        Ok(buf.len())
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

fn is_transient_send_error(e: &io::Error) -> bool {
    const ENOBUFS: i32 = 105;
    e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(ENOBUFS)
}

/// Driver-side state of one UDP socket (lives on the F-Stack thread).
pub(crate) struct UdpEntry {
    socket: UdpSocket,
    rx_tx: mpsc::Sender<RxItem>,
    tx_rx: mpsc::Receiver<(Bytes, SocketAddrV4)>,
    /// The `TetoUdpSocket` is gone and its send queue is empty.
    tx_done: bool,
    /// A datagram F-Stack couldn't take yet; retried next tick.
    pending: Option<(Bytes, SocketAddrV4)>,
    rx_buf: BytesMut,
    shared: Arc<Shared>,
}

impl UdpEntry {
    pub(crate) fn new(socket: UdpSocket, parts: UdpParts) -> Self {
        UdpEntry {
            socket,
            rx_tx: parts.rx_tx,
            tx_rx: parts.tx_rx,
            tx_done: false,
            pending: None,
            rx_buf: BytesMut::new(),
            shared: parts.shared,
        }
    }

    pub(crate) fn tick(&mut self) {
        self.send_batch();
        self.recv_batch();
    }

    /// The `TetoUdpSocket` has been dropped and every queued datagram sent.
    pub(crate) fn finished(&self) -> bool {
        self.tx_done && self.pending.is_none() && self.rx_tx.is_closed()
    }

    fn send_batch(&mut self) {
        for _ in 0..BATCH {
            let (data, addr) = match self.pending.take() {
                Some(d) => d,
                None => match self.tx_rx.try_recv() {
                    Ok(d) => d,
                    Err(TryRecvError::Empty) => return,
                    Err(TryRecvError::Disconnected) => {
                        self.tx_done = true;
                        return;
                    }
                },
            };
            match self.socket.send_to(&data, addr.into()) {
                Ok(_) => {}
                Err(e) if is_transient_send_error(&e) => {
                    self.pending = Some((data, addr));
                    return;
                }
                Err(e) => {
                    lock(&self.shared.send_error).get_or_insert(e);
                }
            }
        }
    }

    fn recv_batch(&mut self) {
        for _ in 0..BATCH {
            // Reserve queue space first: if the application is behind, leave
            // datagrams in F-Stack's receive buffer (which drops on overflow).
            let permit = match self.rx_tx.try_reserve() {
                Ok(p) => p,
                Err(TrySendError::Full(()) | TrySendError::Closed(())) => return,
            };
            if self.rx_buf.capacity() - self.rx_buf.len() < MAX_DATAGRAM {
                self.rx_buf = BytesMut::with_capacity(RX_BLOCK);
            }
            let spare = &mut self.rx_buf.spare_capacity_mut()[..MAX_DATAGRAM];
            match self.socket.recv_from_uninit(spare) {
                Ok((n, from)) => {
                    // SAFETY: F-Stack initialised the first `n` spare bytes.
                    unsafe { self.rx_buf.set_len(n) };
                    permit.send(Ok((self.rx_buf.split().freeze(), from.into())));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    // At most one error per tick, so a persistent error
                    // can't flood the queue.
                    permit.send(Err(e));
                    return;
                }
            }
        }
    }
}

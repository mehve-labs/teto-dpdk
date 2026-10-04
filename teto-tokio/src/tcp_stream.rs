use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Buf;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use teto_dpdk::TcpSocketOptions;

use crate::conn::{Conn, ConnState, WriteShutdown, RX_LOW, TX_LIMIT};
use crate::driver::Connected;
use crate::runtime::{Cmd, TetoRuntime};

/// An async TCP stream backed by an F-Stack connection.
///
/// Implements [`tokio::io::AsyncRead`] and [`tokio::io::AsyncWrite`], so it
/// works with the usual `tokio::io` utilities (`read`, `write_all`, `copy`,
/// `BufReader`, `split`, ...).
///
/// - Reads return data received by the F-Stack thread. `Ok(0)` means the peer
///   shut down its write side; the stream can still be written to.
/// - Writes are buffered (up to 256 KiB per connection) and handed to F-Stack
///   on its next poll iteration. `poll_write` returns `Pending` while the
///   buffer is full, so a slow peer slows the writer down. `flush` completes
///   once F-Stack has accepted everything written, so unlike a kernel socket
///   it can wait indefinitely on a peer that stops reading (wrap it in a
///   timeout if that matters).
/// - `shutdown` flushes and then shuts down the write side (TCP FIN).
/// - Connection failures (e.g. reset by peer) are returned as errors from
///   reads and writes.
/// - Dropping the stream closes the connection gracefully: buffered writes
///   are sent, then FIN, and the socket is closed once the peer has
///   acknowledged everything. A peer that doesn't take the data within 30 s
///   gets a reset.
pub struct TetoTcpStream {
    conn: Arc<Conn>,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    _rt: TetoRuntime,
}

impl TetoTcpStream {
    /// Open a connection to `addr` (IPv4 only). `opts` are applied before
    /// connecting. Fails with the connect error, e.g.
    /// [`ConnectionRefused`](io::ErrorKind::ConnectionRefused); an
    /// unreachable peer times out after F-Stack's SYN retries (over a
    /// minute), so wrap it in `tokio::time::timeout` if that matters —
    /// cancelling closes the half-open socket.
    pub async fn connect(rt: &TetoRuntime, addr: SocketAddr, opts: TcpSocketOptions) -> io::Result<Self> {
        crate::require_v4(addr)?;
        opts.validate()?;
        let connected = rt.call(|reply| Cmd::Connect { addr, opts, reply }).await?;
        Ok(Self::from_connected(connected, rt.clone()))
    }

    pub(crate) fn from_connected(connected: Connected, rt: TetoRuntime) -> Self {
        let (peer_addr, local_addr) = (connected.peer, connected.local);
        Self { conn: connected.take_conn(), peer_addr, local_addr, _rt: rt }
    }

    /// Called when the application takes the stream from the accept queue.
    pub(crate) fn mark_accepted(&self) {
        let mut st = self.conn.lock();
        st.accepted = true;
        self.conn.notify(&mut st);
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl std::fmt::Debug for TetoTcpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TetoTcpStream")
            .field("id", &self.conn.id)
            .field("peer_addr", &self.peer_addr)
            .field("local_addr", &self.local_addr)
            .finish()
    }
}

fn write_error(st: &ConnState) -> Option<io::Error> {
    if let Some(e) = st.error {
        return Some(e.to_io());
    }
    if st.wr_shutdown != WriteShutdown::Open {
        return Some(io::Error::new(io::ErrorKind::BrokenPipe, "write side shut down"));
    }
    None
}

impl AsyncRead for TetoTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let conn = &self.conn;
        let mut st = conn.lock();

        if !st.rx.is_empty() {
            let n = st.rx.len().min(buf.remaining());
            buf.put_slice(&st.rx[..n]);
            st.rx.advance(n);
            if st.rx_paused && st.rx.len() < RX_LOW {
                conn.notify(&mut st);
            }
            return Poll::Ready(Ok(()));
        }
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if let Some(e) = st.error {
            return Poll::Ready(Err(e.to_io()));
        }
        if st.rx_eof {
            return Poll::Ready(Ok(()));
        }
        st.read_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for TetoTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let conn = &self.conn;
        let mut st = conn.lock();
        if let Some(e) = write_error(&st) {
            return Poll::Ready(Err(e));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if st.tx.len() >= TX_LIMIT {
            st.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = buf.len().min(TX_LIMIT - st.tx.len());
        st.tx.extend_from_slice(&buf[..n]);
        conn.notify(&mut st);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut st = self.conn.lock();
        if let Some(e) = st.error {
            return Poll::Ready(Err(e.to_io()));
        }
        if st.tx.is_empty() {
            return Poll::Ready(Ok(()));
        }
        st.write_waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let conn = &self.conn;
        let mut st = conn.lock();
        if st.wr_shutdown == WriteShutdown::Done {
            return Poll::Ready(Ok(()));
        }
        if let Some(e) = st.error {
            return Poll::Ready(Err(e.to_io()));
        }
        if st.wr_shutdown == WriteShutdown::Open {
            st.wr_shutdown = WriteShutdown::Requested;
            conn.notify(&mut st);
        }
        st.write_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for TetoTcpStream {
    fn drop(&mut self) {
        let conn = &self.conn;
        let mut st = conn.lock();
        st.dropped = true;
        st.read_waker = None;
        st.write_waker = None;
        conn.notify(&mut st);
    }
}

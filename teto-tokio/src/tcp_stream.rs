use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Buf;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use teto_dpdk::TcpSocketOptions;

use crate::conn::{register, Conn, ConnState, WriteShutdown, RX_LOW, TX_LIMIT};
use crate::driver::Connected;
use crate::runtime::{Cmd, TetoRuntime};

/// An async TCP stream backed by an F-Stack connection.
///
/// Implements [`tokio::io::AsyncRead`] and [`tokio::io::AsyncWrite`], so it
/// works with the usual `tokio::io` utilities (`read`, `write_all`, `copy`,
/// `BufReader`, ...). [`into_split`](Self::into_split) gives owned halves for
/// separate reader and writer tasks.
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
/// - Dropping the stream (or both halves) closes the connection gracefully:
///   buffered writes are sent, then FIN, and the socket is closed once the
///   peer has acknowledged everything. A peer that doesn't take the data
///   within 30 s gets a reset.
pub struct TetoTcpStream {
    core: Arc<StreamCore>,
}

/// State shared by a stream and its owned halves. The connection is closed
/// when the last of them is dropped.
struct StreamCore {
    conn: Arc<Conn>,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    rt: TetoRuntime,
}

impl Drop for StreamCore {
    fn drop(&mut self) {
        let mut st = self.conn.lock();
        st.dropped = true;
        st.read_wakers.clear();
        st.write_wakers.clear();
        self.conn.notify(&mut st);
    }
}

impl TetoTcpStream {
    /// Open a connection to `addr`. `opts` are applied before connecting.
    /// Fails with the connect error, e.g.
    /// [`ConnectionRefused`](io::ErrorKind::ConnectionRefused); an
    /// unreachable peer times out after F-Stack's SYN retries (over a
    /// minute), so wrap it in `tokio::time::timeout` if that matters —
    /// cancelling closes the half-open socket.
    pub async fn connect(rt: &TetoRuntime, addr: SocketAddr, opts: TcpSocketOptions) -> io::Result<Self> {
        let connected = rt.call(|reply| Cmd::Connect { local: None, addr, opts, reply }).await?;
        Ok(Self::from_connected(connected, rt.clone()))
    }

    /// Like [`connect`](Self::connect), from the local address `local` (port 0
    /// picks a free port). `local` and `addr` must be the same address
    /// family; set [`TcpSocketOptions::reuse_port`] to share a local port.
    pub async fn connect_from(
        rt: &TetoRuntime,
        local: SocketAddr,
        addr: SocketAddr,
        opts: TcpSocketOptions,
    ) -> io::Result<Self> {
        let connected = rt.call(|reply| Cmd::Connect { local: Some(local), addr, opts, reply }).await?;
        Ok(Self::from_connected(connected, rt.clone()))
    }

    pub(crate) fn from_connected(connected: Connected, rt: TetoRuntime) -> Self {
        let (peer_addr, local_addr) = (connected.peer, connected.local);
        let core = StreamCore { conn: connected.take_conn(), peer_addr, local_addr, rt };
        TetoTcpStream { core: Arc::new(core) }
    }

    /// Called when the application takes the stream from the accept queue.
    pub(crate) fn mark_accepted(&self) {
        let conn = &self.core.conn;
        let mut st = conn.lock();
        st.accepted = true;
        conn.notify(&mut st);
    }

    /// The remote address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.core.peer_addr
    }

    /// The local address of this connection.
    pub fn local_addr(&self) -> SocketAddr {
        self.core.local_addr
    }

    /// Apply socket options to the live connection (unset fields are left
    /// alone). Unlike tokio's synchronous setters this is async: the option is
    /// set on the F-Stack thread, and its result reported back.
    pub async fn set_options(&self, opts: &TcpSocketOptions) -> io::Result<()> {
        let id = self.core.conn.id;
        let opts = opts.clone();
        self.core.rt.call(|reply| Cmd::SetOptions { id, opts, reply }).await
    }

    /// Enable or disable Nagle's algorithm (`TCP_NODELAY`). See
    /// [`set_options`](Self::set_options).
    pub async fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.set_options(&TcpSocketOptions::default().nodelay(nodelay)).await
    }

    /// Wait until a read would make progress: data is buffered, the peer has
    /// shut down its write side, or the connection failed.
    pub async fn readable(&self) -> io::Result<()> {
        poll_fn(|cx| poll_readable(&self.core.conn, cx)).await
    }

    /// Wait until a write would make progress (there's room in the send
    /// buffer), or the connection failed.
    pub async fn writable(&self) -> io::Result<()> {
        poll_fn(|cx| poll_writable(&self.core.conn, cx)).await
    }

    /// Read without waiting. Returns [`io::ErrorKind::WouldBlock`] when
    /// nothing is buffered, `Ok(0)` at end of stream.
    pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        try_read(&self.core.conn, buf)
    }

    /// Write without waiting. Returns [`io::ErrorKind::WouldBlock`] when the
    /// send buffer is full.
    pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        try_write(&self.core.conn, buf)
    }

    /// Copy received data into `buf` without consuming it, waiting until some
    /// is available. Returns `Ok(0)` at end of stream.
    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| poll_peek(&self.core.conn, cx, buf)).await
    }

    /// Split into a read half and a write half that can be used from
    /// different tasks. The connection closes when both are dropped; dropping
    /// the write half also shuts down the write side (FIN), as with tokio.
    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
        let core = self.core;
        (OwnedReadHalf { core: core.clone() }, OwnedWriteHalf { core, shutdown_on_drop: true })
    }
}

impl std::fmt::Debug for TetoTcpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TetoTcpStream")
            .field("id", &self.core.conn.id)
            .field("peer_addr", &self.core.peer_addr)
            .field("local_addr", &self.core.local_addr)
            .finish()
    }
}

/// The read half of a [`TetoTcpStream`], from [`TetoTcpStream::into_split`].
pub struct OwnedReadHalf {
    core: Arc<StreamCore>,
}

/// The write half of a [`TetoTcpStream`], from [`TetoTcpStream::into_split`].
/// Dropping it shuts down the write side (FIN) unless
/// [`forget`](Self::forget) was called.
pub struct OwnedWriteHalf {
    core: Arc<StreamCore>,
    shutdown_on_drop: bool,
}

/// [`OwnedReadHalf::reunite`] was given halves of different streams.
#[derive(Debug)]
pub struct ReuniteError(pub OwnedReadHalf, pub OwnedWriteHalf);

impl std::fmt::Display for ReuniteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tried to reunite halves that are not from the same stream")
    }
}

impl std::error::Error for ReuniteError {}

impl OwnedReadHalf {
    /// Put the halves back together into a stream.
    pub fn reunite(self, mut other: OwnedWriteHalf) -> Result<TetoTcpStream, ReuniteError> {
        if !Arc::ptr_eq(&self.core, &other.core) {
            return Err(ReuniteError(self, other));
        }
        other.shutdown_on_drop = false;
        Ok(TetoTcpStream { core: self.core.clone() })
    }

    /// The remote address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.core.peer_addr
    }

    /// The local address of the connection.
    pub fn local_addr(&self) -> SocketAddr {
        self.core.local_addr
    }

    /// See [`TetoTcpStream::readable`].
    pub async fn readable(&self) -> io::Result<()> {
        poll_fn(|cx| poll_readable(&self.core.conn, cx)).await
    }

    /// See [`TetoTcpStream::try_read`].
    pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        try_read(&self.core.conn, buf)
    }

    /// See [`TetoTcpStream::peek`].
    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| poll_peek(&self.core.conn, cx, buf)).await
    }
}

impl OwnedWriteHalf {
    /// Drop this half without shutting down the write side.
    pub fn forget(mut self) {
        self.shutdown_on_drop = false;
    }

    /// Put the halves back together into a stream.
    pub fn reunite(self, other: OwnedReadHalf) -> Result<TetoTcpStream, ReuniteError> {
        other.reunite(self)
    }

    /// The remote address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.core.peer_addr
    }

    /// The local address of the connection.
    pub fn local_addr(&self) -> SocketAddr {
        self.core.local_addr
    }

    /// See [`TetoTcpStream::writable`].
    pub async fn writable(&self) -> io::Result<()> {
        poll_fn(|cx| poll_writable(&self.core.conn, cx)).await
    }

    /// See [`TetoTcpStream::try_write`].
    pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        try_write(&self.core.conn, buf)
    }
}

impl Drop for OwnedWriteHalf {
    fn drop(&mut self) {
        if self.shutdown_on_drop {
            let conn = &self.core.conn;
            let mut st = conn.lock();
            if st.wr_shutdown == WriteShutdown::Open && st.error.is_none() {
                st.wr_shutdown = WriteShutdown::Requested;
                conn.notify(&mut st);
            }
        }
    }
}

impl std::fmt::Debug for OwnedReadHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedReadHalf").field("id", &self.core.conn.id).finish()
    }
}

impl std::fmt::Debug for OwnedWriteHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedWriteHalf").field("id", &self.core.conn.id).finish()
    }
}

// --- the actual I/O, shared by the stream and its halves ---

fn write_error(st: &ConnState) -> Option<io::Error> {
    if let Some(e) = st.error {
        return Some(e.to_io());
    }
    if st.wr_shutdown != WriteShutdown::Open {
        return Some(io::Error::new(io::ErrorKind::BrokenPipe, "write side shut down"));
    }
    None
}

/// Drop `n` consumed bytes; tell the driver to resume reading if it paused
/// and the backlog has drained enough.
fn consume_rx(conn: &Conn, st: &mut ConnState, n: usize) {
    st.rx.advance(n);
    if st.rx_paused && st.rx.len() < RX_LOW {
        conn.notify(st);
    }
}

fn poll_read(conn: &Conn, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
    let mut st = conn.lock();
    if !st.rx.is_empty() {
        let n = st.rx.len().min(buf.remaining());
        buf.put_slice(&st.rx[..n]);
        consume_rx(conn, &mut st, n);
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
    register(&mut st.read_wakers, cx.waker());
    Poll::Pending
}

fn poll_peek(conn: &Conn, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
    let mut st = conn.lock();
    if !st.rx.is_empty() || buf.is_empty() {
        let n = st.rx.len().min(buf.len());
        buf[..n].copy_from_slice(&st.rx[..n]);
        return Poll::Ready(Ok(n));
    }
    if let Some(e) = st.error {
        return Poll::Ready(Err(e.to_io()));
    }
    if st.rx_eof {
        return Poll::Ready(Ok(0));
    }
    register(&mut st.read_wakers, cx.waker());
    Poll::Pending
}

fn poll_readable(conn: &Conn, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    let mut st = conn.lock();
    if !st.rx.is_empty() || st.rx_eof {
        return Poll::Ready(Ok(()));
    }
    if let Some(e) = st.error {
        return Poll::Ready(Err(e.to_io()));
    }
    register(&mut st.read_wakers, cx.waker());
    Poll::Pending
}

fn try_read(conn: &Conn, buf: &mut [u8]) -> io::Result<usize> {
    let mut st = conn.lock();
    if !st.rx.is_empty() {
        let n = st.rx.len().min(buf.len());
        buf[..n].copy_from_slice(&st.rx[..n]);
        consume_rx(conn, &mut st, n);
        return Ok(n);
    }
    if let Some(e) = st.error {
        return Err(e.to_io());
    }
    if st.rx_eof || buf.is_empty() {
        return Ok(0);
    }
    Err(io::ErrorKind::WouldBlock.into())
}

/// Queue up to the free space in the send buffer.
fn push_tx(conn: &Conn, st: &mut ConnState, buf: &[u8]) -> usize {
    let n = buf.len().min(TX_LIMIT - st.tx.len());
    st.tx.extend_from_slice(&buf[..n]);
    conn.notify(st);
    n
}

fn poll_write(conn: &Conn, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
    let mut st = conn.lock();
    if let Some(e) = write_error(&st) {
        return Poll::Ready(Err(e));
    }
    if buf.is_empty() {
        return Poll::Ready(Ok(0));
    }
    if st.tx.len() >= TX_LIMIT {
        register(&mut st.write_wakers, cx.waker());
        return Poll::Pending;
    }
    Poll::Ready(Ok(push_tx(conn, &mut st, buf)))
}

fn poll_writable(conn: &Conn, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    let mut st = conn.lock();
    if let Some(e) = write_error(&st) {
        return Poll::Ready(Err(e));
    }
    if st.tx.len() < TX_LIMIT {
        return Poll::Ready(Ok(()));
    }
    register(&mut st.write_wakers, cx.waker());
    Poll::Pending
}

fn try_write(conn: &Conn, buf: &[u8]) -> io::Result<usize> {
    let mut st = conn.lock();
    if let Some(e) = write_error(&st) {
        return Err(e);
    }
    if buf.is_empty() {
        return Ok(0);
    }
    if st.tx.len() >= TX_LIMIT {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    Ok(push_tx(conn, &mut st, buf))
}

fn poll_flush(conn: &Conn, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    let mut st = conn.lock();
    if let Some(e) = st.error {
        return Poll::Ready(Err(e.to_io()));
    }
    if st.tx.is_empty() {
        return Poll::Ready(Ok(()));
    }
    register(&mut st.write_wakers, cx.waker());
    Poll::Pending
}

fn poll_shutdown(conn: &Conn, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
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
    register(&mut st.write_wakers, cx.waker());
    Poll::Pending
}

impl AsyncRead for TetoTcpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        poll_read(&self.core.conn, cx, buf)
    }
}

impl AsyncWrite for TetoTcpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        poll_write(&self.core.conn, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_flush(&self.core.conn, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_shutdown(&self.core.conn, cx)
    }
}

impl AsyncRead for OwnedReadHalf {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        poll_read(&self.core.conn, cx, buf)
    }
}

impl AsyncWrite for OwnedWriteHalf {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        poll_write(&self.core.conn, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_flush(&self.core.conn, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_shutdown(&self.core.conn, cx)
    }
}

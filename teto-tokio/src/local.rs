//! Run async code on the F-Stack thread itself, with no cross-thread hop.
//!
//! [`TetoRuntime`](crate::TetoRuntime) runs F-Stack on its own thread and
//! hands data to tokio's worker threads through per-connection buffers: one
//! extra copy and a cross-thread wakeup per operation. In local mode,
//! [`run`] makes the calling thread the F-Stack thread *and* a single-threaded
//! tokio runtime: tasks (spawned with [`tokio::task::spawn_local`]) call
//! F-Stack directly through [`LocalTcpListener`], [`LocalTcpStream`] and
//! [`LocalUdpSocket`]. tokio's timers and sync primitives work as usual.
//!
//! Trade-offs: everything runs on one thread (CPU-heavy work stalls the
//! network; use `spawn_blocking` for it), and the socket types are `!Send`.
//!
//! ```rust,no_run
//! use teto_dpdk::{FStackConfig, TcpSocketOptions};
//! use teto_tokio::local::{self, LocalTcpListener};
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! fn main() -> std::io::Result<()> {
//!     local::run(FStackConfig::for_docker(), async {
//!         let listener = LocalTcpListener::bind("0.0.0.0:8080".parse().unwrap(), &TcpSocketOptions::default())?;
//!         loop {
//!             let (mut stream, _) = listener.accept().await?;
//!             tokio::task::spawn_local(async move {
//!                 let mut buf = [0u8; 4096];
//!                 while let Ok(n @ 1..) = stream.read(&mut buf).await {
//!                     if stream.write_all(&buf[..n]).await.is_err() {
//!                         break;
//!                     }
//!                 }
//!             });
//!         }
//!     })?
//! }
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::io;
use std::mem::MaybeUninit;
use std::net::{Shutdown, SocketAddr};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use teto_dpdk::event::{Events, Interest, Kqueue, Source};
use teto_dpdk::net::{TcpListener, TcpStream, UdpSocket};
use teto_dpdk::{FStack, FStackConfig, TcpSocketOptions};

/// How long a dropped stream may take to get its data acknowledged.
const DROP_GRACE: Duration = Duration::from_secs(30);
/// How long the loop keeps running after the last connection closed, so
/// final FINs/ACKs leave before F-Stack is torn down.
const STOP_GRACE: Duration = Duration::from_secs(1);

thread_local! {
    static REACTOR: RefCell<Option<Reactor>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct Slot {
    read: Option<Waker>,
    write: Option<Waker>,
}

/// Wakes tasks when F-Stack reports their sockets ready. Lives in a
/// thread-local on the F-Stack thread for the duration of [`run`].
struct Reactor {
    fs: FStack,
    kq: Kqueue,
    events: Events,
    next_token: u64,
    slots: HashMap<u64, Slot>,
    /// Dropped streams still delivering their data (FIN sent), by token.
    closing: HashMap<u64, (TcpStream, Instant)>,
}

fn with_reactor<R>(f: impl FnOnce(&mut Reactor) -> R) -> R {
    REACTOR.with(|r| {
        let mut r = r.borrow_mut();
        f(r.as_mut().expect("teto_tokio::local types can only be used inside teto_tokio::local::run"))
    })
}

impl Reactor {
    fn token(&mut self) -> u64 {
        let t = self.next_token;
        self.next_token += 1;
        self.slots.insert(t, Slot::default());
        t
    }

    /// Wake the current task once `source` is ready for `interest`
    /// (`READABLE` or `WRITABLE`). The one-shot registration is armed only if
    /// no earlier wait on this direction is still pending.
    fn wait(&mut self, token: u64, source: &impl Source, interest: Interest, waker: &Waker) -> io::Result<()> {
        let slot = self.slots.entry(token).or_default();
        let stored = if interest.is_readable() { &mut slot.read } else { &mut slot.write };
        let armed = stored.is_some();
        *stored = Some(waker.clone());
        if !armed {
            self.kq.register_oneshot(source, token, interest)?;
        }
        Ok(())
    }

    /// Whether a wait for writability on `token` is still pending (the event
    /// hasn't fired yet).
    fn write_pending(&self, token: u64) -> bool {
        self.slots.get(&token).is_some_and(|s| s.write.is_some())
    }

    /// Poll the kqueue and wake whoever waits on ready sockets.
    fn poll(&mut self) -> io::Result<()> {
        self.kq.poll(&mut self.events)?;
        let mut done = Vec::new();
        for ev in self.events.iter() {
            let token = ev.token();
            if ev.is_send_empty() {
                done.push(token);
                continue;
            }
            if let Some(slot) = self.slots.get_mut(&token) {
                if ev.is_readable()
                    && let Some(w) = slot.read.take()
                {
                    w.wake();
                }
                if ev.is_writable()
                    && let Some(w) = slot.write.take()
                {
                    w.wake();
                }
            }
        }
        for token in done {
            self.closing.remove(&token);
        }
        let now = Instant::now();
        self.closing.retain(|_, (_, since)| now.duration_since(*since) < DROP_GRACE);
        Ok(())
    }

    /// Hand a dropped stream over: send FIN and close it once the peer has
    /// acknowledged everything (or after `DROP_GRACE`).
    fn close_gracefully(&mut self, token: u64, stream: TcpStream) {
        self.slots.remove(&token);
        let _ = stream.shutdown(Shutdown::Write);
        if stream.unsent_bytes().map_or(true, |n| n == 0) {
            return; // nothing left in flight: close now
        }
        if self.kq.register_oneshot(&stream, token, Interest::SEND_EMPTY).is_ok() {
            self.closing.insert(token, (stream, Instant::now()));
        }
    }
}

/// Run `main` on this thread with F-Stack, until it completes.
///
/// Initialises F-Stack on the calling thread (once per process, like
/// [`FStack::init`]), then runs a single-threaded tokio runtime and F-Stack's
/// poll loop together. Inside, use the `Local*` socket types and
/// [`tokio::task::spawn_local`]. When `main` finishes, tasks still running
/// are dropped; `run` returns `main`'s output once every connection has
/// delivered its data (or timed out). F-Stack is torn down afterwards and
/// can't be restarted.
pub fn run<F: Future + 'static>(cfg: FStackConfig, main: F) -> io::Result<F::Output>
where
    F::Output: 'static,
{
    let fs = FStack::init(&cfg)?;
    let kq = Kqueue::new(&fs)?;
    REACTOR.with(|r| {
        *r.borrow_mut() = Some(Reactor {
            fs,
            kq,
            events: Events::with_capacity(1024),
            next_token: 1,
            slots: HashMap::new(),
            closing: HashMap::new(),
        })
    });
    struct ClearReactor;
    impl Drop for ClearReactor {
        fn drop(&mut self) {
            REACTOR.with(|r| r.borrow_mut().take());
        }
    }
    let _clear = ClearReactor;

    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build()?;
    let mut tasks = Some(tokio::task::LocalSet::new());
    let output = Rc::new(RefCell::new(None));
    let out = output.clone();
    tasks.as_ref().unwrap().spawn_local(async move {
        let value = main.await;
        *out.borrow_mut() = Some(value);
    });

    let mut idle_since: Option<Instant> = None;
    let mut failed = None;
    fs.run(|| {
        if let Err(e) = with_reactor(Reactor::poll) {
            failed = Some(e);
            fs.stop();
            return;
        }
        // Let every task that is ready make progress. Yielding once also has
        // the runtime poll its timer without blocking. (A zero sleep would
        // wait for the timer's 1 ms tick on every iteration.)
        if let Some(t) = &tasks {
            rt.block_on(t.run_until(tokio::task::yield_now()));
        }
        if output.borrow().is_some() {
            if tasks.is_some() {
                // `main` is done: drop the remaining tasks now, while F-Stack
                // still runs, so the connections they hold drain gracefully.
                let _ctx = rt.enter();
                tasks = None;
            }
            if with_reactor(|r| r.closing.is_empty()) {
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= STOP_GRACE {
                    fs.stop();
                }
            } else {
                idle_since = None;
            }
        }
    })?;
    if let Some(e) = failed {
        return Err(e);
    }
    let value = output.borrow_mut().take();
    value.ok_or_else(|| io::Error::other("main future did not complete"))
}

/// A TCP listener for [`run`]. `!Send`.
#[derive(Debug)]
pub struct LocalTcpListener {
    inner: TcpListener,
    token: u64,
}

impl LocalTcpListener {
    /// Listen on `addr`; `opts` apply to every accepted connection.
    pub fn bind(addr: SocketAddr, opts: &TcpSocketOptions) -> io::Result<Self> {
        let (inner, token) = with_reactor(|r| Ok::<_, io::Error>((TcpListener::bind(&r.fs, addr, opts)?, r.token())))?;
        Ok(LocalTcpListener { inner, token })
    }

    /// Accept the next connection.
    pub async fn accept(&self) -> io::Result<(LocalTcpStream, SocketAddr)> {
        poll_fn(|cx| match self.inner.accept() {
            Ok((stream, peer)) => Poll::Ready(Ok((LocalTcpStream::new(stream), peer))),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                match with_reactor(|r| r.wait(self.token, &self.inner, Interest::READABLE, cx.waker())) {
                    Ok(()) => Poll::Pending,
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            Err(e) => Poll::Ready(Err(e)),
        })
        .await
    }

    /// The local address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

impl Drop for LocalTcpListener {
    fn drop(&mut self) {
        let _ = REACTOR.try_with(|r| r.borrow_mut().as_mut().map(|r| r.slots.remove(&self.token)));
    }
}

/// A TCP connection for [`run`]: reads and writes go straight to F-Stack.
/// `!Send`. Dropping it sends FIN and closes once the peer has acknowledged
/// everything written.
#[derive(Debug)]
pub struct LocalTcpStream {
    inner: Option<TcpStream>,
    token: u64,
}

impl LocalTcpStream {
    fn new(inner: TcpStream) -> Self {
        let token = with_reactor(|r| r.token());
        LocalTcpStream { inner: Some(inner), token }
    }

    fn stream(&self) -> &TcpStream {
        self.inner.as_ref().expect("stream present until drop")
    }

    /// Connect to `addr`.
    pub async fn connect(addr: SocketAddr, opts: &TcpSocketOptions) -> io::Result<Self> {
        let stream = with_reactor(|r| TcpStream::connect(&r.fs, addr, opts))?;
        let this = LocalTcpStream::new(stream);
        // FreeBSD reports a connecting socket writable only once the connect
        // has completed or failed; then SO_ERROR says which.
        let mut armed = false;
        poll_fn(|cx| {
            if !armed || with_reactor(|r| r.write_pending(this.token)) {
                armed = true;
                return match with_reactor(|r| r.wait(this.token, this.stream(), Interest::WRITABLE, cx.waker())) {
                    Ok(()) => Poll::Pending,
                    Err(e) => Poll::Ready(Err(e)),
                };
            }
            Poll::Ready(match this.stream().take_error() {
                Ok(None) => Ok(()),
                Ok(Some(e)) | Err(e) => Err(e),
            })
        })
        .await?;
        Ok(this)
    }

    /// The remote address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.stream().peer_addr()
    }

    /// The local address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.stream().local_addr()
    }

    /// Apply socket options to the connection.
    pub fn set_options(&self, opts: &TcpSocketOptions) -> io::Result<()> {
        self.stream().set_options(opts)
    }
}

impl AsyncRead for LocalTcpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // SAFETY: F-Stack only writes into the buffer; we mark exactly the
        // bytes it reported as initialised.
        let unfilled: &mut [MaybeUninit<u8>] = unsafe { buf.unfilled_mut() };
        match this.stream().read_uninit(unfilled) {
            Ok(n) => {
                unsafe { buf.assume_init(n) };
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                match with_reactor(|r| r.wait(this.token, this.stream(), Interest::READABLE, cx.waker())) {
                    Ok(()) => Poll::Pending,
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl AsyncWrite for LocalTcpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.stream().write(buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                match with_reactor(|r| r.wait(this.token, this.stream(), Interest::WRITABLE, cx.waker())) {
                    Ok(()) => Poll::Pending,
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// Writes go straight into F-Stack's send buffer, so there is nothing to
    /// flush (as with a kernel socket).
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.stream().shutdown(Shutdown::Write))
    }
}

impl Drop for LocalTcpStream {
    fn drop(&mut self) {
        if let Some(stream) = self.inner.take() {
            let token = self.token;
            // Outside `run` (e.g. dropped during teardown) there's nothing to
            // hand over to; the socket just closes.
            let _ = REACTOR.try_with(|r| {
                if let Some(r) = r.borrow_mut().as_mut() {
                    r.close_gracefully(token, stream);
                }
            });
        }
    }
}

/// A UDP socket for [`run`]. `!Send`.
#[derive(Debug)]
pub struct LocalUdpSocket {
    inner: UdpSocket,
    token: u64,
}

impl LocalUdpSocket {
    /// Bind to `addr`.
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let (inner, token) = with_reactor(|r| Ok::<_, io::Error>((UdpSocket::bind(&r.fs, addr)?, r.token())))?;
        Ok(LocalUdpSocket { inner, token })
    }

    /// Receive a datagram (truncated to `buf`'s length).
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| match self.inner.recv_from(buf) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                match with_reactor(|r| r.wait(self.token, &self.inner, Interest::READABLE, cx.waker())) {
                    Ok(()) => Poll::Pending,
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            other => Poll::Ready(other),
        })
        .await
    }

    /// Send a datagram to `addr`, waiting while F-Stack can't take it.
    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        poll_fn(|cx| match self.inner.send_to(buf, addr) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                match with_reactor(|r| r.wait(self.token, &self.inner, Interest::WRITABLE, cx.waker())) {
                    Ok(()) => Poll::Pending,
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            other => Poll::Ready(other),
        })
        .await
    }

    /// The local address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

impl Drop for LocalUdpSocket {
    fn drop(&mut self) {
        let _ = REACTOR.try_with(|r| r.borrow_mut().as_mut().map(|r| r.slots.remove(&self.token)));
    }
}

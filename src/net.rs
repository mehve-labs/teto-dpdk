//! Non-blocking F-Stack sockets.
//!
//! All sockets are non-blocking: operations that would block return
//! [`io::ErrorKind::WouldBlock`]. Use [`crate::event::Kqueue`] to learn when to
//! retry. Sockets close their F-Stack descriptor on drop. IPv4 and IPv6 are
//! supported (for IPv6, configure `addr6`/`prefix_len` for the port in
//! `config.ini`); an IPv6 socket carries IPv6 traffic only.

use std::io;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::net::{Shutdown, SocketAddr};

use crate::config::TcpSocketOptions;
use crate::runtime::{self, FStack};
use crate::sys::{cvt, cvt32, ffi};

const LISTEN_BACKLOG: i32 = 1024;

/// Owned F-Stack descriptor. `!Send`: F-Stack calls must stay on its thread.
#[derive(Debug)]
pub(crate) struct Fd {
    raw: i32,
    _not_send: PhantomData<*const ()>,
}

impl Fd {
    pub(crate) fn new(raw: i32) -> Self {
        Fd { raw, _not_send: PhantomData }
    }

    /// The descriptor, if F-Stack is still running.
    pub(crate) fn get(&self) -> io::Result<i32> {
        runtime::ensure_alive()?;
        Ok(self.raw)
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        // After the poll loop returns F-Stack has torn itself down; there is
        // nothing left to close.
        if runtime::is_alive() {
            let _ = ffi::sock_close(self.raw);
        }
    }
}

fn local_addr(fd: &Fd) -> io::Result<SocketAddr> {
    let mut out = ffi::SockAddr::default();
    cvt32(ffi::sock_local_addr(fd.get()?, &mut out))?;
    Ok(out.to_std())
}

fn bind(fd: &Fd, addr: SocketAddr) -> io::Result<()> {
    cvt32(ffi::sock_bind(fd.get()?, &ffi::SockAddr::from_std(addr))).map(drop)
}

fn set_opt(fd: &Fd, opt: ffi::SockOpt, value: i32) -> io::Result<()> {
    cvt32(ffi::sock_set_opt(fd.get()?, opt, value)).map(drop)
}

fn opt_i32(name: &str, v: u32) -> io::Result<i32> {
    i32::try_from(v).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{name} value {v} is too large"))
    })
}

fn apply_tcp_options(fd: &Fd, opts: &TcpSocketOptions) -> io::Result<()> {
    use ffi::SockOpt;
    if let Some(v) = opts.nodelay {
        set_opt(fd, SockOpt::NoDelay, v.into())?;
    }
    if let Some(v) = opts.keepalive {
        set_opt(fd, SockOpt::KeepAlive, v.into())?;
    }
    if let Some(v) = opts.keepalive_idle_secs {
        set_opt(fd, SockOpt::KeepIdle, opt_i32("keepalive_idle_secs", v)?)?;
    }
    if let Some(v) = opts.keepalive_interval_secs {
        set_opt(fd, SockOpt::KeepIntvl, opt_i32("keepalive_interval_secs", v)?)?;
    }
    if let Some(v) = opts.keepalive_count {
        set_opt(fd, SockOpt::KeepCnt, opt_i32("keepalive_count", v)?)?;
    }
    if let Some(v) = opts.recv_buf {
        set_opt(fd, SockOpt::RecvBuf, opt_i32("recv_buf", v)?)?;
    }
    if let Some(v) = opts.send_buf {
        set_opt(fd, SockOpt::SendBuf, opt_i32("send_buf", v)?)?;
    }
    if let Some(v) = opts.linger_secs {
        set_opt(fd, SockOpt::Linger, opt_i32("linger_secs", v)?)?;
    }
    Ok(())
}

fn nonblocking_socket(create: fn(bool) -> i32, v6: bool) -> io::Result<Fd> {
    runtime::ensure_alive()?;
    let fd = Fd::new(cvt32(create(v6))?);
    cvt32(ffi::sock_set_nonblocking(fd.get()?))?;
    Ok(fd)
}

/// A listening TCP socket.
#[derive(Debug)]
pub struct TcpListener {
    fd: Fd,
    opts: TcpSocketOptions,
}

impl TcpListener {
    /// Bind and listen on `addr`. `opts` are applied to every accepted
    /// connection; they are also applied to the listening socket first, so an
    /// option F-Stack rejects fails here rather than on every accept.
    pub fn bind(_fs: &FStack, addr: SocketAddr, opts: &TcpSocketOptions) -> io::Result<Self> {
        let fd = nonblocking_socket(ffi::sock_tcp, addr.is_ipv6())?;
        set_opt(&fd, ffi::SockOpt::ReuseAddr, 1)?;
        if opts.reuse_port == Some(true) {
            set_opt(&fd, ffi::SockOpt::ReusePort, 1)?;
        }
        apply_tcp_options(&fd, opts)?;
        bind(&fd, addr)?;
        cvt32(ffi::sock_listen(fd.get()?, LISTEN_BACKLOG))?;
        Ok(TcpListener { fd, opts: opts.clone() })
    }

    /// Accept one pending connection.
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when none is pending. If applying
    /// the socket options to the new connection fails, the connection is
    /// closed and the error returned.
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let mut peer = ffi::SockAddr::default();
        let fd = Fd::new(cvt32(ffi::sock_accept(self.fd.get()?, &mut peer))?);
        cvt32(ffi::sock_set_nonblocking(fd.get()?))?;
        apply_tcp_options(&fd, &self.opts)?;
        let peer = peer.to_std();
        Ok((TcpStream { fd, peer }, peer))
    }

    /// The local address (resolves a port-0 bind to the assigned port).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        local_addr(&self.fd)
    }
}

/// A connected TCP socket.
#[derive(Debug)]
pub struct TcpStream {
    fd: Fd,
    peer: SocketAddr,
}

/// `EINPROGRESS` (Linux numbering, as F-Stack reports it).
const EINPROGRESS: i32 = 115;

impl TcpStream {
    /// Start connecting to `addr`. Returns immediately with the connection in
    /// progress: wait for it to become writable (kqueue `WRITABLE`), then call
    /// [`take_error`](Self::take_error) — `Ok(None)` means it connected.
    /// `opts` are applied before connecting.
    pub fn connect(fs: &FStack, addr: SocketAddr, opts: &TcpSocketOptions) -> io::Result<Self> {
        Self::start_connect(fs, None, addr, opts)
    }

    /// Like [`connect`](Self::connect), from the local address `local`
    /// (port 0 picks a port). `local` and `addr` must be the same family.
    /// With several F-Stack processes, the port isn't chosen to match this
    /// process's RSS queue (see `docs/architecture.md`).
    pub fn connect_from(
        fs: &FStack,
        local: SocketAddr,
        addr: SocketAddr,
        opts: &TcpSocketOptions,
    ) -> io::Result<Self> {
        Self::start_connect(fs, Some(local), addr, opts)
    }

    fn start_connect(
        _fs: &FStack,
        local: Option<SocketAddr>,
        addr: SocketAddr,
        opts: &TcpSocketOptions,
    ) -> io::Result<Self> {
        if local.is_some_and(|l| l.is_ipv6() != addr.is_ipv6()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local and remote addresses must be the same family",
            ));
        }
        let fd = nonblocking_socket(ffi::sock_tcp, addr.is_ipv6())?;
        apply_tcp_options(&fd, opts)?;
        if let Some(local) = local {
            if opts.reuse_port == Some(true) {
                set_opt(&fd, ffi::SockOpt::ReusePort, 1)?;
            }
            bind(&fd, local)?;
        }
        match cvt32(ffi::sock_connect(fd.get()?, &ffi::SockAddr::from_std(addr))) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(EINPROGRESS) => {}
            Err(e) => return Err(e),
        }
        Ok(TcpStream { fd, peer: addr })
    }

    /// Apply socket options to the connection now (`TCP_NODELAY`, keepalive,
    /// buffer sizes, ...). Unset fields are left as they are.
    pub fn set_options(&self, opts: &TcpSocketOptions) -> io::Result<()> {
        apply_tcp_options(&self.fd, opts)
    }

    /// Take the socket's pending error (`SO_ERROR`), e.g. why a connect
    /// failed. `Ok(None)` if there is none.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        let code = cvt32(ffi::sock_take_error(self.fd.get()?))?;
        Ok((code != 0).then(|| io::Error::from_raw_os_error(code)))
    }

    /// Read into `buf`. `Ok(0)` means the peer shut down its write side.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
        let n = cvt(unsafe { ffi::sock_read(self.fd.get()?, buf.as_mut_ptr(), buf.len()) })?;
        Ok(n as usize)
    }

    /// Like [`read`](Self::read), into possibly uninitialised memory. On
    /// success the first `n` bytes of `buf` are initialised.
    pub fn read_uninit(&self, buf: &mut [MaybeUninit<u8>]) -> io::Result<usize> {
        // SAFETY: F-Stack only writes into the buffer, never reads from it.
        let n = cvt(unsafe { ffi::sock_read(self.fd.get()?, buf.as_mut_ptr().cast(), buf.len()) })?;
        Ok(n as usize)
    }

    /// Write as much of `buf` as the send buffer accepts; may be less than
    /// `buf.len()`.
    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        Ok(cvt(ffi::sock_write(self.fd.get()?, buf))? as usize)
    }

    /// Shut down the read side, the write side (sends FIN) or both.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Read => 0,
            Shutdown::Write => 1,
            Shutdown::Both => 2,
        };
        cvt32(ffi::sock_shutdown(self.fd.get()?, how)).map(drop)
    }

    /// Close the connection abortively: the peer gets RST and anything still
    /// unsent or unacknowledged is discarded.
    pub fn abort(self) {
        if let Ok(fd) = self.fd.get() {
            let _ = ffi::sock_set_opt(fd, ffi::SockOpt::Linger, 0);
        }
    }

    /// Bytes written but not yet acknowledged by the peer (FreeBSD
    /// `FIONWRITE`). Zero after a write shutdown means everything, up to
    /// the FIN, has been delivered.
    pub fn unsent_bytes(&self) -> io::Result<usize> {
        Ok(cvt32(ffi::sock_unsent(self.fd.get()?))? as usize)
    }

    /// The remote address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// The local address (resolves a port-0 bind to the assigned port).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        local_addr(&self.fd)
    }
}

/// A UDP socket.
#[derive(Debug)]
pub struct UdpSocket {
    fd: Fd,
}

impl UdpSocket {
    /// Bind a non-blocking UDP socket to `addr`.
    pub fn bind(_fs: &FStack, addr: SocketAddr) -> io::Result<Self> {
        let fd = nonblocking_socket(ffi::sock_udp, addr.is_ipv6())?;
        bind(&fd, addr)?;
        Ok(UdpSocket { fd })
    }

    /// Receive one datagram. If it is larger than `buf`, the excess is
    /// discarded.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        // SAFETY: an initialised slice is a valid `MaybeUninit` slice.
        let buf = unsafe { &mut *(buf as *mut [u8] as *mut [MaybeUninit<u8>]) };
        self.recv_from_uninit(buf)
    }

    /// Like [`recv_from`](Self::recv_from), into possibly uninitialised memory.
    pub fn recv_from_uninit(
        &self,
        buf: &mut [MaybeUninit<u8>],
    ) -> io::Result<(usize, SocketAddr)> {
        let mut from = ffi::SockAddr::default();
        // SAFETY: F-Stack only writes into the buffer, never reads from it.
        let n = cvt(unsafe { ffi::sock_recvfrom(self.fd.get()?, buf.as_mut_ptr().cast(), buf.len(), &mut from) })?;
        Ok((n as usize, from.to_std()))
    }

    /// Send one datagram to `addr`. Returns [`io::ErrorKind::WouldBlock`] if
    /// F-Stack can't take it right now.
    pub fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        Ok(cvt(ffi::sock_sendto(self.fd.get()?, buf, &ffi::SockAddr::from_std(addr)))? as usize)
    }

    /// The local address (resolves a port-0 bind to the assigned port).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        local_addr(&self.fd)
    }
}

impl crate::event::sealed::Sealed for TcpListener {
    fn fd(&self) -> io::Result<i32> {
        self.fd.get()
    }
}
impl crate::event::sealed::Sealed for TcpStream {
    fn fd(&self) -> io::Result<i32> {
        self.fd.get()
    }
}
impl crate::event::sealed::Sealed for UdpSocket {
    fn fd(&self) -> io::Result<i32> {
        self.fd.get()
    }
}
impl crate::event::Source for TcpListener {}
impl crate::event::Source for TcpStream {}
impl crate::event::Source for UdpSocket {}

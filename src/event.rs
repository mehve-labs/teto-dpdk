//! Readiness notification via F-Stack's kqueue.
//!
//! Registrations are level-triggered: an event keeps being reported on every
//! [`Kqueue::poll`] while the condition holds. Each registration carries a
//! caller-chosen `u64` token that is returned with its events, so callers can
//! use tokens that are never reused (unlike F-Stack descriptor numbers).

use std::io;
use std::ops::BitOr;

use crate::net::Fd;
use crate::runtime::FStack;
use crate::sys::{cvt32, ffi};

// Values from F-Stack's ff_event.h (FreeBSD numbering).
const EVFILT_READ: i16 = -1;
const EVFILT_WRITE: i16 = -2;
const EV_ADD: u16 = 0x0001;
const EV_DELETE: u16 = 0x0002;
const EV_ERROR: u16 = 0x4000;
const EV_EOF: u16 = 0x8000;
const ENOENT: i32 = 2;

pub(crate) mod sealed {
    pub trait Sealed {
        fn fd(&self) -> std::io::Result<i32>;
    }
}

/// A socket that can be registered with a [`Kqueue`].
pub trait Source: sealed::Sealed {}

/// The readiness a registration asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Interest(u8);

impl Interest {
    /// No interest; registering with it removes all filters.
    pub const NONE: Interest = Interest(0);
    pub const READABLE: Interest = Interest(1);
    pub const WRITABLE: Interest = Interest(2);

    pub fn is_readable(self) -> bool {
        self.0 & 1 != 0
    }

    pub fn is_writable(self) -> bool {
        self.0 & 2 != 0
    }

    pub fn is_none(self) -> bool {
        self.0 == 0
    }

    pub fn with(self, other: Interest) -> Interest {
        Interest(self.0 | other.0)
    }

    pub fn without(self, other: Interest) -> Interest {
        Interest(self.0 & !other.0)
    }
}

impl BitOr for Interest {
    type Output = Interest;
    fn bitor(self, rhs: Interest) -> Interest {
        self.with(rhs)
    }
}

/// One readiness event returned by [`Kqueue::poll`].
#[derive(Clone, Copy, Debug)]
pub struct Event {
    token: u64,
    readable: bool,
    writable: bool,
    eof: bool,
    error: bool,
}

impl Event {
    pub fn token(&self) -> u64 {
        self.token
    }

    /// Data (or EOF) can be read, or a listener has a pending connection.
    pub fn is_readable(&self) -> bool {
        self.readable
    }

    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// The peer closed this direction, or the connection failed. The next
    /// read/write call reports which.
    pub fn is_eof(&self) -> bool {
        self.eof
    }

    /// The kqueue could not report on this registration. Retry the socket
    /// operation to obtain the underlying error.
    pub fn is_error(&self) -> bool {
        self.error
    }
}

/// Buffer of events filled by [`Kqueue::poll`].
#[derive(Debug)]
pub struct Events {
    buf: Vec<ffi::KEvent>,
    len: usize,
}

impl Events {
    pub fn with_capacity(capacity: usize) -> Self {
        Events { buf: vec![ffi::KEvent::default(); capacity.max(1)], len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        self.buf[..self.len].iter().map(|k| Event {
            token: k.udata,
            readable: k.filter == EVFILT_READ,
            writable: k.filter == EVFILT_WRITE,
            eof: k.flags & EV_EOF != 0,
            error: k.flags & EV_ERROR != 0,
        })
    }
}

/// An F-Stack kqueue.
#[derive(Debug)]
pub struct Kqueue {
    fd: Fd,
}

impl Kqueue {
    pub fn new(_fs: &FStack) -> io::Result<Self> {
        crate::runtime::ensure_alive()?;
        let raw = cvt32(ffi::kq_create())?;
        Ok(Kqueue { fd: Fd::new(raw) })
    }

    /// Set the interest for `source` to exactly `interest`, tagged with
    /// `token`. Filters not in `interest` are removed, so this both registers
    /// and re-registers. Closing a socket removes its registrations.
    pub fn register(&self, source: &impl Source, token: u64, interest: Interest) -> io::Result<()> {
        let fd = source.fd()?;
        self.set_filter(fd, EVFILT_READ, token, interest.is_readable())?;
        self.set_filter(fd, EVFILT_WRITE, token, interest.is_writable())
    }

    /// Remove all interest for `source`.
    pub fn deregister(&self, source: &impl Source) -> io::Result<()> {
        self.register(source, 0, Interest::NONE)
    }

    fn set_filter(&self, fd: i32, filter: i16, token: u64, on: bool) -> io::Result<()> {
        let change = ffi::KEvent {
            ident: fd as u64,
            filter,
            flags: if on { EV_ADD } else { EV_DELETE },
            fflags: 0,
            data: 0,
            udata: token,
        };
        match cvt32(ffi::kq_change(self.fd.get()?, &change)) {
            Ok(_) => Ok(()),
            Err(e) if !on && e.raw_os_error() == Some(ENOENT) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Collect ready events without blocking. Returns the number of events.
    pub fn poll(&self, events: &mut Events) -> io::Result<usize> {
        events.len = 0;
        let n = cvt32(ffi::kq_poll(self.fd.get()?, &mut events.buf))? as usize;
        events.len = n;
        Ok(n)
    }
}

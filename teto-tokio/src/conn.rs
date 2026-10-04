//! State shared between a `TetoTcpStream` (tokio side) and the F-Stack driver
//! thread. Each connection has a never-reused `u64` id; the F-Stack descriptor
//! number never leaves the driver, so a recycled fd can't be confused with an
//! old connection.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Waker;

use bytes::BytesMut;

/// Stop reading from F-Stack once this many received bytes are unconsumed.
pub(crate) const RX_HIGH: usize = 256 * 1024;
/// Resume reading once the consumer has brought the backlog below this.
pub(crate) const RX_LOW: usize = RX_HIGH / 2;
/// `poll_write` returns `Pending` while this many bytes are queued.
pub(crate) const TX_LIMIT: usize = 256 * 1024;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Queue of connection ids the tokio side wants the driver to look at.
#[derive(Default)]
pub(crate) struct Notifier {
    queue: Mutex<Vec<u64>>,
    pending: AtomicBool,
}

impl Notifier {
    fn push(&self, id: u64) {
        lock(&self.queue).push(id);
        self.pending.store(true, Ordering::Release);
    }

    /// Move all queued ids into `out` (driver side).
    pub(crate) fn drain_into(&self, out: &mut Vec<u64>) {
        if self.pending.swap(false, Ordering::Acquire) {
            out.append(&mut lock(&self.queue));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum WriteShutdown {
    #[default]
    Open,
    Requested,
    Done,
}

/// Why a connection can no longer be used.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ConnError {
    Os(i32),
    Kind(io::ErrorKind),
    RuntimeGone,
}

impl ConnError {
    pub(crate) fn from_io(e: &io::Error) -> Self {
        match e.raw_os_error() {
            Some(code) => ConnError::Os(code),
            None => ConnError::Kind(e.kind()),
        }
    }

    pub(crate) fn to_io(self) -> io::Error {
        match self {
            ConnError::Os(code) => io::Error::from_raw_os_error(code),
            ConnError::Kind(kind) => io::Error::from(kind),
            ConnError::RuntimeGone => {
                io::Error::new(io::ErrorKind::BrokenPipe, "F-Stack runtime stopped")
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct ConnState {
    /// Received, not yet consumed by the stream.
    pub rx: BytesMut,
    /// Peer shut down its write side; delivered after `rx` drains.
    pub rx_eof: bool,
    /// Driver stopped reading because `rx` hit `RX_HIGH`.
    pub rx_paused: bool,
    /// Written by the stream, not yet accepted by F-Stack.
    pub tx: BytesMut,
    pub wr_shutdown: WriteShutdown,
    /// The `TetoTcpStream` was dropped; driver closes after `tx` drains.
    pub dropped: bool,
    pub error: Option<ConnError>,
    /// Already queued in the `Notifier`; avoids duplicate entries.
    pub notified: bool,
    pub read_waker: Option<Waker>,
    pub write_waker: Option<Waker>,
}

pub(crate) struct Conn {
    pub id: u64,
    notifier: Arc<Notifier>,
    pub state: Mutex<ConnState>,
}

impl Conn {
    pub(crate) fn new(id: u64, notifier: Arc<Notifier>) -> Self {
        Conn { id, notifier, state: Mutex::new(ConnState::default()) }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, ConnState> {
        lock(&self.state)
    }

    /// Ask the driver to service this connection. Call with the state locked.
    pub(crate) fn notify(&self, st: &mut ConnState) {
        if !st.notified {
            st.notified = true;
            self.notifier.push(self.id);
        }
    }
}

/// Wakers collected under the lock and fired after it is released.
#[derive(Default)]
pub(crate) struct Wakes(Vec<Waker>);

impl Wakes {
    pub(crate) fn read(&mut self, st: &mut ConnState) {
        self.0.extend(st.read_waker.take());
    }

    pub(crate) fn write(&mut self, st: &mut ConnState) {
        self.0.extend(st.write_waker.take());
    }

    pub(crate) fn fire(self) {
        for w in self.0 {
            w.wake();
        }
    }
}

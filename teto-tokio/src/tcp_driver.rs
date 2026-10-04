//! Runs on the F-Stack thread: accepts connections, moves bytes between
//! F-Stack sockets and the per-connection buffers in `conn.rs`, and wakes tokio
//! tasks.

use std::collections::HashMap;
use std::io;
use std::net::{Shutdown, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Buf;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use teto_dpdk::event::{Event, Events, Interest, Kqueue};
use teto_dpdk::net::{TcpListener, TcpStream};
use teto_dpdk::{FStack, TcpSocketOptions};

use crate::conn::{Conn, ConnError, ConnState, Notifier, Wakes, WriteShutdown, RX_HIGH, RX_LOW, TX_LIMIT};
use crate::tcp_stream::TetoTcpStream;

pub(crate) type AcceptItem = io::Result<(TetoTcpStream, SocketAddr)>;

const LISTENER_TOKEN: u64 = 0;
const READ_CHUNK: usize = 64 * 1024;
const EVENTS_CAPACITY: usize = 1024;
/// How long a dropped stream may take to get its data acknowledged by the
/// peer before it is closed regardless.
const DROP_GRACE: Duration = Duration::from_secs(30);
const DEADLINE_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// How long the loop keeps running after the last socket is gone, so final
/// FINs/ACKs (and anything F-Stack is batching) actually leave: stopping the
/// loop tears F-Stack down and discards whatever it still holds.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(1);

struct Entry {
    stream: TcpStream,
    conn: Arc<Conn>,
    interest: Interest,
}

enum Outcome {
    Keep,
    Close(Option<ConnError>),
}

pub(crate) struct TcpDriver {
    kq: Kqueue,
    /// `None` once the application dropped the `TetoTcpListener`; the
    /// listening socket is then closed so new clients are refused.
    listener: Option<TcpListener>,
    local_addr: SocketAddr,
    accept_tx: mpsc::Sender<AcceptItem>,
    accept_paused: bool,
    conns: HashMap<u64, Entry>,
    next_id: u64,
    notifier: Arc<Notifier>,
    notified: Vec<u64>,
    events: Events,
    ready: Vec<Event>,
    /// Dropped streams still delivering their data, with the deadline after
    /// which they are aborted.
    draining: HashMap<u64, Instant>,
    next_deadline_check: Instant,
    idle_since: Option<Instant>,
}

impl TcpDriver {
    pub(crate) fn new(
        fs: &FStack,
        addr: SocketAddr,
        opts: &TcpSocketOptions,
        accept_tx: mpsc::Sender<AcceptItem>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(fs, addr, opts)?;
        let local_addr = listener.local_addr()?.into();
        let kq = Kqueue::new(fs)?;
        kq.register(&listener, LISTENER_TOKEN, Interest::READABLE)?;
        Ok(TcpDriver {
            kq,
            listener: Some(listener),
            local_addr,
            accept_tx,
            accept_paused: false,
            conns: HashMap::new(),
            next_id: LISTENER_TOKEN + 1,
            notifier: Arc::new(Notifier::default()),
            notified: Vec::new(),
            events: Events::with_capacity(EVENTS_CAPACITY),
            ready: Vec::with_capacity(EVENTS_CAPACITY),
            draining: HashMap::new(),
            next_deadline_check: Instant::now(),
            idle_since: None,
        })
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// One poll-loop iteration. Never blocks. Returns `false` once nothing
    /// can use the runtime any more (listener dropped, no connections left)
    /// or it failed; the caller then stops the loop.
    pub(crate) fn tick(&mut self) -> bool {
        let mut notified = std::mem::take(&mut self.notified);
        self.notifier.drain_into(&mut notified);
        for id in notified.drain(..) {
            self.service(id);
        }
        self.notified = notified;

        // The application dropped the listener: close the socket so new
        // clients are refused instead of queued forever.
        if self.listener.is_some() && self.accept_tx.is_closed() {
            self.listener = None;
            self.drop_unaccepted();
        }

        // Resume accepting once the application has drained the accept queue.
        if self.accept_paused
            && self.accept_tx.capacity() == self.accept_tx.max_capacity()
            && let Some(listener) = &self.listener
            && self.kq.register(listener, LISTENER_TOKEN, Interest::READABLE).is_ok()
        {
            self.accept_paused = false;
        }

        if let Err(e) = self.kq.poll(&mut self.events) {
            // Can't happen while F-Stack runs; if it does, fail every stream
            // rather than leave them waiting.
            self.fail_all(ConnError::from_io(&e));
            return false;
        }
        let mut ready = std::mem::take(&mut self.ready);
        ready.extend(self.events.iter());
        for ev in ready.drain(..) {
            if ev.token() == LISTENER_TOKEN {
                self.accept_ready();
                continue;
            }
            if ev.is_send_empty() {
                // A dropped stream's data (and FIN) has been acknowledged.
                self.close(ev.token(), None);
                continue;
            }
            if ev.is_readable() {
                self.on_readable(ev.token());
            }
            if ev.is_writable() {
                self.on_writable(ev.token());
            }
        }
        self.ready = ready;

        if !self.draining.is_empty() && Instant::now() >= self.next_deadline_check {
            self.next_deadline_check = Instant::now() + DEADLINE_CHECK_INTERVAL;
            self.abort_overdue();
        }

        if self.listener.is_some() || !self.conns.is_empty() {
            self.idle_since = None;
            return true;
        }
        let idle_since = *self.idle_since.get_or_insert_with(Instant::now);
        idle_since.elapsed() < STOP_GRACE
    }

    /// With the listener gone nobody can accept these any more (normally
    /// they were dropped with the channel; this catches one pushed into it
    /// just as the receiver was dropped).
    fn drop_unaccepted(&mut self) {
        let ids: Vec<u64> = self.conns.keys().copied().collect();
        for id in ids {
            let Some(entry) = self.conns.get(&id) else { continue };
            let mut st = entry.conn.lock();
            if !st.accepted && !st.dropped {
                st.dropped = true;
                st.notified = false;
                drop(st);
                self.service(id);
            }
        }
    }

    fn accept_ready(&mut self) {
        let Some(listener) = &self.listener else { return };
        // `Some(closed)`: stop accepting for now (`closed` = for good).
        let stop = loop {
            let permit = match self.accept_tx.try_reserve() {
                Ok(p) => p,
                Err(TrySendError::Full(())) => break Some(false),
                Err(TrySendError::Closed(())) => break Some(true),
            };
            match listener.accept() {
                Ok((stream, peer)) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    // Not registered until the application accepts it (see
                    // `desired_interest`), so queued connections don't buffer.
                    let conn = Arc::new(Conn::new(id, self.notifier.clone()));
                    let peer = SocketAddr::V4(peer);
                    let teto = TetoTcpStream::new(conn.clone(), peer, self.local_addr);
                    self.conns.insert(id, Entry { stream, conn, interest: Interest::NONE });
                    permit.send(Ok((teto, peer)));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break None,
                // The client went away before or during setup (the failed
                // connection is already closed): nothing to report.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(e) => {
                    // Persistent errors (e.g. descriptor exhaustion) would
                    // otherwise repeat every tick: report once, then back off
                    // until the application has drained the accept queue.
                    permit.send(Err(e));
                    break Some(false);
                }
            }
        };
        match stop {
            Some(true) => {
                // The application dropped the listener: close the socket so
                // new clients are refused instead of queued forever.
                self.listener = None;
                self.drop_unaccepted();
            }
            Some(false) => {
                let _ = self.kq.deregister(listener);
                self.accept_paused = true;
            }
            None => {}
        }
    }

    fn service(&mut self, id: u64) {
        let Some(entry) = self.conns.get_mut(&id) else { return };
        let conn = entry.conn.clone();
        let mut st = conn.lock();
        st.notified = false;
        if st.dropped {
            st.rx.clear();
            st.rx_paused = false;
            self.draining.entry(id).or_insert_with(|| Instant::now() + DROP_GRACE);
        } else if st.rx_paused && st.rx.len() < RX_LOW {
            st.rx_paused = false;
        }
        let mut wakes = Wakes::default();
        let outcome = flush(&self.kq, entry, &mut st, &mut wakes);
        drop(st);
        wakes.fire();
        self.finish(id, outcome);
    }

    fn on_readable(&mut self, id: u64) {
        let Some(entry) = self.conns.get_mut(&id) else { return };
        let conn = entry.conn.clone();
        let mut st = conn.lock();
        let mut wakes = Wakes::default();
        let outcome = read_ready(&self.kq, entry, &mut st, &mut wakes);
        drop(st);
        wakes.fire();
        self.finish(id, outcome);
    }

    fn on_writable(&mut self, id: u64) {
        let Some(entry) = self.conns.get_mut(&id) else { return };
        let conn = entry.conn.clone();
        let mut st = conn.lock();
        let mut wakes = Wakes::default();
        let outcome = flush(&self.kq, entry, &mut st, &mut wakes);
        drop(st);
        wakes.fire();
        self.finish(id, outcome);
    }

    fn finish(&mut self, id: u64, outcome: Outcome) {
        if let Outcome::Close(err) = outcome {
            self.close(id, err);
        }
    }

    /// Remove a connection; dropping its `TcpStream` closes the descriptor and
    /// its kqueue registrations.
    fn close(&mut self, id: u64, err: Option<ConnError>) {
        let Some(entry) = self.conns.remove(&id) else { return };
        self.draining.remove(&id);
        let mut st = entry.conn.lock();
        if let Some(e) = err {
            st.error.get_or_insert(e);
        }
        st.tx.clear();
        let mut wakes = Wakes::default();
        wakes.read(&mut st);
        wakes.write(&mut st);
        drop(st);
        wakes.fire();
    }

    fn fail_all(&mut self, err: ConnError) {
        let ids: Vec<u64> = self.conns.keys().copied().collect();
        for id in ids {
            self.close(id, Some(err));
        }
    }

    /// Abort dropped streams whose peer hasn't taken their data within
    /// `DROP_GRACE`, so it sees a reset instead of a silent stall.
    fn abort_overdue(&mut self) {
        let now = Instant::now();
        let overdue: Vec<u64> =
            self.draining.iter().filter(|(_, deadline)| now >= **deadline).map(|(id, _)| *id).collect();
        for id in overdue {
            self.draining.remove(&id);
            if let Some(entry) = self.conns.remove(&id) {
                entry.stream.abort();
            }
        }
    }
}

impl Drop for TcpDriver {
    fn drop(&mut self) {
        // The poll loop is gone (normally only on panic): fail every stream
        // instead of leaving its tasks waiting forever.
        for entry in self.conns.values() {
            let mut st = entry.conn.lock();
            st.error.get_or_insert(ConnError::RuntimeGone);
            let mut wakes = Wakes::default();
            wakes.read(&mut st);
            wakes.write(&mut st);
            drop(st);
            wakes.fire();
        }
    }
}

/// The interest a connection needs given its state.
fn desired_interest(st: &ConnState) -> Interest {
    let mut i = Interest::NONE;
    // Connections waiting in the accept queue aren't read yet. A dropped
    // stream keeps reading (and discarding) so the socket closes with an
    // empty receive buffer, i.e. with FIN rather than RST.
    if !st.rx_eof && (st.dropped || (st.accepted && !st.rx_paused)) {
        i = i.with(Interest::READABLE);
    }
    if !st.tx.is_empty() {
        i = i.with(Interest::WRITABLE);
    }
    // A dropped stream that has sent FIN closes once everything is acked.
    if st.dropped && st.tx.is_empty() && st.wr_shutdown == WriteShutdown::Done {
        i = i.with(Interest::SEND_EMPTY);
    }
    i
}

fn sync_interest(kq: &Kqueue, entry: &mut Entry, st: &ConnState) -> Outcome {
    let want = desired_interest(st);
    if want != entry.interest {
        if let Err(e) = kq.register(&entry.stream, entry.conn.id, want) {
            return Outcome::Close(Some(ConnError::from_io(&e)));
        }
        entry.interest = want;
    }
    Outcome::Keep
}

fn read_ready(kq: &Kqueue, entry: &mut Entry, st: &mut ConnState, wakes: &mut Wakes) -> Outcome {
    if st.dropped {
        if discard_input(entry, st).is_err() {
            return Outcome::Close(None);
        }
        return sync_interest(kq, entry, st);
    }

    let mut got = false;
    loop {
        if st.rx.len() >= RX_HIGH {
            st.rx_paused = true;
            break;
        }
        let want = READ_CHUNK.min(RX_HIGH - st.rx.len());
        st.rx.reserve(want);
        let spare = &mut st.rx.spare_capacity_mut()[..want];
        match entry.stream.read_uninit(spare) {
            Ok(0) => {
                st.rx_eof = true;
                got = true;
                break;
            }
            Ok(n) => {
                // SAFETY: F-Stack initialised the first `n` spare bytes.
                unsafe { st.rx.set_len(st.rx.len() + n) };
                got = true;
                if n < want {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => {
                wakes.read(st);
                wakes.write(st);
                return Outcome::Close(Some(ConnError::from_io(&e)));
            }
        }
    }
    if got {
        wakes.read(st);
    }
    sync_interest(kq, entry, st)
}

/// Hand queued bytes to F-Stack, perform a requested write shutdown, and close
/// a dropped stream once everything is written.
fn flush(kq: &Kqueue, entry: &mut Entry, st: &mut ConnState, wakes: &mut Wakes) -> Outcome {
    let before = st.tx.len();
    while !st.tx.is_empty() {
        match entry.stream.write(&st.tx) {
            // FreeBSD never accepts 0 bytes of a non-empty write; treat it
            // like EAGAIN rather than spin.
            Ok(0) => break,
            Ok(n) => st.tx.advance(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => {
                wakes.read(st);
                wakes.write(st);
                return Outcome::Close(Some(ConnError::from_io(&e)));
            }
        }
    }
    if st.tx.len() < before && st.tx.len() < TX_LIMIT {
        wakes.write(st);
    }

    if st.tx.is_empty() {
        if st.dropped {
            return finish_dropped(kq, entry, st);
        }
        if st.wr_shutdown == WriteShutdown::Requested {
            match entry.stream.shutdown(Shutdown::Write) {
                Ok(()) => {}
                Err(e) => {
                    wakes.read(st);
                    wakes.write(st);
                    return Outcome::Close(Some(ConnError::from_io(&e)));
                }
            }
            st.wr_shutdown = WriteShutdown::Done;
            wakes.write(st);
        }
    }
    sync_interest(kq, entry, st)
}

/// Read and throw away whatever the peer sent, so the eventual close sends
/// FIN rather than RST. `Err` means the connection is gone.
fn discard_input(entry: &Entry, st: &mut ConnState) -> Result<(), ()> {
    let mut scratch = [0u8; 4096];
    while !st.rx_eof {
        match entry.stream.read(&mut scratch) {
            Ok(0) => st.rx_eof = true,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

fn fully_acked(entry: &Entry) -> bool {
    // An error means the connection is already gone: nothing left to wait for.
    entry.stream.unsent_bytes().map_or(true, |n| n == 0)
}

/// A dropped stream whose queued bytes are all with F-Stack: send FIN, and
/// close once the peer has acknowledged everything (signalled by the
/// SEND_EMPTY event), or abort after `DROP_GRACE`.
fn finish_dropped(kq: &Kqueue, entry: &mut Entry, st: &mut ConnState) -> Outcome {
    if st.wr_shutdown != WriteShutdown::Done {
        if entry.stream.shutdown(Shutdown::Write).is_err() {
            return Outcome::Close(None);
        }
        st.wr_shutdown = WriteShutdown::Done;
    }
    if discard_input(entry, st).is_err() || fully_acked(entry) {
        return Outcome::Close(None);
    }
    sync_interest(kq, entry, st)
}

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
/// How long a dropped stream may take to hand its buffered writes to F-Stack.
const DROP_GRACE: Duration = Duration::from_secs(30);

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
    listener: TcpListener,
    local_addr: SocketAddr,
    accept_tx: mpsc::Sender<AcceptItem>,
    accept_paused: bool,
    accept_closed: bool,
    conns: HashMap<u64, Entry>,
    next_id: u64,
    notifier: Arc<Notifier>,
    notified: Vec<u64>,
    events: Events,
    ready: Vec<Event>,
    draining: Vec<(u64, Instant)>,
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
            listener,
            local_addr,
            accept_tx,
            accept_paused: false,
            accept_closed: false,
            conns: HashMap::new(),
            next_id: LISTENER_TOKEN + 1,
            notifier: Arc::new(Notifier::default()),
            notified: Vec::new(),
            events: Events::with_capacity(EVENTS_CAPACITY),
            ready: Vec::with_capacity(EVENTS_CAPACITY),
            draining: Vec::new(),
        })
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// One poll-loop iteration. Never blocks.
    pub(crate) fn tick(&mut self) {
        let mut notified = std::mem::take(&mut self.notified);
        self.notifier.drain_into(&mut notified);
        for id in notified.drain(..) {
            self.service(id);
        }
        self.notified = notified;

        // Resume accepting once the application has drained the accept queue.
        if self.accept_paused
            && !self.accept_closed
            && self.accept_tx.capacity() == self.accept_tx.max_capacity()
            && self.kq.register(&self.listener, LISTENER_TOKEN, Interest::READABLE).is_ok()
        {
            self.accept_paused = false;
        }

        if self.kq.poll(&mut self.events).is_err() {
            return;
        }
        let mut ready = std::mem::take(&mut self.ready);
        ready.extend(self.events.iter());
        for ev in ready.drain(..) {
            if ev.token() == LISTENER_TOKEN {
                self.accept_ready();
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

        if !self.draining.is_empty() {
            self.expire_draining();
        }
    }

    fn accept_ready(&mut self) {
        // `Some(closed)`: stop accepting for now (`closed` = for good).
        let stop = loop {
            let permit = match self.accept_tx.try_reserve() {
                Ok(p) => p,
                Err(TrySendError::Full(())) => break Some(false),
                Err(TrySendError::Closed(())) => break Some(true),
            };
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    if let Err(e) = self.kq.register(&stream, id, Interest::READABLE) {
                        permit.send(Err(e));
                        continue;
                    }
                    let conn = Arc::new(Conn::new(id, self.notifier.clone()));
                    let peer = SocketAddr::V4(peer);
                    let teto = TetoTcpStream::new(conn.clone(), peer, self.local_addr);
                    self.conns.insert(id, Entry { stream, conn, interest: Interest::READABLE });
                    permit.send(Ok((teto, peer)));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break None,
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => continue,
                Err(e) => {
                    // Persistent errors (e.g. descriptor exhaustion) would
                    // otherwise repeat every tick: report once, then back off
                    // until the application has drained the accept queue.
                    permit.send(Err(e));
                    break Some(false);
                }
            }
        };
        if let Some(closed) = stop {
            let _ = self.kq.deregister(&self.listener);
            self.accept_paused = true;
            self.accept_closed |= closed;
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
            if !self.draining.iter().any(|(d, _)| *d == id) {
                self.draining.push((id, Instant::now()));
            }
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
        self.draining.retain(|(d, _)| *d != id);
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

    fn expire_draining(&mut self) {
        let now = Instant::now();
        let expired: Vec<u64> = self
            .draining
            .iter()
            .filter(|(_, since)| now.duration_since(*since) >= DROP_GRACE)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.close(id, None);
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
    // A dropped stream keeps reading (and discarding) so the socket closes
    // with an empty receive buffer, i.e. with FIN rather than RST.
    if !st.rx_eof && (st.dropped || !st.rx_paused) {
        i = i.with(Interest::READABLE);
    }
    if !st.tx.is_empty() {
        i = i.with(Interest::WRITABLE);
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
        let mut scratch = [0u8; 4096];
        loop {
            match entry.stream.read(&mut scratch) {
                Ok(0) => {
                    st.rx_eof = true;
                    break;
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => return Outcome::Close(None),
            }
        }
        if st.tx.is_empty() {
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
            return Outcome::Close(None);
        }
        if st.wr_shutdown == WriteShutdown::Requested {
            match entry.stream.shutdown(Shutdown::Write) {
                Ok(()) => {}
                // Already disconnected: nothing left to shut down.
                Err(e) if e.kind() == io::ErrorKind::NotConnected => {}
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

//! Runs on the F-Stack thread: executes commands from `TetoRuntime` handles
//! (listen, bind, connect), accepts connections, moves bytes between F-Stack
//! sockets and the per-connection buffers in `conn.rs`, drives UDP sockets,
//! and wakes tokio tasks.

use std::collections::HashMap;
use std::io;
use std::net::{Shutdown, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Buf;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{mpsc, oneshot};

use teto_dpdk::event::{Event, Events, Interest, Kqueue};
use teto_dpdk::net::{TcpListener, TcpStream, UdpSocket};
use teto_dpdk::FStack;

use crate::conn::{Conn, ConnError, ConnState, Notifier, Wakes, WriteShutdown, RX_HIGH, RX_LOW, TX_LIMIT};
use crate::runtime::Cmd;
use crate::udp_socket::{RecvStop, UdpEntry};

const READ_CHUNK: usize = 64 * 1024;
const EVENTS_CAPACITY: usize = 1024;
/// How long a dropped stream may take to get its data acknowledged by the
/// peer before it is aborted.
const DROP_GRACE: Duration = Duration::from_secs(30);
const DEADLINE_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// How long the loop keeps running after the last socket is gone, so final
/// FINs/ACKs (and anything F-Stack is batching) actually leave: stopping the
/// loop tears F-Stack down and discards whatever it still holds.
const STOP_GRACE: Duration = Duration::from_secs(1);

/// A connection handed to the tokio side (from `accept` or `connect`). If it
/// is dropped before becoming a `TetoTcpStream` — left in an accept queue, or
/// a cancelled `connect` — the connection is closed like a dropped stream.
pub(crate) struct Connected {
    conn: Option<Arc<Conn>>,
    pub peer: SocketAddr,
    pub local: SocketAddr,
}

impl Connected {
    pub(crate) fn take_conn(mut self) -> Arc<Conn> {
        self.conn.take().expect("connection already taken")
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut st = conn.lock();
            st.dropped = true;
            conn.notify(&mut st);
        }
    }
}

pub(crate) type AcceptItem = io::Result<Connected>;

struct Entry {
    stream: TcpStream,
    conn: Arc<Conn>,
    interest: Interest,
}

struct ListenerEntry {
    listener: TcpListener,
    local: SocketAddr,
    accept_tx: mpsc::Sender<AcceptItem>,
    /// Not registered with the kqueue because the accept queue is full or
    /// accepting just failed; re-registered once the queue has been drained.
    paused: bool,
}

struct Connecting {
    stream: TcpStream,
    reply: oneshot::Sender<io::Result<Connected>>,
}

enum Outcome {
    Keep,
    Close(Option<ConnError>),
}

pub(crate) struct Driver {
    fs: FStack,
    kq: Kqueue,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    /// Every `TetoRuntime` handle (and so every socket) has been dropped.
    handles_gone: bool,
    listeners: HashMap<u64, ListenerEntry>,
    conns: HashMap<u64, Entry>,
    connecting: HashMap<u64, Connecting>,
    udp: HashMap<u64, UdpEntry>,
    /// Kqueue token and connection id source; never reused.
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

impl Driver {
    pub(crate) fn new(fs: FStack, cmd_rx: mpsc::UnboundedReceiver<Cmd>) -> io::Result<Self> {
        Ok(Driver {
            fs,
            kq: Kqueue::new(&fs)?,
            cmd_rx,
            handles_gone: false,
            listeners: HashMap::new(),
            conns: HashMap::new(),
            connecting: HashMap::new(),
            udp: HashMap::new(),
            next_id: 1,
            notifier: Arc::new(Notifier::default()),
            notified: Vec::new(),
            events: Events::with_capacity(EVENTS_CAPACITY),
            ready: Vec::with_capacity(EVENTS_CAPACITY),
            draining: HashMap::new(),
            next_deadline_check: Instant::now(),
            idle_since: None,
        })
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// One poll-loop iteration. Never blocks. Returns `false` once nothing
    /// can use the runtime any more (all handles dropped, every socket gone)
    /// or it failed; the caller then stops the loop.
    pub(crate) fn tick(&mut self) -> bool {
        self.run_commands();

        let mut notified = std::mem::take(&mut self.notified);
        self.notifier.drain_into(&mut notified);
        for id in notified.drain(..) {
            self.service(id);
        }
        self.notified = notified;

        self.maintain_listeners();
        self.maintain_connecting();
        self.maintain_udp();

        if let Err(e) = self.kq.poll(&mut self.events) {
            // Can't happen while F-Stack runs; if it does, fail every stream
            // rather than leave them waiting.
            self.fail_all(ConnError::from_io(&e));
            return false;
        }
        let mut ready = std::mem::take(&mut self.ready);
        ready.extend(self.events.iter());
        for ev in ready.drain(..) {
            let id = ev.token();
            if self.listeners.contains_key(&id) {
                self.accept_ready(id);
            } else if self.connecting.contains_key(&id) {
                self.finish_connect(id);
            } else if let Some(u) = self.udp.get_mut(&id) {
                if let RecvStop::QueueUnavailable = u.recv_ready() {
                    let _ = self.kq.deregister(u.socket());
                    u.rx_paused = true;
                }
            } else if ev.is_send_empty() {
                // A dropped stream's data (and FIN) has been acknowledged.
                self.close(id, None);
            } else {
                if ev.is_readable() {
                    self.on_readable(id);
                }
                if ev.is_writable() {
                    self.on_writable(id);
                }
            }
        }
        self.ready = ready;

        if !self.draining.is_empty() && Instant::now() >= self.next_deadline_check {
            self.next_deadline_check = Instant::now() + DEADLINE_CHECK_INTERVAL;
            self.abort_overdue();
        }

        let busy = !self.handles_gone
            || !self.listeners.is_empty()
            || !self.conns.is_empty()
            || !self.connecting.is_empty()
            || !self.udp.is_empty();
        if busy {
            self.idle_since = None;
            return true;
        }
        let idle_since = *self.idle_since.get_or_insert_with(Instant::now);
        idle_since.elapsed() < STOP_GRACE
    }

    fn run_commands(&mut self) {
        loop {
            match self.cmd_rx.try_recv() {
                Ok(cmd) => self.run_command(cmd),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.handles_gone = true;
                    return;
                }
            }
        }
    }

    fn run_command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::ListenTcp { addr, opts, accept_tx, reply } => {
                let id = self.next_id();
                let listening = TcpListener::bind(&self.fs, addr, &opts).and_then(|listener| {
                    let local = listener.local_addr()?.into();
                    self.kq.register(&listener, id, Interest::READABLE)?;
                    Ok((listener, local))
                });
                match listening {
                    Ok((listener, local)) => {
                        if reply.send(Ok(local)).is_ok() {
                            self.listeners
                                .insert(id, ListenerEntry { listener, local, accept_tx, paused: false });
                        }
                    }
                    Err(e) => drop(reply.send(Err(e))),
                }
            }
            Cmd::BindUdp { addr, parts, reply } => {
                let id = self.next_id();
                let bound = UdpSocket::bind(&self.fs, addr).and_then(|s| {
                    let local = s.local_addr()?.into();
                    self.kq.register(&s, id, Interest::READABLE)?;
                    Ok((s, local))
                });
                match bound {
                    Ok((socket, local)) => {
                        if reply.send(Ok(local)).is_ok() {
                            self.udp.insert(id, UdpEntry::new(socket, parts));
                        }
                    }
                    Err(e) => drop(reply.send(Err(e))),
                }
            }
            Cmd::Connect { addr, opts, reply } => {
                let id = self.next_id();
                let started = TcpStream::connect(&self.fs, addr, &opts).and_then(|stream| {
                    self.kq.register(&stream, id, Interest::WRITABLE)?;
                    Ok(stream)
                });
                match started {
                    Ok(stream) => {
                        self.connecting.insert(id, Connecting { stream, reply });
                    }
                    Err(e) => drop(reply.send(Err(e))),
                }
            }
        }
    }

    /// Close listeners the application dropped (so new clients are refused
    /// instead of queued forever) and resume paused ones.
    fn maintain_listeners(&mut self) {
        let mut closed = Vec::new();
        for (&id, l) in &mut self.listeners {
            if l.accept_tx.is_closed() {
                closed.push(id);
            } else if l.paused
                && l.accept_tx.capacity() == l.accept_tx.max_capacity()
                && self.kq.register(&l.listener, id, Interest::READABLE).is_ok()
            {
                l.paused = false;
            }
        }
        for id in closed {
            self.listeners.remove(&id);
        }
    }

    /// Send queued datagrams, resume receiving on sockets whose queue has
    /// room again, and drop sockets the application is done with.
    fn maintain_udp(&mut self) {
        let kq = &self.kq;
        self.udp.retain(|&id, u| {
            u.tick();
            if u.rx_paused && u.can_resume() && kq.register(u.socket(), id, Interest::READABLE).is_ok() {
                u.rx_paused = false;
            }
            !u.finished()
        });
    }

    /// Abandon connects whose caller gave up (e.g. a timeout dropped the
    /// future); dropping the socket closes it.
    fn maintain_connecting(&mut self) {
        self.connecting.retain(|_, c| !c.reply.is_closed());
    }

    fn accept_ready(&mut self, listener_id: u64) {
        let Some(l) = self.listeners.get_mut(&listener_id) else { return };
        // `Some(closed)`: stop accepting for now (`closed` = for good).
        let stop = loop {
            let permit = match l.accept_tx.try_reserve() {
                Ok(p) => p,
                Err(TrySendError::Full(())) => break Some(false),
                Err(TrySendError::Closed(())) => break Some(true),
            };
            match l.listener.accept() {
                Ok((stream, peer)) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    // Not registered until the application accepts it (see
                    // `desired_interest`), so queued connections don't buffer.
                    // The listener may be bound to a wildcard address; report
                    // the address this connection actually arrived on.
                    let local = stream.local_addr().map(SocketAddr::V4).unwrap_or(l.local);
                    let conn = Arc::new(Conn::new(id, self.notifier.clone()));
                    self.conns.insert(id, Entry { stream, conn: conn.clone(), interest: Interest::NONE });
                    permit.send(Ok(Connected { conn: Some(conn), peer: peer.into(), local }));
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
                self.listeners.remove(&listener_id);
            }
            Some(false) => {
                let _ = self.kq.deregister(&l.listener);
                l.paused = true;
            }
            None => {}
        }
    }

    fn finish_connect(&mut self, id: u64) {
        let Some(Connecting { stream, reply }) = self.connecting.remove(&id) else { return };
        let connected = match stream.take_error() {
            Ok(None) => stream.local_addr(),
            Ok(Some(e)) | Err(e) => Err(e),
        };
        let local = match connected {
            Ok(local) => local,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let peer = stream.peer_addr();
        let conn = Arc::new(Conn::new(id, self.notifier.clone()));
        conn.lock().accepted = true;
        // Still registered for WRITABLE from connecting; `service` below
        // switches it to what the connection needs.
        self.conns.insert(id, Entry { stream, conn: conn.clone(), interest: Interest::WRITABLE });
        // If the caller is gone, `Connected`'s drop marks the stream dropped.
        let _ = reply.send(Ok(Connected { conn: Some(conn), peer: peer.into(), local: local.into() }));
        self.service(id);
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

impl Drop for Driver {
    fn drop(&mut self) {
        // The poll loop is gone (normally only on panic): fail every stream
        // instead of leaving its tasks waiting forever. Pending connects,
        // accept queues and UDP channels are dropped, which their tokio
        // sides report as "runtime stopped".
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

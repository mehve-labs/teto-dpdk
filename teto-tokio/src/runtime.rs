use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc;

use teto_dpdk::fstack::ffi::{FStackTcpListener, FStackUdpSocket, TcpMessage, UdpMessage};

use crate::tcp_stream::TetoTcpStream;

// ---------------------------------------------------------------------------
// Commands sent from tokio tasks → F-Stack thread
// ---------------------------------------------------------------------------

pub(crate) enum FStackCmd {
    TcpWrite { fd: i32, data: Vec<u8> },
    TcpClose { fd: i32 },
    UdpSend { data: Vec<u8>, addr: SocketAddr },
}

// ---------------------------------------------------------------------------
// TCP channel hub (global, one per process)
// ---------------------------------------------------------------------------

pub(crate) struct TcpChannelHub {
    pub accept_tx: mpsc::UnboundedSender<(TetoTcpStream, SocketAddr)>,
    pub cmd_tx: mpsc::UnboundedSender<FStackCmd>,
    pub cmd_rx: Mutex<mpsc::UnboundedReceiver<FStackCmd>>,
    pub connections: Mutex<HashMap<i32, mpsc::UnboundedSender<Vec<u8>>>>,
    pub listener_ptr: AtomicPtr<()>,
}

// SAFETY: TcpChannelHub is accessed from two threads (F-Stack + tokio) but
// all mutable fields are behind Mutex or atomic. The listener_ptr is set once
// with Release ordering on the F-Stack thread before any tick callback fires.
unsafe impl Send for TcpChannelHub {}
unsafe impl Sync for TcpChannelHub {}

pub(crate) static TCP_HUB: OnceLock<TcpChannelHub> = OnceLock::new();

// ---------------------------------------------------------------------------
// TCP callbacks (called on the F-Stack thread)
// ---------------------------------------------------------------------------

pub(crate) fn tcp_on_connect(fd: i32, ip: &String, port: u16) {
    let hub = TCP_HUB.get().expect("TCP_HUB not initialized");

    let (data_tx, data_rx) = mpsc::unbounded_channel();
    hub.connections.lock().unwrap().insert(fd, data_tx);

    let addr: SocketAddr = format!("{ip}:{port}")
        .parse()
        .expect("invalid peer address from F-Stack");

    let stream = TetoTcpStream::new(fd, addr, data_rx, hub.cmd_tx.clone());
    let _ = hub.accept_tx.send((stream, addr));
}

pub(crate) fn tcp_on_data(fd: i32, msg: &TcpMessage) {
    let hub = TCP_HUB.get().expect("TCP_HUB not initialized");

    let conns = hub.connections.lock().unwrap();
    if let Some(data_tx) = conns.get(&fd) {
        let _ = data_tx.send(msg.payload.clone());
    }
}

pub(crate) fn tcp_on_disconnect(fd: i32) {
    let hub = TCP_HUB.get().expect("TCP_HUB not initialized");
    hub.connections.lock().unwrap().remove(&fd);
}

/// Drains queued write/close commands and dispatches them on the F-Stack thread.
pub(crate) fn tcp_tick() {
    let hub = TCP_HUB.get().expect("TCP_HUB not initialized");

    let ptr = hub.listener_ptr.load(Ordering::Acquire);
    if ptr.is_null() {
        return;
    }
    let listener = unsafe { &*(ptr as *const FStackTcpListener) };

    let mut cmd_rx = hub.cmd_rx.lock().unwrap();
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            FStackCmd::TcpWrite { fd, data } => {
                listener.send_to(fd, &data);
            }
            FStackCmd::TcpClose { fd } => {
                listener.close_connection(fd);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// UDP channel hub (global, one per process)
// ---------------------------------------------------------------------------

pub(crate) struct UdpChannelHub {
    pub recv_tx: mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
    #[allow(dead_code)]
    pub cmd_tx: mpsc::UnboundedSender<FStackCmd>,
    pub cmd_rx: Mutex<mpsc::UnboundedReceiver<FStackCmd>>,
    pub socket_ptr: AtomicPtr<()>,
}

unsafe impl Send for UdpChannelHub {}
unsafe impl Sync for UdpChannelHub {}

pub(crate) static UDP_HUB: OnceLock<UdpChannelHub> = OnceLock::new();

// ---------------------------------------------------------------------------
// UDP callback (called on the F-Stack thread)
// ---------------------------------------------------------------------------

pub(crate) fn udp_on_packet(_fd: i32, msg: &UdpMessage) {
    let hub = UDP_HUB.get().expect("UDP_HUB not initialized");

    let addr: SocketAddr = format!("{}:{}", msg.src_ip, msg.src_port)
        .parse()
        .expect("invalid peer address from F-Stack");

    let _ = hub.recv_tx.send((msg.payload.clone(), addr));
}

/// Drains queued send commands and dispatches them on the F-Stack thread.
pub(crate) fn udp_tick() {
    let hub = UDP_HUB.get().expect("UDP_HUB not initialized");

    let ptr = hub.socket_ptr.load(Ordering::Acquire);
    if ptr.is_null() {
        return;
    }
    let socket = unsafe { &*(ptr as *const FStackUdpSocket) };

    let mut cmd_rx = hub.cmd_rx.lock().unwrap();
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            FStackCmd::UdpSend { data, addr } => {
                let ip = addr.ip().to_string();
                socket.send_to(&data, &ip, addr.port());
            }
            _ => {}
        }
    }
}

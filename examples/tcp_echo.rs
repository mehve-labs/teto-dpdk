/// TCP echo server on the low-level API: one kqueue, non-blocking sockets,
/// and per-connection buffering when the peer reads slower than it writes.
///
/// Run with:
///   cargo run --example tcp_echo
///
/// Test from inside the container:
///   nc -w3 10.0.0.1 8080
///   (type a message, press Enter, see it echoed back)
use std::collections::HashMap;
use std::io;

use teto_dpdk::event::{Events, Interest, Kqueue};
use teto_dpdk::net::{TcpListener, TcpStream};
use teto_dpdk::{FStack, FStackConfig, TcpSocketOptions};

const LISTENER: u64 = 0;

struct Conn {
    stream: TcpStream,
    /// Bytes read but not yet echoed back.
    pending: Vec<u8>,
    peer_done: bool,
}

/// Echo what's readable; buffer what the peer can't take yet. Returns
/// `Ok(false)` once the connection should be closed.
fn on_ready(conn: &mut Conn, buf: &mut [u8]) -> io::Result<bool> {
    // Flush earlier leftovers before reading more, so a peer that doesn't read
    // can't make us buffer without bound.
    while !conn.pending.is_empty() {
        match conn.stream.write(&conn.pending) {
            Ok(n) => drop(conn.pending.drain(..n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(e) => return Err(e),
        }
    }
    while !conn.peer_done {
        let n = match conn.stream.read(buf) {
            Ok(0) => {
                conn.peer_done = true;
                break;
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        };
        let mut written = 0;
        while written < n {
            match conn.stream.write(&buf[written..n]) {
                Ok(w) => written += w,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    conn.pending.extend_from_slice(&buf[written..n]);
                    return Ok(true);
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(!(conn.peer_done && conn.pending.is_empty()))
}

fn main() -> io::Result<()> {
    // TETO_PROFILE=bare-metal for a real NIC (default: Docker).
    let fs = FStack::init(&FStackConfig::from_env()?)?;
    let opts = TcpSocketOptions::default().nodelay(true);
    let listener = TcpListener::bind(&fs, "0.0.0.0:8080".parse().unwrap(), &opts)?;
    let kq = Kqueue::new(&fs)?;
    kq.register(&listener, LISTENER, Interest::READABLE)?;
    println!("Listening on {}", listener.local_addr()?);

    let mut conns: HashMap<u64, Conn> = HashMap::new();
    let mut next_token = LISTENER + 1;
    let mut events = Events::with_capacity(256);
    let mut buf = vec![0u8; 64 * 1024];

    fs.run(|| {
        if let Err(e) = kq.poll(&mut events) {
            eprintln!("kqueue poll failed: {e}");
            return;
        }
        for ev in events.iter() {
            if ev.token() == LISTENER {
                loop {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            let token = next_token;
                            next_token += 1;
                            if kq.register(&stream, token, Interest::READABLE).is_ok() {
                                println!("[{peer}] connected");
                                conns.insert(token, Conn { stream, pending: Vec::new(), peer_done: false });
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            eprintln!("accept failed: {e}");
                            break;
                        }
                    }
                }
                continue;
            }

            let Some(conn) = conns.get_mut(&ev.token()) else { continue };
            let keep = match on_ready(conn, &mut buf) {
                Ok(keep) => keep,
                Err(e) => {
                    eprintln!("[{}] error: {e}", conn.stream.peer_addr());
                    false
                }
            };
            if !keep {
                println!("[{}] closed", conn.stream.peer_addr());
                conns.remove(&ev.token());
                continue;
            }
            // Wait for writability while output is pending, otherwise for input.
            let interest = if conn.pending.is_empty() { Interest::READABLE } else { Interest::WRITABLE };
            let _ = kq.register(&conn.stream, ev.token(), interest);
        }
    })
}

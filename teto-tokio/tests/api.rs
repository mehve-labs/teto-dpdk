//! tokio-parity API: owned halves, shared accept, readiness and try_*, peek,
//! live socket options, local bind for connect, runtime shutdown, connected
//! UDP, IPv6 (see tests/common for the setup).

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener as StdTcpListener, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use teto_dpdk::TcpSocketOptions;
use teto_tokio::{TetoTcpListener, TetoTcpStream, TetoUdpSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

const PORT: u16 = 8080;

/// Kernel-side echo server on an ephemeral port of `ip`; returns its address
/// and a channel that yields each accepted connection's peer address.
fn kernel_echo(ip: &str) -> (SocketAddr, std::sync::mpsc::Receiver<SocketAddr>) {
    let listener = StdTcpListener::bind((ip, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let _ = tx.send(conn.peer_addr().unwrap());
            std::thread::spawn(move || {
                let mut buf = [0u8; 65536];
                while let Ok(n @ 1..) = conn.read(&mut buf) {
                    if conn.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = conn.shutdown(Shutdown::Write);
            });
        }
    });
    (addr, rx)
}

/// A1: halves used from separate tasks; dropping the write half sends FIN
/// while the read half keeps reading; reunite.
#[tokio::test(flavor = "multi_thread")]
async fn owned_halves_work_from_separate_tasks() {
    const LEN: usize = 2 * 1024 * 1024;
    let listener = listen(PORT).await;
    let (server, mut client) = pair(&listener, PORT).await;
    let (mut rd, mut wr) = server.into_split();
    assert_eq!(rd.peer_addr(), wr.peer_addr());

    // Writer task streams data and drops its half (FIN); the reader task
    // reads what the client sends, after the FIN went out.
    let writer = tokio::spawn(async move {
        wr.write_all(&pattern(LEN, 1)).await.unwrap();
        drop(wr);
    });
    let client = blocking(move || {
        let got = read_all(&mut client); // ends with the FIN from the dropped write half
        assert!(got == pattern(LEN, 1), "data corrupted");
        client.write_all(b"after FIN").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        client
    })
    .await;
    writer.await.unwrap();
    let mut v = Vec::new();
    timeout(T, rd.read_to_end(&mut v)).await.unwrap().unwrap();
    assert_eq!(v, b"after FIN");
    drop(client);

    // Halves of different streams don't reunite; matching halves do.
    let (a, _ca) = pair(&listener, PORT).await;
    let (b, _cb) = pair(&listener, PORT).await;
    let (a_rd, a_wr) = a.into_split();
    let (b_rd, b_wr) = b.into_split();
    let err = a_rd.reunite(b_wr).unwrap_err();
    let (a_rd, b_wr) = (err.0, err.1);
    let a = a_rd.reunite(a_wr).expect("same stream");
    let b = b_wr.reunite(b_rd).expect("same stream");
    assert_ne!(a.peer_addr(), b.peer_addr());
}

/// A3: several tasks accept from one listener.
#[tokio::test(flavor = "multi_thread")]
async fn accept_from_several_tasks() {
    const N: usize = 20;
    let listener = Arc::new(listen(PORT).await);
    let acceptors: Vec<_> = (0..4)
        .map(|_| {
            let listener = listener.clone();
            tokio::spawn(async move {
                let mut served = 0;
                while let Ok(Ok((mut s, _))) = timeout(Duration::from_secs(3), listener.accept()).await {
                    let mut v = Vec::new();
                    s.read_to_end(&mut v).await.unwrap();
                    s.write_all(&v).await.unwrap();
                    served += 1;
                }
                served
            })
        })
        .collect();
    blocking(|| {
        for i in 0..N {
            let mut c = connect(fstack(PORT));
            c.write_all(format!("c{i}").as_bytes()).unwrap();
            c.shutdown(Shutdown::Write).unwrap();
            assert_eq!(read_all(&mut c), format!("c{i}").as_bytes());
        }
    })
    .await;
    let mut total = 0;
    for a in acceptors {
        total += a.await.unwrap();
    }
    assert_eq!(total, N);
}

/// A5: readable/writable, try_read/try_write, peek.
#[tokio::test(flavor = "multi_thread")]
async fn readiness_try_and_peek() {
    let listener = listen(PORT).await;
    let (server, mut client) = pair(&listener, PORT).await;
    let mut buf = [0u8; 16];
    assert_eq!(server.try_read(&mut buf).unwrap_err().kind(), ErrorKind::WouldBlock);

    client.write_all(b"hello").unwrap();
    timeout(T, server.readable()).await.unwrap().unwrap();
    // peek doesn't consume.
    let n = timeout(T, server.peek(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"hello");
    let n = server.try_read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello");
    assert_eq!(server.try_read(&mut buf).unwrap_err().kind(), ErrorKind::WouldBlock);

    timeout(T, server.writable()).await.unwrap().unwrap();
    assert_eq!(server.try_write(b"world").unwrap(), 5);
    let mut got = [0u8; 5];
    client.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"world");

    // try_write reports a full send buffer instead of waiting.
    let mut queued = 0;
    let chunk = vec![0u8; 64 * 1024];
    let full = loop {
        match server.try_write(&chunk) {
            Ok(n) => queued += n,
            Err(e) => break e,
        }
        assert!(queued < 64 * 1024 * 1024, "send buffer never filled");
    };
    assert_eq!(full.kind(), ErrorKind::WouldBlock);

    // EOF: try_read returns 0, peek returns 0.
    client.shutdown(Shutdown::Write).unwrap();
    timeout(T, server.readable()).await.unwrap().unwrap();
    assert_eq!(server.try_read(&mut buf).unwrap(), 0);
    assert_eq!(timeout(T, server.peek(&mut buf)).await.unwrap().unwrap(), 0);
}

/// A4: options on a live connection.
#[tokio::test(flavor = "multi_thread")]
async fn options_on_live_connection() {
    let listener = listen(PORT).await;
    let (server, client) = pair(&listener, PORT).await;
    timeout(T, server.set_nodelay(false)).await.unwrap().unwrap();
    let opts = TcpSocketOptions::default().keepalive(true).keepalive_idle_secs(30).send_buf(256 * 1024);
    timeout(T, server.set_options(&opts)).await.unwrap().unwrap();
    let mut server = server;
    echo_check(&mut server, client, b"still works").await;
}

/// A4: connect from a chosen local port.
#[tokio::test(flavor = "multi_thread")]
async fn connect_from_local_port() {
    let rt = start().await;
    let (echo, peers) = kernel_echo("10.0.0.2");
    let local = sa("10.0.0.1:40000");
    let mut s = timeout(T, TetoTcpStream::connect_from(&rt, local, echo, TcpSocketOptions::default()))
        .await
        .unwrap()
        .expect("connect_from");
    assert_eq!(s.local_addr(), local);
    assert_eq!(peers.recv_timeout(T).unwrap(), local, "kernel saw a different source");
    s.write_all(b"ping").await.unwrap();
    let mut pong = [0u8; 4];
    timeout(T, s.read_exact(&mut pong)).await.unwrap().unwrap();
    assert_eq!(&pong, b"ping");

    let err = TetoTcpStream::connect_from(&rt, sa("[::]:0"), echo, TcpSocketOptions::default()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

/// A2: shutdown() waits until dropped connections have delivered their data
/// and the F-Stack thread has stopped.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_waits_for_delivery() {
    const LEN: usize = 4 * 1024 * 1024;
    let rt = start().await;
    let listener = TetoTcpListener::bind(&rt, fstack(PORT), TcpSocketOptions::default()).await.unwrap();
    let (mut server, mut client) = pair(&listener, PORT).await;
    drop(listener);
    let reader = tokio::task::spawn_blocking(move || read_all(&mut client));
    server.write_all(&pattern(LEN, 3)).await.unwrap();
    drop(server); // data still being delivered

    timeout(Duration::from_secs(30), rt.shutdown()).await.expect("shutdown hung");
    assert!(!fstack_thread_running(), "shutdown returned before the F-Stack thread stopped");
    let got = reader.await.unwrap();
    assert_eq!(got.len(), LEN);
    assert!(got == pattern(LEN, 3), "data corrupted");
}

/// A5: connected UDP: send/recv to the default peer; other senders are
/// filtered out.
#[tokio::test(flavor = "multi_thread")]
async fn connected_udp() {
    let rt = start().await;
    let socket = TetoUdpSocket::bind(&rt, fstack(9000)).await.unwrap();
    assert_eq!(socket.peer_addr().unwrap_err().kind(), ErrorKind::NotConnected);
    assert_eq!(socket.send(b"x").await.unwrap_err().kind(), ErrorKind::NotConnected);

    let peer = StdUdpSocket::bind("10.0.0.2:0").unwrap();
    let stranger = StdUdpSocket::bind("10.0.0.2:0").unwrap();
    peer.set_read_timeout(Some(T)).unwrap();
    socket.connect(peer.local_addr().unwrap()).await.unwrap();
    assert_eq!(socket.peer_addr().unwrap(), peer.local_addr().unwrap());

    socket.send(b"to peer").await.unwrap();
    let mut buf = [0u8; 32];
    let (n, _) = peer.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"to peer");

    stranger.send_to(b"from stranger", fstack(9000)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    peer.send_to(b"from peer", fstack(9000)).unwrap();
    let n = timeout(T, socket.recv(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"from peer", "datagram from a stranger got through");

    let err = socket.send_to(b"x", sa("[fd00::2]:9")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "IPv6 destination on an IPv4 socket");
}

/// A6: TCP over IPv6, both directions.
#[tokio::test(flavor = "multi_thread")]
async fn tcp_over_ipv6() {
    let rt = start().await;
    let addr6 = sa("[fd00::1]:8080");
    let listener = TetoTcpListener::bind(&rt, addr6, TcpSocketOptions::default()).await.expect("bind v6");
    assert_eq!(listener.local_addr(), addr6);
    let client = tokio::task::spawn_blocking(move || connect(addr6));
    let (mut server, peer) = timeout(T * 3, listener.accept()).await.unwrap().unwrap();
    let client = client.await.unwrap();
    assert_eq!(peer, client.local_addr().unwrap());
    assert!(peer.is_ipv6());
    echo_check(&mut server, client, b"over IPv6").await;

    // Outbound over IPv6.
    let (echo, _) = kernel_echo("fd00::2");
    let mut up = timeout(T, TetoTcpStream::connect(&rt, echo, TcpSocketOptions::default())).await.unwrap().unwrap();
    assert_eq!(up.local_addr().ip().to_string(), "fd00::1");
    up.write_all(b"ping6").await.unwrap();
    let mut pong = [0u8; 5];
    timeout(T, up.read_exact(&mut pong)).await.unwrap().unwrap();
    assert_eq!(&pong, b"ping6");
}

/// A6: UDP over IPv6.
#[tokio::test(flavor = "multi_thread")]
async fn udp_over_ipv6() {
    let rt = start().await;
    let socket = TetoUdpSocket::bind(&rt, sa("[fd00::1]:9000")).await.expect("bind v6");
    let client = StdUdpSocket::bind("[fd00::2]:0").unwrap();
    client.set_read_timeout(Some(T)).unwrap();
    client.send_to(b"hello6", "[fd00::1]:9000").unwrap();
    let mut buf = [0u8; 16];
    let (n, from) = timeout(T, socket.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"hello6");
    assert_eq!(from, client.local_addr().unwrap());
    socket.send_to(b"reply6", from).await.unwrap();
    let (n, _) = client.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"reply6");
}

//! UDP behaviour against a live F-Stack (see tests/common for the setup).

mod common;

use std::collections::HashSet;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use teto_tokio::TetoUdpSocket;
use tokio::time::timeout;

const PORT: u16 = 9000;

/// Kernel-side UDP client; waits until F-Stack echoes a first datagram.
fn udp_client(server: SocketAddr) -> StdUdpSocket {
    let c = StdUdpSocket::bind("0.0.0.0:0").unwrap();
    c.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let mut buf = [0u8; 16];
    for _ in 0..50 {
        c.send_to(b"ping", server).unwrap();
        if c.recv_from(&mut buf).is_ok() {
            while c.recv_from(&mut buf).is_ok() {} // late duplicates
            return c;
        }
    }
    panic!("no echo from {server}");
}

/// Spawn an echo loop on `socket`.
fn echo(socket: Arc<TetoUdpSocket>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            let (n, peer) = socket.recv_from(&mut buf).await.unwrap();
            socket.send_to(&buf[..n], peer).await.unwrap();
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn ipv6_is_rejected() {
    let rt = start().await;
    let err = TetoUdpSocket::bind(&rt, sa("[::1]:9000")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    let socket = TetoUdpSocket::bind(&rt, fstack(PORT)).await.expect("bind");
    let err = socket.send_to(b"x", sa("[::1]:9")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

/// A burst is drained in batches, not one datagram per poll iteration.
#[tokio::test(flavor = "multi_thread")]
async fn bursts_are_echoed() {
    let rt = start().await;
    let socket = Arc::new(TetoUdpSocket::bind(&rt, fstack(PORT)).await.expect("bind"));
    let _echo = echo(socket);

    let seen = blocking(|| {
        const N: usize = 500;
        let client = udp_client(fstack(PORT));
        // Read echoes on another thread while sending, so the client's own
        // receive buffer doesn't overflow.
        let reader = client.try_clone().unwrap();
        let collector = std::thread::spawn(move || {
            reader.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut buf = [0u8; 65535];
            let mut seen = HashSet::new();
            while seen.len() < N {
                let Ok((n, _)) = reader.recv_from(&mut buf) else { break };
                let s = std::str::from_utf8(&buf[..n]).unwrap();
                if !s.starts_with("dgram-") {
                    continue; // late echo of a warm-up ping
                }
                let i: usize = s[6..10].parse().unwrap();
                assert_eq!(s, format!("dgram-{i:04}-{}", "x".repeat(i % 900)));
                seen.insert(i);
            }
            seen.len()
        });
        // Bursts of 100 (~45 KB) arrive faster than one poll iteration, so
        // the driver drains many per tick. (One 225 KB burst could overflow
        // F-Stack's default ~42 KB UDP receive buffer when the emulated
        // F-Stack thread stalls: legitimate UDP loss, not a driver bug.)
        for i in 0..N {
            client.send_to(format!("dgram-{i:04}-{}", "x".repeat(i % 900)).as_bytes(), fstack(PORT)).unwrap();
            if i % 100 == 99 {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let seen = collector.join().unwrap();
        assert!(seen >= N * 95 / 100, "only {seen} of {N} echoes received");
        seen
    })
    .await;
    eprintln!("{seen} of 500 echoes");
}

/// Datagrams queued right before the socket is dropped still go out, and the
/// F-Stack thread exits afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn queued_datagrams_sent_after_drop() {
    const N: usize = 20;
    let rt = start().await;
    let socket = TetoUdpSocket::bind(&rt, fstack(PORT)).await.expect("bind");
    drop(rt);
    let client = StdUdpSocket::bind("0.0.0.0:0").unwrap();
    client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let client_addr = SocketAddr::from(([10, 0, 0, 2], client.local_addr().unwrap().port()));
    // One exchange first, so F-Stack has resolved the client's MAC: datagrams
    // sent before ARP completes wait in FreeBSD's ARP hold queue, which keeps
    // only 16 (net.link.arp.maxhold) -- that's FreeBSD, not what this tests.
    client.send_to(b"hello", fstack(PORT)).unwrap();
    let mut buf = [0u8; 16];
    let (_, from) = timeout(T, socket.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(from, client_addr);
    socket.send_to(b"hi", client_addr).await.unwrap();
    let client = blocking(move || {
        client.recv_from(&mut [0u8; 16]).expect("reply");
        client
    })
    .await;
    for i in 0..N {
        socket.send_to(format!("last-{i}").as_bytes(), client_addr).await.unwrap();
    }
    drop(socket);
    let got = blocking(move || {
        let mut buf = [0u8; 64];
        (0..N).take_while(|_| client.recv_from(&mut buf).is_ok()).count()
    })
    .await;
    assert_eq!(got, N, "datagrams lost when the socket was dropped");
    wait_for_fstack_exit().await;
}

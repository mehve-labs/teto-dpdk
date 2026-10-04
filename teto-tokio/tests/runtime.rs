//! One `TetoRuntime` serving several listeners, UDP sockets and outbound
//! connections (see tests/common for the setup).

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener as StdTcpListener, UdpSocket as StdUdpSocket};
use std::time::Duration;

use common::*;
use teto_dpdk::TcpSocketOptions;
use teto_tokio::{TetoRuntime, TetoTcpListener, TetoTcpStream, TetoUdpSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

fn opts() -> TcpSocketOptions {
    TcpSocketOptions::default().nodelay(true)
}

/// Accept one connection on `listener` and echo a message from a kernel client.
async fn check_listener(listener: &mut TetoTcpListener, addr: SocketAddr, msg: &'static [u8]) {
    let client = tokio::task::spawn_blocking(move || {
        let mut c = connect(addr);
        c.write_all(msg).unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        read_all(&mut c)
    });
    let (mut s, _) = timeout(T * 3, listener.accept()).await.unwrap().unwrap();
    // The real local address, not the listener's wildcard.
    assert_eq!(s.local_addr(), addr);
    let mut v = Vec::new();
    timeout(T, s.read_to_end(&mut v)).await.unwrap().unwrap();
    timeout(T, s.write_all(&v)).await.unwrap().unwrap();
    timeout(T, s.shutdown()).await.unwrap().unwrap();
    assert_eq!(client.await.unwrap(), msg);
}

/// Kernel-side echo server for outbound connections; returns its address.
/// (Port 0: connections left over from an earlier test process can still
/// hold a fixed port.)
fn kernel_echo_server() -> SocketAddr {
    let listener = StdTcpListener::bind("10.0.0.2:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
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
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn init_output_is_captured() {
    let rt = start().await;
    assert!(rt.init_output().contains("EAL"), "init output not captured: {:?}", rt.init_output());
}

/// A failed bind leaves the runtime usable.
#[tokio::test(flavor = "multi_thread")]
async fn failed_bind_leaves_runtime_usable() {
    let rt = start().await;
    let err = TetoTcpListener::bind(&rt, sa("10.0.0.99:8080"), opts()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AddrNotAvailable, "{err:?}");
    let mut a = TetoTcpListener::bind(&rt, fstack(8080), opts()).await.expect("bind 8080");
    let err = TetoTcpListener::bind(&rt, fstack(8080), opts()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AddrInUse, "{err:?}");
    check_listener(&mut a, fstack(8080), b"after a failed bind").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn several_listeners_and_udp_on_one_runtime() {
    let rt = start().await;
    let mut a = TetoTcpListener::bind(&rt, sa("0.0.0.0:8080"), opts()).await.expect("bind 8080");
    let mut b = TetoTcpListener::bind(&rt, sa("0.0.0.0:8081"), opts()).await.expect("bind 8081");
    let udp = TetoUdpSocket::bind(&rt, sa("0.0.0.0:9000")).await.expect("bind udp");
    check_listener(&mut a, fstack(8080), b"via 8080").await;
    check_listener(&mut b, fstack(8081), b"via 8081").await;

    let peer = blocking(|| {
        let c = StdUdpSocket::bind("0.0.0.0:0").unwrap();
        c.send_to(b"udp hello", "10.0.0.1:9000").unwrap();
        c
    })
    .await;
    let mut buf = [0u8; 64];
    let (n, from) = timeout(T, udp.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"udp hello");
    timeout(T, udp.send_to(b"udp reply", from)).await.unwrap().unwrap();
    let got = blocking(move || {
        peer.set_read_timeout(Some(T)).unwrap();
        let mut b = [0u8; 64];
        let (n, _) = peer.recv_from(&mut b).expect("udp reply");
        b[..n].to_vec()
    })
    .await;
    assert_eq!(got, b"udp reply");
}

/// UDP receive pauses while the application's queue is full and resumes once
/// it has been drained.
#[tokio::test(flavor = "multi_thread")]
async fn udp_receive_pauses_and_resumes() {
    let rt = start().await;
    let udp = TetoUdpSocket::bind(&rt, fstack(9001)).await.expect("bind udp 9001");
    let flood = blocking(|| {
        let c = StdUdpSocket::bind("0.0.0.0:0").unwrap();
        // Paced: an instant flood would overrun af_packet's 512-frame receive
        // ring before F-Stack sees it (environment-level UDP loss).
        for i in 0..3000u32 {
            let _ = c.send_to(&i.to_be_bytes(), "10.0.0.1:9001");
            if i % 100 == 99 {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        c
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut buf = [0u8; 64];
    let mut drained = 0;
    while timeout(Duration::from_millis(300), udp.recv_from(&mut buf)).await.is_ok() {
        drained += 1;
    }
    assert!(drained >= 1024, "only {drained} datagrams queued before the pause");
    blocking(move || {
        for _ in 0..10 {
            flood.send_to(b"after", "10.0.0.1:9001").unwrap();
        }
    })
    .await;
    let mut after = 0;
    while after < 10 {
        let (n, _) = timeout(T, udp.recv_from(&mut buf)).await.expect("receive resumed").unwrap();
        if &buf[..n] == b"after" {
            after += 1;
        }
    }
}

/// Outbound connection: a 4 MiB echo through a kernel-side server.
#[tokio::test(flavor = "multi_thread")]
async fn connect_and_echo() {
    let rt = start().await;
    let echo = kernel_echo_server();
    let mut up = timeout(T, TetoTcpStream::connect(&rt, echo, opts()))
        .await
        .unwrap()
        .expect("connect");
    assert_eq!(up.peer_addr(), echo);
    assert_eq!(up.local_addr().ip().to_string(), "10.0.0.1");
    const LEN: usize = 4 * 1024 * 1024;
    let (mut r, mut w) = tokio::io::split(&mut up);
    let writer = async {
        w.write_all(&pattern(LEN, 0)).await.unwrap();
        w.shutdown().await.unwrap();
    };
    let reader = async {
        let mut v = Vec::with_capacity(LEN);
        r.read_to_end(&mut v).await.unwrap();
        v
    };
    let ((), echoed) =
        timeout(Duration::from_secs(60), async { tokio::join!(writer, reader) }).await.unwrap();
    assert_eq!(echoed.len(), LEN);
    assert!(echoed == pattern(LEN, 0), "data corrupted");
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_refused() {
    let rt = start().await;
    // A port nobody listens on (bound and released by the kernel just now).
    let closed = StdTcpListener::bind("10.0.0.2:0").unwrap().local_addr().unwrap();
    let err = timeout(T, TetoTcpStream::connect(&rt, closed, opts())).await.unwrap().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ConnectionRefused, "{err:?}");
}

/// A connect nobody answers can be cancelled; the runtime carries on.
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_connect_leaves_runtime_usable() {
    let rt = start().await;
    let echo = kernel_echo_server();
    let pending = timeout(Duration::from_millis(500), TetoTcpStream::connect(&rt, sa("10.0.0.77:80"), opts())).await;
    assert!(pending.is_err(), "connect to a silent address finished: {pending:?}");
    let mut again = timeout(T, TetoTcpStream::connect(&rt, echo, opts()))
        .await
        .unwrap()
        .expect("connect after cancelled connect");
    again.write_all(b"ping").await.unwrap();
    let mut pong = [0u8; 4];
    timeout(T, again.read_exact(&mut pong)).await.unwrap().unwrap();
    assert_eq!(&pong, b"ping");
}

/// Once every handle and socket is gone, the F-Stack thread exits.
#[tokio::test(flavor = "multi_thread")]
async fn runtime_exits_when_unused() {
    let rt = start().await;
    let listener = TetoTcpListener::bind(&rt, fstack(8080), opts()).await.expect("bind");
    let udp = TetoUdpSocket::bind(&rt, fstack(9000)).await.expect("bind udp");
    let clone: TetoRuntime = rt.clone();
    drop((rt, listener, udp));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(fstack_thread_running(), "stopped while a runtime handle was alive");
    drop(clone);
    wait_for_fstack_exit().await;
}

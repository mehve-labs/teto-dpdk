//! Local mode: async code on the F-Stack thread (see tests/common).

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener as StdTcpListener, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use teto_dpdk::TcpSocketOptions;
use teto_tokio::local::{self, LocalTcpListener, LocalTcpStream, LocalUdpSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

fn opts() -> TcpSocketOptions {
    TcpSocketOptions::default().nodelay(true)
}

/// Echo server: one `spawn_local` task per connection until `stop` is set.
async fn serve_echo(listener: LocalTcpListener, stop: Arc<AtomicBool>) -> usize {
    let mut served = 0;
    while !stop.load(Ordering::Acquire) {
        let Ok(Ok((mut s, _))) = timeout(Duration::from_millis(200), listener.accept()).await else { continue };
        served += 1;
        tokio::task::spawn_local(async move {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n @ 1..) = s.read(&mut buf).await {
                if s.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        });
    }
    served
}

#[test]
fn echo_many_connections() {
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let clients = std::thread::spawn(move || {
        let handles: Vec<_> = (0..50)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut c = connect(fstack(8080));
                    let msg = pattern(64 * 1024, i);
                    let mut reader = c.try_clone().unwrap();
                    let echo = std::thread::spawn(move || {
                        let mut got = vec![0u8; 64 * 1024];
                        reader.read_exact(&mut got).unwrap();
                        got
                    });
                    c.write_all(&msg).unwrap();
                    assert!(echo.join().unwrap() == msg, "connection {i}: wrong echo");
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        done.store(true, Ordering::Release);
    });
    let served = local::run(config(), async move {
        let listener = LocalTcpListener::bind(fstack(8080), &opts()).expect("bind");
        serve_echo(listener, stop).await
    })
    .expect("run");
    clients.join().expect("clients");
    assert_eq!(served, 50);
}

#[test]
fn tokio_timers_work() {
    let elapsed = local::run(config(), async {
        let started = Instant::now();
        tokio::time::sleep(Duration::from_millis(200)).await;
        started.elapsed()
    })
    .expect("run");
    assert!(elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn connect_outbound_and_refused() {
    let echo = StdTcpListener::bind("10.0.0.2:0").unwrap();
    let echo_addr = echo.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut c, _) = echo.accept().unwrap();
        let mut buf = [0u8; 65536];
        while let Ok(n @ 1..) = c.read(&mut buf) {
            c.write_all(&buf[..n]).unwrap();
        }
        c.shutdown(Shutdown::Write).unwrap();
    });
    let closed = StdTcpListener::bind("10.0.0.2:0").unwrap().local_addr().unwrap();
    local::run(config(), async move {
        const LEN: usize = 1024 * 1024;
        let mut s = timeout(T, LocalTcpStream::connect(echo_addr, &opts())).await.unwrap().expect("connect");
        assert_eq!(s.peer_addr(), echo_addr);
        let data = pattern(LEN, 5);
        let (mut sent, mut got) = (0, Vec::with_capacity(LEN));
        let mut buf = vec![0u8; 64 * 1024];
        while got.len() < LEN {
            if sent < LEN {
                sent += s.write(&data[sent..(sent + 16 * 1024).min(LEN)]).await.unwrap();
            }
            if let Ok(Ok(n)) = timeout(Duration::from_millis(5), s.read(&mut buf)).await {
                got.extend_from_slice(&buf[..n]);
            }
        }
        assert!(got == data, "echo corrupted");

        let err = timeout(T, LocalTcpStream::connect(closed, &opts())).await.unwrap().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ConnectionRefused, "{err:?}");
    })
    .expect("run");
}

#[test]
fn udp_echo() {
    let client = std::thread::spawn(|| {
        let c = StdUdpSocket::bind("10.0.0.2:0").unwrap();
        c.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut buf = [0u8; 64];
        for _ in 0..50 {
            c.send_to(b"hello", "10.0.0.1:9000").unwrap();
            if let Ok((n, _)) = c.recv_from(&mut buf) {
                return buf[..n].to_vec();
            }
        }
        panic!("no echo");
    });
    local::run(config(), async {
        let socket = LocalUdpSocket::bind(fstack(9000)).expect("bind");
        let mut buf = [0u8; 64];
        let (n, from) = timeout(T * 3, socket.recv_from(&mut buf)).await.unwrap().unwrap();
        socket.send_to(&buf[..n], from).await.unwrap();
        // Let the reply leave before `run` ends.
        tokio::time::sleep(Duration::from_millis(200)).await;
    })
    .expect("run");
    assert_eq!(client.join().unwrap(), b"hello");
}

/// A writer blocked on a peer that doesn't read waits without stalling the
/// rest: another connection is served meanwhile.
#[test]
fn blocked_writer_does_not_stall_others() {
    const LEN: usize = 16 * 1024 * 1024;
    let (slow_tx, slow_rx) = std::sync::mpsc::channel::<std::net::TcpStream>();
    let slow_client = std::thread::spawn(move || {
        let c = connect(fstack(8080));
        slow_tx.send(c.try_clone().unwrap()).unwrap();
        c
    });
    local::run(config(), async move {
        let listener = LocalTcpListener::bind(fstack(8080), &opts()).expect("bind");
        let (mut slow, _) = timeout(T * 3, listener.accept()).await.unwrap().unwrap();
        let writer = tokio::task::spawn_local(async move {
            slow.write_all(&pattern(LEN, 0)).await.unwrap();
            slow.shutdown().await.unwrap();
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!writer.is_finished(), "16 MiB to a non-reading peer finished: no backpressure?");

        let other = std::thread::spawn(|| {
            let mut c = connect(fstack(8080));
            c.write_all(b"not stalled").unwrap();
            c.shutdown(Shutdown::Write).unwrap();
            read_all(&mut c)
        });
        let (mut s, _) = timeout(T, listener.accept()).await.unwrap().unwrap();
        let mut v = Vec::new();
        timeout(T, s.read_to_end(&mut v)).await.unwrap().unwrap();
        s.write_all(&v).await.unwrap();
        s.shutdown().await.unwrap();
        drop(s);

        let mut slow_reader = slow_rx.recv().unwrap();
        let reader = std::thread::spawn(move || read_all(&mut slow_reader));
        timeout(Duration::from_secs(60), writer).await.unwrap().unwrap();
        while !other.is_finished() || !reader.is_finished() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(other.join().unwrap(), b"not stalled");
        let got = reader.join().unwrap();
        assert_eq!(got.len(), LEN);
    })
    .expect("run");
    drop(slow_client.join().unwrap());
}

/// Data written just before a stream is dropped (and `main` returns) is
/// delivered in full before `run` returns.
#[test]
fn dropped_stream_delivers_before_run_returns() {
    const LEN: usize = 4 * 1024 * 1024;
    let client = std::thread::spawn(|| read_all(&mut connect(fstack(8080))));
    local::run(config(), async {
        let listener = LocalTcpListener::bind(fstack(8080), &opts()).expect("bind");
        let (mut s, _) = timeout(T * 3, listener.accept()).await.unwrap().unwrap();
        s.write_all(&pattern(LEN, 9)).await.unwrap();
        // main returns with the stream dropped and data in flight
    })
    .expect("run");
    let got = client.join().unwrap();
    assert_eq!(got.len(), LEN);
    assert!(got == pattern(LEN, 9), "data corrupted");
}

#[test]
fn tcp_over_ipv6() {
    let client = std::thread::spawn(|| {
        let mut c = connect(sa("[fd00::1]:8080"));
        c.write_all(b"v6").unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        read_all(&mut c)
    });
    local::run(config(), async {
        let listener = LocalTcpListener::bind(sa("[fd00::1]:8080"), &opts()).expect("bind v6");
        let (mut s, peer) = timeout(T * 3, listener.accept()).await.unwrap().unwrap();
        assert!(peer.is_ipv6());
        let mut v = Vec::new();
        s.read_to_end(&mut v).await.unwrap();
        s.write_all(&v).await.unwrap();
    })
    .expect("run");
    assert_eq!(client.join().unwrap(), b"v6");
}

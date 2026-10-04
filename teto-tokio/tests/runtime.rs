//! One `TetoRuntime` serving several listeners, a UDP socket and outbound
//! connections; bind failures leave it usable; it exits once every handle is
//! dropped. Needs the Docker environment (see tests/tcp.rs).

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream, UdpSocket as StdUdpSocket};
use std::time::{Duration, Instant};

use teto_dpdk::{FStackConfig, TcpSocketOptions};
use teto_tokio::{TetoRuntime, TetoTcpListener, TetoTcpStream, TetoUdpSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

const T: Duration = Duration::from_secs(10);

fn config() -> FStackConfig {
    FStackConfig::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../config.ini"))
        .with_eal_arg("--vdev=net_af_packet0,iface=teto0-dpdk")
        .with_eal_arg("--no-pci")
        .with_eal_arg("--iova-mode=va")
        .capture_init_output(true)
}

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// Kernel-side client: retries until the TAP device is configured.
fn connect_kernel(addr: SocketAddr) -> StdTcpStream {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match StdTcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
            Ok(s) => {
                s.set_read_timeout(Some(T)).unwrap();
                return s;
            }
            Err(e) => {
                assert!(Instant::now() < deadline, "{addr} never became reachable: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Accept one connection on `listener` and echo a message from a kernel client.
async fn check_listener(listener: &mut TetoTcpListener, addr: SocketAddr, msg: &'static [u8]) {
    let client = tokio::task::spawn_blocking(move || {
        let mut c = connect_kernel(addr);
        c.write_all(msg).unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        let mut v = Vec::new();
        c.read_to_end(&mut v).unwrap();
        v
    });
    let (mut s, _) = timeout(Duration::from_secs(100), listener.accept()).await.unwrap().unwrap();
    // The real local address, not the listener's wildcard.
    assert_eq!(s.local_addr(), addr);
    let mut v = Vec::new();
    timeout(T, s.read_to_end(&mut v)).await.unwrap().unwrap();
    timeout(T, s.write_all(&v)).await.unwrap().unwrap();
    timeout(T, s.shutdown()).await.unwrap().unwrap();
    assert_eq!(client.await.unwrap(), msg);
}

/// Kernel-side echo server for outbound connections.
fn kernel_echo_server(port: u16) {
    let listener = StdTcpListener::bind(("0.0.0.0", port)).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 65536];
                loop {
                    match conn.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if conn.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let _ = conn.shutdown(Shutdown::Write);
            });
        }
    });
}

#[test]
fn runtime_suite() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let rt = TetoRuntime::start(config()).await.expect("start");
        assert!(rt.init_output().contains("EAL"), "init output not captured: {:?}", rt.init_output());
        let opts = TcpSocketOptions::default().nodelay(true);

        // A failed bind leaves the runtime usable.
        let err = TetoTcpListener::bind(&rt, sa("10.0.0.99:8080"), opts.clone()).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AddrNotAvailable, "{err:?}");
        let mut a = TetoTcpListener::bind(&rt, sa("0.0.0.0:8080"), opts.clone()).await.expect("bind 8080");
        let err = TetoTcpListener::bind(&rt, sa("0.0.0.0:8080"), opts.clone()).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AddrInUse, "{err:?}");

        // Several listeners and a UDP socket on one runtime.
        let mut b = TetoTcpListener::bind(&rt, sa("0.0.0.0:8081"), opts.clone()).await.expect("bind 8081");
        let udp = TetoUdpSocket::bind(&rt, sa("0.0.0.0:9000")).await.expect("bind udp");
        check_listener(&mut a, sa("10.0.0.1:8080"), b"via 8080").await;
        check_listener(&mut b, sa("10.0.0.1:8081"), b"via 8081").await;

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
            loop {
                match peer.recv_from(&mut b) {
                    Ok((n, _)) => return b[..n].to_vec(),
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => panic!("udp reply: {e}"),
                }
            }
        })
        .await;
        assert_eq!(got, b"udp reply");

        // UDP receive pauses while the application's queue is full and
        // resumes once it has been drained.
        let udp2 = TetoUdpSocket::bind(&rt, sa("0.0.0.0:9001")).await.expect("bind udp 9001");
        let flood = blocking(|| {
            let c = StdUdpSocket::bind("0.0.0.0:0").unwrap();
            for i in 0..3000u32 {
                let _ = c.send_to(&i.to_be_bytes(), "10.0.0.1:9001");
            }
            c
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut drained = 0;
        while timeout(Duration::from_millis(300), udp2.recv_from(&mut buf)).await.is_ok() {
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
            let (n, _) = timeout(T, udp2.recv_from(&mut buf)).await.expect("receive resumed").unwrap();
            if &buf[..n] == b"after" {
                after += 1;
            }
        }

        // F1: outbound connections.
        kernel_echo_server(9100);
        let mut up = timeout(T, TetoTcpStream::connect(&rt, sa("10.0.0.2:9100"), opts.clone()))
            .await
            .unwrap()
            .expect("connect");
        assert_eq!(up.peer_addr(), sa("10.0.0.2:9100"));
        assert_eq!(up.local_addr().ip().to_string(), "10.0.0.1");
        const LEN: usize = 4 * 1024 * 1024;
        let (mut r, mut w) = tokio::io::split(&mut up);
        let writer = async {
            w.write_all(&pattern(LEN)).await.unwrap();
            w.shutdown().await.unwrap();
        };
        let reader = async {
            let mut v = Vec::with_capacity(LEN);
            r.read_to_end(&mut v).await.unwrap();
            v
        };
        let ((), echoed) = timeout(Duration::from_secs(60), async { tokio::join!(writer, reader) })
            .await
            .unwrap();
        assert_eq!(echoed.len(), LEN);
        assert!(echoed == pattern(LEN), "data corrupted");
        drop(up);

        let err = timeout(T, TetoTcpStream::connect(&rt, sa("10.0.0.2:9101"), opts.clone()))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ConnectionRefused, "{err:?}");

        // A connect nobody answers can be cancelled; the runtime carries on.
        let pending = timeout(Duration::from_millis(500), TetoTcpStream::connect(&rt, sa("10.0.0.77:80"), opts.clone())).await;
        assert!(pending.is_err(), "connect to a silent address finished: {pending:?}");
        let mut again = timeout(T, TetoTcpStream::connect(&rt, sa("10.0.0.2:9100"), opts.clone()))
            .await
            .unwrap()
            .expect("connect after cancelled connect");
        again.write_all(b"ping").await.unwrap();
        let mut pong = [0u8; 4];
        timeout(T, again.read_exact(&mut pong)).await.unwrap().unwrap();
        assert_eq!(&pong, b"ping");

        // Once every handle is gone the F-Stack thread exits.
        drop((rt, a, b, udp, udp2, again));
        let deadline = Instant::now() + Duration::from_secs(15);
        while fstack_thread_running() {
            assert!(Instant::now() < deadline, "F-Stack thread didn't exit");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}

/// Whether this process still has F-Stack's thread (`TetoRuntime` names it
/// "fstack"). It exits after teardown, which is how these tests observe that
/// the runtime stopped.
fn fstack_thread_running() -> bool {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .any(|comm| comm.trim() == "fstack")
}

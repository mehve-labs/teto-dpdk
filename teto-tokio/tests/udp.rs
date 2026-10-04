//! UDP integration tests against a live F-Stack instance (see tests/tcp.rs
//! for the environment this needs).

use std::collections::HashSet;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use teto_dpdk::FStackConfig;
use teto_tokio::TetoUdpSocket;

fn config() -> FStackConfig {
    FStackConfig::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../config.ini"))
        .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
        .with_eal_arg("--no-pci")
        .with_eal_arg("--iova-mode=va")
}

const ADDR: &str = "10.0.0.1:8080";

#[test]
fn udp_suite() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let v6 = TetoUdpSocket::bind(config(), "[::1]:8080".parse().unwrap()).await;
        assert_eq!(v6.err().unwrap().kind(), ErrorKind::InvalidInput);

        let server = Arc::new(TetoUdpSocket::bind(config(), ADDR.parse().unwrap()).await.expect("bind"));
        let v6 = server.send_to(b"x", "[::1]:9".parse().unwrap()).await;
        assert_eq!(v6.err().unwrap().kind(), ErrorKind::InvalidInput);

        // Echo server.
        let echo = server.clone();
        let echo_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                let (n, peer) = echo.recv_from(&mut buf).await.unwrap();
                echo.send_to(&buf[..n], peer).await.unwrap();
            }
        });

        tokio::task::spawn_blocking(|| {
            let client = StdUdpSocket::bind("0.0.0.0:0").unwrap();
            client.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
            let server: SocketAddr = ADDR.parse().unwrap();

            // Wait for the TAP device to be configured.
            let deadline = Instant::now() + Duration::from_secs(90);
            let mut buf = [0u8; 65535];
            loop {
                // Fails with ENETUNREACH until the TAP device is configured.
                // (recv errors, including EINTR from DPDK's TAP signal, just retry.)
                if client.send_to(b"ping", server).is_err() {
                    std::thread::sleep(Duration::from_millis(500));
                }
                if let Ok((n, _)) = client.recv_from(&mut buf) {
                    assert_eq!(&buf[..n], b"ping");
                    break;
                }
                assert!(Instant::now() < deadline, "F-Stack never became reachable");
            }
            while client.recv_from(&mut buf).is_ok() {}

            // P3: a burst is drained in batches, not one datagram per tick.
            // Echoes are read on another thread while sending, so the
            // client's own receive buffer doesn't overflow.
            const N: usize = 500;
            let reader = client.try_clone().unwrap();
            let collector = std::thread::spawn(move || {
                reader.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut buf = [0u8; 65535];
                let mut seen = HashSet::new();
                while seen.len() < N {
                    match reader.recv_from(&mut buf) {
                        Ok((n, _)) => {
                            let s = std::str::from_utf8(&buf[..n]).unwrap();
                            if !s.starts_with("dgram-") {
                                continue; // late echo of a warm-up ping
                            }
                            let i: usize = s[6..10].parse().unwrap();
                            assert_eq!(s, format!("dgram-{i:04}-{}", "x".repeat(i % 900)));
                            seen.insert(i);
                        }
                        // DPDK's TAP driver signals the process on every packet,
                        // and sockets with a read timeout aren't restarted.
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                seen
            });
            for i in 0..N {
                let msg = format!("dgram-{i:04}-{}", "x".repeat(i % 900));
                client.send_to(msg.as_bytes(), server).unwrap();
            }
            let seen = collector.join().unwrap();
            // UDP over TAP should be lossless at this rate; allow a little slack.
            assert!(seen.len() >= N * 95 / 100, "only {} of {N} echoes received", seen.len());
        })
        .await
        .unwrap();

        // Datagrams queued right before the socket is dropped still go out,
        // and the F-Stack thread exits afterwards.
        echo_task.abort();
        let _ = echo_task.await;
        let client = StdUdpSocket::bind("0.0.0.0:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let client_addr: SocketAddr = format!("10.0.0.2:{}", client.local_addr().unwrap().port()).parse().unwrap();
        const LAST: usize = 20;
        for i in 0..LAST {
            server.send_to(format!("last-{i}").as_bytes(), client_addr).await.unwrap();
        }
        drop(Arc::try_unwrap(server).ok().expect("socket still shared"));
        let got = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 64];
            let mut n = 0;
            while n < LAST {
                match client.recv_from(&mut buf) {
                    Ok(_) => n += 1,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            n
        })
        .await
        .unwrap();
        assert_eq!(got, LAST, "datagrams lost when the socket was dropped");

        let deadline = Instant::now() + Duration::from_secs(15);
        while std::path::Path::new("/sys/class/net/dtap0").exists() {
            assert!(Instant::now() < deadline, "F-Stack thread didn't exit");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}

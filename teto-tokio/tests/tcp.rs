//! TCP integration tests against a live F-Stack instance.
//!
//! Requires the project's Docker environment (privileged container running
//! `entrypoint.sh`, which configures the kernel side of the TAP device). The
//! server runs on F-Stack at 10.0.0.1; clients are kernel sockets reaching it
//! through dtap0. F-Stack can be initialised once per process, so all
//! scenarios share one listener inside a single test.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream as StdTcpStream};
use std::time::{Duration, Instant};

use teto_dpdk::{FStackConfig, TcpSocketOptions};
use teto_tokio::{TetoTcpListener, TetoTcpStream, TetoUdpSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

const ADDR: &str = "10.0.0.1:8080";
const T: Duration = Duration::from_secs(10);

fn config() -> FStackConfig {
    FStackConfig::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../config.ini"))
        .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
        .with_eal_arg("--no-pci")
        .with_eal_arg("--iova-mode=va")
}

fn addr() -> SocketAddr {
    ADDR.parse().unwrap()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn connect() -> StdTcpStream {
    let s = StdTcpStream::connect_timeout(&addr(), Duration::from_secs(5)).expect("connect");
    s.set_read_timeout(Some(T)).unwrap();
    s.set_write_timeout(Some(T)).unwrap();
    s
}

/// The kernel side of dtap0 is configured asynchronously after F-Stack
/// creates it; retry until a connection gets through.
async fn wait_until_reachable(listener: &mut TetoTcpListener) {
    let client = tokio::task::spawn_blocking(|| {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            match StdTcpStream::connect_timeout(&addr(), Duration::from_secs(1)) {
                Ok(s) => return s,
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => panic!("F-Stack never became reachable: {e}"),
            }
        }
    });
    let client = client.await.unwrap();
    let (server, _) = timeout(T, listener.accept()).await.unwrap().unwrap();
    drop(client);
    drop(server);
}

async fn pair(listener: &mut TetoTcpListener) -> (TetoTcpStream, StdTcpStream) {
    let client = tokio::task::spawn_blocking(connect);
    let (server, peer) = timeout(T, listener.accept()).await.expect("accept timed out").unwrap();
    let client = client.await.unwrap();
    assert_eq!(peer, client.local_addr().unwrap());
    (server, client)
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.unwrap()
}

async fn echo_check(server: &mut TetoTcpStream, client: StdTcpStream, msg: &'static [u8]) -> StdTcpStream {
    let mut client = blocking(move || {
        let mut client = client;
        client.write_all(msg).unwrap();
        client
    })
    .await;
    let mut buf = vec![0u8; msg.len()];
    timeout(T, server.read_exact(&mut buf)).await.unwrap().unwrap();
    assert_eq!(buf, msg);
    timeout(T, server.write_all(&buf)).await.unwrap().unwrap();
    blocking(move || {
        let mut got = vec![0u8; msg.len()];
        client.read_exact(&mut got).unwrap();
        assert_eq!(got, msg);
        client
    })
    .await
}

#[test]
fn tcp_suite() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        // Rejected before F-Stack is touched.
        let v6 = TetoTcpListener::bind(config(), "[::1]:8080".parse().unwrap(), TcpSocketOptions::default()).await;
        assert_eq!(v6.err().unwrap().kind(), ErrorKind::InvalidInput);
        #[allow(deprecated)]
        let quickack = TcpSocketOptions::default().quickack(true);
        let qa = TetoTcpListener::bind(config(), addr(), quickack).await;
        assert_eq!(qa.err().unwrap().kind(), ErrorKind::Unsupported);

        let opts = TcpSocketOptions::default().nodelay(true).keepalive(true);
        let mut listener = TetoTcpListener::bind(config(), addr(), opts).await.expect("bind");
        assert_eq!(listener.local_addr(), addr());

        // F-Stack is a per-process singleton.
        let again = TetoTcpListener::bind(config(), "10.0.0.1:8081".parse().unwrap(), TcpSocketOptions::default()).await;
        assert_eq!(again.err().unwrap().kind(), ErrorKind::AlreadyExists);
        let udp = TetoUdpSocket::bind(config(), "10.0.0.1:9000".parse().unwrap()).await;
        assert_eq!(udp.err().unwrap().kind(), ErrorKind::AlreadyExists);

        wait_until_reachable(&mut listener).await;

        basic_echo(&mut listener).await;
        peer_half_close(&mut listener).await;
        server_half_close(&mut listener).await;
        reset_and_stale_handle(&mut listener).await;
        drop_then_reuse(&mut listener).await;
        slow_reader_backpressure(&mut listener).await;
        slow_consumer_receive(&mut listener).await;
        drop_flushes_pending_writes(&mut listener).await;
        many_connections(&mut listener).await;
        reset_after_peer_half_close(&mut listener).await;
        dropped_listener_refuses(listener).await;
    });
}

async fn basic_echo(listener: &mut TetoTcpListener) {
    let (mut server, client) = pair(listener).await;
    let client = echo_check(&mut server, client, b"hello").await;
    drop(client);
    let mut rest = Vec::new();
    timeout(T, server.read_to_end(&mut rest)).await.unwrap().unwrap();
    assert!(rest.is_empty());
}

/// C4: the peer shuts down its write side and waits for the reply.
async fn peer_half_close(listener: &mut TetoTcpListener) {
    let (mut server, mut client) = pair(listener).await;
    client.write_all(b"request").unwrap();
    client.shutdown(Shutdown::Write).unwrap();

    let mut req = Vec::new();
    timeout(T, server.read_to_end(&mut req)).await.unwrap().unwrap();
    assert_eq!(req, b"request");
    // EOF is sticky.
    assert_eq!(timeout(T, server.read(&mut [0u8; 8])).await.unwrap().unwrap(), 0);

    timeout(T, server.write_all(b"response")).await.unwrap().unwrap();
    timeout(T, server.shutdown()).await.unwrap().unwrap();
    let reply = blocking(move || {
        let mut v = Vec::new();
        client.read_to_end(&mut v).unwrap();
        v
    })
    .await;
    assert_eq!(reply, b"response");
}

/// Our shutdown sends FIN but the peer can keep sending.
async fn server_half_close(listener: &mut TetoTcpListener) {
    let (mut server, mut client) = pair(listener).await;
    timeout(T, server.write_all(b"bye")).await.unwrap().unwrap();
    timeout(T, server.shutdown()).await.unwrap().unwrap();
    let err = server.write_all(b"more").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);

    let client = blocking(move || {
        let mut v = Vec::new();
        client.read_to_end(&mut v).unwrap();
        assert_eq!(v, b"bye");
        client.write_all(b"still talking").unwrap();
        client
    })
    .await;
    let mut buf = [0u8; 13];
    timeout(T, server.read_exact(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf, b"still talking");
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(timeout(T, server.read(&mut [0u8; 1])).await.unwrap().unwrap(), 0);
}

/// C6 + C1: a reset surfaces as an error, and the dead handle can't touch the
/// connection that reuses its descriptor.
async fn reset_and_stale_handle(listener: &mut TetoTcpListener) {
    let (mut a, ca) = pair(listener).await;
    timeout(T, a.write_all(b"unread")).await.unwrap().unwrap();
    timeout(T, a.flush()).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Closing with unread data makes the kernel send RST.
    drop(ca);

    let err = timeout(T, a.read(&mut [0u8; 16])).await.unwrap().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ConnectionReset, "{err:?}");

    // The driver has closed a's descriptor; the next connection likely gets it.
    let (mut b, cb) = pair(listener).await;

    let err = a.write_all(b"STALE").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ConnectionReset);
    drop(a);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let cb = blocking(move || {
        cb.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut buf = [0u8; 16];
        loop {
            match (&cb).read(&mut buf) {
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                // DPDK's TAP driver signals the process on every packet, and
                // sockets with a read timeout aren't restarted.
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                other => panic!("new connection received stale data or closed: {other:?}"),
            }
        }
        cb.set_read_timeout(Some(T)).unwrap();
        cb
    })
    .await;
    echo_check(&mut b, cb, b"still alive").await;
}

/// Dropping a stream closes it; the next connection is unaffected.
async fn drop_then_reuse(listener: &mut TetoTcpListener) {
    for _ in 0..5 {
        let (a, mut ca) = pair(listener).await;
        drop(a);
        let n = blocking(move || ca.read(&mut [0u8; 4]).unwrap()).await;
        assert_eq!(n, 0);
        let (mut b, cb) = pair(listener).await;
        echo_check(&mut b, cb, b"fresh").await;
    }
}

/// C2 + C5 (write side): writing to a peer that doesn't read applies
/// backpressure instead of buffering without bound or freezing the stack.
async fn slow_reader_backpressure(listener: &mut TetoTcpListener) {
    const LEN: usize = 16 * 1024 * 1024;
    let (mut slow, mut slow_client) = pair(listener).await;
    let writer = tokio::spawn(async move {
        slow.write_all(&pattern(LEN)).await.unwrap();
        slow.shutdown().await.unwrap();
    });

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!writer.is_finished(), "16 MiB write to a non-reading peer completed: no backpressure");

    // The stack is still serving other connections.
    let (mut other, other_client) = pair(listener).await;
    echo_check(&mut other, other_client, b"not frozen").await;

    let got = blocking(move || {
        let mut v = Vec::with_capacity(LEN);
        slow_client.read_to_end(&mut v).unwrap();
        v
    })
    .await;
    assert_eq!(got.len(), LEN);
    assert!(got == pattern(LEN), "data corrupted");
    timeout(T, writer).await.unwrap().unwrap();
}

/// C5 (read side): a consumer that falls behind loses nothing; reading pauses
/// and resumes.
async fn slow_consumer_receive(listener: &mut TetoTcpListener) {
    const LEN: usize = 16 * 1024 * 1024;
    let (mut server, mut client) = pair(listener).await;
    let sender = tokio::task::spawn_blocking(move || {
        client.write_all(&pattern(LEN)).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        client
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!sender.is_finished(), "peer wrote 16 MiB to a consumer that wasn't reading");

    let mut got = Vec::with_capacity(LEN);
    timeout(Duration::from_secs(60), server.read_to_end(&mut got)).await.unwrap().unwrap();
    assert_eq!(got.len(), LEN);
    assert!(got == pattern(LEN), "data corrupted");
    sender.await.unwrap();
}

/// Bytes accepted by `write` are delivered even if the stream is dropped
/// right after.
async fn drop_flushes_pending_writes(listener: &mut TetoTcpListener) {
    const LEN: usize = 1024 * 1024;
    let (mut server, mut client) = pair(listener).await;
    let data = pattern(LEN);
    let mut sent = 0;
    // Fill the user-space buffer without waiting for it to drain.
    while sent < LEN {
        match timeout(Duration::from_millis(50), server.write(&data[sent..])).await {
            Ok(n) => sent += n.unwrap(),
            Err(_) => break,
        }
    }
    drop(server);
    let got = blocking(move || {
        let mut v = Vec::new();
        client.read_to_end(&mut v).unwrap();
        v
    })
    .await;
    assert_eq!(got.len(), sent);
    assert!(got[..] == data[..sent]);
}

/// P2 sanity: many concurrent connections all get served.
async fn many_connections(listener: &mut TetoTcpListener) {
    const N: usize = 200;
    let clients = tokio::task::spawn_blocking(|| {
        let handles: Vec<_> = (0..N)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut s = connect();
                    let msg = format!("conn-{i}");
                    s.write_all(msg.as_bytes()).unwrap();
                    s.shutdown(Shutdown::Write).unwrap();
                    let mut v = Vec::new();
                    s.read_to_end(&mut v).unwrap();
                    assert_eq!(v, msg.as_bytes());
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    });
    let mut tasks = Vec::new();
    for _ in 0..N {
        let (mut s, _) = timeout(T, listener.accept()).await.unwrap().unwrap();
        tasks.push(tokio::spawn(async move {
            let mut v = Vec::new();
            s.read_to_end(&mut v).await.unwrap();
            s.write_all(&v).await.unwrap();
            s.shutdown().await.unwrap();
        }));
    }
    for t in tasks {
        timeout(T, t).await.unwrap().unwrap();
    }
    timeout(Duration::from_secs(60), clients).await.unwrap().unwrap();
}

/// A reset that arrives while the stream is idle (peer already half-closed,
/// nothing buffered) is reported on the next write. (`shutdown` alone may
/// still succeed: F-Stack's `shutdown` on a reset socket returns 0.)
async fn reset_after_peer_half_close(listener: &mut TetoTcpListener) {
    let (mut server, client) = pair(listener).await;
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(timeout(T, server.read(&mut [0u8; 8])).await.unwrap().unwrap(), 0);
    timeout(T, server.write_all(b"unread")).await.unwrap().unwrap();
    timeout(T, server.flush()).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(client); // unread data: the kernel sends RST
    tokio::time::sleep(Duration::from_millis(300)).await;
    let res = async {
        server.write_all(b"after reset").await?;
        server.flush().await
    };
    let err = timeout(T, res).await.unwrap().unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::ConnectionReset | ErrorKind::BrokenPipe), "{err:?}");
}

/// Dropping the listener closes the listening socket: new clients are
/// refused instead of left waiting in the backlog, and existing streams keep
/// working.
async fn dropped_listener_refuses(mut listener: TetoTcpListener) {
    let (mut kept, kept_client) = pair(&mut listener).await;
    drop(listener);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let refused = blocking(|| StdTcpStream::connect_timeout(&addr(), Duration::from_secs(5))).await;
    assert_eq!(refused.unwrap_err().kind(), ErrorKind::ConnectionRefused);
    echo_check(&mut kept, kept_client, b"still served").await;
}

//! TCP behaviour against a live F-Stack (see tests/common for the setup).

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream as StdTcpStream};
use std::time::Duration;

use common::*;
use teto_dpdk::TcpSocketOptions;
use teto_tokio::{TetoRuntime, TetoTcpListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

const PORT: u16 = 8080;

#[tokio::test(flavor = "multi_thread")]
async fn runtime_is_a_process_wide_singleton() {
    let _rt = start().await;
    let again = TetoRuntime::start(config()).await.unwrap_err();
    assert_eq!(again.kind(), ErrorKind::AlreadyExists);
}

#[tokio::test(flavor = "multi_thread")]
async fn ipv6_is_rejected() {
    let rt = start().await;
    let err = TetoTcpListener::bind(&rt, sa("[::1]:8080"), TcpSocketOptions::default()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_and_clean_close() {
    let mut listener = listen(PORT).await;
    assert_eq!(listener.local_addr(), fstack(PORT));
    let (mut server, client) = pair(&mut listener, PORT).await;
    let client = echo_check(&mut server, client, b"hello").await;
    drop(client);
    let mut rest = Vec::new();
    timeout(T, server.read_to_end(&mut rest)).await.unwrap().unwrap();
    assert!(rest.is_empty());
}

/// The peer shuts down its write side and waits for the reply.
#[tokio::test(flavor = "multi_thread")]
async fn peer_half_close_still_gets_reply() {
    let mut listener = listen(PORT).await;
    let (mut server, mut client) = pair(&mut listener, PORT).await;
    client.write_all(b"request").unwrap();
    client.shutdown(Shutdown::Write).unwrap();

    let mut req = Vec::new();
    timeout(T, server.read_to_end(&mut req)).await.unwrap().unwrap();
    assert_eq!(req, b"request");
    // EOF is sticky.
    assert_eq!(timeout(T, server.read(&mut [0u8; 8])).await.unwrap().unwrap(), 0);

    timeout(T, server.write_all(b"response")).await.unwrap().unwrap();
    timeout(T, server.shutdown()).await.unwrap().unwrap();
    assert_eq!(blocking(move || read_all(&mut client)).await, b"response");
}

/// Our shutdown sends FIN, but the peer can keep sending.
#[tokio::test(flavor = "multi_thread")]
async fn server_half_close_keeps_reading() {
    let mut listener = listen(PORT).await;
    let (mut server, mut client) = pair(&mut listener, PORT).await;
    timeout(T, server.write_all(b"bye")).await.unwrap().unwrap();
    timeout(T, server.shutdown()).await.unwrap().unwrap();
    let err = server.write_all(b"more").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);

    let client = blocking(move || {
        assert_eq!(read_all(&mut client), b"bye");
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

/// A reset surfaces as an error, and the dead handle can't touch the
/// connection that reuses its descriptor.
#[tokio::test(flavor = "multi_thread")]
async fn reset_is_an_error_and_stale_handle_is_harmless() {
    let mut listener = listen(PORT).await;
    let (mut a, ca) = pair(&mut listener, PORT).await;
    timeout(T, a.write_all(b"unread")).await.unwrap().unwrap();
    timeout(T, a.flush()).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(ca); // closing with unread data makes the kernel send RST

    let err = timeout(T, a.read(&mut [0u8; 16])).await.unwrap().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ConnectionReset, "{err:?}");

    // a's descriptor is closed; the next connection most likely reuses it.
    let (mut b, cb) = pair(&mut listener, PORT).await;
    let err = a.write_all(b"STALE").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ConnectionReset);
    drop(a);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let cb = blocking(move || {
        cb.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        match (&cb).read(&mut [0u8; 16]) {
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            other => panic!("new connection received stale data or closed: {other:?}"),
        }
        cb.set_read_timeout(Some(T)).unwrap();
        cb
    })
    .await;
    echo_check(&mut b, cb, b"still alive").await;
}

/// A reset that arrives while the stream is idle (peer already half-closed,
/// nothing buffered) is reported on the next write. (`shutdown` alone may
/// still succeed: F-Stack's `shutdown` on a reset socket returns 0.)
#[tokio::test(flavor = "multi_thread")]
async fn idle_reset_reported_on_next_write() {
    let mut listener = listen(PORT).await;
    let (mut server, client) = pair(&mut listener, PORT).await;
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

/// Dropping a stream closes it; the next connection is unaffected.
#[tokio::test(flavor = "multi_thread")]
async fn drop_closes_and_next_connection_is_unaffected() {
    let mut listener = listen(PORT).await;
    for _ in 0..5 {
        let (a, mut ca) = pair(&mut listener, PORT).await;
        drop(a);
        assert_eq!(blocking(move || ca.read(&mut [0u8; 4]).unwrap()).await, 0);
        let (mut b, cb) = pair(&mut listener, PORT).await;
        echo_check(&mut b, cb, b"fresh").await;
    }
}

/// Writing to a peer that doesn't read applies backpressure instead of
/// buffering without bound or freezing the stack.
#[tokio::test(flavor = "multi_thread")]
async fn slow_reader_gets_backpressure_and_stack_stays_responsive() {
    const LEN: usize = 16 * 1024 * 1024;
    let mut listener = listen(PORT).await;
    let (mut slow, mut slow_client) = pair(&mut listener, PORT).await;
    let writer = tokio::spawn(async move {
        slow.write_all(&pattern(LEN, 0)).await.unwrap();
        slow.shutdown().await.unwrap();
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!writer.is_finished(), "16 MiB write to a non-reading peer completed: no backpressure");

    let (mut other, other_client) = pair(&mut listener, PORT).await;
    echo_check(&mut other, other_client, b"not frozen").await;

    let got = blocking(move || read_all(&mut slow_client)).await;
    assert_eq!(got.len(), LEN);
    assert!(got == pattern(LEN, 0), "data corrupted");
    timeout(T, writer).await.unwrap().unwrap();
}

/// A consumer that falls behind loses nothing: reading pauses and resumes.
#[tokio::test(flavor = "multi_thread")]
async fn slow_consumer_loses_nothing() {
    const LEN: usize = 16 * 1024 * 1024;
    let mut listener = listen(PORT).await;
    let (mut server, mut client) = pair(&mut listener, PORT).await;
    let sender = tokio::task::spawn_blocking(move || {
        client.write_all(&pattern(LEN, 0)).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        client
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!sender.is_finished(), "peer wrote 16 MiB to a consumer that wasn't reading");

    let mut got = Vec::with_capacity(LEN);
    timeout(Duration::from_secs(60), server.read_to_end(&mut got)).await.unwrap().unwrap();
    assert_eq!(got.len(), LEN);
    assert!(got == pattern(LEN, 0), "data corrupted");
    sender.await.unwrap();
}

/// Bytes accepted by `write` are delivered even if the stream is dropped
/// right after.
#[tokio::test(flavor = "multi_thread")]
async fn drop_delivers_buffered_writes() {
    const LEN: usize = 1024 * 1024;
    let mut listener = listen(PORT).await;
    let (mut server, mut client) = pair(&mut listener, PORT).await;
    let data = pattern(LEN, 0);
    let mut sent = 0;
    // Fill the user-space buffer without waiting for it to drain.
    while sent < LEN {
        match timeout(Duration::from_millis(50), server.write(&data[sent..])).await {
            Ok(n) => sent += n.unwrap(),
            Err(_) => break,
        }
    }
    drop(server);
    let got = blocking(move || read_all(&mut client)).await;
    assert_eq!(got.len(), sent);
    assert!(got[..] == data[..sent]);
}

#[tokio::test(flavor = "multi_thread")]
async fn many_concurrent_connections() {
    const N: usize = 200;
    let mut listener = listen(PORT).await;
    let clients = tokio::task::spawn_blocking(|| {
        let handles: Vec<_> = (0..N)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut s = connect(fstack(PORT));
                    let msg = format!("conn-{i}");
                    s.write_all(msg.as_bytes()).unwrap();
                    s.shutdown(Shutdown::Write).unwrap();
                    assert_eq!(read_all(&mut s), msg.as_bytes());
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    });
    let mut tasks = Vec::new();
    for _ in 0..N {
        let (mut s, _) = timeout(T * 3, listener.accept()).await.unwrap().unwrap();
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

/// Dropping the listener closes the listening socket: new clients are
/// refused instead of left waiting in the backlog, and existing streams keep
/// working. Once the last stream is dropped, its data still arrives in full
/// (with FIN), and only then does the F-Stack thread exit.
#[tokio::test(flavor = "multi_thread")]
async fn dropped_listener_refuses_and_last_stream_is_delivered() {
    const LEN: usize = 4 * 1024 * 1024;
    let mut listener = listen(PORT).await; // the listener holds the only runtime handle
    let (mut kept, kept_client) = pair(&mut listener, PORT).await;
    drop(listener);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let refused = blocking(|| StdTcpStream::connect_timeout(&fstack(PORT), Duration::from_secs(5))).await;
    assert_eq!(refused.unwrap_err().kind(), ErrorKind::ConnectionRefused);
    let mut kept_client = echo_check(&mut kept, kept_client, b"still served").await;

    let reader = tokio::task::spawn_blocking(move || read_all(&mut kept_client));
    timeout(T, kept.write_all(&pattern(LEN, 0))).await.unwrap().unwrap();
    drop(kept); // last socket, with data still queued: the runtime may now shut down
    let got = reader.await.unwrap();
    assert_eq!(got.len(), LEN);
    assert!(got == pattern(LEN, 0), "data corrupted");

    wait_for_fstack_exit().await;
}

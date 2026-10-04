//! Shared helpers for teto-tokio's integration tests.
//!
//! The tests need the project's Docker environment: F-Stack runs at 10.0.0.1
//! on one end of a veth pair created by `entrypoint.sh`, and clients are
//! kernel sockets on the other end (10.0.0.2). F-Stack can be started once
//! per process, so every test runs in its own process: use
//! `cargo nextest run` (see scripts/ci-test.sh).
#![allow(dead_code)] // each test file uses a different subset

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::time::{Duration, Instant};

use teto_dpdk::{FStackConfig, TcpSocketOptions};
use teto_tokio::{TetoRuntime, TetoTcpListener, TetoTcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

/// Generous per-step timeout (the CI machine may be emulating x86).
pub const T: Duration = Duration::from_secs(10);

pub fn config() -> FStackConfig {
    FStackConfig::for_docker()
        .with_config_file(concat!(env!("CARGO_MANIFEST_DIR"), "/../config.ini"))
        .capture_init_output(true)
}

pub fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// F-Stack's address with `port`.
pub fn fstack(port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], port))
}

/// Start the runtime, explaining the one-per-process rule if it's broken.
pub async fn start() -> TetoRuntime {
    match TetoRuntime::start(config()).await {
        Ok(rt) => rt,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            panic!("F-Stack can be started once per process: run the integration tests with `cargo nextest run`")
        }
        Err(e) => panic!("TetoRuntime::start: {e}"),
    }
}

/// Start the runtime and listen on 10.0.0.1:`port`.
pub async fn listen(port: u16) -> TetoTcpListener {
    let rt = start().await;
    TetoTcpListener::bind(&rt, fstack(port), TcpSocketOptions::default().nodelay(true)).await.expect("bind")
}

/// Deterministic test payload.
pub fn pattern(len: usize, seed: usize) -> Vec<u8> {
    (0..len).map(|i| ((i + seed) % 251) as u8).collect()
}

pub async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// Kernel-side client connection to `addr`, retrying briefly while F-Stack
/// comes up (startup and the first ARP exchange).
pub fn connect(addr: SocketAddr) -> StdTcpStream {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match StdTcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
            Ok(s) => {
                s.set_read_timeout(Some(T)).unwrap();
                s.set_write_timeout(Some(T)).unwrap();
                return s;
            }
            Err(e) => {
                assert!(Instant::now() < deadline, "{addr} not reachable: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

pub fn read_all(s: &mut StdTcpStream) -> Vec<u8> {
    let mut v = Vec::new();
    s.read_to_end(&mut v).expect("read to EOF");
    v
}

/// Connect a kernel client to `listener` (on `port`) and accept it.
pub async fn pair(listener: &mut TetoTcpListener, port: u16) -> (TetoTcpStream, StdTcpStream) {
    let client = tokio::task::spawn_blocking(move || connect(fstack(port)));
    let (server, peer) = timeout(T * 3, listener.accept()).await.expect("accept timed out").unwrap();
    let client = client.await.unwrap();
    assert_eq!(peer, client.local_addr().unwrap());
    (server, client)
}

/// The client sends `msg`; the server reads it and echoes it back.
pub async fn echo_check(server: &mut TetoTcpStream, mut client: StdTcpStream, msg: &'static [u8]) -> StdTcpStream {
    client = blocking(move || {
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

/// Whether this process still has F-Stack's thread (`TetoRuntime` names it
/// "fstack"); it exits after F-Stack's teardown.
pub fn fstack_thread_running() -> bool {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .any(|comm| comm.trim() == "fstack")
}

pub async fn wait_for_fstack_exit() {
    let deadline = Instant::now() + Duration::from_secs(15);
    while fstack_thread_running() {
        assert!(Instant::now() < deadline, "F-Stack thread didn't exit");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

//! Network fault injection: packet loss, outages and peers that vanish.
//!
//! Faults are applied to the kernel side of the TAP device with `tc` (needs
//! root, which the Docker test environment has):
//! - egress `netem` on dtap0 impairs kernel -> F-Stack packets;
//! - an ingress `gact` filter drops F-Stack -> kernel packets, which is what
//!   exercises F-Stack's own retransmission and keepalive timers.
//!
//! See tests/tcp.rs for the environment these tests need.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream as StdTcpStream};
use std::process::Command;
use std::time::{Duration, Instant};

use teto_dpdk::{FStackConfig, TcpSocketOptions};
use teto_tokio::{TetoRuntime, TetoTcpListener, TetoTcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

const DEV: &str = "dtap0";

fn config() -> FStackConfig {
    FStackConfig::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../config.ini"))
        .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
        .with_eal_arg("--no-pci")
        .with_eal_arg("--iova-mode=va")
        .capture_init_output(true)
}

fn addr() -> SocketAddr {
    "10.0.0.1:8080".parse().unwrap()
}

fn pattern(len: usize, seed: usize) -> Vec<u8> {
    (0..len).map(|i| ((i + seed) % 251) as u8).collect()
}

fn tc(args: &[&str]) {
    let out = Command::new("tc").args(args).output().expect("tc must be installed (iproute2)");
    assert!(out.status.success(), "tc {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Impairments on dtap0, removed when dropped (also if a test panics).
struct Faults;

impl Faults {
    fn new() -> Self {
        Self::clear();
        Faults
    }

    fn clear() {
        let _ = Command::new("tc").args(["qdisc", "del", "dev", DEV, "root"]).output();
        let _ = Command::new("tc").args(["qdisc", "del", "dev", DEV, "ingress"]).output();
    }

    /// Impair kernel -> F-Stack packets (`netem` options, e.g. `loss 3%`).
    fn to_fstack(&self, netem: &str) {
        let _ = Command::new("tc").args(["qdisc", "del", "dev", DEV, "root"]).output();
        let mut args = vec!["qdisc", "add", "dev", DEV, "root", "netem"];
        args.extend(netem.split_whitespace());
        tc(&args);
    }

    /// Drop one in `one_in` F-Stack -> kernel packets (`1` drops everything).
    fn drop_from_fstack(&self, one_in: u32) {
        let _ = Command::new("tc").args(["qdisc", "del", "dev", DEV, "ingress"]).output();
        tc(&["qdisc", "add", "dev", DEV, "ingress"]);
        let one_in = one_in.to_string();
        let mut args = vec![
            "filter", "add", "dev", DEV, "parent", "ffff:", "protocol", "ip", "u32", "match", "u32", "0", "0",
        ];
        if one_in == "1" {
            args.extend(["action", "drop"]);
        } else {
            args.extend(["action", "gact", "ok", "random", "netrand", "drop", &one_in]);
        }
        tc(&args);
    }

    fn heal(&self) {
        Self::clear();
    }

    /// Packets dropped so far by (netem on kernel -> F-Stack, filter on
    /// F-Stack -> kernel), to prove the impairments took effect.
    fn dropped(&self) -> (u64, u64) {
        let stat = |args: &[&str]| {
            let out = Command::new("tc").args(args).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.split("dropped ")
                .skip(1)
                .filter_map(|rest| rest.split(|c: char| !c.is_ascii_digit()).next()?.parse::<u64>().ok())
                .sum::<u64>()
        };
        (
            stat(&["-s", "qdisc", "show", "dev", DEV, "root"]),
            stat(&["-s", "filter", "show", "dev", DEV, "parent", "ffff:"]),
        )
    }
}

impl Drop for Faults {
    fn drop(&mut self) {
        Self::clear();
    }
}

fn connect_kernel() -> StdTcpStream {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match StdTcpStream::connect_timeout(&addr(), Duration::from_secs(1)) {
            Ok(s) => return s,
            Err(e) => {
                assert!(Instant::now() < deadline, "F-Stack never became reachable: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Read until EOF, retrying EINTR (DPDK's TAP driver signals the process).
fn read_all(s: &mut StdTcpStream) -> Vec<u8> {
    let mut v = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return v,
            Ok(n) => v.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => panic!("read after {} bytes: {e}", v.len()),
        }
    }
}

async fn accept(listener: &mut TetoTcpListener) -> TetoTcpStream {
    timeout(Duration::from_secs(60), listener.accept()).await.expect("accept timed out").unwrap().0
}

/// Kernel client: send `len` bytes, half-close, return everything echoed.
fn client_echo(len: usize, seed: usize) -> Vec<u8> {
    let mut s = connect_kernel();
    let mut reader = s.try_clone().unwrap();
    let echo = std::thread::spawn(move || read_all(&mut reader));
    s.write_all(&pattern(len, seed)).unwrap();
    s.shutdown(Shutdown::Write).unwrap();
    echo.join().unwrap()
}

/// Server side: echo until EOF, then close.
async fn serve_echo(mut server: TetoTcpStream) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = server.read(&mut buf).await.expect("server read");
        if n == 0 {
            break;
        }
        server.write_all(&buf[..n]).await.expect("server write");
    }
    server.shutdown().await.expect("server shutdown");
}

#[test]
fn fault_suite() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let rt = TetoRuntime::start(config()).await.expect("start");
        let mut listener =
            TetoTcpListener::bind(&rt, addr(), TcpSocketOptions::default().nodelay(true)).await.expect("bind");
        // Reachability (the TAP device is configured asynchronously).
        let warmup = tokio::task::spawn_blocking(connect_kernel);
        drop(accept(&mut listener).await);
        drop(warmup.await.unwrap());

        lossy_link(&mut listener).await;
        outage_from_fstack(&mut listener).await;
        outage_to_fstack(&mut listener).await;
        silent_peer_detected_by_keepalive(&rt).await;
    });
}

/// Loss, delay and reordering in both directions: every byte still arrives,
/// in order, on several concurrent connections.
async fn lossy_link(listener: &mut TetoTcpListener) {
    const LEN: usize = 512 * 1024;
    let faults = Faults::new();
    faults.to_fstack("delay 2ms 1ms loss 3% reorder 5% 50%");
    faults.drop_from_fstack(30);
    let started = Instant::now();
    for round in 0..3 {
        let clients: Vec<_> = (0..4)
            .map(|i| {
                let seed = round * 10 + i;
                (seed, tokio::task::spawn_blocking(move || client_echo(LEN, seed)))
            })
            .collect();
        let mut servers = Vec::new();
        for _ in 0..4 {
            servers.push(tokio::spawn(serve_echo(accept(listener).await)));
        }
        for (seed, client) in clients {
            let echoed = timeout(Duration::from_secs(180), client).await.expect("lossy transfer stalled").unwrap();
            assert_eq!(echoed.len(), LEN, "echo length (seed {seed})");
            assert!(echoed == pattern(LEN, seed), "echo corrupted (seed {seed})");
        }
        for server in servers {
            timeout(Duration::from_secs(30), server).await.unwrap().unwrap();
        }
    }
    let (to_fstack, from_fstack) = faults.dropped();
    faults.heal();
    assert!(to_fstack > 0 && from_fstack > 0, "impairments had no effect: {to_fstack}/{from_fstack} drops");
    eprintln!(
        "lossy_link: 12 x 512 KiB echoes in {:?}; dropped {to_fstack} packets to F-Stack, {from_fstack} from it",
        started.elapsed()
    );
}

/// F-Stack's packets are black-holed for 3 s mid-transfer: its
/// retransmission timer must recover the connection.
async fn outage_from_fstack(listener: &mut TetoTcpListener) {
    let faults = Faults::new();
    let len = 2 * 1024 * 1024;
    let client = tokio::task::spawn_blocking(move || {
        let mut s = connect_kernel();
        s.write_all(b"SEND").unwrap();
        read_all(&mut s)
    });
    let mut server = accept(listener).await;
    let mut cmd = [0u8; 4];
    server.read_exact(&mut cmd).await.unwrap();
    let writer = tokio::spawn(async move {
        server.write_all(&pattern(len, 7)).await.unwrap();
        server.shutdown().await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    faults.drop_from_fstack(1);
    tokio::time::sleep(Duration::from_secs(3)).await;
    faults.heal();
    let healed = Instant::now();
    let got = timeout(Duration::from_secs(60), client).await.expect("no recovery after outage").unwrap();
    timeout(Duration::from_secs(10), writer).await.unwrap().unwrap();
    assert_eq!(got.len(), len);
    assert!(got == pattern(len, 7), "data corrupted");
    eprintln!("outage_from_fstack: recovered {:?} after the outage ended", healed.elapsed());
}

/// The kernel's packets to F-Stack are lost for 3 s mid-upload.
async fn outage_to_fstack(listener: &mut TetoTcpListener) {
    let faults = Faults::new();
    let len = 2 * 1024 * 1024;
    let client = tokio::task::spawn_blocking(move || {
        let mut s = connect_kernel();
        s.write_all(&pattern(len, 11)).unwrap();
        s.shutdown(Shutdown::Write).unwrap();
        read_all(&mut s)
    });
    let mut server = accept(listener).await;
    let reader = tokio::spawn(async move {
        let mut v = Vec::with_capacity(len);
        server.read_to_end(&mut v).await.unwrap();
        server.write_all(b"done").await.unwrap();
        server.shutdown().await.unwrap();
        v
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    faults.to_fstack("loss 100%");
    tokio::time::sleep(Duration::from_secs(3)).await;
    faults.heal();
    let got = timeout(Duration::from_secs(60), reader).await.expect("no recovery after outage").unwrap();
    assert_eq!(got.len(), len);
    assert!(got == pattern(len, 11), "data corrupted");
    assert_eq!(timeout(Duration::from_secs(10), client).await.unwrap().unwrap(), b"done");
}

/// A peer that disappears without FIN or RST is detected by TCP keepalive
/// and reported as an error, instead of the stream waiting forever.
async fn silent_peer_detected_by_keepalive(rt: &TetoRuntime) {
    let keepalive = TcpSocketOptions::default()
        .keepalive(true)
        .keepalive_idle_secs(1)
        .keepalive_interval_secs(1)
        .keepalive_count(3);
    let mut listener = TetoTcpListener::bind(rt, "10.0.0.1:8081".parse().unwrap(), keepalive).await.expect("bind 8081");
    let client = tokio::task::spawn_blocking(|| StdTcpStream::connect("10.0.0.1:8081").unwrap());
    let (mut server, _) = timeout(Duration::from_secs(10), listener.accept()).await.unwrap().unwrap();
    let client = client.await.unwrap();

    let faults = Faults::new();
    faults.drop_from_fstack(1);
    faults.to_fstack("loss 100%");
    let started = Instant::now();
    let err = timeout(Duration::from_secs(30), server.read(&mut [0u8; 16]))
        .await
        .expect("keepalive never declared the peer dead")
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TimedOut, "{err:?}");
    eprintln!("silent peer detected after {:?}", started.elapsed());
    faults.heal();
    drop(client);
}

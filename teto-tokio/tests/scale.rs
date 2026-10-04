//! Many concurrent connections, then sustained connection churn with a
//! memory check. The RSS check catches leaks of heap memory (Rust buffers,
//! F-Stack's sockets and PCBs, which use the host allocator); DPDK's mbuf pool
//! is preallocated, so mbuf leaks don't show up in it. Churn length: `TETO_SOAK_SECS` (default 15; set it to hours
//! for a soak run). See tests/tcp.rs for the environment these tests need.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use teto_dpdk::{FStackConfig, TcpSocketOptions};
use teto_tokio::{TetoRuntime, TetoTcpListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream as KernelTcpStream;
use tokio::time::timeout;

const CONCURRENT: usize = 1000;
const CHURN_WORKERS: usize = 32;

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

/// Sets sysctls for the test and restores the previous values when dropped,
/// so running this outside a container doesn't leave the host retuned.
struct Sysctls(Vec<(String, String)>);

impl Sysctls {
    fn set(values: &[(&str, &str)]) -> Self {
        let mut saved = Vec::new();
        for (key, value) in values {
            let old = std::process::Command::new("sysctl").args(["-n", key]).output().expect("sysctl");
            let old = String::from_utf8_lossy(&old.stdout).trim().to_owned();
            let out = std::process::Command::new("sysctl").args(["-w", &format!("{key}={value}")]).output();
            assert!(out.is_ok_and(|o| o.status.success()), "sysctl {key} failed (needs root)");
            saved.push((key.to_string(), old));
        }
        Sysctls(saved)
    }
}

impl Drop for Sysctls {
    fn drop(&mut self) {
        for (key, old) in &self.0 {
            let _ = std::process::Command::new("sysctl").args(["-w", &format!("{key}={old}")]).output();
        }
    }
}

/// Resident set size of this process, in KiB.
fn rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// One kernel-side client exchange: send `msg`, half-close, expect it back.
async fn exchange(msg: Vec<u8>) {
    let mut s = timeout(Duration::from_secs(30), KernelTcpStream::connect(addr()))
        .await
        .expect("connect timed out")
        .expect("connect");
    s.write_all(&msg).await.unwrap();
    s.shutdown().await.unwrap();
    let mut back = Vec::new();
    timeout(Duration::from_secs(30), s.read_to_end(&mut back)).await.expect("echo timed out").unwrap();
    assert_eq!(back, msg);
}

#[test]
fn scale_suite() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        // Load-generator tuning for the kernel-side client: it closes first,
        // so every connection leaves a TIME_WAIT on its side, and at churn
        // rates the default port range runs out. Restored afterwards.
        let _tuning = Sysctls::set(&[("net.ipv4.ip_local_port_range", "1024 65535"), ("net.ipv4.tcp_tw_reuse", "1")]);
        let rt = TetoRuntime::start(config()).await.expect("start");
        let mut listener =
            TetoTcpListener::bind(&rt, addr(), TcpSocketOptions::default().nodelay(true)).await.expect("bind");

        // Echo server: one task per connection.
        let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = served.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.expect("accept");
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut v = Vec::new();
                    if s.read_to_end(&mut v).await.is_ok() && s.write_all(&v).await.is_ok() {
                        let _ = s.shutdown().await;
                        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
        });

        // Wait for the TAP device.
        let deadline = Instant::now() + Duration::from_secs(90);
        while !matches!(timeout(Duration::from_secs(2), KernelTcpStream::connect(addr())).await, Ok(Ok(_))) {
            assert!(Instant::now() < deadline, "F-Stack never became reachable");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Many connections open at once: all connect first, then all talk.
        let started = Instant::now();
        let mut conns = Vec::with_capacity(CONCURRENT);
        for _ in 0..CONCURRENT {
            conns.push(timeout(Duration::from_secs(30), KernelTcpStream::connect(addr())).await.unwrap().unwrap());
        }
        let tasks: Vec<_> = conns
            .into_iter()
            .enumerate()
            .map(|(i, mut s)| {
                tokio::spawn(async move {
                    let msg = format!("conn-{i}-").repeat(64).into_bytes();
                    s.write_all(&msg).await.unwrap();
                    s.shutdown().await.unwrap();
                    let mut back = Vec::new();
                    timeout(Duration::from_secs(60), s.read_to_end(&mut back)).await.unwrap().unwrap();
                    assert_eq!(back, msg, "connection {i}");
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        eprintln!("{CONCURRENT} concurrent connections served in {:?}", started.elapsed());

        // Churn: short connections back to back; memory must stay flat.
        let soak = Duration::from_secs(std::env::var("TETO_SOAK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(15));
        // Warm-up pass so allocator pools and buffers reach steady state.
        for i in 0..200 {
            exchange(format!("warm-{i}").into_bytes()).await;
        }
        // The server counts a connection after its shutdown completes, which
        // can be just after the client saw EOF: let warm-up counts settle.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let rss_before = rss_kib();
        let served_before = served.load(std::sync::atomic::Ordering::Relaxed);
        let end = Instant::now() + soak;
        let workers: Vec<_> = (0..CHURN_WORKERS)
            .map(|w| {
                tokio::spawn(async move {
                    let mut n = 0u64;
                    while Instant::now() < end {
                        exchange(format!("churn-{w}-{n}").into_bytes()).await;
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        let mut total = 0;
        for w in workers {
            total += w.await.unwrap();
        }
        // Let dropped connections finish closing before measuring.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let rss_after = rss_kib();
        let served_during = served.load(std::sync::atomic::Ordering::Relaxed) - served_before;
        eprintln!(
            "churn: {total} connections in {soak:?} ({:.0}/s), server completed {served_during}; RSS {} -> {} MiB",
            total as f64 / soak.as_secs_f64(),
            rss_before / 1024,
            rss_after / 1024
        );
        assert_eq!(served_during as u64, total, "server didn't complete every churned connection");
        let growth_mib = rss_after.saturating_sub(rss_before) / 1024;
        assert!(growth_mib < 32, "RSS grew by {growth_mib} MiB during churn (leak?)");

        // Still healthy afterwards.
        exchange(b"after churn".to_vec()).await;
    });
}

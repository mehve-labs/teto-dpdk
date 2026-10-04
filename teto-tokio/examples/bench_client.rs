//! Echo benchmark client (kernel sockets). Point it at any TCP echo server —
//! `tcp_echo_async` (teto/F-Stack) or `kernel_echo` (tokio::net) — and compare.
//!
//!   cargo run --release -p teto-tokio --example bench_client -- <addr> rtt [count] [size]
//!   cargo run --release -p teto-tokio --example bench_client -- <addr> throughput [secs] [conns] [size]
//!
//! `rtt`: one connection, `count` sequential round trips of `size` bytes;
//! prints latency percentiles. `throughput`: `conns` connections each echoing
//! `size`-byte writes for `secs` seconds (pipelined: writing and reading the
//! echo concurrently); prints the aggregate echoed rate.
//!
//! See scripts/bench.sh for how to run a like-for-like comparison and what
//! the numbers do and don't mean.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn arg<T: std::str::FromStr>(args: &[String], i: usize, default: T) -> T {
    args.get(i).and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn connect(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).expect("connect");
    s.set_nodelay(true).unwrap();
    s
}

fn rtt(addr: SocketAddr, count: usize, size: usize) {
    let mut s = connect(addr);
    let msg = vec![0x5a; size];
    let mut buf = vec![0; size];
    // Warm up.
    for _ in 0..count.min(1000) / 10 {
        s.write_all(&msg).unwrap();
        s.read_exact(&mut buf).unwrap();
    }
    let mut samples = Vec::with_capacity(count);
    let start = Instant::now();
    for _ in 0..count {
        let t = Instant::now();
        s.write_all(&msg).unwrap();
        s.read_exact(&mut buf).unwrap();
        samples.push(t.elapsed());
    }
    let total = start.elapsed();
    samples.sort();
    let pct = |p: f64| samples[((samples.len() as f64 * p) as usize).min(samples.len() - 1)];
    println!(
        "rtt {addr} n={count} size={size}B: p50={:?} p90={:?} p99={:?} p99.9={:?} max={:?} ({:.0} rt/s)",
        pct(0.50),
        pct(0.90),
        pct(0.99),
        pct(0.999),
        samples[samples.len() - 1],
        count as f64 / total.as_secs_f64()
    );
}

fn throughput(addr: SocketAddr, secs: u64, conns: usize, size: usize) {
    // Pipelined: per connection one thread writes continuously while another
    // reads the echo, so this measures bulk transfer, not round trips.
    let stop = Arc::new(AtomicBool::new(false));
    let bytes = Arc::new(AtomicU64::new(0));
    let mut threads = Vec::new();
    for _ in 0..conns {
        let mut writer = connect(addr);
        let mut reader = writer.try_clone().unwrap();
        let (stop_w, stop_r, bytes) = (stop.clone(), stop.clone(), bytes.clone());
        threads.push(std::thread::spawn(move || {
            let msg = vec![0x5a; size];
            while !stop_w.load(Ordering::Relaxed) {
                if writer.write_all(&msg).is_err() {
                    break;
                }
            }
            let _ = writer.shutdown(std::net::Shutdown::Write);
        }));
        threads.push(std::thread::spawn(move || {
            let mut buf = vec![0; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) if !stop_r.load(Ordering::Relaxed) => {
                        bytes.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    Ok(_) => {}
                }
            }
        }));
    }
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let elapsed = start.elapsed().as_secs_f64();
    let total = bytes.load(Ordering::Relaxed) as f64;
    for t in threads {
        t.join().unwrap();
    }
    println!(
        "throughput {addr} conns={conns} write_size={size}B: {:.1} MB/s echoed",
        total / elapsed / 1e6
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: bench_client <addr> rtt [count] [size] | <addr> throughput [secs] [conns] [size]";
    let addr: SocketAddr = args.get(1).and_then(|s| s.parse().ok()).expect(usage);
    match args.get(2).map(String::as_str) {
        Some("rtt") => rtt(addr, arg(&args, 3, 10_000), arg(&args, 4, 64)),
        Some("throughput") => throughput(addr, arg(&args, 3, 10), arg(&args, 4, 8), arg(&args, 5, 16 * 1024)),
        _ => panic!("{usage}"),
    }
}

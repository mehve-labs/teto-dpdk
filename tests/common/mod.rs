//! Shared helpers for teto-dpdk's integration tests. They need the project's
//! Docker environment (F-Stack on the veth pair created by `entrypoint.sh`),
//! and each test runs in its own process: use `cargo nextest run`.
#![allow(dead_code)] // each test file uses a different subset

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use teto_dpdk::{FStack, FStackConfig};

pub fn config() -> FStackConfig {
    FStackConfig::for_docker()
        .with_config_file(concat!(env!("CARGO_MANIFEST_DIR"), "/config.ini"))
        .capture_init_output(true)
}

pub fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// Initialise F-Stack, explaining the one-per-process rule if it's broken.
pub fn init() -> FStack {
    // Remove tc impairments a killed fault test may have left on teto0.
    for args in [["qdisc", "del", "dev", "teto0", "root"], ["qdisc", "del", "dev", "teto0", "ingress"]] {
        let _ = std::process::Command::new("tc").args(args).output();
    }
    match FStack::init(&config()) {
        Ok(fs) => fs,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            panic!("F-Stack can be initialised once per process: run the integration tests with `cargo nextest run`")
        }
        Err(e) => panic!("FStack::init: {e}"),
    }
}

/// Run the poll loop, calling `tick` each iteration, until `client` (a
/// kernel-side client thread) has finished; 120 s safety limit.
pub fn run_until_done(fs: &FStack, client: &JoinHandle<()>, mut tick: impl FnMut()) {
    let deadline = Instant::now() + Duration::from_secs(120);
    fs.run(|| {
        tick();
        if client.is_finished() || Instant::now() > deadline {
            fs.stop();
        }
    })
    .unwrap();
    assert!(client.is_finished(), "client didn't finish within 120 s");
}

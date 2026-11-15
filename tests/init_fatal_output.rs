//! F-Stack/DPDK end the process on many init failures (`rte_exit`). With
//! output capture on, what they printed must still reach stderr instead of
//! vanishing with the redirected descriptors.
//!
//! The failure ends the process, so the test re-runs itself as a child.

use std::process::Command;

use teto_dpdk::{FStack, FStackConfig};

const CHILD_ENV: &str = "TETO_FATAL_INIT_CHILD";
const TEST_NAME: &str = "fatal_init_error_is_not_silent";

#[test]
fn fatal_init_error_is_not_silent() {
    if std::env::var_os(CHILD_ENV).is_some() {
        // An interface that doesn't exist: DPDK can't probe the port and
        // exits the process from inside init.
        let cfg = FStackConfig::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config.ini"))
            .with_eal_arg("--vdev=net_af_packet0,iface=teto-missing0")
            .with_eal_arg("--no-pci")
            .with_eal_arg("--iova-mode=va")
            .capture_init_output(true);
        let result = FStack::init(&cfg);
        // Only reached if this DPDK version reports the failure as an error.
        eprintln!("init returned instead of exiting: {:?}", result.err());
        std::process::exit(42);
    }

    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(42), "init didn't exit, so the replay isn't exercised:\n{stderr}");
    assert!(!out.status.success(), "child should have failed:\n{stderr}");
    assert!(
        stderr.contains("F-Stack exited the process during initialisation"),
        "captured output was not replayed:\n{stderr}"
    );
    // Something DPDK printed while failing, not just our header.
    assert!(stderr.contains("EAL") || stderr.contains("teto-missing0"), "replay is empty:\n{stderr}");
}

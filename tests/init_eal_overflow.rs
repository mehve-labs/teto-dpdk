//! S3: too many EAL arguments is an error instead of a buffer overflow.

use teto_dpdk::{FStack, FStackConfig};

#[test]
fn too_many_eal_args_is_an_error() {
    let mut cfg = FStackConfig::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config.ini"));
    for _ in 0..32 {
        cfg = cfg.with_eal_arg("--no-pci");
    }
    let err = FStack::init(&cfg).unwrap_err();
    assert!(err.to_string().contains("too many EAL arguments"), "{err}");
}

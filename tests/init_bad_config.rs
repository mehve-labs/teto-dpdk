//! C3: a failed initialisation is an `Err`, not a process abort. With
//! output capture on, F-Stack's own explanation is part of the error.

use std::io::ErrorKind;

use teto_dpdk::{FStack, FStackConfig};

#[test]
fn missing_config_file_is_an_error() {
    let cfg = FStackConfig::new("/nonexistent/teto.ini").capture_init_output(true);
    let err = FStack::init(&cfg).unwrap_err();
    assert_ne!(err.kind(), ErrorKind::AlreadyExists, "{err}");
    let msg = err.to_string();
    assert!(msg.contains("config load failed"), "{msg}");
    assert!(msg.contains("/nonexistent/teto.ini"), "F-Stack's output not captured: {msg}");
    // A failed init can't be retried (DPDK may be half-initialised).
    let again = FStack::init(&FStackConfig::new("/nonexistent/teto.ini")).unwrap_err();
    assert_eq!(again.kind(), ErrorKind::AlreadyExists);
    // stdout/stderr work again after the capture.
    println!("stdout restored");
    eprintln!("stderr restored");
}

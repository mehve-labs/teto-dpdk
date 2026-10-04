//! C3: a failed initialisation is an `Err`, not a process abort.

use std::io::ErrorKind;

use teto_dpdk::{FStack, FStackConfig};

#[test]
fn missing_config_file_is_an_error() {
    let err = FStack::init(&FStackConfig::new("/nonexistent/teto.ini")).unwrap_err();
    assert_ne!(err.kind(), ErrorKind::AlreadyExists, "{err}");
    // A failed init can't be retried (DPDK may be half-initialised).
    let again = FStack::init(&FStackConfig::new("/nonexistent/teto.ini")).unwrap_err();
    assert_eq!(again.kind(), ErrorKind::AlreadyExists);
}

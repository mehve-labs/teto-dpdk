//! C3: a failed initialisation is an `Err`, not a process abort.
//! D3: a missing config file is reported before F-Stack is touched, so it
//! can be fixed and retried; F-Stack's own complaints are captured into the
//! error when output capture is on.

use std::io::ErrorKind;

use teto_dpdk::{FStack, FStackConfig};

#[test]
fn bad_config_is_an_error() {
    // Missing file: clear error, and init can be retried.
    for _ in 0..2 {
        let err = FStack::init(&FStackConfig::new("/nonexistent/teto.ini")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound, "{err}");
        assert!(err.to_string().contains("/nonexistent/teto.ini"), "{err}");
    }

    // A file F-Stack can't parse: its message ends up in the error.
    let bad = std::env::temp_dir().join("teto-bad-config.ini");
    std::fs::write(&bad, "this is not ini\n[[[\n").unwrap();
    let cfg = FStackConfig::new(bad.to_str().unwrap()).capture_init_output(true);
    let err = FStack::init(&cfg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("config load failed"), "{msg}");
    assert!(msg.contains("failed on line 1"), "F-Stack's output not captured: {msg}");

    // A failed F-Stack init can't be retried (DPDK may be half-initialised).
    let again = FStack::init(&FStackConfig::new(bad.to_str().unwrap())).unwrap_err();
    assert_eq!(again.kind(), ErrorKind::AlreadyExists);
    let _ = std::fs::remove_file(&bad);

    // stdout/stderr work again after the capture.
    println!("stdout restored");
    eprintln!("stderr restored");
}

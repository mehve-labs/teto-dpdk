//! A panic inside the poll loop resumes from `FStack::run`; F-Stack is then
//! shut down and sockets created earlier can be dropped safely.

use std::io::ErrorKind;
use std::panic::{self, AssertUnwindSafe};

mod common;

use teto_dpdk::net::UdpSocket;

#[test]
fn panic_in_tick_propagates() {
    let fs = common::init();
    // F-Stack's interface setup chatter was captured rather than printed.
    assert!(fs.init_output().contains("Ethernet address"), "{}", fs.init_output());
    let socket = UdpSocket::bind(&fs, "0.0.0.0:9000".parse().unwrap()).unwrap();

    let mut ticks = 0;
    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        fs.run(|| {
            ticks += 1;
            if ticks == 3 {
                panic!("boom");
            }
        })
    }));
    assert_eq!(*caught.unwrap_err().downcast::<&str>().unwrap(), "boom");
    assert_eq!(ticks, 3, "tick ran again after panicking");

    assert_eq!(fs.run(|| {}).unwrap_err().kind(), ErrorKind::BrokenPipe);
    assert_eq!(socket.recv_from(&mut [0u8; 8]).unwrap_err().kind(), ErrorKind::BrokenPipe);
    drop(socket);
}

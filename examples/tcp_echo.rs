/// TCP echo server using F-Stack.
///
/// Run with:
///   cargo run --example tcp_echo
///
/// Test from inside the container:
///   nc -w3 10.0.0.1 8080
///   (type a message, press Enter, see it echoed back)

use teto_dpdk::config::{FStackConfig, TcpSocketOptions};
use teto_dpdk::fstack::ffi::{
    init_fstack, create_tcp_listener, run_fstack_tcp, TcpMessage, FStackTcpListener,
};
use cxx::UniquePtr;

static mut GLOBAL_LISTENER: Option<*const FStackTcpListener> = None;

fn on_connect(fd: i32, ip: &String, port: u16) {
    println!("[TCP] New connection fd={} from {}:{}", fd, ip, port);
}

fn on_data(fd: i32, msg: &TcpMessage) {
    println!(
        "[TCP] Received {} bytes from {}:{} on fd={}",
        msg.payload.len(), msg.src_ip, msg.src_port, fd
    );
    unsafe {
        if let Some(ptr) = GLOBAL_LISTENER {
            (*ptr).send_to(fd, &msg.payload);
        }
    }
}

fn on_disconnect(fd: i32) {
    println!("[TCP] Connection closed fd={}", fd);
}

fn main() {
    // Docker/TAP configuration — swap for FStackConfig::for_bare_metal() on bare metal.
    let cfg = FStackConfig::for_docker();

    println!("Initializing F-Stack...");
    init_fstack(&cfg.config_args(), &cfg.eal_args());

    let bind_ip   = "0.0.0.0".to_string();
    let bind_port = 8080u16;

    let tcp_opts = TcpSocketOptions::default()
        .nodelay(true)
        .quickack(true);

    println!("Creating TCP listener on {}:{}...", bind_ip, bind_port);
    let listener: UniquePtr<FStackTcpListener> =
        create_tcp_listener(&bind_ip, bind_port, &tcp_opts.to_ffi(), on_connect, on_data, on_disconnect);

    unsafe {
        GLOBAL_LISTENER = Some(&*listener as *const FStackTcpListener);
    }

    println!("Starting F-Stack TCP event loop...");
    run_fstack_tcp(&listener);
}

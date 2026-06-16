/// UDP echo server using F-Stack.
///
/// Run with:
///   cargo run --example udp_echo
///
/// Test from inside the container:
///   echo "Hello F-Stack!" | nc -u -w1 10.0.0.1 8080

use teto_dpdk::config::FStackConfig;
use teto_dpdk::fstack::ffi::{
    init_fstack, create_udp_socket, run_fstack, UdpMessage, FStackUdpSocket,
};
use cxx::UniquePtr;

static mut GLOBAL_SOCKET: Option<*const FStackUdpSocket> = None;

fn on_packet(_fd: i32, msg: &UdpMessage) {
    println!(
        "[UDP] Received {} bytes from {}:{}",
        msg.payload.len(), msg.src_ip, msg.src_port
    );
    unsafe {
        if let Some(ptr) = GLOBAL_SOCKET {
            (*ptr).send_to(&msg.payload, &msg.src_ip, msg.src_port);
        }
    }
}

fn main() {
    // Docker/TAP configuration — swap for FStackConfig::for_bare_metal() on bare metal.
    let cfg = FStackConfig::for_docker();

    println!("Initializing F-Stack...");
    init_fstack(&cfg.config_args(), &cfg.eal_args());

    let bind_ip   = "0.0.0.0".to_string();
    let bind_port = 8080u16;

    println!("Creating UDP socket on {}:{}...", bind_ip, bind_port);
    let socket: UniquePtr<FStackUdpSocket> =
        create_udp_socket(&bind_ip, bind_port, on_packet);

    unsafe {
        GLOBAL_SOCKET = Some(&*socket as *const FStackUdpSocket);
    }

    println!("Starting F-Stack UDP event loop...");
    run_fstack(&socket);
}

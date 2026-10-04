/// UDP echo server on the low-level API.
///
/// Run with:
///   cargo run --example udp_echo
///
/// Test from inside the container:
///   echo "Hello F-Stack!" | nc -u -w1 10.0.0.1 8080
use std::io;

use teto_dpdk::net::UdpSocket;
use teto_dpdk::{FStack, FStackConfig};

fn main() -> io::Result<()> {
    // Docker/TAP configuration — swap for FStackConfig::for_bare_metal() on bare metal.
    let fs = FStack::init(&FStackConfig::for_docker())?;
    let socket = UdpSocket::bind(&fs, "0.0.0.0:8080".parse().unwrap())?;
    println!("Bound to {}", socket.local_addr()?);

    let mut buf = vec![0u8; 65535];
    fs.run(|| {
        // Drain everything that arrived since the last iteration.
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    if let Err(e) = socket.send_to(&buf[..n], peer.into()) {
                        eprintln!("[{peer}] send failed: {e}");
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("recv failed: {e}");
                    break;
                }
            }
        }
    })
}

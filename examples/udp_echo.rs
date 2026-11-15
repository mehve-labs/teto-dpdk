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
    // TETO_PROFILE=bare-metal for a real NIC (default: Docker).
    let fs = FStack::init(&FStackConfig::from_env()?)?;
    let socket = UdpSocket::bind(&fs, "0.0.0.0:8080".parse().unwrap())?;
    println!("Bound to {}", socket.local_addr()?);

    let mut buf = vec![0u8; 65535];
    fs.run(|| {
        // Drain everything that arrived since the last iteration.
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    if let Err(e) = socket.send_to(&buf[..n], peer) {
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

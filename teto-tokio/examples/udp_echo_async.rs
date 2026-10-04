//! Async UDP echo server using F-Stack + Tokio.
//!
//! Run with:
//!   cargo run -p teto-tokio --example udp_echo_async
//!
//! Test from inside the container:
//!   echo "Hello Teto!" | nc -u -w1 10.0.0.1 8080

use teto_dpdk::config::FStackConfig;
use teto_tokio::{TetoRuntime, TetoUdpSocket};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // TETO_PROFILE=bare-metal for a real NIC (default: Docker/TAP).
    let cfg = FStackConfig::from_env()?;
    let addr = "0.0.0.0:8080".parse()?;

    println!("Starting async UDP echo server on {addr}...");
    let rt = TetoRuntime::start(cfg).await?;
    let socket = TetoUdpSocket::bind(&rt, addr).await?;
    println!("Bound — ready for packets.");

    let mut buf = [0u8; 65535];
    loop {
        let (n, peer) = socket.recv_from(&mut buf).await?;
        println!("[{peer}] echoing {n} bytes");
        socket.send_to(&buf[..n], peer).await?;
    }
}

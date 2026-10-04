//! Async TCP echo server in local mode: tasks run on the F-Stack thread and
//! call F-Stack directly (no cross-thread hop). Compare with tcp_echo_async.
//!
//! Run with:
//!   cargo run -p teto-tokio --example tcp_echo_local
//!
//! Test from inside the container:
//!   echo "Hello Teto!" | nc -w3 10.0.0.1 8080

use teto_dpdk::{FStackConfig, TcpSocketOptions};
use teto_tokio::local::{self, LocalTcpListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // TETO_PROFILE=bare-metal for a real NIC (default: Docker).
    let cfg = FStackConfig::from_env()?;
    local::run(cfg, async {
        let opts = TcpSocketOptions::default().nodelay(true);
        let listener = LocalTcpListener::bind("0.0.0.0:8080".parse().unwrap(), &opts)?;
        println!("Listening — ready for connections.");
        loop {
            let (mut stream, _) = listener.accept().await?;
            tokio::task::spawn_local(async move {
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n @ 1..) = stream.read(&mut buf).await {
                    if stream.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    })?
}

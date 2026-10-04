//! Async TCP echo server using F-Stack + Tokio.
//!
//! Run with:
//!   cargo run -p teto-tokio --example tcp_echo_async
//!
//! Test from inside the container:
//!   echo "Hello Teto!" | nc -w3 10.0.0.1 8080

use teto_dpdk::config::{FStackConfig, TcpSocketOptions};
use teto_tokio::TetoTcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = FStackConfig::for_docker();
    let addr = "0.0.0.0:8080".parse()?;
    let opts = TcpSocketOptions::default().nodelay(true);

    println!("Starting async TCP echo server on {addr}...");
    let mut listener = TetoTcpListener::bind(cfg, addr, opts).await?;
    println!("Listening — ready for connections.");

    loop {
        let (mut stream, peer) = listener.accept().await?;
        println!("[{peer}] connected");

        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        println!("[{peer}] echoing {n} bytes");
                        if stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("[{peer}] read error: {e}");
                        break;
                    }
                }
            }
            println!("[{peer}] disconnected");
        });
    }
}

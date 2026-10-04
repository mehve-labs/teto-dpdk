//! Baseline for benchmarks: the same echo server as `tcp_echo_async`, on
//! tokio's kernel sockets instead of F-Stack.
//!
//!   cargo run --release -p teto-tokio --example kernel_echo -- [addr]   (default 0.0.0.0:8081)

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "0.0.0.0:8081".into());
    let listener = TcpListener::bind(&addr).await?;
    println!("kernel echo listening on {addr}");
    loop {
        let (mut stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?;
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}

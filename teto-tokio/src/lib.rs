//! Async [tokio](https://tokio.rs) adapter for `teto-dpdk`.
//!
//! A dedicated thread runs the F-Stack poll loop. It moves bytes between
//! F-Stack sockets and bounded per-connection buffers, and wakes tokio tasks;
//! tokio tasks never call into F-Stack directly.

mod conn;
mod tcp_driver;
mod tcp_listener;
mod tcp_stream;
mod udp_socket;

pub use tcp_listener::TetoTcpListener;
pub use tcp_stream::TetoTcpStream;
pub use udp_socket::TetoUdpSocket;

fn require_v4(addr: std::net::SocketAddr) -> std::io::Result<std::net::SocketAddrV4> {
    match addr {
        std::net::SocketAddr::V4(a) => Ok(a),
        std::net::SocketAddr::V6(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "IPv6 is not supported by teto-dpdk",
        )),
    }
}

#![warn(missing_docs)]
//! Async [tokio](https://tokio.rs) adapter for `teto-dpdk`.
//!
//! [`TetoRuntime`] starts a dedicated thread running the F-Stack poll loop.
//! Listeners, UDP sockets and outbound connections are created from it; the
//! F-Stack thread moves bytes between F-Stack sockets and bounded
//! per-connection buffers and wakes tokio tasks. Tokio tasks never call into
//! F-Stack directly.

mod conn;
mod driver;
mod runtime;
mod tcp_listener;
mod tcp_stream;
mod udp_socket;

pub use runtime::TetoRuntime;
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

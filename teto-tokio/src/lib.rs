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
pub use tcp_stream::{OwnedReadHalf, OwnedWriteHalf, ReuniteError, TetoTcpStream};
pub use udp_socket::TetoUdpSocket;

//! Rust bindings for [F-Stack](https://github.com/F-Stack/f-stack): userspace
//! TCP/UDP over DPDK.
//!
//! ```no_run
//! use teto_dpdk::event::{Events, Interest, Kqueue};
//! use teto_dpdk::net::TcpListener;
//! use teto_dpdk::{FStack, FStackConfig, TcpSocketOptions};
//!
//! # fn main() -> std::io::Result<()> {
//! let fs = FStack::init(&FStackConfig::for_docker())?;
//! let listener = TcpListener::bind(&fs, "0.0.0.0:8080".parse().unwrap(), &TcpSocketOptions::default())?;
//! let kq = Kqueue::new(&fs)?;
//! kq.register(&listener, 0, Interest::READABLE)?;
//! let mut events = Events::with_capacity(256);
//! fs.run(|| {
//!     kq.poll(&mut events).expect("kqueue poll");
//!     for ev in events.iter() {
//!         // accept / read / write without blocking ...
//!     }
//! })?;
//! # Ok(())
//! # }
//! ```
//!
//! F-Stack is single-threaded: [`FStack`] and every socket are `!Send`, and
//! all work happens inside [`FStack::run`]. See `teto-tokio` for an async
//! adapter.

pub mod config;
pub mod event;
pub mod net;
mod runtime;
mod sys;

pub use config::{FStackConfig, TcpSocketOptions};
pub use runtime::FStack;

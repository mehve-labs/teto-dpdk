# Changelog

Both crates (`teto-dpdk`, `teto-tokio`) are versioned together.

## 0.3.0 — unreleased

A rework of both crates after a review that found data-corruption, hang,
abort and soundness bugs in 0.2.0. **Breaking.**

### Breaking changes
- **teto-dpdk:** the callback API (`fstack::ffi`: `init_fstack`, `create_tcp_listener`,
  `run_fstack_tcp`, …) is replaced by a safe one: `FStack` (init once, run once),
  `net::{TcpListener, TcpStream, UdpSocket}` (non-blocking, RAII) and
  `event::{Kqueue, Interest, Events}`.
- **teto-tokio:** sockets are created from a `TetoRuntime`:
  `TetoTcpListener::bind(&rt, addr, opts)`, `TetoUdpSocket::bind(&rt, addr)`.
  `TetoUdpSocket::recv_from` takes `&self`.
- `TcpSocketOptions::quickack` and `TcpSocketOptions::to_ffi` are removed
  (FreeBSD has no `TCP_QUICKACK`; use `net.inet.tcp.delayed_ack=0` in `config.ini`).
- Addresses are no longer silently rewritten: 0.2.0 bound IPv6 addresses to `0.0.0.0`; IPv6 is now supported properly.
- Low-level socket addresses are `SocketAddr` (IPv4 or IPv6) instead of `SocketAddrV4`.
- `TetoTcpListener::accept` takes `&self`; `TetoUdpSocket::recv_from` takes `&self`.
- `FStackConfig::for_docker()` targets the project's Docker setup, which is now a
  veth pair created by `entrypoint.sh`
  (`--vdev=net_af_packet0,iface=teto0-dpdk`) instead of a DPDK TAP device.
  For a TAP-based setup, build the config with `FStackConfig::new(..)` and your
  own `--vdev` argument.

### Added
- `TetoRuntime`: any number of listeners, UDP sockets and connections on one
  F-Stack thread; a failed bind leaves it usable; it exits when unused.
- Outbound TCP: `TetoTcpStream::connect` / `connect_from`, `net::TcpStream::connect` / `connect_from` / `take_error`.
- IPv6 (configure `addr6`/`prefix_len` per port).
- Local mode, `teto_tokio::local::run`: async tasks on the F-Stack thread
  with `LocalTcpListener`/`LocalTcpStream`/`LocalUdpSocket` calling F-Stack
  directly (no cross-thread hop; about half the echo latency in Docker).
- `event::Kqueue::register_oneshot`.
- tokio parity: `TetoTcpStream::into_split` (+ `reunite`), `readable`/`writable`,
  `try_read`/`try_write`, `peek`, `set_nodelay`/`set_options` on live
  connections; `TetoUdpSocket::connect`/`send`/`recv`/`peer_addr`;
  `TetoRuntime::shutdown().await`.
- `FStackConfig::capture_init_output`: keep F-Stack/DPDK init chatter off the
  terminal and attach it to init errors.
- `net::TcpStream::{unsent_bytes, abort}`, `Interest::SEND_EMPTY`.
- Benchmark harness (`scripts/bench.sh`, `bench_client`, `kernel_echo`),
  integration tests, and CI that runs them against F-Stack in Docker.

### Fixed
- Data from a closed connection could reach a new connection that reused its
  descriptor.
- A slow or non-reading peer froze the whole stack (busy-spin on `EAGAIN`).
- Init and bind failures aborted the process (`std::terminate`).
- Half-closed connections were fully closed, so replies were lost.
- No backpressure: unbounded memory growth, writes "succeeded" on dead
  connections; resets were reported as clean EOF.
- Reading every connection on every poll iteration; one UDP datagram per
  iteration; several copies and allocations per message.
- Dropping a stream now delivers its buffered data and FIN before closing.
- Downstream crates failed to link (link flags didn't propagate to dependents).
- Double free when F-Stack's poll loop returned (an F-Stack argv bug is now
  worked around).

### Changed
- The Docker image pins F-Stack v1.25 (master as of mid-2026 runs no FreeBSD
  kernel timers, so TCP retransmission doesn't work) and Rust 1.99.0.
- The shipped `config.ini` sets `pkt_tx_delay=0` (send immediately; lower latency)
  and drops the TAP-era `tx_csum_offoad_skip=1` / `net.inet.udp.checksum=0`.
- Docker: a veth pair set up once at container start replaces the TAP device
  and its per-run reconfiguration; containers need only `NET_ADMIN` (no
  `--privileged`). `scripts/test.sh` runs the whole suite; integration tests
  run under cargo-nextest, one process per test.

## 0.2.0 — 2026-08-01
- Relicensed to Apache-2.0.

## 0.1.0 – 0.1.2 — 2026-07-11 / 2026-07-12
- Initial releases, licensed AGPL-3.0-only (a commercial licence was offered).

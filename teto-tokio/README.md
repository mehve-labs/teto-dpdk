# teto-tokio

**Async [tokio](https://tokio.rs) adapter for [`teto-dpdk`](https://crates.io/crates/teto-dpdk)** — familiar async/await networking over F-Stack/DPDK userspace TCP/UDP.

[![Crates.io](https://img.shields.io/crates/v/teto-tokio.svg)](https://crates.io/crates/teto-tokio)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](../LICENSE)

## Overview

`teto-tokio` wraps [`teto-dpdk`](https://crates.io/crates/teto-dpdk)'s F-Stack thread in tokio-style APIs so you can write latency-sensitive network services with ordinary `async`/`await`:

| Type | Description |
|------|-------------|
| `TetoTcpListener` | Accept incoming TCP connections. |
| `TetoTcpStream` | Bidirectional TCP stream implementing `AsyncRead + AsyncWrite`. |
| `TetoUdpSocket` | Send and receive UDP datagrams. |

All I/O runs on `teto-dpdk`'s dedicated F-Stack/DPDK poll-mode thread, bypassing the Linux kernel network stack entirely.

## Requirements

Like `teto-dpdk`, this crate links against F-Stack and DPDK and is intended to run inside the project's Docker environment, which handles hugepages and kernel module setup. See the [`teto-dpdk` README](https://github.com/moewe-labs/teto-dpdk) for build and deployment details.

## License

AGPL-3.0-only. A commercial license is available — see [`LICENSE-COMMERCIAL.md`](../LICENSE-COMMERCIAL.md).

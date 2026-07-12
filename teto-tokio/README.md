# teto-tokio

**Async [tokio](https://tokio.rs) adapter for [`teto-dpdk`](https://crates.io/crates/teto-dpdk)** — familiar async/await networking over F-Stack/DPDK userspace TCP/UDP.

[![Crates.io](https://img.shields.io/crates/v/teto-tokio.svg)](https://crates.io/crates/teto-tokio)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)

## Overview

`teto-tokio` wraps [`teto-dpdk`](https://crates.io/crates/teto-dpdk)'s F-Stack thread in tokio-style APIs so you can write latency-sensitive network services with ordinary `async`/`await`:

| Type | Description |
|------|-------------|
| `TetoTcpListener` | Accept incoming TCP connections. |
| `TetoTcpStream` | Bidirectional TCP stream implementing `AsyncRead + AsyncWrite`. |
| `TetoUdpSocket` | Send and receive UDP datagrams. |

All I/O runs on `teto-dpdk`'s dedicated F-Stack/DPDK poll-mode thread, bypassing the Linux kernel network stack entirely.

## Requirements

Like `teto-dpdk`, this crate links against F-Stack and DPDK and is intended to run inside the project's Docker environment, which handles hugepages and kernel module setup. See the [`teto-dpdk` README](https://github.com/mehve-labs/teto-dpdk) for build and deployment details.

## License

Dual-licensed. You may use it under **either** license — your choice:

- **Open Source**: [AGPL-3.0-only](LICENSE) -- free for everyone, any purpose (including commercial), provided you meet the AGPL-3.0 copyleft terms.
- **Commercial**: A proprietary license for those who prefer not to comply with the AGPL-3.0 copyleft obligations. See [`LICENSE-COMMERCIAL.md`](LICENSE-COMMERCIAL.md).

There are no restrictions based on company size or revenue: anyone may use teto-tokio for free under the AGPL-3.0. Contributions are accepted under the repository's [Contributor License Agreement](https://github.com/mehve-labs/teto-dpdk/blob/master/CONTRIBUTING.md).

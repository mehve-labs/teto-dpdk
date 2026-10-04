# teto-tokio

**Async [tokio](https://tokio.rs) adapter for [`teto-dpdk`](https://crates.io/crates/teto-dpdk)** — familiar async/await networking over F-Stack/DPDK userspace TCP/UDP.

[![Crates.io](https://img.shields.io/crates/v/teto-tokio.svg)](https://crates.io/crates/teto-tokio)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

## Overview

`teto-tokio` wraps [`teto-dpdk`](https://crates.io/crates/teto-dpdk)'s F-Stack thread in tokio-style APIs so you can write latency-sensitive network services with ordinary `async`/`await`:

| Type | Description |
|------|-------------|
| `TetoRuntime` | Starts the F-Stack thread; everything else is created from it. |
| `TetoTcpListener` | Accept incoming TCP connections. |
| `TetoTcpStream` | Bidirectional TCP stream implementing `AsyncRead + AsyncWrite`; accepted, or opened with `connect`. |
| `TetoUdpSocket` | Send and receive UDP datagrams. |

All I/O runs on `teto-dpdk`'s dedicated F-Stack/DPDK poll-mode thread, bypassing the Linux kernel network stack entirely.

## Requirements

Like `teto-dpdk`, this crate links against F-Stack and DPDK and is intended to run inside the project's Docker environment, which handles hugepages and kernel module setup. See the [`teto-dpdk` README](https://github.com/mehve-labs/teto-dpdk) for build and deployment details.

## License

Licensed under the [Apache License 2.0](LICENSE) — free for everyone, any purpose (including proprietary and closed-source use), subject only to the attribution and notice terms of the license. See [NOTICE](NOTICE) for third-party attributions.

**Note for downstream:** teto-tokio is licensed permissively, but it builds on `teto-dpdk`, which statically links F-Stack/DPDK and related components, some of which are BSD- or GPL-2.0-licensed (see [NOTICE](NOTICE)). If you distribute a **compiled binary** that statically links these, that binary's redistribution terms are governed by those components' licenses — not by teto-tokio's Apache-2.0 license. Using teto-tokio as a source dependency imposes no such obligation on you.

Versions 0.1.0–0.1.2 were released under AGPL-3.0-only; 0.2.0 and later are Apache-2.0 (see the repository README's licensing history).

Unless you explicitly state otherwise, any contribution you submit for inclusion in teto-tokio shall be licensed under the Apache License 2.0, without any additional terms or conditions. See the repository's [CONTRIBUTING.md](https://github.com/mehve-labs/teto-dpdk/blob/master/CONTRIBUTING.md).

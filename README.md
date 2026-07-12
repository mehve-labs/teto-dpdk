# teto-dpdk

**Rust bindings for [F-Stack](https://github.com/F-Stack/f-stack)** — high-performance userspace TCP/UDP networking via DPDK, bypassing the Linux kernel network stack entirely.

*Named after Nausicaä's fox-squirrel companion: small, fast, and fiercely reliable.*

[![Crates.io](https://img.shields.io/crates/v/teto-dpdk.svg)](https://crates.io/crates/teto-dpdk)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)

## Overview

teto-dpdk eliminates syscall overhead and kernel-to-userspace copies by running a full FreeBSD TCP/IP stack in userspace on top of DPDK's poll-mode driver. It is suitable for latency-sensitive or high-throughput network workloads where kernel socket overhead is a bottleneck.

Two crates are provided:

| Crate | Description |
|-------|-------------|
| [`teto-dpdk`](https://crates.io/crates/teto-dpdk) | Low-level F-Stack bindings (cxx bridge, raw callbacks). Use this with mio or other frameworks that manage their own event loop. |
| [`teto-tokio`](https://crates.io/crates/teto-tokio) | Async adapter: `TetoTcpListener`, `TetoTcpStream` (`AsyncRead + AsyncWrite`), `TetoUdpSocket`. Familiar tokio-style API over teto-dpdk's F-Stack thread. |

## Architecture

```
Kernel                              Userspace (DPDK)
┌────────────────┐                  ┌──────────────────────────────┐
│  sender        │                  │  DPDK TAP PMD (port 0)       │
│  (nc/app)      │    TAP fd        │  MAC: aa:bb:cc:dd:ee:ff      │
│      │         │                  │         │                    │
│      ▼         │                  │  F-Stack (FreeBSD TCP/IP)    │
│  dtap0 ────────┼──────────────────▶  ff_socket / ff_recvfrom    │
│  10.0.0.2      │◀─────────────────┼─ ff_sendto (echo reply)     │
│  MAC: 02:00:*  │                  │         │                    │
└────────────────┘                  │  Rust callback (via cxx)     │
                                    └──────────────────────────────┘
  MACs must differ — FreeBSD drops frames with src MAC == own MAC.
```

The Rust layer interfaces with F-Stack through a C++ wrapper (`cxx_layer/`) using the [cxx](https://cxx.rs) bridge. The C++ wrapper calls F-Stack's `ff_*` APIs and delivers received packets to Rust callbacks.

## Quick Start

DPDK requires specific kernel modules and hugepage configuration that are complex to set up on a host. The included Docker image handles all of this.

```bash
docker build -t teto-dpdk .
docker run --privileged --network=host -it -v $(pwd):/app teto-dpdk bash

# Inside the container — choose one:
cargo run --example udp_echo                      # Low-level UDP echo (raw callbacks)
cargo run --example tcp_echo                      # Low-level TCP echo (raw callbacks)
cargo run -p teto-tokio --example tcp_echo_async    # Async TCP echo (tokio)
cargo run -p teto-tokio --example udp_echo_async    # Async UDP echo (tokio)
```

See [docs/testing-docker.md](docs/testing-docker.md) for the full walkthrough, expected startup output, and a diagnostic checklist.

## Async API (teto-tokio)

The `teto-tokio` crate provides familiar async/await wrappers. A dedicated F-Stack thread runs the DPDK poll loop and bridges data to tokio tasks via async channels.

```toml
[dependencies]
teto-tokio = "0.1"
teto-dpdk = "0.1"
```

```rust
use teto_tokio::TetoTcpListener;
use teto_dpdk::config::{FStackConfig, TcpSocketOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cfg = FStackConfig::for_docker();
    let opts = TcpSocketOptions::default().nodelay(true);
    let mut listener = TetoTcpListener::bind(cfg, "0.0.0.0:8080".parse().unwrap(), opts).await?;

    loop {
        let (mut stream, peer) = listener.accept().await?;
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => { let _ = stream.write_all(&buf[..n]).await; }
                }
            }
        });
    }
}
```

`TetoTcpStream` implements `AsyncRead + AsyncWrite`, so it works with `BufReader`, `copy`, `LinesCodec`, and all other tokio I/O utilities. `TetoUdpSocket` provides async `recv_from`/`send_to`.

## Low-Level API (teto-dpdk)

The raw callback API is available for users who need zero-overhead access or want to integrate with `mio` or other frameworks.

```toml
[dependencies]
teto-dpdk = "0.1"
```

```rust
use teto_dpdk::config::FStackConfig;
use teto_dpdk::fstack::ffi::{init_fstack, create_tcp_listener, run_fstack_tcp};

fn on_connect(fd: i32, ip: &String, port: u16) { /* ... */ }
fn on_data(fd: i32, msg: &TcpMessage) { /* ... */ }
fn on_disconnect(fd: i32) { /* ... */ }

fn main() {
    let cfg = FStackConfig::for_docker();
    init_fstack(&cfg.config_args(), &cfg.eal_args());
    let opts = TcpSocketOptions::default().nodelay(true);
    let listener = create_tcp_listener(
        &"0.0.0.0".to_string(), 8080, &opts.to_ffi(),
        on_connect, on_data, on_disconnect,
    );
    run_fstack_tcp(&listener); // blocking
}
```

## Configuration

F-Stack is configured via `config.ini`. The `FStackConfig` builder generates the correct arguments for different environments:

```rust
// Docker / TAP device (development)
let cfg = FStackConfig::for_docker();

// Bare metal / AWS with a real NIC bound via VFIO
let cfg = FStackConfig::for_bare_metal();

// Custom
let cfg = FStackConfig::new("config.ini")
    .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
    .with_eal_arg("--no-pci");
```

## Project Structure

```
teto-dpdk/                          (Cargo workspace root)
├── src/
│   ├── lib.rs              # Re-exports fstack + config modules
│   ├── config.rs           # FStackConfig + TcpSocketOptions builders
│   └── fstack.rs           # cxx bridge definition (Rust ↔ C++)
├── examples/
│   ├── udp_echo.rs         # Low-level UDP echo
│   └── tcp_echo.rs         # Low-level TCP echo
├── cxx_layer/
│   ├── fstack_wrapper.h    # C++ header: UDP + TCP types and classes
│   └── fstack_wrapper.cpp  # F-Stack API calls, DPDK init, TCP listener
├── teto-tokio/             # Async adapter crate
│   ├── src/
│   │   ├── lib.rs          # Re-exports TetoTcpListener, TetoTcpStream, TetoUdpSocket
│   │   ├── runtime.rs      # Channel bridge between F-Stack thread and tokio
│   │   ├── tcp_listener.rs # Async TCP listener (mirrors tokio::net::TcpListener)
│   │   ├── tcp_stream.rs   # Async TCP stream (AsyncRead + AsyncWrite)
│   │   └── udp_socket.rs   # Async UDP socket (mirrors tokio::net::UdpSocket)
│   └── examples/
│       ├── tcp_echo_async.rs   # Async TCP echo (cargo run -p teto-tokio --example tcp_echo_async)
│       └── udp_echo_async.rs   # Async UDP echo (cargo run -p teto-tokio --example udp_echo_async)
├── build.rs                # Links F-Stack, DPDK, and the cxx layer
├── config.ini              # F-Stack / DPDK configuration (Docker/TAP)
├── Dockerfile              # Builds DPDK + F-Stack from source
├── entrypoint.sh           # Configures the kernel-side TAP device
└── docs/
    ├── testing-docker.md   # Docker testing guide and diagnostics
    ├── bare-metal-setup.md # Bare metal and AWS setup guide
    └── config-reference.md # All config.ini keys explained
```

## How It Works

1. **DPDK TAP PMD** creates a virtual network device pair: a DPDK ethdev (polled in userspace) and a kernel-visible TAP interface (`dtap0`).

2. **F-Stack** runs a FreeBSD TCP/IP stack on top of the DPDK ethdev. It processes Ethernet frames, handles ARP, and delivers payload to `ff_socket` descriptors.

3. **The entrypoint** configures the kernel side of `dtap0` with IP `10.0.0.2/24` and a **different MAC address** from the DPDK side. The distinct MAC is critical: FreeBSD's `ether_input` drops frames whose source MAC matches the interface MAC (anti-loop), so if both sides of the TAP share the same MAC, F-Stack silently drops all ARP replies.

4. **`cargo run`** initializes F-Stack, creates a socket bound to `0.0.0.0:8080`, and enters the DPDK poll loop. When a packet or connection arrives, the Rust callback fires.

## Documentation

| Document | Description |
|----------|-------------|
| [docs/testing-docker.md](docs/testing-docker.md) | Step-by-step Docker testing guide, startup output walkthrough, and a full diagnostic checklist |
| [docs/bare-metal-setup.md](docs/bare-metal-setup.md) | Setting up hugepages, IOMMU, NIC binding with VFIO, and AWS SR-IOV configuration |
| [docs/config-reference.md](docs/config-reference.md) | Every `config.ini` key explained, including common pitfalls and silently-ignored keys |

## Requirements

- **Docker testing**: Docker with `--privileged` support, Linux or WSL2
- **Bare metal / AWS**: hugepages, IOMMU/VT-d enabled, NIC bound via `vfio-pci` — see [docs/bare-metal-setup.md](docs/bare-metal-setup.md)

## License

This project is dual-licensed. You may use it under **either** license — your choice:

- **Open Source**: [AGPL-3.0-only](LICENSE) -- free for everyone, any purpose (including commercial), provided you meet the AGPL-3.0 copyleft terms.
- **Commercial**: A proprietary license for those who prefer not to comply with the AGPL-3.0 copyleft obligations (e.g. building a closed-source or hosted product, or avoiding the Section 13 network-use disclosure). See [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md) for details.

There are no restrictions based on company size or revenue: anyone may use teto-dpdk for free under the AGPL-3.0. See [NOTICE](NOTICE) for third-party attributions.

Contributions are accepted under a lightweight [Contributor License Agreement](CONTRIBUTING.md), which lets the project maintain its dual-licensing model.

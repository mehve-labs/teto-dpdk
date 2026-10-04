# teto-dpdk

**Rust bindings for [F-Stack](https://github.com/F-Stack/f-stack)** — high-performance userspace TCP/UDP networking via DPDK, bypassing the Linux kernel network stack entirely.

*Named after Nausicaä's fox-squirrel companion: small, fast, and fiercely reliable.*

[![Crates.io](https://img.shields.io/crates/v/teto-dpdk.svg)](https://crates.io/crates/teto-dpdk)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

## Overview

teto-dpdk runs a full FreeBSD TCP/IP stack in userspace on top of DPDK's poll-mode driver, so packet processing involves no syscalls, interrupts or kernel network stack. It is aimed at latency-sensitive or high-throughput network workloads where the kernel socket path is a bottleneck.

Two crates are provided:

| Crate | Description |
|-------|-------------|
| [`teto-dpdk`](https://crates.io/crates/teto-dpdk) | Low-level F-Stack bindings: non-blocking sockets and a kqueue poller, run on F-Stack's own poll loop. Use this when you want to drive the event loop yourself. |
| [`teto-tokio`](https://crates.io/crates/teto-tokio) | Async adapter: `TetoRuntime`, `TetoTcpListener`, `TetoTcpStream` (`AsyncRead + AsyncWrite`, inbound and outbound), `TetoUdpSocket`. Familiar tokio-style API over teto-dpdk's F-Stack thread. |

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
└────────────────┘                  │  Rust poll-loop tick (cxx)   │
                                    └──────────────────────────────┘
  MACs must differ — FreeBSD drops frames with src MAC == own MAC.
```

The Rust layer interfaces with F-Stack through a C++ wrapper (`cxx_layer/`) using the [cxx](https://cxx.rs) bridge. The C++ wrapper is a thin layer over F-Stack's `ff_*` calls that returns `-errno` on failure; the socket, kqueue and lifecycle logic is in Rust.

## Quick Start

DPDK requires specific kernel modules and hugepage configuration that are complex to set up on a host. The included Docker image handles all of this.

```bash
docker build -t teto-dpdk .
docker run --privileged --network=host -it -v $(pwd):/app teto-dpdk bash

# Inside the container — choose one:
cargo run --example udp_echo                      # Low-level UDP echo
cargo run --example tcp_echo                      # Low-level TCP echo (kqueue)
cargo run -p teto-tokio --example tcp_echo_async    # Async TCP echo (tokio)
cargo run -p teto-tokio --example udp_echo_async    # Async UDP echo (tokio)
```

See [docs/testing-docker.md](docs/testing-docker.md) for the full walkthrough, expected startup output, and a diagnostic checklist.

## Async API (teto-tokio)

The `teto-tokio` crate provides familiar async/await wrappers. A dedicated F-Stack thread runs the DPDK poll loop and bridges data to tokio tasks via async channels.

```toml
[dependencies]
teto-tokio = "0.3"
teto-dpdk = "0.3"
```

```rust
use teto_tokio::{TetoRuntime, TetoTcpListener};
use teto_dpdk::config::{FStackConfig, TcpSocketOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cfg = FStackConfig::for_docker();
    let opts = TcpSocketOptions::default().nodelay(true);
    let rt = TetoRuntime::start(cfg).await?;
    let mut listener = TetoTcpListener::bind(&rt, "0.0.0.0:8080".parse().unwrap(), opts).await?;

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

Behaviour worth knowing:

- **Backpressure.** Each connection buffers at most 256 KiB in each direction between tokio and the F-Stack thread. `write` returns `Pending` when the send buffer is full; when your task falls behind on reads, the F-Stack thread stops reading the socket and TCP flow control slows the peer down. `flush` completes once F-Stack has accepted everything written.
- **Half-close.** `read` returning `Ok(0)` means the peer shut down its write side; you can still write a reply. `shutdown()` flushes and sends FIN.
- **Errors.** A reset connection fails reads and writes with `ConnectionReset`. Dropping a stream closes it gracefully: buffered writes are sent, then FIN, and the socket is released once the peer has acknowledged everything. A peer that doesn't take the data within 30 s gets a reset.
- **Runtime.** `TetoRuntime::start` runs F-Stack on a dedicated thread, once per process. Any number of listeners, UDP sockets and outbound connections (`TetoTcpStream::connect`) can be created from it. A failed bind leaves the runtime usable. The thread exits once every handle and socket has been dropped, after closing connections have delivered their data. F-Stack can't be restarted in the same process.
- **Limits.** IPv4 only.

## Low-Level API (teto-dpdk)

The low-level API gives you non-blocking F-Stack sockets and a kqueue, and runs your code once per iteration of F-Stack's poll loop. Everything is single-threaded: `FStack` and the sockets are `!Send`, and all calls happen inside `FStack::run`.

```toml
[dependencies]
teto-dpdk = "0.3"
```

```rust
use std::io;
use teto_dpdk::event::{Events, Interest, Kqueue};
use teto_dpdk::net::TcpListener;
use teto_dpdk::{FStack, FStackConfig, TcpSocketOptions};

fn main() -> io::Result<()> {
    let fs = FStack::init(&FStackConfig::for_docker())?;
    let opts = TcpSocketOptions::default().nodelay(true);
    let listener = TcpListener::bind(&fs, "0.0.0.0:8080".parse().unwrap(), &opts)?;
    let kq = Kqueue::new(&fs)?;
    kq.register(&listener, 0, Interest::READABLE)?;
    let mut events = Events::with_capacity(256);

    fs.run(|| {
        // Called once per poll-loop iteration; must not block.
        kq.poll(&mut events).expect("kqueue");
        for ev in events.iter() {
            // accept / read / write; WouldBlock means "try again later"
        }
    })
}
```

See [`examples/tcp_echo.rs`](examples/tcp_echo.rs) for a complete echo server that handles slow peers, and [`examples/udp_echo.rs`](examples/udp_echo.rs) for UDP.

Rules the API enforces:

- `FStack::init` succeeds once per process (DPDK can't be re-initialised).
- `FStack::run` runs once. When it returns (after `FStack::stop`, or a panic in your closure, which is re-raised), F-Stack tears itself down; afterwards every socket operation returns `BrokenPipe`.
- kqueue registrations are level-triggered and carry a `u64` token you choose. Use tokens that are never reused, not descriptor numbers.
- Unsupported input is an error, not a silent no-op: IPv6 addresses fail with `InvalidInput`, and a socket option F-Stack rejects fails the bind. There is no `quickack` option: FreeBSD has no per-socket `TCP_QUICKACK`; set `net.inet.tcp.delayed_ack=0` in `config.ini` to ACK immediately (global).

## Configuration

F-Stack is configured via `config.ini`. The `FStackConfig` builder generates the correct arguments for different environments:

```rust
// Docker / TAP device (development)
let cfg = FStackConfig::for_docker();

// Bare metal / AWS with a real NIC bound via VFIO
let cfg = FStackConfig::for_bare_metal();

// Chosen at run time: TETO_PROFILE=docker|bare-metal (default docker)
let cfg = FStackConfig::from_env()?;

// Custom
let cfg = FStackConfig::new("/etc/teto/config.ini")
    .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
    .with_eal_arg("--no-pci");
```

The profiles read `config.ini` from the working directory unless `TETO_CONFIG` names another file (or use `with_config_file`). A missing file is reported before F-Stack is initialised, so it can be fixed without restarting the process.

F-Stack and DPDK print a few dozen lines while initialising (EAL messages, a config echo, interface setup). `FStackConfig::capture_init_output(true)` keeps them off the terminal. On failure they're appended to the error, which is usually the best diagnostic available; on success they're available from `FStack::init_output()` / `TetoRuntime::init_output()`. It redirects the process's stdout/stderr for the duration of init, so leave it off if other threads may be printing then. For DPDK's runtime logging, the `[dpdk]` `log_level` key in `config.ini` applies.

## Project Structure

```
teto-dpdk/                          (Cargo workspace root)
├── src/
│   ├── lib.rs              # Crate root and re-exports
│   ├── config.rs           # FStackConfig + TcpSocketOptions builders
│   ├── runtime.rs          # FStack: init / run / stop lifecycle
│   ├── net.rs              # TcpListener, TcpStream, UdpSocket
│   ├── event.rs            # Kqueue, Interest, Events
│   └── sys.rs              # cxx bridge definition (Rust ↔ C++)
├── examples/
│   ├── udp_echo.rs         # Low-level UDP echo
│   └── tcp_echo.rs         # Low-level TCP echo (kqueue)
├── tests/                  # Integration tests (need the Docker environment)
├── cxx_layer/
│   ├── fstack_wrapper.h    # C++ shim declarations
│   └── fstack_wrapper.cpp  # Thin errno-returning wrappers over ff_* calls
├── teto-tokio/             # Async adapter crate
│   ├── src/
│   │   ├── lib.rs          # Re-exports TetoTcpListener, TetoTcpStream, TetoUdpSocket
│   │   ├── conn.rs         # Per-connection state shared with the F-Stack thread
│   │   ├── tcp_driver.rs   # F-Stack-thread side: kqueue loop, accept, read/write
│   │   ├── tcp_listener.rs # Async TCP listener (mirrors tokio::net::TcpListener)
│   │   ├── tcp_stream.rs   # Async TCP stream (AsyncRead + AsyncWrite)
│   │   └── udp_socket.rs   # Async UDP socket and its F-Stack-thread driver
│   ├── tests/              # Integration tests (need the Docker environment)
│   └── examples/
│       ├── tcp_echo_async.rs   # Async TCP echo (cargo run -p teto-tokio --example tcp_echo_async)
│       ├── udp_echo_async.rs   # Async UDP echo (cargo run -p teto-tokio --example udp_echo_async)
│       ├── kernel_echo.rs      # Same echo on tokio::net, as a benchmark baseline
│       └── bench_client.rs     # RTT / throughput client
├── scripts/bench.sh        # teto vs kernel echo benchmark
├── build.rs                # Builds the cxx layer; emits F-Stack/DPDK link settings that reach dependents
├── ci/downstream/          # Out-of-workspace crate CI builds to check downstream linking
├── config.ini              # F-Stack / DPDK configuration (Docker/TAP)
├── Dockerfile              # Builds DPDK + F-Stack from source
├── entrypoint.sh           # Configures the kernel-side TAP device
└── docs/
    ├── architecture.md     # Threads, buffers, lifecycles, linking, limits
    ├── testing-docker.md   # Docker testing guide and diagnostics
    ├── bare-metal-setup.md # Bare metal and AWS setup guide
    └── config-reference.md # All config.ini keys explained
```

## How It Works

1. **DPDK TAP PMD** creates a virtual network device pair: a DPDK ethdev (polled in userspace) and a kernel-visible TAP interface (`dtap0`).

2. **F-Stack** runs a FreeBSD TCP/IP stack on top of the DPDK ethdev. It processes Ethernet frames, handles ARP, and delivers payload to `ff_socket` descriptors.

3. **The entrypoint** configures the kernel side of `dtap0` with IP `10.0.0.2/24` and a **different MAC address** from the DPDK side. The distinct MAC is critical: FreeBSD's `ether_input` drops frames whose source MAC matches the interface MAC (anti-loop), so if both sides of the TAP share the same MAC, F-Stack silently drops all ARP replies.

4. **`cargo run`** initializes F-Stack, creates a socket bound to `0.0.0.0:8080`, and enters the DPDK poll loop, calling your code once per iteration.

## Testing

The integration tests need F-Stack, so they run in the Docker environment (each test binary starts its own F-Stack instance on the TAP device; cargo runs them one at a time):

```bash
docker run --privileged -it -v $(pwd):/app teto-dpdk bash
cargo test --workspace     # inside the container
```

They cover the failure modes that matter for a network stack: half-close, connection reset, descriptor reuse after close, slow readers and slow consumers (backpressure in both directions), dropping a stream with unsent data, many concurrent connections, UDP bursts, init/bind errors, and panics in the poll loop.

## Performance

No performance numbers are published yet. `scripts/bench.sh` runs the same echo benchmark (RTT percentiles and throughput) against teto-tokio and a `tokio::net` baseline. Numbers from the Docker/TAP setup mostly measure the TAP device and, on Apple Silicon, x86 emulation. A meaningful comparison needs two hosts with real NICs; the script header describes how to run one.

The shipped `config.ini` sets `pkt_tx_delay=0`, so F-Stack transmits immediately instead of batching for up to 100 µs (its own default). In the Docker setup that took echo p50 RTT from 200 µs to 36 µs, with no measurable throughput change. On a real NIC at high packet rates, batching may matter for bulk throughput, so raise it toward 100 for throughput-bound workloads.

The data path copies each payload once between F-Stack and the per-connection buffer, and once between that buffer and your buffer (the same count as a kernel socket read). There are no per-message allocations on the TCP path. UDP datagrams are carved out of 1 MiB blocks.

## Documentation

| Document | Description |
|----------|-------------|
| [docs/architecture.md](docs/architecture.md) | How the crates fit together: threads, buffers and backpressure, connection and runtime lifecycles, linking, known limits |
| [docs/testing-docker.md](docs/testing-docker.md) | Step-by-step Docker testing guide, startup output walkthrough, and a full diagnostic checklist |
| [docs/bare-metal-setup.md](docs/bare-metal-setup.md) | Setting up hugepages, IOMMU, NIC binding with VFIO, and AWS SR-IOV configuration |
| [docs/config-reference.md](docs/config-reference.md) | Every `config.ini` key explained, including common pitfalls and silently-ignored keys |

## Requirements

- **Docker testing**: Docker with `--privileged` support, Linux or WSL2
- **Bare metal / AWS**: hugepages, IOMMU/VT-d enabled, NIC bound via `vfio-pci` — see [docs/bare-metal-setup.md](docs/bare-metal-setup.md)

## License

This project is licensed under the [Apache License 2.0](LICENSE) — free for everyone, any purpose (including proprietary and closed-source use), subject only to the attribution and notice terms of the license.

teto-dpdk statically links against [F-Stack](https://github.com/F-Stack/f-stack) and, through it, other third-party components (DPDK, FreeBSD, Nginx, Redis and others) under their own permissive and copyleft licenses. Those licenses are unaffected by this project's license and continue to govern their respective components — see [NOTICE](NOTICE) for the full attributions and terms.

**Note for downstream:** teto-dpdk is licensed permissively, but building it links against F-Stack/DPDK and related components, some of which are BSD- or GPL-2.0-licensed (see [NOTICE](NOTICE)). If you distribute a **compiled binary** that statically links these, that binary's redistribution terms are governed by those components' licenses — not by teto-dpdk's Apache-2.0 license. Using teto-dpdk as a source dependency imposes no such obligation on you.

Unless you explicitly state otherwise, any contribution you submit for inclusion in teto-dpdk shall be licensed under the Apache License 2.0, without any additional terms or conditions. See [CONTRIBUTING.md](CONTRIBUTING.md).

### Licensing history

| Versions | License |
|---|---|
| 0.1.0 – 0.1.2 (July 2026) | AGPL-3.0-only (with a separate commercial licence offered) |
| 0.2.0 and later | Apache-2.0 |

The 0.1.x releases remain on crates.io under their original AGPL-3.0 terms. A licence change applies only to new releases, so if you depend on 0.1.x, those terms still apply to that code. Upgrade to 0.2.0 or later for Apache-2.0.

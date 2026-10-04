# Architecture

How teto is put together, and why. Read this before changing the runtime,
the driver or the build script.

## Layers

```
 your code ───────────────────────────────────────────────────────────────
   teto-tokio   TetoRuntime · TetoTcpListener · TetoTcpStream · TetoUdpSocket
                (async, Send + Sync; talks to the F-Stack thread, never to F-Stack)
 ─────────────────────────────────────────────────────────────────────────
   teto-dpdk    FStack (init / run / stop) · net::{TcpListener, TcpStream,
                UdpSocket} · event::Kqueue          (safe, !Send, non-blocking)
                src/sys.rs: cxx bridge
 ─────────────────────────────────────────────────────────────────────────
   cxx_layer/   thin C++ shim over ff_* calls: socket calls return -errno, init
                throws (→ Err), one trampoline calls Rust each loop iteration
 ─────────────────────────────────────────────────────────────────────────
   F-Stack      FreeBSD TCP/IP stack in user space (libfstack.a)
   DPDK         poll-mode NIC drivers (in Docker: af_packet on a veth pair)
```

You can use either crate on its own terms:

- **teto-dpdk** runs your code on the F-Stack thread itself, once per
  poll-loop iteration (`FStack::run(|| ...)`). There's no extra thread hop,
  but you write a non-blocking event loop by hand (see `examples/tcp_echo.rs`).
- **teto-tokio** gives you ordinary async/await sockets. A dedicated F-Stack
  thread does the socket work, and data crosses threads through per-connection
  buffers.

## teto-dpdk

### The F-Stack lifecycle

F-Stack is a process-wide singleton, and initialisation can't be undone:

```
UNINIT ──init──▶ READY ──run──▶ RUNNING ──loop returns──▶ FINISHED
```

- `FStack::init` succeeds once per process. A second call, or a call after a
  failed one, returns `AlreadyExists`, because DPDK can't be re-initialised.
  A missing config file is detected *before* touching F-Stack, so that one
  mistake stays recoverable.
- `FStack::run` runs at most once. When F-Stack's loop returns, `ff_run` tears
  the stack down (`ff_unload_config`, `rte_eal_cleanup`). From then on every
  socket call returns `BrokenPipe`, and dropping a socket skips `ff_close`,
  which would otherwise be a use-after-free.
- `FStack` and every socket are `!Send`, because F-Stack must only be called
  from the thread that initialised it. Holding an `FStack` proves you're on that
  thread after a successful init.
- A panic in the `run` closure is caught at the FFI boundary, the loop is
  stopped, and the panic resumes from `run`. Unwinding through F-Stack's C
  frames would abort.

### Sockets and the kqueue

All sockets are non-blocking. Operations that would block return
`WouldBlock`, and the `Kqueue` tells you when to retry. Registrations are
**level-triggered** and carry a caller-chosen `u64` token. Always use tokens
that are never reused, never descriptor numbers. F-Stack reuses descriptors
immediately, and that was the cause of the original cross-connection data bug.

Errors are Linux `errno` values (F-Stack translates FreeBSD's), so
`io::ErrorKind` works as usual. `SO_ERROR` comes back in FreeBSD numbering
and the shim translates it.

## teto-tokio

### Threads

```
 tokio worker threads                            F-Stack thread (one per process)
 ────────────────────                            ───────────────────────────────
 TetoTcpStream ──┐  per-connection Mutex<ConnState>  ┌── Driver::tick() once per
 TetoTcpStream ──┼─▶  rx buffer ◀── read ─────────────┤   poll-loop iteration:
 ...             │    tx buffer ─── write ───────────▶│    1. run commands
                 │    flags, error, wakers            │    2. service notified conns
                 └─▶ Notifier (ids needing service) ─▶│    3. housekeeping: closed listeners,
 TetoRuntime ─────▶ command channel ─────────────────▶│       cancelled connects, UDP sends
   (bind, listen, connect)                            │    4. kqueue poll → events: accept,
                                                      │       read/write, connect done, UDP rx
                                                      │    5. drain deadlines, stop check
```

- **Commands** (`ListenTcp`, `BindUdp`, `Connect`) carry a oneshot reply. A
  failed bind is just an `Err`, and the runtime stays usable.
- **Connections** share an `Arc<Conn>`: a mutex-protected `ConnState` plus the
  connection's id. Tokio tasks only touch buffers and flags. Every F-Stack call
  happens on the F-Stack thread.
- **Notifications**: when a task needs the driver (it queued bytes, drained the
  receive buffer, requested shutdown, dropped the stream), it pushes the
  connection id onto the `Notifier`. A `notified` flag keeps each id queued at
  most once. Lock order is always connection → notifier. Wakers are collected
  under the lock and fired after it is released.
- **Ids** come from one counter, are never reused, and double as kqueue
  tokens. A stale handle can only reach its own `Arc<Conn>`.

### Data path and backpressure

| Buffer | Bound | When full |
|---|---|---|
| per-connection receive (`rx`) | 256 KiB (`RX_HIGH`) | driver stops reading the socket (read interest removed); TCP flow control slows the peer; resumes below 128 KiB (`RX_LOW`) |
| per-connection send (`tx`) | 256 KiB (`TX_LIMIT`) | `poll_write` returns `Pending` until the driver has handed bytes to F-Stack |
| accept queue | 1024 per listener | listener stops accepting until the queue is empty again; clients wait in F-Stack's backlog (an accept error also pauses it, after being reported once) |
| UDP receive / send queues | 1024 datagrams each | stop reading (F-Stack's socket buffer drops on overflow) / `send_to` waits |

Payloads are copied twice in each direction: received data from F-Stack's
mbufs into `rx` (read straight into spare `BytesMut` capacity), then from
`rx` into your buffer; written data into `tx`, then from `tx` into F-Stack's
mbufs. That is one copy more than a kernel socket (which copies once between
its buffers and yours), the price of handing data between threads. There are
no per-message allocations on the TCP path; UDP datagrams are carved out of
1 MiB blocks.

`write` completes once bytes are in `tx`; `flush` completes once `tx` is
empty, i.e. F-Stack has them in its socket send buffer. That is the point a
kernel socket's `write` already reaches. (Kernel sockets' `flush` is a
no-op.) Because of that, `flush` and `shutdown` can wait as long as the peer
doesn't read.

### Connection lifecycle

```
accept/connect ──▶ open ──peer FIN──▶ read EOF (Ok(0)); writing still allowed
                    │
                    ├─ shutdown() ──▶ flush tx, send FIN; reading still allowed
                    │                 (Ok if the FIN was already sent)
                    ├─ error (RST, timeout) ──▶ reads return data already
                    │                            buffered, then the error;
                    │                            writes return the error
                    └─ drop ──▶ draining:
                                  1. discard unread input; hand remaining tx
                                     to F-Stack
                                  2. FIN; keep discarding input
                                  3. close as soon as the peer has ACKed
                                     everything (immediately if it already
                                     has; otherwise on kqueue EVFILT_EMPTY)
                                  4. after 30 s: abort (RST) instead
```

- Accepted connections aren't read until your code takes them from the
  accept queue, so unaccepted clients can't make the driver buffer data.
- A connection lost on its way to your code (left in a dropped listener's
  queue, or a `connect` cancelled at the wrong moment) travels as a
  `Connected` value whose `Drop` starts the same draining sequence.

### Runtime lifecycle

The F-Stack thread runs until every `TetoRuntime` clone and every socket
created from one has been dropped (each holds a runtime handle). It then
waits for draining connections, then for a further second (`STOP_GRACE`) so
final FINs and ACKs leave, and then stops the loop, which tears F-Stack down.
Data still being delivered is lost if the process exits first.

If a driver tick panics, the loop stops. Every stream then gets a "runtime
stopped" error, and channels close so pending calls fail instead of hanging.

## Building and linking

`build.rs` compiles the C++ shim and emits link settings as
`rustc-link-lib`/`rustc-link-search`, which cargo propagates to every crate
that depends on teto. (`rustc-link-arg` would only reach this package's own
targets, and downstream applications would fail to link; CI builds
`ci/downstream` to catch that.)

- DPDK's `pkg-config --static` flags are translated: archives inside its
  `--whole-archive` group become `static:+whole-archive,-bundle` libraries,
  system libraries become `dylib`.
- `libfstack.a` is pre-linked (`ld -r`) into one object. A small linker
  script defines the FreeBSD linker sets' `__start_set_*`/`__stop_set_*`
  symbols inside those sections. FreeBSD finds SYSINITs and sysctls by walking
  these sets, and linkers that garbage-collect sections only referenced by
  such symbols (lld, Rust's default on x86_64 Linux since 1.90) would
  otherwise drop them. Defining the symbols keeps the sections alive with any
  linker, without `-z nostart-stop-gc`, which a library can't pass to its
  dependents.
- The pre-linked object is re-archived as `libfstack_teto.a` in the build's
  `OUT_DIR`. Building needs GNU binutils (`ld`, `readelf`, `ar`), i.e. Linux.
- `FF_PATH` selects the F-Stack tree (default `/opt/f-stack`).

## Scaling across cores

One F-Stack instance is one thread on one NIC queue. To use more cores, run
more processes: F-Stack's multi-process mode gives each process its own core,
its own NIC queue and its own FreeBSD stack, and nothing is shared on the data
path.

- `lcore_mask` in `config.ini` lists the cores, and the port's `lcore_list`
  lists the same cores (one RX/TX queue each). Every process uses the same
  `config.ini`.
- Each process is started with
  `FStackConfig::with_process(ProcType::Primary | ProcType::Secondary, id)`.
  `id` picks the process's core: the `id`-th set bit of `lcore_mask`. Process
  0 is the primary: it configures the NIC (one queue per process, RSS
  enabled) and the shared memory. Start it first and let it finish
  initialising before starting the secondaries.
- Each process runs its own `FStack` or teto-tokio runtime (`TetoRuntime`
  and local mode are per-process singletons, so nothing changes in the
  code), and each listens on the same address and port. The NIC's RSS hash
  sends each flow to one queue, so a connection lives entirely in one
  process. ARP replies are copied to every process.
- Outbound IPv4 connections pick a local port whose RSS hash maps back to
  the connecting process's queue, so replies reach the right process.
  F-Stack doesn't do this for IPv6 or for `connect_from` (which binds
  before connecting, even with port 0), so those can have replies land on
  another process.
- If the primary exits, the secondaries have to be restarted with it.

DPDK multi-process requires hugepages: a secondary maps the primary's memory
from hugepage files. It can't run with `no_huge=1`, so it can't run in the
Docker setup. Tried there, the secondary fails with
`Could not open /var/run/dpdk/rte/hugepage_data`. Because DPDK calls
`rte_exit` on that failure, the process exits instead of `init` returning an
error. The primary alone runs fine in Docker with two af_packet queues
(`qpairs=2`). Setup steps are in
[bare-metal-setup.md](bare-metal-setup.md#8-multiple-cores-optional).

## Known limits

- **One core per process.** Scaling is one process per core (see above),
  and it's untested by the project because it needs hugepages. Within a
  process, everything runs on the F-Stack thread.
- **Cross-thread hop (`TetoRuntime`).** Every operation crosses between
  tokio's threads and the F-Stack thread, with a mutex and a wakeup each way.
  In the Docker setup that roughly doubles echo latency (p50 11.6 µs vs
  5.4 µs). Local mode (`teto_tokio::local`) avoids it: a current-thread tokio
  runtime runs *on* the F-Stack thread, driven from F-Stack's poll loop
  (each iteration polls the kqueue, wakes tasks whose sockets are ready via
  one-shot registrations, then lets ready tasks run), and its socket types
  call F-Stack directly. The price is a single thread for everything.
- **IPv6** needs an `addr6`/`prefix_len` for the port in `config.ini` (the
  Docker config has `fd00::1/64`). Each socket is one family: an IPv6
  listener doesn't accept IPv4 connections.
- **UDP bursts to a peer F-Stack hasn't resolved yet.** While ARP resolves,
  FreeBSD queues at most 16 packets per destination (`net.link.arp.maxhold`,
  settable under `[freebsd.sysctl]` in `config.ini`); more are dropped. TCP
  recovers by retransmitting; UDP doesn't.
- **x86_64 Linux only.** F-Stack's arm64 support is incomplete, and the build
  script needs GNU binutils.
- **F-Stack version.** Pinned to v1.25. F-Stack master (as of July 2026) runs no
  FreeBSD kernel timers, which breaks retransmission.
  `teto-tokio/tests/faults.rs` fails on it, so run that suite when upgrading.

## Testing

Everything except docs builds needs F-Stack, so tests run in the Docker image.
`entrypoint.sh` creates the veth pair `teto0` ⇄ `teto0-dpdk` that F-Stack
attaches to with DPDK's `af_packet` driver; the container needs only
`NET_ADMIN`. F-Stack can start once per process, so the tests run under
cargo-nextest, which gives each test its own process (one at a time: they
share the veth pair). `scripts/test.sh` runs everything in a container; it
is also what CI runs:

- **Behaviour:** `teto-tokio/tests/{tcp,udp,runtime}.rs` and
  `tests/lowlevel.rs`: half-close, resets, descriptor reuse, backpressure in
  both directions, drop delivery, multiple sockets, outbound connect, runtime
  exit.
- **Init and failure paths:** `tests/{init_bad_config,init_eal_overflow,run_panic}.rs`.
- **Network faults:** `teto-tokio/tests/faults.rs` impairs the veth link with `tc`
  (loss, delay, reordering, outages, silent peers).
- **Scale:** `teto-tokio/tests/scale.rs` (1000 concurrent connections, churn
  with a memory check; `TETO_SOAK_SECS` for long soaks).
- **Downstream linking:** `ci/downstream`.

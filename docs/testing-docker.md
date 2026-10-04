# Testing in Docker

DPDK and F-Stack are built inside the project's Docker image, so the only
thing your machine needs is Docker. Everything runs in a container with its
own network namespace; nothing on the host's network changes.

> **Tested against:** [F-Stack](https://github.com/F-Stack/f-stack) v1.25
> (DPDK 23.11.5), pinned in the Dockerfile. Avoid F-Stack master as of
> mid-2026: its FreeBSD 15 port never runs kernel timers, so TCP
> retransmission, delayed ACKs and keepalives don't work. When changing
> versions, run the test suite (it includes timer checks) first.

## How the container is set up

There's no NIC in a container, so `entrypoint.sh` creates a veth pair when the
container starts, before anything else runs:

```
 kernel side                         F-Stack side
 ┌─────────────────┐                ┌───────────────────────────────┐
 │ client (nc,     │                │ your program (teto)           │
 │ tests)          │                │ F-Stack            10.0.0.1   │
 │ teto0  10.0.0.2 │◀═══ veth ═════▶│ teto0-dpdk (no IP)            │
 └─────────────────┘                │ DPDK af_packet driver         │
                                    └───────────────────────────────┘
```

DPDK's `af_packet` driver attaches to `teto0-dpdk` in place of a NIC
(`FStackConfig::for_docker()` passes `--vdev=net_af_packet0,iface=teto0-dpdk`).
The kernel reaches F-Stack at `10.0.0.1` through `teto0`. The entrypoint also
turns off TX checksum offload on `teto0`. Otherwise veth hands packets over with
their checksums left for "hardware" to fill in, and F-Stack drops them as
corrupt.

The container needs the `NET_ADMIN` capability, for creating the veth pair
and for the fault-injection tests' `tc` rules. It doesn't need
`--privileged`. Never run it with `--network=host`: the veth pair would be
created in the host's network namespace.

## Run the test suite

```bash
scripts/test.sh                              # everything
scripts/test.sh -E 'test(half_close)'        # a subset (cargo-nextest filter)
TETO_SOAK_SECS=3600 scripts/test.sh -E 'test(churn)'   # an hour-long churn soak
```

The script builds the image if needed and runs `scripts/ci-test.sh` in a
container: clippy, the MSRV build, the integration tests, doctests, and a
build of a separate crate that depends on teto (a linking check). CI runs the
same script.

The first image build compiles DPDK and F-Stack, which takes about 20 minutes
under x86 emulation on Apple Silicon. Later runs reuse Docker's cache.

F-Stack can start only once per process, so the integration tests run under
[cargo-nextest](https://nexte.st), which gives every test its own process.
Tests run one at a time because they share the veth pair. Plain `cargo test`
stops with a message saying to use nextest.

## Try it by hand

Start a long-running container and open shells in it. Every shell in the same
container shares its network namespace, which is what you need to reach
F-Stack.

```bash
docker compose up -d
docker compose exec teto-dpdk bash      # shell 1: run an example
docker compose exec teto-dpdk bash      # shell 2: talk to it
```

In shell 1, run one of:

```bash
cargo run --example udp_echo                       # low-level UDP echo
cargo run --example tcp_echo                       # low-level TCP echo (kqueue)
cargo run -p teto-tokio --example tcp_echo_async   # async TCP echo
cargo run -p teto-tokio --example udp_echo_async   # async UDP echo
```

F-Stack prints a few dozen lines while it initialises. They come from DPDK,
the config echo and interface setup, and end with:

```
f-stack-0: Ethernet address: xx:xx:xx:xx:xx:xx
f-stack-0: Successed to register dpdk interface
```

followed by the example's own line, e.g. `Listening on 0.0.0.0:8080` or
`Listening — ready for connections.`

In shell 2:

```bash
echo "Hello F-Stack!" | nc -u -w1 10.0.0.1 8080     # UDP examples
echo "Hello F-Stack!" | nc -w3 10.0.0.1 8080        # TCP examples
```

You should see the message echoed back. Stop with `docker compose down`.

Run one F-Stack program at a time per container: they would share
`teto0-dpdk`.

## Diagnostics

**F-Stack doesn't start.** Its error message ends with F-Stack's and DPDK's
own output when the config sets `capture_init_output(true)`; otherwise that
output went to the terminal just before the error. Check that the veth pair
exists (`ip link show teto0-dpdk`). If it doesn't, the container wasn't
started through `entrypoint.sh` or lacks `NET_ADMIN`.

**Nothing comes back.**

```bash
ip -br addr show dev teto0          # UP, 10.0.0.2/24
ethtool -k teto0 | grep tx-check    # tx-checksumming: off
ip neigh show dev teto0             # 10.0.0.1 lladdr <F-Stack's MAC> REACHABLE
tcpdump -i teto0 -nn -e             # watch both directions
ip -s link show teto0               # TX = sent to F-Stack, RX = from F-Stack
```

- No `10.0.0.1` neighbour entry: F-Stack isn't answering ARP. It isn't
  running, or its `config.ini` `[port0]` address isn't `10.0.0.1/24`.
- Requests leave `teto0` but nothing returns: F-Stack is dropping them. A
  common cause is TX checksum offload left on.
- Only some UDP datagrams return after a burst: while F-Stack resolves a new
  peer's MAC, FreeBSD queues at most 16 packets
  (`net.link.arp.maxhold`); the rest are dropped.

**Faults left behind.** The fault tests remove their `tc` rules when they
finish or fail, and every test clears leftovers when it starts. A test killed
outright (e.g. by a timeout) can still leave rules on `teto0` in a long-running
dev container until the next test runs. To clear by hand:
`tc qdisc del dev teto0 root; tc qdisc del dev teto0 ingress`.

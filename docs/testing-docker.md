# Testing with Docker

Docker is the recommended way to develop and test teto-dpdk. DPDK and F-Stack require specific system libraries that are complex to install on a host, and the Dockerfile handles all of that automatically.

> **Note:** The Docker image is tagged `teto-dpdk` in the commands below. Rebuild after any changes to `entrypoint.sh` or the `Dockerfile`.

## Prerequisites

- Docker with `--privileged` support
- Works on Linux and WSL2
- ~6 GB disk space for the image

> **Tested against:** [F-Stack](https://github.com/F-Stack/f-stack) v1.25 (DPDK 23.11.5), pinned in the Dockerfile. Avoid F-Stack master as of mid-2026: its FreeBSD 15 port never runs kernel timers, so TCP retransmission, delayed ACKs and keepalives don't work. Avoid 1.21.x too: its bundled DPDK has a `net_tap` RX checksum bug that drops every TCP packet. When changing versions, run `cargo test --workspace` in the container first.

## 1. Build the image

```bash
docker build -t teto-dpdk .
```

The first build compiles DPDK and F-Stack from source and takes **10–15 minutes**. Subsequent builds are cached and take a few seconds.

## 2. Start the container

```bash
docker run --privileged --network=host -it -v $(pwd):/app teto-dpdk bash
```

The `--privileged` flag is required for DPDK to map memory and create TAP devices. `--network=host` shares the host's network namespace so the TAP device is accessible across terminals.

## 3. Choose an example and run it

Four entry points are available. All use the same Docker image and `config.ini`.

### Low-level UDP echo

```bash
cargo run --example udp_echo
```

### Low-level TCP echo

```bash
cargo run --example tcp_echo
```

### Async TCP echo (teto-tokio)

```bash
cargo run -p teto-tokio --example tcp_echo_async
```

### Async UDP echo (teto-tokio)

```bash
cargo run -p teto-tokio --example udp_echo_async
```

The first run compiles the Rust + C++ code (~1 minute). On success you will see:

```
f-stack --no-huge -c1 -m512 --proc-type=auto
...
Port 0 Link Up - speed 10000 Mbps - full-duplex
...
f-stack-0: Ethernet address: xx:xx:xx:xx:xx:xx
f-stack-0: Successed to register dpdk interface
```

followed by the example's own line, e.g. `Listening on 0.0.0.0:8080` (low-level TCP), `Bound to 0.0.0.0:8080` (low-level UDP) or `Listening — ready for connections.` (async TCP).

Shortly after, the entrypoint's background script detects `dtap0` and prints:

```
=== F-Stack Network Config ===
TAP device     : dtap0
DPDK MAC       : xx:xx:xx:xx:xx:xx
Kernel MAC     : 02:00:00:00:00:02
Kernel IP      : 10.0.0.2
F-Stack IP     : 10.0.0.1
===============================
```

## 4. Send a test packet

Open a **second terminal into the same container**. You must send from inside the container because the `dtap0` TAP device only exists in the container's network namespace.

```bash
# Find the container ID
docker ps

# Open a second shell inside it
docker exec -it <CONTAINER_ID> bash
```

### UDP test

```bash
echo "Hello F-Stack!" | nc -u -w1 10.0.0.1 8080
```

You should see `Hello F-Stack!` echoed back. The async example also prints `[10.0.0.2:XXXXX] echoing 15 bytes`; the low-level one echoes silently.

### TCP test

```bash
# nc without -u opens a TCP connection
echo "Hello TCP!" | nc -w3 10.0.0.1 8080
```

Or for an interactive session:

```bash
nc -w10 10.0.0.1 8080
# Type a message, press Enter — it echoes back
# Ctrl-C to close
```

The first terminal prints:

```
[10.0.0.2:XXXXX] connected
[10.0.0.2:XXXXX] disconnected      (async example; the low-level one prints "closed")
```

---

## Diagnostic Checklist

If packets aren't received or echoes aren't coming back, check in this order.

### 1. Is dtap0 up and configured?

```bash
ip addr show dtap0
# Expected: state UP, inet 10.0.0.2/24
```

### 2. Does dtap0 have the right MAC?

```bash
ip link show dtap0 | grep ether
# Kernel-side MAC should be 02:00:00:00:00:02
# NOT the same as the DPDK MAC printed at F-Stack startup
```

The two sides of the TAP must have different MACs. FreeBSD's `ether_input` drops frames whose source MAC matches the interface MAC (anti-loop protection). If they match, F-Stack silently drops all ARP replies and can never send packets back.

### 3. Is the ARP entry correct?

```bash
arp -n | grep 10.0.0.1
# Should show 10.0.0.1 mapped to the DPDK MAC (not 02:00:00:00:00:02)
```

### 4. Are packets reaching dtap0?

```bash
# In a third terminal inside the container:
tcpdump -i dtap0 -nn -e udp port 8080
# Send a packet from terminal 2 — you should see:
# 1. UDP packet from 10.0.0.2 → 10.0.0.1
# 2. ARP request from 10.0.0.1 asking for 10.0.0.2
# 3. ARP reply from 10.0.0.2
# 4. UDP echo reply from 10.0.0.1 → 10.0.0.2
```

If you see steps 1–3 but not step 4, the MAC addresses are the same (see check 2).

### 5. Are packets reaching DPDK?

```bash
ip -s link show dtap0
```

The kernel's **TX** counters on `dtap0` are frames handed to DPDK; **RX** counters are frames F-Stack sent back.

- TX not incrementing → the kernel isn't routing to `dtap0` (check the address and route from step 3)
- TX incrementing, RX not → F-Stack receives frames but doesn't answer: wrong destination IP, checksum drop (step 7), or ARP failing because the MACs match (step 2)

### 6. Check the EAL arguments

F-Stack prints the arguments it built from `config.ini` (e.g. `f-stack --no-huge -c1 -m512 --proc-type=auto`). The Docker profile (`FStackConfig::for_docker()`) appends `--vdev=net_tap0,iface=dtap0,mac=fixed`, `--no-pci` and `--iova-mode=va`, which aren't echoed. If `dtap0` exists (`ip link show dtap0`) while the example runs, the `--vdev` argument took effect. F-Stack accepts at most 16 EAL arguments in total; more is reported as an initialisation error.

### 7. Check checksum offload

```bash
ethtool -k dtap0 | grep checksumming
# tx-checksumming: off
# rx-checksumming: off [fixed]
```

If tx-checksumming is `on`, the kernel sends UDP frames with incomplete checksums, which F-Stack's FreeBSD stack may reject.

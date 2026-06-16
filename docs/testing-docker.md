# Testing with Docker

Docker is the recommended way to develop and test teto-dpdk. DPDK and F-Stack require specific system libraries that are complex to install on a host, and the Dockerfile handles all of that automatically.

> **Note:** The Docker image is tagged `teto-dpdk` in the commands below. Rebuild after any changes to `entrypoint.sh` or the `Dockerfile`.

## Prerequisites

- Docker with `--privileged` support
- Works on Linux and WSL2
- ~6 GB disk space for the image

> **Tested against:** [F-Stack](https://github.com/F-Stack/f-stack) master. The Dockerfile deliberately tracks master rather than the 1.21.6 LTS tag: the DPDK bundled with 1.21.6 has a `net_tap` RX checksum bug that silently drops every valid TCP packet (UDP is unaffected). If you pin to a release, re-run the TCP echo tests below first.

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

### Low-level UDP echo (raw callbacks)

```bash
cargo run --example udp_echo
```

### Low-level TCP echo (raw callbacks)

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
Initializing F-Stack...
[EAL] arg[0]: f-stack
[EAL] arg[1]: --no-huge
...
[DPDK] Available ports: 1
[DPDK] Port 0 MAC=xx:xx:xx:xx:xx:xx link=UP speed=10000Mbps
...
f-stack-0: Ethernet address: xx:xx:xx:xx:xx:xx
f-stack-0: Successed to register dpdk interface
```

For TCP you will additionally see:

```
[TCP] Listening on 0.0.0.0:8080 fd=0
Starting F-Stack TCP event loop...
```

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

You should see `Hello F-Stack!` echoed back, and the first terminal prints:

```
[UDP] Received 15 bytes from 10.0.0.2:XXXXX
```

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
[TCP] New connection fd=1 from 10.0.0.2:XXXXX
[TCP] Received 11 bytes from 10.0.0.2:XXXXX on fd=1
[TCP] Connection closed fd=1
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

### 5. Are DPDK stats incrementing?

The event loop prints DPDK stats every ~500k iterations. Look for:

```
[DPDK] ipackets=N opackets=N ierrors=0 imissed=0 rx_nombuf=0
```

- `ipackets` not incrementing → packets not reaching DPDK from the TAP at all
- `ipackets` incrementing but no `Received UDP packet` log → F-Stack is receiving frames but dropping at the IP/UDP layer (checksum issue or wrong destination IP)
- `ipackets` and logs present but no echo → ARP resolution failing (MAC mismatch)

### 6. Check EAL args at startup

```
[EAL] arg[N]: ...
```

Verify these are all present:
- `--no-pci` — prevents DPDK from scanning PCI and stealing port 0
- `--vdev=net_tap0,iface=dtap0` — creates the TAP device
- `--no-huge` — required in Docker (comes from `no_huge=1` in config.ini)

### 7. Check checksum offload

```bash
ethtool -k dtap0 | grep checksumming
# tx-checksumming: off
# rx-checksumming: off [fixed]
```

If tx-checksumming is `on`, the kernel sends UDP frames with incomplete checksums, which F-Stack's FreeBSD stack may reject.

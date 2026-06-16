# config.ini Reference

F-Stack reads its configuration from `config.ini` at startup via `ff_load_config`. The file uses INI syntax. This reference is derived from F-Stack's `ff_config.c` parser and `ff_config.h` struct definitions and covers every key the parser recognises.

> **Parser pitfall**: Key names are matched exactly. Hyphenated variants like `no-huge` and `iova-mode` are silently ignored. Always use the names in this document.

---

## [dpdk]

Controls DPDK EAL (Environment Abstraction Layer) initialization.

### Core assignment

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `lcore_mask` | hex | — | Bitmask of CPU cores DPDK may use. `1` = core 0, `2` = core 1, `3` = cores 0 and 1. **Required.** |

### Memory

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `no_huge` | 0/1 | `0` | Use anonymous memory instead of hugepages. **Required in Docker.** Remove on bare metal and configure real hugepages. Note: `no_huge` with an underscore — `no-huge` is silently ignored. |
| `memory` | int (MB) | — | Memory allocation in megabytes. Only meaningful with `no_huge=1`. |
| `channel` | int | — | Number of DPDK memory channels. Match your hardware (commonly 2 or 4). Omit with `no_huge=1`. |
| `base_virtaddr` | string | — | Base virtual address for DPDK memory mapping (e.g. `0x7f0000000000`). Useful to avoid address conflicts in containers. |

### NIC / port

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `port_list` | int list | — | Comma/range list of DPDK port IDs that F-Stack manages (e.g. `0`, `0,1`, `0-3`). **Required.** Must have a matching `[portN]` section for each ID. |
| `allow` | PCI addr | — | PCI address of the NIC to bind (bare metal / AWS SR-IOV). Equivalent to DPDK `--allow`. Omit when using a TAP vdev. |
| `promiscuous` | 0/1 | `1` | Put the NIC in promiscuous mode (accept all Ethernet frames). |
| `numa_on` | 0/1 | `1` | Enable NUMA-aware memory allocation. Set to `0` in Docker or single-socket machines. |

### Offloading

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `tx_csum_offoad_skip` | 0/1 | `0` | Skip TX checksum offloading. **Set to `1` for TAP devices**, which have no hardware offload. |
| `tso` | 0/1 | `0` | TCP Segmentation Offload. Only enable if your NIC supports it. |
| `vlan_strip` | 0/1 | `1` | Strip VLAN tags in hardware on RX. |
| `vlan_filter` | int list | — | VLAN IDs to enable filtering for (comma/range). When set, `[vlanN]` sections override `[portN]` IP config entirely. |

### Pacing

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `idle_sleep` | int (µs) | `0` | Sleep this many microseconds when no packets arrive. `0` = busy poll (max throughput, burns a full core). Increase to reduce CPU usage at the cost of latency. |
| `pkt_tx_delay` | int (µs) | `100` | Delay before flushing a TX burst smaller than 32 packets. `0` = send immediately. Capped at 100. |

### RSS

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `symmetric_rss` | 0/1 | `0` | Use symmetric RSS hash key so both directions of a flow hash to the same RX queue. |

### Multi-process

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `file_prefix` | string | — | Unique prefix for DPDK shared memory files. Required when running multiple F-Stack processes on the same host. Produces `--file-prefix=container-<value>`. |

### Logging

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `log_level` | 1–8 | — | DPDK log verbosity. 1=EMERG, 4=ERR, 7=INFO, 8=DEBUG. |
| `fstack_log_level` | 0–8 | `0` | F-Stack internal log level. `0` = disable file logging (output to stderr). |
| `fstack_log_file_prefix` | string | — | Path prefix for F-Stack log files (e.g. `./f-stack` → `./f-stack-0.log`). |

### Virtual / bonded devices

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `nb_vdev` | int | `0` | Number of `[vdevN]` sections. Required if using F-Stack's vdev config for virtio-user or vhost-user. When `nb_vdev > 0`, F-Stack also automatically adds `--no-pci` and `--file-prefix=container`. |
| `nb_bond` | int | `0` | Number of `[bondN]` sections for NIC bonding / LACP. |

---

## [portN]

One section per port ID in `port_list` (e.g. `[port0]`, `[port1]`). F-Stack creates a FreeBSD network interface for each and configures it with the settings below.

### Required

| Key | Type | Description |
|-----|------|-------------|
| `addr` | IPv4 | IP address for the F-Stack interface. |
| `netmask` | IPv4 | Subnet mask. |
| `broadcast` | IPv4 | Broadcast address. |
| `gateway` | IPv4 | Default gateway. For TAP: the kernel-side IP. For bare metal: your router. |

### Optional

| Key | Type | Description |
|-----|------|-------------|
| `lcore_list` | int list | CPU cores that handle this port's RX/TX. Must be within `lcore_mask`. Defaults to all cores in `lcore_mask`. |
| `if_name` | string | Override the FreeBSD interface name (default: `f-stack-0`, `f-stack-1`, …). |
| `vip_addr` | string | Semicolon-separated list of additional virtual IPs on this port (max 64). E.g. `10.0.0.3;10.0.0.4`. |
| `vip_ifname` | string | Interface name for VIP aliases (default: `lo0`). |
| `slave_port_list` | int list | Slave port IDs when this port is a bonding master. |

### IPv6 (requires F-Stack compiled with `INET6`)

| Key | Type | Description |
|-----|------|-------------|
| `addr6` | IPv6 | IPv6 address for this interface. |
| `prefix_len` | int | IPv6 prefix length (e.g. `64`). |
| `gateway6` | IPv6 | IPv6 default gateway. |
| `vip_addr6` | string | Semicolon-separated list of virtual IPv6 addresses. |
| `vip_prefix_len` | int | Prefix length shared by all `vip_addr6` entries. |

---

## [vdevN]

Configures a software virtual device (virtio-user or vhost-user). Requires `nb_vdev` ≥ N+1 in `[dpdk]`. Index starts at 0.

> Note: teto-dpdk injects the TAP vdev directly as an EAL argument (`--vdev=net_tap0,iface=dtap0`) and does not use this section. Use `[vdevN]` if you switch to F-Stack's built-in vdev config system.

| Key | Required | Description |
|-----|----------|-------------|
| `path` | Yes | Path to the vhost-user socket (e.g. `/var/run/openvswitch/vhost-user0`). |
| `iface` | No | Override the kernel-visible TAP interface name. |
| `queues` | No | Number of queues (default: 1). Must be ≥ number of F-Stack processes. |
| `queue_size` | No | Queue depth (default: 256). |
| `mac` | No | MAC address (default: random). |
| `cq` | No | Enable completion queue. Required when `queues > 1` (default: 0). |

---

## [bondN]

Configures NIC bonding. Requires `nb_bond` ≥ N+1 in `[dpdk]`. Index starts at 0.

| Key | Required | Description |
|-----|----------|-------------|
| `mode` | Yes | Bonding mode: 0=round-robin, 1=active-backup, 2=balance-xor, 3=broadcast, 4=802.3ad LACP, 5=balance-tlb, 6=balance-alb. |
| `slave` | Yes | Comma-separated PCI addresses of slave NICs. |
| `primary` | No | Primary slave PCI address (used with mode 1). |
| `mac` | No | Override bonded interface MAC. |
| `socket_id` | No | NUMA socket ID for this bond. |
| `xmit_policy` | No | Transmit hash policy for modes 2/4: `l2`, `l23`, `l34`. |
| `lsc_poll_period_ms` | No | Link status check polling interval in ms. |
| `up_delay` | No | Delay in ms before marking a slave as active. |
| `down_delay` | No | Delay in ms before marking a slave as inactive. |

---

## [vlanN]

Configures IP addressing per VLAN. Requires the VLAN ID to appear in `dpdk.vlan_filter`. When any `[vlanN]` section is present, **all `[portN]` IP configuration is ignored**.

| Key | Required | Description |
|-----|----------|-------------|
| `portid` | Yes | DPDK port ID this VLAN lives on. |
| `addr` | Yes | IPv4 address. |
| `netmask` | Yes | Subnet mask. |
| `broadcast` | Yes | Broadcast address. |
| `gateway` | Yes | Default gateway. |
| `vip_addr` | No | Semicolon-separated virtual IPs. |

IPv6 keys (`addr6`, `prefix_len`, `gateway6`, `vip_addr6`, `vip_prefix_len`) are also supported if compiled with `INET6`.

---

## [kni]

Kernel NIC Interface. When enabled, packets not handled by F-Stack are forwarded to the Linux kernel through a KNI ring, making the NIC visible to both F-Stack and the kernel simultaneously.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enable` | 0/1 | `0` | Enable KNI. |
| `method` | string | — | **Required when `enable=1`.** `accept`: forward only `tcp_port`/`udp_port` to kernel. `reject`: forward everything except `tcp_port`/`udp_port`. |
| `tcp_port` | int list | — | TCP ports for the `method` filter. |
| `udp_port` | int list | — | UDP ports for the `method` filter. |
| `kni_action` | string | `default` | Override method: `alltokni` sends all packets to the kernel, `alltoff` drops all KNI packets. |
| `console_packets_ratelimit` | int (pps) | `0` | Rate limit for ARP/OSPF and other console packets into the KNI ring. 0 = unlimited. |
| `general_packets_ratelimit` | int (pps) | `0` | Rate limit for general data packets into the KNI ring. |
| `kernel_packets_ratelimit` | int (pps) | `0` | Rate limit for forwarding from KNI ring to the kernel. |

---

## [pcap]

Per-core packet capture.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enable` | 0/1 | `0` | Enable pcap capture. |
| `snaplen` | int (bytes) | `96` | Bytes to capture per packet. Minimum: 94. |
| `savelen` | int (bytes) | `16777216` | Rotate pcap file after this many bytes. Minimum: ~8 MB. |
| `savepath` | string | `.` | Directory for pcap files. |

---

## [rss_check]

Static RSS check table. When enabled, F-Stack pre-computes which local ports will hash to the current core's RX queue, avoiding repeated RSS probes when making outbound connections. Useful when F-Stack actively connects to remote servers at high rate.

| Key | Type | Description |
|-----|------|-------------|
| `enable` | 0/1 | Enable the RSS check table. |
| `rss_tbl` | string | Semicolon-separated 4-tuples: `port_id local_ip remote_ip remote_port`. Max 64 entries. E.g. `0 192.168.1.1 192.168.2.1 80;0 192.168.1.1 192.168.2.1 443`. |

---

## [freebsd.boot]

Passed to the embedded FreeBSD kernel as boot loader variables before the kernel starts.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `hz` | int | `100` | Kernel tick rate in Hz. `100` is standard. Use `1000000` (1 µs ticks) when enabling RACK or BBR congestion control. |
| `fd_reserve` | int | `0` | Block out this many low-numbered file descriptors to avoid overlap with the C library's fd space. `1024` is a safe value for complex applications. |
| `physmem` | int (bytes) | `268435456` | Physical memory to report to FreeBSD (256 MB default). |
| `memsz_MB` | int (MB) | `256` | FreeBSD heap size in MB. |

Any other key in this section is passed directly as a FreeBSD boot loader variable (equivalent to `loader.conf`).

---

## [freebsd.sysctl]

Passed to FreeBSD's sysctl interface after the kernel starts. Any valid FreeBSD sysctl name is accepted.

### UDP / IP

| Key | Default | Description |
|-----|---------|-------------|
| `net.inet.udp.checksum` | `1` | Validate UDP checksums on RX. Set to `0` for TAP setups. Restore to `1` on bare metal. |
| `net.inet.udp.blackhole` | `1` | Silently drop UDP packets to closed ports (no ICMP unreachable). |
| `net.inet.ip.redirect` | `0` | Send ICMP redirects. |
| `net.inet.ip.forwarding` | `0` | Act as an IP router. |
| `net.inet.ip.portrange.randomized` | `1` | Randomize ephemeral port selection. |
| `net.inet.ip.portrange.randomtime` | `0` | Always randomize (0 = always, default). |

### TCP

| Key | Default | Description |
|-----|---------|-------------|
| `net.inet.tcp.recvspace` | `8192` | Default TCP receive buffer (bytes). |
| `net.inet.tcp.sendspace` | `16384` | Default TCP send buffer (bytes). |
| `net.inet.tcp.recvbuf_max` | `16777216` | Maximum auto-tuned receive buffer (bytes). |
| `net.inet.tcp.sendbuf_max` | `16777216` | Maximum auto-tuned send buffer (bytes). |
| `net.inet.tcp.recvbuf_auto` | `1` | Enable receive buffer auto-tuning. |
| `net.inet.tcp.sendbuf_auto` | `1` | Enable send buffer auto-tuning. |
| `net.inet.tcp.sack.enable` | `1` | Enable Selective ACK. |
| `net.inet.tcp.blackhole` | `1` | Silently drop TCP segments to closed ports. |
| `net.inet.tcp.msl` | `2000` | Maximum Segment Lifetime in ms. |
| `net.inet.tcp.delayed_ack` | `1` | Enable delayed ACK. |
| `net.inet.tcp.rfc1323` | `1` | Enable window scaling and timestamps (RFC 1323). |
| `net.inet.tcp.fast_finwait2_recycle` | `1` | Recycle FIN_WAIT_2 state quickly. |
| `net.inet.tcp.finwait2_timeout` | `5000` | FIN_WAIT_2 timeout in ms. |
| `net.inet.tcp.maxtcptw` | `128` | Maximum TIME_WAIT sockets. |
| `net.inet.tcp.cc.algorithm` | `freebsd` | Congestion control: `freebsd`, `cubic`, `rack`, `bbr`. RACK/BBR require `hz=1000000` in `[freebsd.boot]`. |
| `net.inet.tcp.functions_default` | `freebsd` | Default TCP stack implementation. |
| `net.inet.tcp.hpts.skip_swi` | `1` | Required for BBR. |
| `net.inet.tcp.hpts.minsleep` | `250` | HPTS minimum sleep in µs. |
| `net.inet.tcp.hpts.maxsleep` | `51200` | HPTS maximum sleep in µs. |

### Sockets

| Key | Default | Description |
|-----|---------|-------------|
| `kern.ipc.maxsockets` | `262144` | Maximum number of open sockets. |
| `kern.ipc.somaxconn` | `32768` | Maximum listen backlog. |
| `kern.ipc.maxsockbuf` | `16777216` | Maximum socket buffer size (bytes). |
| `kern.ncallout` | `262144` | Size of the kernel callout wheel (timers). |

### IPv6

| Key | Default | Description |
|-----|---------|-------------|
| `kern.features.inet6` | `1` | Enable IPv6 support. |
| `net.inet6.ip6.auto_linklocal` | `1` | Auto-assign link-local addresses. |
| `net.inet6.ip6.accept_rtadv` | `2` | Accept router advertisements. |
| `net.inet6.icmp6.rediraccept` | `1` | Accept ICMPv6 redirects. |
| `net.inet6.ip6.forwarding` | `0` | Act as an IPv6 router. |

### Misc

| Key | Default | Description |
|-----|---------|-------------|
| `net.add_addr_allfibs` | `1` | Add addresses to all routing tables (fibs). Set to `0` when using policy routing. |
| `net.link.ether.inet.maxhold` | `5` | Maximum ARP packets held while waiting for ARP resolution. |

---

## Keys that look valid but are silently ignored

F-Stack's parser is strict. These produce no error but have no effect:

| Key | Why ignored | Correct approach |
|-----|-------------|-----------------|
| `no-huge=1` | Hyphen — parser expects `no_huge` | Use `no_huge=1` |
| `vdev=net_tap0,...` | Not a `[dpdk]` key; vdevs use `[vdevN]` + `nb_vdev` | Inject via EAL arg in code, or use `[vdev0]` + `nb_vdev=1` |
| `iova-mode=va` | No handler — not a recognised F-Stack config key | Inject as EAL arg: `--iova-mode=va` |
| `port_id=N` in `[portN]` | Not a recognised port key — port ID comes from the section name | Remove it |

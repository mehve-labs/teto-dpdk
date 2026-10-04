# Bare Metal and AWS Setup

This guide covers running teto-dpdk on a physical machine or an AWS instance with SR-IOV. Unlike the Docker TAP setup, here DPDK binds directly to a real NIC, bypassing both the kernel network stack and (on SR-IOV) the hypervisor data path.

---

## Prerequisites

### System dependencies

Install the same libraries the Dockerfile uses:

```bash
apt install -y \
    build-essential clang cmake \
    libnuma-dev libssl-dev \
    meson ninja-build pkg-config \
    python3 python3-pyelftools \
    pciutils ethtool iproute2
```

### Build DPDK and F-Stack

Follow the same steps as the Dockerfile -- the binaries need to be on the host. Use the F-Stack release the project is tested against (v1.25). F-Stack master as of mid-2026 runs no FreeBSD kernel timers, so TCP retransmission and keepalive don't work there.

```bash
git clone --recurse-submodules --depth 1 --branch v1.25 https://github.com/F-Stack/f-stack.git /opt/f-stack

# Build DPDK
cd /opt/f-stack/dpdk
meson setup build -Ddefault_library=static -Dc_args=-fPIC -Denable_kmods=false
ninja -C build && ninja -C build install && ldconfig

# Build F-Stack
cd /opt/f-stack/lib
FF_DPDK=/usr/local FF_PATH=/opt/f-stack make -j$(nproc)
```

If F-Stack lives somewhere other than `/opt/f-stack`, set `FF_PATH` to its source tree when building teto (`FF_PATH=/path/to/f-stack cargo build`). If DPDK isn't installed under a standard prefix, point `PKG_CONFIG_PATH` at the directory containing `libdpdk.pc`.

### Rust

teto needs Rust 1.97 or newer; CI builds with 1.99.0.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.99.0
```

---

## 1. Configure hugepages

Hugepages are required on bare metal. The `--no-huge` workaround used in Docker trades performance for convenience -- remove it here.

```bash
# Allocate 512 × 2MB hugepages = 1 GB
echo 512 > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages

# Make persistent across reboots
echo "vm.nr_hugepages = 512" >> /etc/sysctl.conf

# Mount the hugepage filesystem (if not already present in /proc/mounts)
mkdir -p /dev/hugepages
mount -t hugetlbfs nodev /dev/hugepages
```

Verify allocation:

```bash
grep HugePages /proc/meminfo
# HugePages_Total: 512
# HugePages_Free:  512   <-- should be non-zero
```

---

## 2. Enable IOMMU

IOMMU is required for DPDK's VFIO driver (the modern, safe way to hand a NIC to DPDK).

### Intel

In `/etc/default/grub`:

```
GRUB_CMDLINE_LINUX="... intel_iommu=on iommu=pt"
```

### AMD

```
GRUB_CMDLINE_LINUX="... amd_iommu=on iommu=pt"
```

Apply and reboot:

```bash
update-grub && reboot
```

Verify:

```bash
dmesg | grep -i iommu | head -5
# Should show: DMAR: IOMMU enabled
```

If you cannot enable IOMMU (some cloud VMs, older hardware), you can use VFIO in unsafe no-IOMMU mode:

```bash
echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode
```

---

## 3. Bind your NIC to DPDK

Find the PCI address of the NIC you want DPDK to own:

```bash
dpdk-devbind.py --status
# Or: lspci | grep -i eth
```

Example output:
```
0000:00:1f.6 'Ethernet Controller' if=eth0 drv=e1000e unused=vfio-pci
```

Take note of the current kernel IP on this interface if you need it for reference, then unbind it from the kernel and bind it to VFIO:

```bash
# Load the VFIO driver
modprobe vfio-pci

# Bind the NIC (replace with your PCI address)
dpdk-devbind.py --bind=vfio-pci 0000:00:1f.6

# Verify
dpdk-devbind.py --status
# Should show: drv=vfio-pci
```

> **Note**: Once bound to VFIO, the interface disappears from the kernel (`ip link` will no longer show it). All traffic to/from that NIC goes through DPDK/F-Stack.

---

## 4. Isolate a CPU core (recommended)

DPDK's poll loop burns a full core. Isolate it so the OS scheduler never preempts it:

In `/etc/default/grub` (example: isolate core 1):

```
GRUB_CMDLINE_LINUX="... isolcpus=1 nohz_full=1 rcu_nocbs=1"
```

Then set `lcore_mask=2` in config.ini (`2` in hex = bit 1 = core 1).

---

## 5. Update config.ini

Remove the Docker-specific workarounds and point DPDK at the real NIC.

```ini
[dpdk]
lcore_mask=2
promiscuous=1
# no_huge and memory are NOT needed -- use real hugepages
# allow tells DPDK to use this specific NIC; omit the TAP vdev
allow=0000:00:1f.6
port_list=0

[port0]
addr=192.168.1.10        # IP you want F-Stack to own on this NIC
netmask=255.255.255.0
broadcast=192.168.1.255
gateway=192.168.1.1      # Your actual router
lcore_list=1             # Core that handles this port (must match lcore_mask)

[freebsd.boot]
hz=100

[freebsd.sysctl]
# Leave net.inet.udp.checksum at default (1) -- real NICs compute correct checksums
```

Keep `pkt_tx_delay=0` in `[dpdk]` (the repository's default) for latency, or raise it toward 100 for bulk throughput. See [config-reference.md](config-reference.md#dpdk).

See [config-reference.md](config-reference.md) for all available keys.

---

## 6. Select the bare-metal profile

Nothing in the code changes between Docker and bare metal. The Docker profile (`FStackConfig::for_docker()`) adds the TAP device's EAL arguments (`--vdev=net_tap0,...`, `--no-pci`, `--iova-mode=va`); the bare-metal profile adds none, and DPDK finds the NIC through the `allow` key in `config.ini`.

In your own code:

```rust
use teto_dpdk::FStackConfig;
use teto_tokio::TetoRuntime;

let cfg = FStackConfig::for_bare_metal().with_config_file("/etc/teto/config.ini");
let rt = TetoRuntime::start(cfg).await?;
```

The examples pick the profile from the environment (`FStackConfig::from_env()`): `TETO_PROFILE=bare-metal` selects the bare-metal profile, and `TETO_CONFIG` names the config file (default: `config.ini` in the working directory).

Don't run `entrypoint.sh` on bare metal: there's no TAP device to configure.

---

## 7. Run

F-Stack needs root (or the capabilities for VFIO and hugepages):

```bash
sudo TETO_PROFILE=bare-metal TETO_CONFIG=$PWD/config.ini \
    $(which cargo) run --release -p teto-tokio --example tcp_echo_async
```

Send test traffic from another machine on the same network:

```bash
echo "Hello F-Stack!" | nc -w3 192.168.1.10 8080
```

To compare against the kernel stack, run `scripts/bench.sh`'s two-host procedure (see the header of that script).

---

## AWS-specific notes

### Instance types with SR-IOV

For meaningful kernel bypass on AWS, use an instance with SR-IOV support. The NIC's Virtual Function (VF) is exposed directly to the instance via PCIe passthrough:

| Family | Notes |
|--------|-------|
| `c5n`, `c6gn`, `c7gn` | High-bandwidth, SR-IOV, good for networking workloads |
| `*.metal` instances | Full bare metal -- no hypervisor at all, best latency |
| `p3dn`, `p4d` | GPU instances with high-bandwidth networking for ML/HPC |

Standard ENA instances without SR-IOV still benefit from removing the kernel stack overhead, but the hypervisor remains in the data path.

### ENA driver

AWS EC2 uses the ENA NIC. DPDK ships an ENA PMD (`librte_net_ena`). The setup is the same as above -- find the ENA VF's PCI address, bind to `vfio-pci`, set `allow=<PCI addr>` in config.ini.

```bash
# Typical ENA PCI address on EC2
lspci | grep -i "elastic network"
# 0000:00:05.0 Ethernet controller: Amazon.com, Inc. Elastic Network Adapter (ENA)

dpdk-devbind.py --bind=vfio-pci 0000:00:05.0
```

### Placement groups

For inter-instance UDP with low latency, put your instances in a **cluster placement group**. This places them on the same underlying hardware rack, minimizing network hops.

```bash
aws ec2 create-placement-group --group-name teto-dpdk-cluster --strategy cluster
aws ec2 run-instances ... --placement "GroupName=teto-dpdk-cluster"
```

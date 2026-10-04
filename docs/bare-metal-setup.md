# Bare Metal and AWS Setup

This guide covers running teto-dpdk on a physical machine or a cloud VM with an SR-IOV NIC. Unlike the Docker TAP setup, here DPDK binds directly to a real NIC, bypassing both the kernel network stack and (with SR-IOV) the hypervisor data path.

> **Status:** the project's tests run in Docker over a TAP device. This guide follows the standard DPDK and F-Stack setup but hasn't been validated end to end on real hardware with the current release. Please report anything that doesn't work. teto builds for x86_64 Linux only.

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
FF_DPDK=/usr/local FF_PATH=/opt/f-stack make -j$(nproc) CC="cc -Wno-error=array-bounds"
```

(`-Wno-error=array-bounds` works around a GCC 12+ false positive in F-Stack's FreeBSD sources.)

If F-Stack lives somewhere other than `/opt/f-stack`, set `FF_PATH` to its source tree when building teto (`FF_PATH=/path/to/f-stack cargo build`). If DPDK isn't installed under a standard prefix, point `PKG_CONFIG_PATH` at the directory containing `libdpdk.pc`.

### Rust

teto needs Rust 1.97 or newer; CI builds with 1.99.0.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.99.0
```

---

## 1. Configure hugepages

Use hugepages on bare metal. The Docker setup runs without them (`no_huge=1` in `config.ini`), which trades performance for convenience.

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

Start from the repository's `config.ini` and change it for the real NIC. Remove the Docker-only settings:

- `no_huge=1` and `memory=512`: use the hugepages from step 1;
- `tx_csum_offoad_skip=1` (sic, F-Stack's spelling): let the NIC offload checksums;
- `net.inet.udp.checksum=0` under `[freebsd.sysctl]`: real NICs deliver correct UDP checksums.

Then point DPDK at the NIC. Comments must be on their own lines: F-Stack's INI parser doesn't strip `#` comments that follow a value.

```ini
[dpdk]
lcore_mask=2
promiscuous=1
# Send immediately (latency); raise toward 100 to batch for bulk throughput.
pkt_tx_delay=0
# The NIC DPDK should use (PCI address from step 3).
allow=0000:00:1f.6
port_list=0

[port0]
# The IP F-Stack owns on this NIC.
addr=192.168.1.10
netmask=255.255.255.0
broadcast=192.168.1.255
# Your actual router.
gateway=192.168.1.1
# The lcore that handles this port (must be in lcore_mask).
lcore_list=1

[freebsd.boot]
hz=100

[freebsd.sysctl]
```

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

### In a container

The bare-metal setup also works inside a container, without the Docker/TAP workarounds. The host does steps 1–4 (hugepages, IOMMU, binding the NIC to `vfio-pci`). The container needs the VFIO devices, the hugepage mount, and permission to lock memory:

```bash
docker run --rm -it \
    --device /dev/vfio/vfio --device /dev/vfio/<group> \
    -v /dev/hugepages:/dev/hugepages \
    --cap-add IPC_LOCK --cap-add SYS_RAWIO --ulimit memlock=-1 \
    -e TETO_PROFILE=bare-metal -e TETO_CONFIG=/etc/teto/config.ini \
    -v /etc/teto:/etc/teto:ro \
    your-image ./tcp_echo_async
```

(`<group>` is the IOMMU group of the NIC: `readlink /sys/bus/pci/devices/<PCI addr>/iommu_group`.) This hasn't been validated by the project yet.

---

## 7. Run

Build as your normal user, then run the binary as root (F-Stack needs VFIO and hugepages). Running `cargo` itself under `sudo` would use root's toolchain and environment and leave a root-owned `target/`:

```bash
cargo build --release -p teto-tokio --example tcp_echo_async
sudo TETO_PROFILE=bare-metal TETO_CONFIG=$PWD/config.ini \
    ./target/release/examples/tcp_echo_async
```

Send test traffic from another machine on the same network:

```bash
echo "Hello F-Stack!" | nc -w3 192.168.1.10 8080
```

To compare against the kernel stack, run `scripts/bench.sh`'s two-host procedure (see the header of that script).

---

## AWS-specific notes

Not validated by this project; these are the standard DPDK-on-EC2 steps.

### Instance types

Nitro instances expose the Elastic Network Adapter (ENA), an SR-IOV virtual function, directly to the instance. teto is x86_64-only, so use Intel/AMD families:

| Family | Notes |
|--------|-------|
| `c5n`, `c6in`, `c7i` / `c7a` | High network bandwidth |
| `*.metal` | No hypervisor at all; best latency |

Attach a second ENA interface for DPDK and keep the primary one for SSH: binding the only interface to DPDK cuts you off.

### ENA driver

DPDK ships an ENA PMD (`librte_net_ena`). Find the ENA interface's PCI address, bind it to `vfio-pci`, and set `allow=<PCI addr>` in config.ini. Most instance types have no IOMMU, so VFIO needs the no-IOMMU mode from step 2. See DPDK's ENA guide for write-combining (LLQ) setup, which affects ENA performance.

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

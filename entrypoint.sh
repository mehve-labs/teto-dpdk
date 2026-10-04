#!/bin/bash
# Set up the network F-Stack uses in Docker, once, then run the command.
#
#   kernel (10.0.0.2) teto0 <══ veth pair ══> teto0-dpdk  ◀── DPDK af_packet ── F-Stack (10.0.0.1)
#
# DPDK's af_packet driver attaches to teto0-dpdk (no IP: the kernel stack
# stays out of it); the kernel talks to F-Stack through teto0. Both ends have
# their own MAC, the pair exists before F-Stack starts and survives it
# exiting, so nothing has to be reconfigured per run.
set -euo pipefail

KERNEL_IF=teto0
DPDK_IF=teto0-dpdk

if ! ip link show "$KERNEL_IF" > /dev/null 2>&1; then
    ip link add "$KERNEL_IF" type veth peer name "$DPDK_IF"
    ip addr add 10.0.0.2/24 dev "$KERNEL_IF"
    # Fill in checksums in the kernel: veth otherwise hands over packets with
    # checksums left for "hardware" to complete, which F-Stack drops as corrupt.
    # (TSO goes off with it; spelled out so frames always fit the MTU.)
    ethtool -K "$KERNEL_IF" tx off tso off > /dev/null
    # No IPv6 link-local addresses, so neither end sends router solicitations,
    # MLD or DAD packets at F-Stack (works without privileges, unlike sysctls).
    ip link set "$KERNEL_IF" addrgenmode none
    ip link set "$DPDK_IF" addrgenmode none
    ip link set "$DPDK_IF" up
    ip link set "$KERNEL_IF" up
fi

exec "$@"

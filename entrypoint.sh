#!/bin/bash
set -e

# Clean up stale devices from previous runs (--network=host shares namespace)
ip link del dtap0 2>/dev/null || true

configure_dtap0() {
    DPDK_MAC=$(cat /sys/class/net/dtap0/address | tr -d '[:space:]')

    # Derive a kernel MAC that is guaranteed to differ from the DPDK MAC.
    # Increment the last octet (wrapping at 0xff → 0x00).
    LAST_OCTET=$(echo "$DPDK_MAC" | awk -F: '{print $6}')
    NEW_OCTET=$(printf '%02x' $(( (0x$LAST_OCTET + 1) % 256 )))
    KERNEL_MAC=$(echo "$DPDK_MAC" | sed "s/[^:]*$/$NEW_OCTET/")

    ip link set dtap0 down
    ip link set dtap0 address "$KERNEL_MAC"
    ip addr add 10.0.0.2/24 dev dtap0 2>/dev/null || true
    ip link set dtap0 up

    ethtool -K dtap0 tx-checksumming off 2>&1 || echo "[WARN] ethtool tx-checksumming off failed"
    ethtool -K dtap0 rx-checksumming off 2>&1 || echo "[WARN] ethtool rx-checksumming off failed"

    arp -d 10.0.0.1 2>/dev/null || true
    arp -i dtap0 -s 10.0.0.1 "$DPDK_MAC"

    echo ""
    echo "=== F-Stack Network Config ==="
    echo "TAP device     : dtap0"
    echo "DPDK MAC       : ${DPDK_MAC}"
    echo "Kernel MAC     : ${KERNEL_MAC}"
    echo "Kernel IP      : 10.0.0.2"
    echo "F-Stack IP     : 10.0.0.1"
    echo "==============================="
    echo ""
    echo "--- Diagnostics ---"
    ip addr show dtap0 2>&1 | head -4
    echo "ARP table:"
    arp -n 2>&1 | grep dtap0 || echo "  (no entries)"
    echo "Offload status:"
    ethtool -k dtap0 2>&1 | grep -E 'tx-checksumming|rx-checksumming' || true
    echo "-------------------"
    echo ""
    echo "To test, open a second terminal inside the container:"
    echo "  docker exec -it \$(hostname) bash"
    echo ""
    echo "  UDP:  echo \"Hello F-Stack\" | nc -u -w1  10.0.0.1 8080"
    echo "  TCP:  echo \"Hello F-Stack\" | nc    -w3  10.0.0.1 8080"
    echo ""
    echo "To debug, capture dtap0 traffic with:"
    echo "  tcpdump -i dtap0 -nn port 8080"
}

# Background loop: (re)configure dtap0 each time it appears.
# The kernel side of the TAP MUST have a different MAC than the DPDK side.
# FreeBSD's ether_input drops frames whose source MAC matches the interface
# MAC (anti-loop). Since both sides of the TAP share the same MAC by default,
# F-Stack would silently drop all ARP replies from the kernel, making the
# return path (echo replies) impossible.
(
    CONFIGURED_FOR=""
    while true; do
        if ip link show dtap0 > /dev/null 2>&1; then
            CURRENT_MAC=$(cat /sys/class/net/dtap0/address | tr -d '[:space:]')
            if [ "$CURRENT_MAC" != "$CONFIGURED_FOR" ]; then
                sleep 3
                configure_dtap0
                CONFIGURED_FOR=$(cat /sys/class/net/dtap0/address | tr -d '[:space:]')
            fi
        else
            CONFIGURED_FOR=""
        fi
        sleep 1
    done
) &

exec "$@"

#!/bin/bash
# Echo benchmark: teto-tokio (F-Stack) vs tokio::net (kernel), same client.
#
# Docker smoke run (inside the container started per the README):
#   scripts/bench.sh
#
# What the Docker numbers mean: very little. The F-Stack path goes through the
# DPDK TAP driver (a kernel tap device read/written with syscalls) and, on
# Apple Silicon, x86 emulation; the kernel baseline goes through loopback.
# Use it to check the harness runs and to catch gross regressions only.
#
# Real comparison (what the project claims): two hosts on the same network,
# the server host with a DPDK-bound NIC for F-Stack and a kernel-managed NIC
# (same model/link) for the baseline. On the server host run either
#   cargo run --release -p teto-tokio --example tcp_echo_async      # F-Stack, port 8080
#   cargo run --release -p teto-tokio --example kernel_echo         # kernel, port 8081
# and on the client host
#   cargo run --release -p teto-tokio --example bench_client -- <server>:<port> rtt 100000 64
#   cargo run --release -p teto-tokio --example bench_client -- <server>:<port> throughput 30 16 16384
# Compare p50/p99 RTT and throughput. Repeat a few times; pin CPUs.
# For latency runs set pkt_tx_delay=0 in config.ini ([dpdk]); the default
# 100 µs TX batching delay otherwise dominates round-trip times.
set -euo pipefail
cd "$(dirname "$0")/.."

RTT_COUNT=${RTT_COUNT:-5000}
SECS=${SECS:-5}

cargo build --release -p teto-tokio --example tcp_echo_async --example kernel_echo --example bench_client
BIN=${CARGO_TARGET_DIR:-target}/release/examples

wait_for() {
    for _ in $(seq 1 90); do
        if python3 -c "import socket;socket.create_connection(('$1',$2),timeout=1).close()" 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    echo "server $1:$2 not reachable" >&2
    return 1
}

run_client() {
    "$BIN/bench_client" "$1" rtt "$RTT_COUNT" 64
    "$BIN/bench_client" "$1" throughput "$SECS" 8 16384
}

echo "== teto-tokio (F-Stack via TAP) =="
"$BIN/tcp_echo_async" > /tmp/teto-bench-server.log 2>&1 &
TETO=$!
trap 'kill -9 $TETO 2>/dev/null || true' EXIT
wait_for 10.0.0.1 8080
run_client 10.0.0.1:8080
kill -9 $TETO; wait $TETO 2>/dev/null || true

echo "== tokio::net (kernel loopback) =="
"$BIN/kernel_echo" 127.0.0.1:8081 > /tmp/kernel-bench-server.log 2>&1 &
KERNEL=$!
trap 'kill -9 $KERNEL 2>/dev/null || true' EXIT
wait_for 127.0.0.1 8081
run_client 127.0.0.1:8081

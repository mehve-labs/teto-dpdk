#!/bin/bash
# Run the whole suite (lint, MSRV build, integration tests, downstream link
# check) in the project's Docker image. Everything happens inside a
# container; the host only needs Docker.
#
#   scripts/test.sh                     # everything
#   scripts/test.sh -E 'test(half_close)'   # extra args go to `cargo nextest run`
#
# Environment:
#   TETO_IMAGE       image tag to build/use (default teto-dpdk:dev)
#   TETO_SKIP_BUILD  set to use an already-built TETO_IMAGE (CI does this)
#   TETO_SOAK_SECS   churn duration for teto-tokio/tests/scale.rs (default 15)
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=${TETO_IMAGE:-teto-dpdk:dev}
if [ -z "${TETO_SKIP_BUILD:-}" ]; then
    # F-Stack's arm64 support is incomplete: build for x86_64 (emulated on
    # Apple Silicon). Cached after the first build.
    docker build --platform linux/amd64 -t "$IMAGE" .
fi

# NET_ADMIN: the entrypoint creates the veth pair, and the fault tests use tc.
# The sysctls tune the kernel-side load generator in tests/scale.rs.
# The named volume caches crates.io downloads between runs.
exec docker run --rm --platform linux/amd64 \
    --cap-add NET_ADMIN \
    --sysctl net.ipv4.ip_local_port_range="1024 65535" \
    --sysctl net.ipv4.tcp_tw_reuse=1 \
    -e TETO_SOAK_SECS \
    -e CARGO_TARGET_DIR=/app/target/docker \
    -v teto-cargo-registry:/root/.cargo/registry \
    -v "$PWD":/app \
    "$IMAGE" scripts/ci-test.sh "$@"

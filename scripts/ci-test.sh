#!/bin/bash
# Lint, build and run the integration suite. Runs inside the project's Docker
# image (privileged, started through entrypoint.sh, which creates the veth
# pair F-Stack uses), from the repository root:
#
#   docker build -t teto-dpdk .
#   docker run --rm --privileged -v "$PWD":/app teto-dpdk scripts/ci-test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

MSRV=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)

echo "::group::clippy"
cargo clippy --workspace --all-targets -- -D warnings
echo "::endgroup::"

echo "::group::build (MSRV $MSRV)"
CARGO_TARGET_DIR=target/msrv cargo "+$MSRV" build --workspace --all-targets
echo "::endgroup::"

# Each integration test binary starts its own F-Stack instance on the shared
# veth pair; cargo runs test binaries one after another.
echo "::group::test"
cargo test --workspace
echo "::endgroup::"

# A separate crate depending on teto-tokio, as a user's application would.
echo "::group::downstream crate"
cargo run --manifest-path ci/downstream/Cargo.toml
echo "::endgroup::"

#!/bin/bash
# Lint, build and run the integration suite. Runs inside the project's Docker
# image (started through entrypoint.sh, which creates the veth pair F-Stack
# uses). From the host, use scripts/test.sh, which runs this in a container.
# Extra arguments go to `cargo nextest run`.
set -euo pipefail
cd "$(dirname "$0")/.."

MSRV=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)

echo "::group::clippy"
cargo clippy --workspace --all-targets -- -D warnings
echo "::endgroup::"

echo "::group::build (MSRV $MSRV)"
CARGO_TARGET_DIR=target/msrv cargo "+$MSRV" build --workspace --all-targets
echo "::endgroup::"

# Integration tests start F-Stack, which can run once per process: nextest
# runs each test in its own process (one at a time; see .config/nextest.toml).
echo "::group::test"
cargo nextest run --workspace "$@"
cargo test --workspace --doc
echo "::endgroup::"

# A separate crate depending on teto-tokio, as a user's application would.
echo "::group::downstream crate"
cargo run --manifest-path ci/downstream/Cargo.toml
echo "::endgroup::"

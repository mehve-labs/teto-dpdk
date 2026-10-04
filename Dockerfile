FROM ubuntu:22.04

# Avoid prompts from apt
ENV DEBIAN_FRONTEND=noninteractive

# Install core dependencies for DPDK, F-Stack, Rust, and debugging
RUN apt-get update && apt-get install -y \
    build-essential \
    clang \
    cmake \
    curl \
    git \
    libnuma-dev \
    libssl-dev \
    meson \
    ninja-build \
    pkg-config \
    python3 \
    python3-pip \
    python3-pyelftools \
    pciutils \
    iproute2 \
    iputils-ping \
    net-tools \
    netcat \
    sudo \
    tcpdump \
    ethtool \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt

# Clone F-Stack and its bundled DPDK (F-Stack carries patches to DPDK, so use
# the bundled copy). Pinned to the v1.25 release (DPDK 23.11.5), verified by
# commit so a moved tag can't change the build. Don't track master: as of
# 956c4158 (Jul 2026) its FreeBSD 15 port runs no kernel callouts at all, so
# delayed ACKs, keepalives and TCP retransmission never fire (a lost packet
# hangs the connection). Older releases are no good either: the DPDK bundled
# with 1.21.6 has a net_tap RX checksum bug that drops every TCP packet.
# When bumping, re-run the integration tests (TCP over TAP, timers).
ARG FSTACK_REF=v1.25
ARG FSTACK_COMMIT=761639943bdda33103aa98241ca6a3079f1c1b7e
RUN git clone --recurse-submodules --depth 1 --branch ${FSTACK_REF} https://github.com/F-Stack/f-stack.git && \
    test "$(git -C f-stack rev-parse HEAD)" = "${FSTACK_COMMIT}"

# Build DPDK
WORKDIR /opt/f-stack/dpdk
# Mock kernel version check for DPDK inside WSL2/Docker wrapper
RUN mkdir -p /lib/modules/$(uname -r)/build && \
    printf "kernelversion:\n\t@echo 6.6.0\n" > /lib/modules/$(uname -r)/build/Makefile

# Build static library with -fPIC to allow linking into C++ / Rust cxx code.
# -Dplatform=generic: build a portable x86-64 baseline instead of the default
# -march=native. Under QEMU emulation (Apple Silicon host) -march=native can
# select CPU features QEMU advertises but doesn't fully emulate, causing SIGILL
# at runtime; the generic baseline avoids that.
RUN meson setup build -Dplatform=generic -Ddefault_library=static -Dc_args=-fPIC -Denable_kmods=false && \
    ninja -C build && \
    ninja -C build install && \
    ldconfig

# Build F-Stack Library
WORKDIR /opt/f-stack/lib
ENV FF_DPDK=/usr/local
ENV FF_PATH=/opt/f-stack
# Build the F-Stack core library.
# -Wno-error=array-bounds: GCC 12 false positive in the bundled FreeBSD
# sources (kern/sys_generic.c); same class as the -Wno-error=stringop-*
# exemptions F-Stack's Makefile already carries.
RUN make -j$(nproc) CC="cc -Wno-error=array-bounds"

# Install Rust (after F-Stack so a toolchain bump doesn't rebuild DPDK).
# RUST_VERSION is what the project is built and tested with; RUST_MSRV is the
# minimum supported version declared in Cargo.toml, checked in CI.
ARG RUST_VERSION=1.99.0
ARG RUST_MSRV=1.97.0
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
        sh -s -- -y --profile minimal --default-toolchain ${RUST_VERSION} -c clippy && \
    /root/.cargo/bin/rustup toolchain install ${RUST_MSRV} --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /app

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
CMD ["bash"]

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

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /opt

# Clone F-Stack and its bundled DPDK submodule
# F-Stack has custom patches to DPDK so it's required to use their submodule.
# Master (not the 1.21.6 LTS tag): the DPDK bundled with 1.21.6 has a net_tap
# RX checksum bug that flags every valid TCP packet PKT_RX_L4_CKSUM_BAD, which
# F-Stack silently drops — UDP echoes work but TCP SYNs vanish. Master bundles
# a DPDK with the upstream fix. If pinning to a release, verify TCP over TAP.
RUN git clone --recurse-submodules --depth 1 https://github.com/F-Stack/f-stack.git

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

WORKDIR /app

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
CMD ["bash"]

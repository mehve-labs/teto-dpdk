/// Builder for F-Stack / DPDK initialisation arguments.
///
/// Separates the config-file arguments (understood by F-Stack's own parser)
/// from extra EAL arguments (passed directly to DPDK's `rte_eal_init`).
///
/// # Examples
///
/// Docker / TAP development:
/// ```rust
/// let cfg = FStackConfig::for_docker();
/// ```
///
/// Bare metal with a real NIC bound via VFIO:
/// ```rust
/// let cfg = FStackConfig::for_bare_metal();
/// ```
///
/// Custom build:
/// ```rust
/// let cfg = FStackConfig::new("config.ini")
///     .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
///     .with_eal_arg("--no-pci");
/// ```
pub struct FStackConfig {
    config_file: String,
    eal_args:    Vec<String>,
}

impl FStackConfig {
    /// Create a config pointing at `config_file` with no extra EAL arguments.
    pub fn new(config_file: impl Into<String>) -> Self {
        Self {
            config_file: config_file.into(),
            eal_args:    Vec::new(),
        }
    }

    /// Append a single extra EAL argument (e.g. `"--no-pci"`).
    pub fn with_eal_arg(mut self, arg: impl Into<String>) -> Self {
        self.eal_args.push(arg.into());
        self
    }

    // ------------------------------------------------------------------
    // Pre-built profiles
    // ------------------------------------------------------------------

    /// Docker / TAP device profile.
    ///
    /// Injects the three EAL arguments that are required when running DPDK
    /// inside a container with a TAP virtual interface instead of a real NIC:
    ///
    /// - `--vdev=net_tap0,iface=dtap0,mac=fixed`  — create a TAP-backed DPDK
    ///   port tied to the kernel interface `dtap0`; `mac=fixed` makes the MAC
    ///   deterministic so that it is stable across restarts. The kernel side of
    ///   the TAP is automatically assigned a *different* MAC by `entrypoint.sh`
    ///   (derived from the DPDK MAC by incrementing the last octet). The two
    ///   MACs must differ: FreeBSD's `ether_input` drops frames whose source
    ///   MAC matches the interface MAC (anti-loop protection).
    /// - `--no-pci`  — skip PCI bus scan; without this DPDK could claim a PCI
    ///   device and push the TAP device to port 1, breaking `port_list=0`.
    /// - `--iova-mode=va`  — force Virtual Address IOVA mode, required in
    ///   containers / WSL2 where physical address access is unavailable.
    pub fn for_docker() -> Self {
        Self::new("config.ini")
            .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
            .with_eal_arg("--no-pci")
            .with_eal_arg("--iova-mode=va")
    }

    /// Bare-metal / AWS profile.
    ///
    /// No extra EAL arguments are needed: DPDK discovers the NIC via the
    /// `allow=` key in `config.ini`, PCI scanning is required, and IOVA mode
    /// is auto-detected based on whether IOMMU is present.
    pub fn for_bare_metal() -> Self {
        Self::new("config.ini")
    }

    // ------------------------------------------------------------------
    // Accessors used by the cxx bridge
    // ------------------------------------------------------------------

    /// The arguments passed to `ff_load_config` (F-Stack's config parser).
    pub fn config_args(&self) -> Vec<String> {
        vec![
            "teto".to_string(),
            "--conf".to_string(),
            self.config_file.clone(),
        ]
    }

    /// Extra EAL arguments injected into `dpdk_argv` after `ff_load_config`.
    pub fn eal_args(&self) -> Vec<String> {
        self.eal_args.clone()
    }
}

// ---------------------------------------------------------------------------
// Per-connection TCP socket options
// ---------------------------------------------------------------------------

/// Per-connection TCP tuning applied to every accepted socket.
///
/// All fields are optional. When `None`, the FreeBSD default is used.
/// Construct with `Default::default()` for all defaults, or use the builder
/// methods to override specific values.
///
/// ```rust
/// let opts = TcpSocketOptions::default()
///     .nodelay(true)
///     .keepalive(true)
///     .keepalive_idle_secs(10)
///     .keepalive_interval_secs(5)
///     .keepalive_count(3);
/// ```
#[derive(Clone, Debug, Default)]
pub struct TcpSocketOptions {
    /// Disable Nagle's algorithm — send data immediately without coalescing
    /// small writes. Essential for latency-sensitive protocols.
    pub nodelay: Option<bool>,

    /// Enable TCP keepalive probes on idle connections. When the remote peer
    /// disappears without sending a FIN (crash, network partition), keepalive
    /// detects it and tears down the connection.
    pub keepalive: Option<bool>,

    /// Seconds of idle time before the first keepalive probe is sent.
    /// FreeBSD default: 7200 (2 hours).
    pub keepalive_idle_secs: Option<u32>,

    /// Seconds between consecutive keepalive probes after the first.
    /// FreeBSD default: 75.
    pub keepalive_interval_secs: Option<u32>,

    /// Number of unacknowledged keepalive probes before the connection is
    /// dropped. FreeBSD default: 8.
    pub keepalive_count: Option<u32>,

    /// Receive buffer size in bytes. Larger buffers allow higher throughput on
    /// high-latency links. FreeBSD default: ~64 KB.
    pub recv_buf: Option<u32>,

    /// Send buffer size in bytes. FreeBSD default: ~64 KB.
    pub send_buf: Option<u32>,

    /// Linger timeout in seconds. When set, `ff_close` blocks (up to this
    /// many seconds) until buffered data is sent, then sends RST if it
    /// couldn't drain in time. When `None`, close returns immediately and
    /// the stack drains in the background.
    pub linger_secs: Option<u32>,

    /// Disable delayed ACKs — acknowledge segments immediately instead of
    /// waiting up to 40ms to piggyback the ACK on outgoing data.
    /// Complementary to `nodelay` for lowest latency.
    pub quickack: Option<bool>,

    /// Allow multiple sockets to bind the same address:port combination.
    /// Useful for multi-process F-Stack setups.
    pub reuse_port: Option<bool>,
}

impl TcpSocketOptions {
    pub fn nodelay(mut self, v: bool) -> Self { self.nodelay = Some(v); self }
    pub fn keepalive(mut self, v: bool) -> Self { self.keepalive = Some(v); self }
    pub fn keepalive_idle_secs(mut self, v: u32) -> Self { self.keepalive_idle_secs = Some(v); self }
    pub fn keepalive_interval_secs(mut self, v: u32) -> Self { self.keepalive_interval_secs = Some(v); self }
    pub fn keepalive_count(mut self, v: u32) -> Self { self.keepalive_count = Some(v); self }
    pub fn recv_buf(mut self, v: u32) -> Self { self.recv_buf = Some(v); self }
    pub fn send_buf(mut self, v: u32) -> Self { self.send_buf = Some(v); self }
    pub fn linger_secs(mut self, v: u32) -> Self { self.linger_secs = Some(v); self }
    pub fn quickack(mut self, v: bool) -> Self { self.quickack = Some(v); self }
    pub fn reuse_port(mut self, v: bool) -> Self { self.reuse_port = Some(v); self }

    /// Convert to the flat cxx-bridge struct. `-1` encodes "not set / use FreeBSD default".
    pub fn to_ffi(&self) -> crate::fstack::ffi::TcpSocketOptionsFfi {
        crate::fstack::ffi::TcpSocketOptionsFfi {
            nodelay:                 self.nodelay.map_or(-1, |v| v as i32),
            keepalive:               self.keepalive.map_or(-1, |v| v as i32),
            keepalive_idle_secs:     self.keepalive_idle_secs.map_or(-1, |v| v as i32),
            keepalive_interval_secs: self.keepalive_interval_secs.map_or(-1, |v| v as i32),
            keepalive_count:         self.keepalive_count.map_or(-1, |v| v as i32),
            recv_buf:                self.recv_buf.map_or(-1, |v| v as i32),
            send_buf:                self.send_buf.map_or(-1, |v| v as i32),
            linger_secs:             self.linger_secs.map_or(-1, |v| v as i32),
            quickack:                self.quickack.map_or(-1, |v| v as i32),
            reuse_port:              self.reuse_port.map_or(-1, |v| v as i32),
        }
    }
}

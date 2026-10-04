use std::io;

/// Config file used by the [`FStackConfig::for_docker`] and
/// [`FStackConfig::for_bare_metal`] profiles: `$TETO_CONFIG` if set,
/// otherwise `config.ini` in the working directory.
pub const CONFIG_ENV: &str = "TETO_CONFIG";

/// Selects the profile used by [`FStackConfig::from_env`].
pub const PROFILE_ENV: &str = "TETO_PROFILE";

fn default_config_file() -> String {
    std::env::var(CONFIG_ENV).unwrap_or_else(|_| "config.ini".into())
}

/// Builder for F-Stack / DPDK initialisation arguments.
///
/// Separates the config-file arguments (understood by F-Stack's own parser)
/// from extra EAL arguments (passed directly to DPDK's `rte_eal_init`).
///
/// # Examples
///
/// Docker / TAP development:
/// ```rust
/// # use teto_dpdk::config::FStackConfig;
/// let cfg = FStackConfig::for_docker();
/// ```
///
/// Bare metal with a real NIC bound via VFIO:
/// ```rust
/// # use teto_dpdk::config::FStackConfig;
/// let cfg = FStackConfig::for_bare_metal();
/// ```
///
/// Custom build:
/// ```rust
/// # use teto_dpdk::config::FStackConfig;
/// let cfg = FStackConfig::new("config.ini")
///     .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
///     .with_eal_arg("--no-pci");
/// ```
pub struct FStackConfig {
    config_file: String,
    eal_args:    Vec<String>,
    capture_init_output: bool,
}

impl FStackConfig {
    /// Create a config pointing at `config_file` with no extra EAL arguments.
    pub fn new(config_file: impl Into<String>) -> Self {
        Self {
            config_file: config_file.into(),
            eal_args:    Vec::new(),
            capture_init_output: false,
        }
    }

    /// Use a different `config.ini`. Relative paths are resolved against the
    /// working directory when F-Stack is initialised.
    pub fn with_config_file(mut self, config_file: impl Into<String>) -> Self {
        self.config_file = config_file.into();
        self
    }

    /// The `config.ini` path F-Stack will be initialised from.
    pub fn config_file(&self) -> &str {
        &self.config_file
    }

    /// Append a single extra EAL argument (e.g. `"--no-pci"`).
    pub fn with_eal_arg(mut self, arg: impl Into<String>) -> Self {
        self.eal_args.push(arg.into());
        self
    }

    /// Capture what F-Stack, DPDK and the FreeBSD stack print during
    /// initialisation (EAL messages, config echo, interface setup — a few
    /// dozen lines) instead of letting it reach stdout/stderr. On failure it
    /// is appended to the error; on success it is available from
    /// [`FStack::init_output`](crate::FStack::init_output).
    ///
    /// This redirects the whole process's stdout and stderr while F-Stack
    /// initialises (typically under a second), so anything other threads
    /// print in that window is captured too. Off by default.
    pub fn capture_init_output(mut self, capture: bool) -> Self {
        self.capture_init_output = capture;
        self
    }

    pub(crate) fn captures_init_output(&self) -> bool {
        self.capture_init_output
    }

    // ------------------------------------------------------------------
    // Pre-built profiles
    // ------------------------------------------------------------------

    /// Docker / TAP device profile. Reads `$TETO_CONFIG`, or `config.ini` in
    /// the working directory.
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
        Self::new(default_config_file())
            .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
            .with_eal_arg("--no-pci")
            .with_eal_arg("--iova-mode=va")
    }

    /// Profile chosen by `$TETO_PROFILE`: `docker` (default) or `bare-metal`,
    /// with the config file from `$TETO_CONFIG` (default `config.ini`). Used
    /// by the examples so they run unchanged in either environment.
    pub fn from_env() -> io::Result<Self> {
        match std::env::var(PROFILE_ENV).as_deref() {
            Err(_) | Ok("docker") => Ok(Self::for_docker()),
            Ok("bare-metal") => Ok(Self::for_bare_metal()),
            Ok(other) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{PROFILE_ENV}={other:?}: expected \"docker\" or \"bare-metal\""),
            )),
        }
    }

    /// Bare-metal / AWS profile. Reads `$TETO_CONFIG`, or `config.ini` in
    /// the working directory.
    ///
    /// No extra EAL arguments are needed: DPDK discovers the NIC via the
    /// `allow=` key in `config.ini`, PCI scanning is required, and IOVA mode
    /// is auto-detected based on whether IOMMU is present.
    pub fn for_bare_metal() -> Self {
        Self::new(default_config_file())
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
/// # use teto_dpdk::config::TcpSocketOptions;
/// let opts = TcpSocketOptions::default()
///     .nodelay(true)
///     .keepalive(true)
///     .keepalive_idle_secs(10)
///     .keepalive_interval_secs(5)
///     .keepalive_count(3);
/// ```
///
/// There is no `TCP_QUICKACK`: it's Linux-only and FreeBSD (so F-Stack) has
/// no per-socket equivalent. To ACK every segment immediately, set
/// `net.inet.tcp.delayed_ack=0` under `[freebsd.sysctl]` in `config.ini`;
/// it applies to all connections.
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

    /// `SO_LINGER` timeout in seconds. Sockets here are non-blocking, so
    /// closing never blocks; `Some(0)` makes close send RST and discard
    /// unsent data (which defeats teto-tokio's flush-on-drop). When `None`,
    /// the stack sends buffered data in the background after close.
    pub linger_secs: Option<u32>,

    /// Allow multiple sockets to bind the same address:port combination.
    /// Useful for multi-process F-Stack setups.
    pub reuse_port: Option<bool>,
}

impl TcpSocketOptions {
    /// Set [`nodelay`](Self::nodelay) (`TCP_NODELAY`).
    pub fn nodelay(mut self, v: bool) -> Self { self.nodelay = Some(v); self }
    /// Set [`keepalive`](Self::keepalive) (`SO_KEEPALIVE`).
    pub fn keepalive(mut self, v: bool) -> Self { self.keepalive = Some(v); self }
    /// Set [`keepalive_idle_secs`](Self::keepalive_idle_secs) (`TCP_KEEPIDLE`).
    pub fn keepalive_idle_secs(mut self, v: u32) -> Self { self.keepalive_idle_secs = Some(v); self }
    /// Set [`keepalive_interval_secs`](Self::keepalive_interval_secs) (`TCP_KEEPINTVL`).
    pub fn keepalive_interval_secs(mut self, v: u32) -> Self { self.keepalive_interval_secs = Some(v); self }
    /// Set [`keepalive_count`](Self::keepalive_count) (`TCP_KEEPCNT`).
    pub fn keepalive_count(mut self, v: u32) -> Self { self.keepalive_count = Some(v); self }
    /// Set [`recv_buf`](Self::recv_buf) (`SO_RCVBUF`).
    pub fn recv_buf(mut self, v: u32) -> Self { self.recv_buf = Some(v); self }
    /// Set [`send_buf`](Self::send_buf) (`SO_SNDBUF`).
    pub fn send_buf(mut self, v: u32) -> Self { self.send_buf = Some(v); self }
    /// Set [`linger_secs`](Self::linger_secs) (`SO_LINGER`).
    pub fn linger_secs(mut self, v: u32) -> Self { self.linger_secs = Some(v); self }
    /// Set [`reuse_port`](Self::reuse_port) (`SO_REUSEPORT`, listeners only).
    pub fn reuse_port(mut self, v: bool) -> Self { self.reuse_port = Some(v); self }
}

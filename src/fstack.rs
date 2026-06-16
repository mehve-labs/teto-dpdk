#[cxx::bridge(namespace = "teto")]
pub mod ffi {
    // --- UDP ---

    struct UdpMessage {
        payload:  Vec<u8>,
        src_ip:   String,
        src_port: u16,
    }

    // --- TCP ---

    struct TcpMessage {
        payload:  Vec<u8>,
        src_ip:   String,
        src_port: u16,
    }

    /// Flat representation of TcpSocketOptions for the cxx bridge.
    /// `-1` means "not set / use default". `0` = false, `1` = true for booleans.
    struct TcpSocketOptionsFfi {
        nodelay:                 i32,
        keepalive:               i32,
        keepalive_idle_secs:     i32,
        keepalive_interval_secs: i32,
        keepalive_count:         i32,
        recv_buf:                i32,
        send_buf:                i32,
        linger_secs:             i32,
        quickack:                i32,
        reuse_port:              i32,
    }

    unsafe extern "C++" {
        include!("fstack_wrapper.h");

        // --- Shared init ---

        /// `config_args` is passed to `ff_load_config` (e.g. `["teto", "--conf", "config.ini"]`).
        /// `eal_args` is injected into `dpdk_argv` before `ff_dpdk_init` (e.g. `["--no-pci"]`).
        fn init_fstack(config_args: &Vec<String>, eal_args: &Vec<String>);

        // --- UDP ---

        type FStackUdpSocket;

        fn create_udp_socket(
            ip:       &String,
            port:     u16,
            callback: fn(i32, &UdpMessage),
        ) -> UniquePtr<FStackUdpSocket>;

        fn send_to(self: &FStackUdpSocket, payload: &[u8], dest_ip: &String, dest_port: u16);

        fn run_fstack(socket: &FStackUdpSocket);

        // --- TCP ---

        type FStackTcpListener;

        fn create_tcp_listener(
            ip:            &String,
            port:          u16,
            opts:          &TcpSocketOptionsFfi,
            on_connect:    fn(i32, &String, u16),
            on_data:       fn(i32, &TcpMessage),
            on_disconnect: fn(i32),
        ) -> UniquePtr<FStackTcpListener>;

        fn send_to(self: &FStackTcpListener, fd: i32, payload: &[u8]);
        fn close_connection(self: &FStackTcpListener, fd: i32);

        fn run_fstack_tcp(listener: &FStackTcpListener);

        /// Register a callback invoked on every F-Stack TCP poll iteration,
        /// before accept/read. Used by teto-tokio to drain the write command queue.
        fn set_tcp_tick_callback(cb: fn());

        /// Register a callback invoked on every F-Stack UDP poll iteration,
        /// before read. Used by teto-tokio to drain the send command queue.
        fn set_udp_tick_callback(cb: fn());
    }
}

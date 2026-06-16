#include "fstack_wrapper.h"
#include "teto-dpdk/src/fstack.rs.h"

extern "C" {
#include <ff_api.h>
#include <ff_config.h>

extern int dpdk_argc;
extern char *dpdk_argv[];

extern int ff_freebsd_init(void);
extern int ff_dpdk_init(int, char **);
extern int ff_dpdk_if_up(void);
}

#include <rte_ethdev.h>
#include <rte_mbuf.h>
#include <rte_ether.h>
#include <rte_ip.h>

#include <stdexcept>
#include <vector>
#include <string>
#include <iostream>
#include <arpa/inet.h>
#include <cstring>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/ioctl.h>

namespace teto {

static struct sockaddr_in make_addr(const std::string& ip, uint16_t port) {
    struct sockaddr_in addr;
    std::memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    if (ip == "0.0.0.0" || ip.empty()) {
        addr.sin_addr.s_addr = INADDR_ANY;
    } else {
        inet_pton(AF_INET, ip.c_str(), &addr.sin_addr.s_addr);
    }
    return addr;
}

void init_fstack(const rust::Vec<rust::String>& config_args,
                 const rust::Vec<rust::String>& eal_args) {
    // Pass config_args to F-Stack's own parser (reads config.ini, builds
    // dpdk_argv from the [dpdk] section).
    std::vector<std::string> cpp_config_args;
    for (const auto& arg : config_args) {
        cpp_config_args.push_back(std::string(arg));
    }
    std::vector<char*> config_argv;
    for (auto& arg : cpp_config_args) {
        config_argv.push_back(arg.data());
    }
    config_argv.push_back(nullptr);
    int config_argc = static_cast<int>(config_argv.size()) - 1;

    if (ff_load_config(config_argc, config_argv.data()) < 0) {
        throw std::runtime_error("F-Stack config load failed.");
    }

    // Inject extra EAL args that F-Stack's config parser doesn't handle.
    // These come from the Rust FStackConfig (e.g. --vdev, --no-pci for Docker;
    // nothing for bare metal).
    for (const auto& arg : eal_args) {
        dpdk_argv[dpdk_argc++] = strdup(std::string(arg).c_str());
    }
    dpdk_argv[dpdk_argc] = nullptr;

    for (int i=0; i<dpdk_argc; i++) {
        std::cerr << "[EAL] arg[" << i << "]: " << dpdk_argv[i] << std::endl;
    }

    if (ff_dpdk_init(dpdk_argc, dpdk_argv) < 0) {
        throw std::runtime_error("F-Stack DPDK init failed.");
    }

    uint16_t nb_ports = rte_eth_dev_count_avail();
    std::cerr << "[DPDK] Available ports: " << nb_ports << std::endl;
    for (uint16_t pid = 0; pid < nb_ports; pid++) {
        struct rte_ether_addr mac;
        rte_eth_macaddr_get(pid, &mac);
        struct rte_eth_link link;
        rte_eth_link_get_nowait(pid, &link);
        char mac_str[18];
        snprintf(mac_str, sizeof(mac_str), "%02x:%02x:%02x:%02x:%02x:%02x",
                 mac.addr_bytes[0], mac.addr_bytes[1], mac.addr_bytes[2],
                 mac.addr_bytes[3], mac.addr_bytes[4], mac.addr_bytes[5]);
        std::cerr << "[DPDK] Port " << pid
                  << " MAC=" << mac_str
                  << " link=" << (link.link_status ? "UP" : "DOWN")
                  << " speed=" << link.link_speed << "Mbps"
                  << std::endl;
    }

    if (ff_freebsd_init() < 0) {
        throw std::runtime_error("F-Stack FreeBSD init failed.");
    }

    if (ff_dpdk_if_up() < 0) {
        throw std::runtime_error("F-Stack DPDK interface up failed.");
    }

    int test_fd = ff_socket(AF_INET, SOCK_DGRAM, 0);
    if (test_fd >= 0) {
        struct sockaddr_in test_addr;
        memset(&test_addr, 0, sizeof(test_addr));
        test_addr.sin_family = AF_INET;
        test_addr.sin_port = htons(9999);
        inet_pton(AF_INET, "10.0.0.2", &test_addr.sin_addr);
        int cr = ff_connect(test_fd, (struct linux_sockaddr *)&test_addr, sizeof(test_addr));

        struct sockaddr_in local_addr;
        socklen_t local_len = sizeof(local_addr);
        ff_getsockname(test_fd, (struct linux_sockaddr *)&local_addr, &local_len);
        char local_ip[INET_ADDRSTRLEN];
        inet_ntop(AF_INET, &local_addr.sin_addr, local_ip, sizeof(local_ip));
        std::cerr << "[DEBUG] Test connect ret=" << cr << ", local IP=" << local_ip
                  << ":" << ntohs(local_addr.sin_port) << std::endl;
        ff_close(test_fd);
    }
}

FStackUdpSocket::FStackUdpSocket(const rust::String& ip, uint16_t port, rust::Fn<void(int32_t, const UdpMessage&)> callback)
    : callback_(callback) {

    fd_ = ff_socket(AF_INET, SOCK_DGRAM, 0);
    if (fd_ < 0) {
        throw std::runtime_error("Failed to create UDP socket in F-Stack.");
    }
    
    int on = 1;
    if (ff_ioctl(fd_, FIONBIO, &on) < 0) {
        std::cerr << "Failed to set UDP socket non-blocking" << std::endl;
    }

    struct sockaddr_in addr = make_addr(std::string(ip), port);
    int bind_ret = ff_bind(fd_, (struct linux_sockaddr *)&addr, sizeof(addr));
    std::cerr << "[DEBUG] ff_socket fd=" << fd_ << ", ff_bind ret=" << bind_ret
              << " (ip=" << std::string(ip) << ", port=" << port << ")" << std::endl;
    if (bind_ret < 0) {
        ff_close(fd_);
        throw std::runtime_error("Failed to bind UDP socket in F-Stack.");
    }
}

FStackUdpSocket::~FStackUdpSocket() {
    if (fd_ >= 0) {
        ff_close(fd_);
    }
}

void FStackUdpSocket::send_to(rust::Slice<const uint8_t> payload, const rust::String& dest_ip, uint16_t dest_port) const {
    struct sockaddr_in dest = make_addr(std::string(dest_ip), dest_port);
    ssize_t sent = ff_sendto(fd_, payload.data(), payload.size(), 0, (struct linux_sockaddr *)&dest, sizeof(dest));
    if (sent < 0) {
        std::cerr << "Failed to send UDP packet via F-Stack" << std::endl;
    }
}

void FStackUdpSocket::read_available() const {
    char buffer[65535];
    struct sockaddr_in src_addr;
    socklen_t addrlen = sizeof(src_addr);

    // Non-blocking try read
    ssize_t len = ff_recvfrom(fd_, buffer, sizeof(buffer), 0, (struct linux_sockaddr *)&src_addr, &addrlen);
    if (len < 0 && errno != EAGAIN && errno != EWOULDBLOCK) {
        static int err_count = 0;
        if (err_count++ < 5) {
            std::cerr << "[DEBUG] ff_recvfrom error: " << errno << " (" << strerror(errno) << ")" << std::endl;
        }
    }
    if (len > 0) {
        UdpMessage msg;
        msg.payload = rust::Vec<uint8_t>();
        std::copy_n(reinterpret_cast<const uint8_t*>(buffer), len, std::back_inserter(msg.payload));
        
        char ip_str[INET_ADDRSTRLEN];
        inet_ntop(AF_INET, &src_addr.sin_addr, ip_str, INET_ADDRSTRLEN);
        msg.src_ip = rust::String(ip_str);
        msg.src_port = ntohs(src_addr.sin_port);

        callback_(fd_, msg);
    }
}

static uint64_t loop_count = 0;
static rust::Fn<void()>* udp_tick_callback_ = nullptr;

static int loop_callback(void* arg) {
    if (udp_tick_callback_) {
        (*udp_tick_callback_)();
    }
    auto* socket = static_cast<const FStackUdpSocket*>(arg);
    socket->read_available();
    loop_count++;
    if (loop_count % 500000 == 0) {
        struct rte_eth_stats stats;
        if (rte_eth_stats_get(0, &stats) == 0) {
            std::cerr << "[DPDK] ipackets=" << stats.ipackets
                      << " opackets=" << stats.opackets
                      << " ierrors=" << stats.ierrors
                      << " imissed=" << stats.imissed
                      << " rx_nombuf=" << stats.rx_nombuf
                      << std::endl;
        }
    }
    return 0;
}

void run_fstack(const FStackUdpSocket& socket) {
    ff_run(loop_callback, (void*)&socket);
}

void set_udp_tick_callback(rust::Fn<void()> cb) {
    udp_tick_callback_ = new rust::Fn<void()>(cb);
}

std::unique_ptr<FStackUdpSocket> create_udp_socket(const rust::String& ip, uint16_t port, rust::Fn<void(int32_t, const UdpMessage&)> callback) {
    return std::make_unique<FStackUdpSocket>(ip, port, callback);
}

// ---------------------------------------------------------------------------
// TCP listener
// ---------------------------------------------------------------------------

FStackTcpListener::FStackTcpListener(
    const rust::String& ip,
    uint16_t port,
    const TcpSocketOptionsFfi& opts,
    rust::Fn<void(int32_t, const rust::String&, uint16_t)> on_connect,
    rust::Fn<void(int32_t, const TcpMessage&)>             on_data,
    rust::Fn<void(int32_t)>                                on_disconnect
) : opts_(std::make_unique<TcpSocketOptionsFfi>(opts)), on_connect_(on_connect), on_data_(on_data), on_disconnect_(on_disconnect) {

    listen_fd_ = ff_socket(AF_INET, SOCK_STREAM, 0);
    if (listen_fd_ < 0) {
        throw std::runtime_error("Failed to create TCP socket in F-Stack.");
    }

    int on = 1;
    ff_setsockopt(listen_fd_, SOL_SOCKET, SO_REUSEADDR, &on, sizeof(on));

    if (opts_->reuse_port >= 0 && opts_->reuse_port) {
        ff_setsockopt(listen_fd_, SOL_SOCKET, SO_REUSEPORT, &on, sizeof(on));
    }

    if (ff_ioctl(listen_fd_, FIONBIO, &on) < 0) {
        ff_close(listen_fd_);
        throw std::runtime_error("Failed to set TCP listen socket non-blocking.");
    }

    struct sockaddr_in addr = make_addr(std::string(ip), port);
    if (ff_bind(listen_fd_, (struct linux_sockaddr*)&addr, sizeof(addr)) < 0) {
        ff_close(listen_fd_);
        throw std::runtime_error("Failed to bind TCP socket in F-Stack.");
    }

    if (ff_listen(listen_fd_, 128) < 0) {
        ff_close(listen_fd_);
        throw std::runtime_error("Failed to listen on TCP socket in F-Stack.");
    }

    std::cerr << "[TCP] Listening on " << std::string(ip) << ":" << port
              << " fd=" << listen_fd_ << std::endl;
}

FStackTcpListener::~FStackTcpListener() {
    for (auto& [fd, _] : connections_) {
        ff_close(fd);
    }
    if (listen_fd_ >= 0) {
        ff_close(listen_fd_);
    }
}

static void apply_socket_options(int fd, const TcpSocketOptionsFfi& opts) {
    int val{};

    if (opts.nodelay >= 0) {
        val = opts.nodelay;
        ff_setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &val, sizeof(val));
    }
    if (opts.keepalive >= 0) {
        val = opts.keepalive;
        ff_setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &val, sizeof(val));
    }
    if (opts.keepalive_idle_secs >= 0) {
        val = opts.keepalive_idle_secs;
        ff_setsockopt(fd, IPPROTO_TCP, TCP_KEEPIDLE, &val, sizeof(val));
    }
    if (opts.keepalive_interval_secs >= 0) {
        val = opts.keepalive_interval_secs;
        ff_setsockopt(fd, IPPROTO_TCP, TCP_KEEPINTVL, &val, sizeof(val));
    }
    if (opts.keepalive_count >= 0) {
        val = opts.keepalive_count;
        ff_setsockopt(fd, IPPROTO_TCP, TCP_KEEPCNT, &val, sizeof(val));
    }
    if (opts.recv_buf >= 0) {
        val = opts.recv_buf;
        ff_setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &val, sizeof(val));
    }
    if (opts.send_buf >= 0) {
        val = opts.send_buf;
        ff_setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &val, sizeof(val));
    }
    if (opts.linger_secs >= 0) {
        struct linger lg = { .l_onoff = 1, .l_linger = opts.linger_secs };
        ff_setsockopt(fd, SOL_SOCKET, SO_LINGER, &lg, sizeof(lg));
    }
    if (opts.quickack >= 0) {
        val = opts.quickack;
        ff_setsockopt(fd, IPPROTO_TCP, TCP_QUICKACK, &val, sizeof(val));
    }
}

void FStackTcpListener::accept_new() const {
    while (true) {
        struct sockaddr_in peer_addr;
        socklen_t peer_len = sizeof(peer_addr);
        int conn_fd = ff_accept(listen_fd_, (struct linux_sockaddr*)&peer_addr, &peer_len);

        if (conn_fd < 0) {
            if (errno != EAGAIN && errno != EWOULDBLOCK) {
                std::cerr << "[TCP] accept error: " << strerror(errno)
                          << " (errno=" << errno << ")" << std::endl;
            }
            return;
        }

        int on = 1;
        ff_ioctl(conn_fd, FIONBIO, &on);
        apply_socket_options(conn_fd, *opts_);

        char ip_str[INET_ADDRSTRLEN];
        inet_ntop(AF_INET, &peer_addr.sin_addr, ip_str, sizeof(ip_str));
        uint16_t peer_port = ntohs(peer_addr.sin_port);

        connections_[conn_fd] = {std::string(ip_str), peer_port};
        on_connect_(conn_fd, rust::String(ip_str), peer_port);
    }
}

void FStackTcpListener::read_all() const {
    char buffer[65536];

    // Collect fds to tear down after the iteration (can't mutate map while
    // iterating it).
    std::vector<int> dead;

    for (auto& [fd, peer] : connections_) {
        while (true) {
            ssize_t len = ff_read(fd, buffer, sizeof(buffer));

            if (len > 0) {
                TcpMessage msg;
                msg.payload = rust::Vec<uint8_t>();
                std::copy_n(reinterpret_cast<const uint8_t*>(buffer), len,
                            std::back_inserter(msg.payload));
                msg.src_ip   = rust::String(peer.first);
                msg.src_port = peer.second;
                on_data_(fd, msg);
                continue;          // drain everything available
            }

            if (len == 0) {        // peer sent FIN
                on_disconnect_(fd);
                dead.push_back(fd);
            } else {               // len < 0
                if (errno == EAGAIN || errno == EWOULDBLOCK) {
                    // Nothing buffered right now — try again next poll cycle.
                } else {
                    std::cerr << "[TCP] Read error fd=" << fd
                              << " errno=" << errno << " (" << strerror(errno) << ")" << std::endl;
                    on_disconnect_(fd);
                    dead.push_back(fd);
                }
            }
            break;
        }
    }

    for (int fd : dead) {
        ff_close(fd);
        connections_.erase(fd);
    }
}

void FStackTcpListener::send_to(int32_t fd, rust::Slice<const uint8_t> payload) const {
    const uint8_t* data = payload.data();
    size_t remaining = payload.size();
    while (remaining > 0) {
        ssize_t sent = ff_write(fd, data, remaining);
        if (sent < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                continue;
            }
            std::cerr << "[TCP] Send error fd=" << fd
                      << " errno=" << errno << " (" << strerror(errno) << ")" << std::endl;
            break;
        }
        data      += sent;
        remaining -= sent;
    }
}

void FStackTcpListener::close_connection(int32_t fd) const {
    auto it = connections_.find(fd);
    if (it != connections_.end()) {
        ff_close(fd);
        connections_.erase(it);
    }
}

static rust::Fn<void()>* tcp_tick_callback_ = nullptr;

static int loop_callback_tcp(void* arg) {
    if (tcp_tick_callback_) {
        (*tcp_tick_callback_)();
    }
    auto* listener = static_cast<const FStackTcpListener*>(arg);
    listener->accept_new();
    listener->read_all();
    return 0;
}

void set_tcp_tick_callback(rust::Fn<void()> cb) {
    tcp_tick_callback_ = new rust::Fn<void()>(cb);
}

void run_fstack_tcp(const FStackTcpListener& listener) {
    ff_run(loop_callback_tcp, (void*)&listener);
}

std::unique_ptr<FStackTcpListener> create_tcp_listener(
    const rust::String& ip,
    uint16_t port,
    const TcpSocketOptionsFfi& opts,
    rust::Fn<void(int32_t, const rust::String&, uint16_t)> on_connect,
    rust::Fn<void(int32_t, const TcpMessage&)>             on_data,
    rust::Fn<void(int32_t)>                                on_disconnect
) {
    return std::make_unique<FStackTcpListener>(ip, port, opts, on_connect, on_data, on_disconnect);
}

} // namespace teto

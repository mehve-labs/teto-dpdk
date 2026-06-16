#pragma once

#include "rust/cxx.h"
#include <memory>
#include <string>
#include <unordered_map>
#include <utility>

namespace teto {

struct UdpMessage;
struct TcpMessage;
struct TcpSocketOptionsFfi;

class FStackUdpSocket {
public:
    FStackUdpSocket(const rust::String& ip, uint16_t port, rust::Fn<void(int32_t, const UdpMessage&)> callback);
    ~FStackUdpSocket();

    void send_to(rust::Slice<const uint8_t> payload, const rust::String& dest_ip, uint16_t dest_port) const;
    void read_available() const;
    int fd() const { return fd_; }

private:
    int fd_;
    rust::Fn<void(int32_t, const UdpMessage&)> callback_;
};

class FStackTcpListener {
public:
    FStackTcpListener(
        const rust::String& ip,
        uint16_t port,
        const TcpSocketOptionsFfi& opts,
        rust::Fn<void(int32_t, const rust::String&, uint16_t)> on_connect,
        rust::Fn<void(int32_t, const TcpMessage&)>             on_data,
        rust::Fn<void(int32_t)>                                on_disconnect
    );
    ~FStackTcpListener();

    void accept_new() const;
    void read_all() const;
    void send_to(int32_t fd, rust::Slice<const uint8_t> payload) const;
    void close_connection(int32_t fd) const;
    int listen_fd() const { return listen_fd_; }

private:
    int listen_fd_;
    mutable std::unordered_map<int, std::pair<std::string, uint16_t>> connections_;
    std::unique_ptr<TcpSocketOptionsFfi> opts_;
    rust::Fn<void(int32_t, const rust::String&, uint16_t)> on_connect_;
    rust::Fn<void(int32_t, const TcpMessage&)>             on_data_;
    rust::Fn<void(int32_t)>                                on_disconnect_;
};

void init_fstack(const rust::Vec<rust::String>& config_args,
                 const rust::Vec<rust::String>& eal_args);

void run_fstack(const FStackUdpSocket& socket);
void run_fstack_tcp(const FStackTcpListener& listener);

std::unique_ptr<FStackUdpSocket> create_udp_socket(const rust::String& ip, uint16_t port, rust::Fn<void(int32_t, const UdpMessage&)> callback);

std::unique_ptr<FStackTcpListener> create_tcp_listener(
    const rust::String& ip,
    uint16_t port,
    const TcpSocketOptionsFfi& opts,
    rust::Fn<void(int32_t, const rust::String&, uint16_t)> on_connect,
    rust::Fn<void(int32_t, const TcpMessage&)>             on_data,
    rust::Fn<void(int32_t)>                                on_disconnect
);

void set_tcp_tick_callback(rust::Fn<void()> cb);
void set_udp_tick_callback(rust::Fn<void()> cb);

} // namespace teto

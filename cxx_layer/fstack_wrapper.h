#pragma once

// Thin shim over F-Stack's ff_* API. Every socket call returns either a
// non-negative result or `-errno`; no callbacks, no state. All policy lives in
// Rust (src/net.rs, src/event.rs, src/runtime.rs).

#include "rust/cxx.h"
#include <cstddef>
#include <cstdint>

namespace teto {

struct KEvent;
struct LoopCtx;
enum class SockOpt : ::std::int32_t;

// Runtime. `init` throws (-> Rust `Err`) on failure.
void init(const rust::Vec<rust::String>& config_args,
          const rust::Vec<rust::String>& eal_args);
void run(LoopCtx& ctx);
void stop();

// Sockets (IPv4 only; addresses in host byte order).
int32_t sock_tcp();
int32_t sock_udp();
int32_t sock_set_nonblocking(int32_t fd);
int32_t sock_set_opt(int32_t fd, SockOpt opt, int32_t value);
int32_t sock_bind_v4(int32_t fd, uint32_t ip, uint16_t port);
int32_t sock_listen(int32_t fd, int32_t backlog);
int32_t sock_connect_v4(int32_t fd, uint32_t ip, uint16_t port);
int32_t sock_take_error(int32_t fd);
int32_t sock_accept_v4(int32_t fd, uint32_t& ip, uint16_t& port);
int32_t sock_local_addr_v4(int32_t fd, uint32_t& ip, uint16_t& port);
int64_t sock_read(int32_t fd, uint8_t* buf, size_t len);
int64_t sock_write(int32_t fd, rust::Slice<const uint8_t> buf);
int64_t sock_recvfrom_v4(int32_t fd, uint8_t* buf, size_t len, uint32_t& ip, uint16_t& port);
int64_t sock_sendto_v4(int32_t fd, rust::Slice<const uint8_t> buf, uint32_t ip, uint16_t port);
int32_t sock_shutdown(int32_t fd, int32_t how);
int32_t sock_unsent(int32_t fd);
int32_t sock_close(int32_t fd);

// kqueue. `kq_poll` never blocks.
int32_t kq_create();
int32_t kq_change(int32_t kq, const KEvent& change);
int32_t kq_poll(int32_t kq, rust::Slice<KEvent> events);

} // namespace teto

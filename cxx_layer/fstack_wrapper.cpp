#include "fstack_wrapper.h"
#include "teto-dpdk/src/sys.rs.h"

extern "C" {
#include <ff_event.h>
#include <ff_api.h>
#include <ff_config.h>

extern int ff_freebsd_init(void);
extern int ff_dpdk_init(int, char **);
extern int ff_dpdk_if_up(void);
// Sets errno to the Linux equivalent of a FreeBSD error number.
extern void ff_os_errno(int error);
}

#include <algorithm>
#include <arpa/inet.h>
#include <cerrno>
#include <cstdio>
#include <cstring>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdexcept>
#include <string>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>
#include <vector>

namespace teto {

namespace {

int64_t neg_errno() {
    return errno != 0 ? -static_cast<int64_t>(errno) : -EIO;
}

int64_t ret64(int64_t r) { return r < 0 ? neg_errno() : r; }
int32_t ret32(int r) { return r < 0 ? static_cast<int32_t>(neg_errno()) : r; }

// Fill a Linux-layout sockaddr (F-Stack translates Linux sockaddrs) from a
// SockAddr; returns its length.
socklen_t to_sockaddr(const SockAddr& a, struct sockaddr_storage& ss) {
    std::memset(&ss, 0, sizeof(ss));
    if (a.v6) {
        auto* s6 = reinterpret_cast<struct sockaddr_in6*>(&ss);
        s6->sin6_family = AF_INET6;
        s6->sin6_port = htons(a.port);
        s6->sin6_flowinfo = htonl(a.flowinfo);
        std::memcpy(&s6->sin6_addr, a.ip.data(), 16);
        s6->sin6_scope_id = a.scope_id;
        return sizeof(struct sockaddr_in6);
    }
    auto* s4 = reinterpret_cast<struct sockaddr_in*>(&ss);
    s4->sin_family = AF_INET;
    s4->sin_port = htons(a.port);
    std::memcpy(&s4->sin_addr, a.ip.data(), 4);
    return sizeof(struct sockaddr_in);
}

void from_sockaddr(const struct sockaddr_storage& ss, SockAddr& out) {
    out = SockAddr{};
    if (ss.ss_family == AF_INET6) {
        const auto* s6 = reinterpret_cast<const struct sockaddr_in6*>(&ss);
        out.v6 = true;
        out.port = ntohs(s6->sin6_port);
        out.flowinfo = ntohl(s6->sin6_flowinfo);
        std::memcpy(out.ip.data(), &s6->sin6_addr, 16);
        out.scope_id = s6->sin6_scope_id;
    } else {
        const auto* s4 = reinterpret_cast<const struct sockaddr_in*>(&ss);
        out.port = ntohs(s4->sin_port);
        std::memcpy(out.ip.data(), &s4->sin_addr, 4);
    }
}

const struct timespec ZERO_TIMEOUT = {0, 0};

// Redirects stdout and stderr to an anonymous temp file for its lifetime
// and hands what was written to `out` when destroyed (also during exception
// unwinding). A file rather than a pipe: init output can exceed a pipe
// buffer, and nothing reads the pipe until init returns.
class CaptureOutput {
public:
    CaptureOutput(bool enabled, rust::String& out) : out_(out) {
        if (!enabled) {
            return;
        }
        file_ = std::tmpfile();
        if (file_ == nullptr) {
            return;
        }
        std::fflush(stdout);
        std::fflush(stderr);
        saved_out_ = dup(STDOUT_FILENO);
        saved_err_ = dup(STDERR_FILENO);
        dup2(fileno(file_), STDOUT_FILENO);
        dup2(fileno(file_), STDERR_FILENO);
    }

    ~CaptureOutput() {
        if (file_ == nullptr) {
            return;
        }
        std::fflush(stdout);
        std::fflush(stderr);
        dup2(saved_out_, STDOUT_FILENO);
        dup2(saved_err_, STDERR_FILENO);
        close(saved_out_);
        close(saved_err_);
        std::string text;
        std::rewind(file_);
        char buf[4096];
        size_t n;
        while ((n = std::fread(buf, 1, sizeof(buf), file_)) > 0) {
            text.append(buf, n);
        }
        std::fclose(file_);
        out_ = rust::String::lossy(text);
    }

    CaptureOutput(const CaptureOutput&) = delete;
    CaptureOutput& operator=(const CaptureOutput&) = delete;

private:
    rust::String& out_;
    std::FILE* file_ = nullptr;
    int saved_out_ = -1;
    int saved_err_ = -1;
};

int loop_trampoline(void* arg) {
    teto_loop_tick(*static_cast<LoopCtx*>(arg));
    return 0;
}

} // namespace

void init(const rust::Vec<rust::String>& config_args,
          const rust::Vec<rust::String>& eal_args,
          bool capture,
          rust::String& output) {
    CaptureOutput captured(capture, output);
    // F-Stack may keep pointers into argv, so these strings are leaked on purpose
    // (init runs once per process).
    std::vector<char*> argv;
    for (const auto& arg : config_args) {
        argv.push_back(strdup(std::string(arg).c_str()));
    }
    argv.push_back(nullptr);

    if (ff_load_config(static_cast<int>(argv.size()) - 1, argv.data()) < 0) {
        throw std::runtime_error("F-Stack config load failed");
    }

    // Extra EAL args that F-Stack's config parser doesn't handle. dpdk_argv
    // has DPDK_CONFIG_NUM slots plus a terminating null.
    if (static_cast<size_t>(dpdk_argc) + eal_args.size() > DPDK_CONFIG_NUM) {
        throw std::runtime_error(
            "too many EAL arguments: config produced " + std::to_string(dpdk_argc) +
            ", " + std::to_string(eal_args.size()) + " extra requested, F-Stack allows " +
            std::to_string(DPDK_CONFIG_NUM) + " in total");
    }
    for (const auto& arg : eal_args) {
        dpdk_argv[dpdk_argc++] = strdup(std::string(arg).c_str());
    }
    dpdk_argv[dpdk_argc] = nullptr;

    // rte_eal_init rewrites argv slots (getopt permutation, and it stores the
    // program name into argv[optind - 1]). Handing it dpdk_argv directly
    // leaves a duplicated pointer there, which ff_unload_config frees twice
    // when ff_run returns. Give DPDK its own copy of the pointer array
    // (leaked: init runs once and EAL may keep it).
    char** eal_argv = new char*[dpdk_argc + 1];
    std::copy(dpdk_argv, dpdk_argv + dpdk_argc + 1, eal_argv);

    if (ff_dpdk_init(dpdk_argc, eal_argv) < 0) {
        throw std::runtime_error("F-Stack DPDK init failed");
    }
    if (ff_freebsd_init() < 0) {
        throw std::runtime_error("F-Stack FreeBSD init failed");
    }
    if (ff_dpdk_if_up() < 0) {
        throw std::runtime_error("F-Stack DPDK interface up failed");
    }
}

void run(LoopCtx& ctx) {
    ff_run(loop_trampoline, &ctx);
}

void stop() {
    ff_stop_run();
}

int32_t sock_tcp(bool v6) { return ret32(ff_socket(v6 ? AF_INET6 : AF_INET, SOCK_STREAM, 0)); }
int32_t sock_udp(bool v6) { return ret32(ff_socket(v6 ? AF_INET6 : AF_INET, SOCK_DGRAM, 0)); }

int32_t sock_set_nonblocking(int32_t fd) {
    int on = 1;
    return ret32(ff_ioctl(fd, FIONBIO, &on));
}

int32_t sock_set_opt(int32_t fd, SockOpt opt, int32_t value) {
    int level = SOL_SOCKET;
    int name = 0;
    switch (opt) {
        case SockOpt::ReuseAddr: name = SO_REUSEADDR; break;
        case SockOpt::ReusePort: name = SO_REUSEPORT; break;
        case SockOpt::KeepAlive: name = SO_KEEPALIVE; break;
        case SockOpt::RecvBuf:   name = SO_RCVBUF; break;
        case SockOpt::SendBuf:   name = SO_SNDBUF; break;
        case SockOpt::NoDelay:   level = IPPROTO_TCP; name = TCP_NODELAY; break;
        case SockOpt::KeepIdle:  level = IPPROTO_TCP; name = TCP_KEEPIDLE; break;
        case SockOpt::KeepIntvl: level = IPPROTO_TCP; name = TCP_KEEPINTVL; break;
        case SockOpt::KeepCnt:   level = IPPROTO_TCP; name = TCP_KEEPCNT; break;
        case SockOpt::Linger: {
            struct linger lg;
            lg.l_onoff = 1;
            lg.l_linger = value;
            return ret32(ff_setsockopt(fd, SOL_SOCKET, SO_LINGER, &lg, sizeof(lg)));
        }
        default:
            return -EINVAL;
    }
    int v = value;
    return ret32(ff_setsockopt(fd, level, name, &v, sizeof(v)));
}

int32_t sock_bind(int32_t fd, const SockAddr& addr) {
    struct sockaddr_storage ss;
    socklen_t len = to_sockaddr(addr, ss);
    return ret32(ff_bind(fd, reinterpret_cast<struct linux_sockaddr*>(&ss), len));
}

int32_t sock_listen(int32_t fd, int32_t backlog) {
    return ret32(ff_listen(fd, backlog));
}

int32_t sock_connect(int32_t fd, const SockAddr& addr) {
    struct sockaddr_storage ss;
    socklen_t len = to_sockaddr(addr, ss);
    return ret32(ff_connect(fd, reinterpret_cast<struct linux_sockaddr*>(&ss), len));
}

int32_t sock_take_error(int32_t fd) {
    int err = 0;
    socklen_t len = sizeof(err);
    if (ff_getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &len) < 0) {
        return static_cast<int32_t>(neg_errno());
    }
    if (err == 0) {
        return 0;
    }
    // SO_ERROR holds a FreeBSD error number; translate it like ff_* calls do.
    ff_os_errno(err);
    return errno != 0 ? errno : EIO;
}

int32_t sock_accept(int32_t fd, SockAddr& peer) {
    struct sockaddr_storage ss;
    std::memset(&ss, 0, sizeof(ss));
    socklen_t len = sizeof(ss);
    int r = ff_accept(fd, reinterpret_cast<struct linux_sockaddr*>(&ss), &len);
    if (r < 0) {
        return static_cast<int32_t>(neg_errno());
    }
    from_sockaddr(ss, peer);
    return r;
}

int32_t sock_local_addr(int32_t fd, SockAddr& out) {
    struct sockaddr_storage ss;
    std::memset(&ss, 0, sizeof(ss));
    socklen_t len = sizeof(ss);
    if (ff_getsockname(fd, reinterpret_cast<struct linux_sockaddr*>(&ss), &len) < 0) {
        return static_cast<int32_t>(neg_errno());
    }
    from_sockaddr(ss, out);
    return 0;
}

int64_t sock_read(int32_t fd, uint8_t* buf, size_t len) {
    return ret64(ff_read(fd, buf, len));
}

int64_t sock_write(int32_t fd, rust::Slice<const uint8_t> buf) {
    return ret64(ff_write(fd, buf.data(), buf.size()));
}

int64_t sock_recvfrom(int32_t fd, uint8_t* buf, size_t len, SockAddr& from) {
    struct sockaddr_storage ss;
    std::memset(&ss, 0, sizeof(ss));
    socklen_t addrlen = sizeof(ss);
    ssize_t r = ff_recvfrom(fd, buf, len, 0, reinterpret_cast<struct linux_sockaddr*>(&ss), &addrlen);
    if (r < 0) {
        return neg_errno();
    }
    from_sockaddr(ss, from);
    return r;
}

int64_t sock_sendto(int32_t fd, rust::Slice<const uint8_t> buf, const SockAddr& to) {
    struct sockaddr_storage ss;
    socklen_t len = to_sockaddr(to, ss);
    return ret64(ff_sendto(fd, buf.data(), buf.size(), 0, reinterpret_cast<struct linux_sockaddr*>(&ss), len));
}

int32_t sock_shutdown(int32_t fd, int32_t how) {
    return ret32(ff_shutdown(fd, how));
}

int32_t sock_unsent(int32_t fd) {
    // FreeBSD's FIONWRITE: bytes in the send buffer, i.e. not yet
    // acknowledged by the peer. ff_ioctl would translate Linux request
    // numbers, and Linux has no FIONWRITE, so use the FreeBSD entry point.
    const unsigned long FREEBSD_FIONWRITE = 0x40046677UL; // _IOR('f', 119, int)
    int n = 0;
    if (ff_ioctl_freebsd(fd, FREEBSD_FIONWRITE, &n) < 0) {
        return static_cast<int32_t>(neg_errno());
    }
    return n;
}

int32_t sock_close(int32_t fd) {
    return ret32(ff_close(fd));
}

int32_t kq_create() {
    return ret32(ff_kqueue());
}

int32_t kq_change(int32_t kq, const KEvent& c) {
    struct kevent kev;
    EV_SET(&kev, c.ident, c.filter, c.flags, c.fflags, c.data,
           reinterpret_cast<void*>(static_cast<uintptr_t>(c.udata)));
    return ret32(ff_kevent(kq, &kev, 1, nullptr, 0, &ZERO_TIMEOUT));
}

int32_t kq_poll(int32_t kq, rust::Slice<KEvent> events) {
    const size_t want = std::min<size_t>(events.size(), 1 << 20);
    thread_local std::vector<struct kevent> buf;
    if (buf.size() < want) {
        buf.resize(want);
    }
    int n = ff_kevent(kq, nullptr, 0, buf.data(), static_cast<int>(want), &ZERO_TIMEOUT);
    if (n < 0) {
        return static_cast<int32_t>(neg_errno());
    }
    for (int i = 0; i < n; i++) {
        KEvent& out = events[i];
        out.ident = buf[i].ident;
        out.filter = buf[i].filter;
        out.flags = buf[i].flags;
        out.fflags = buf[i].fflags;
        out.data = buf[i].data;
        out.udata = static_cast<uint64_t>(reinterpret_cast<uintptr_t>(buf[i].udata));
    }
    return n;
}

} // namespace teto

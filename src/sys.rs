//! Raw bridge to `cxx_layer/fstack_wrapper.cpp`.
//!
//! Socket functions return a non-negative value on success or `-errno`
//! (Linux numbering; F-Stack translates FreeBSD errors). Nothing here is
//! public: the safe API lives in `runtime`, `net` and `event`.

use std::any::Any;
use std::io;
use std::panic::{self, AssertUnwindSafe};

#[cxx::bridge(namespace = "teto")]
pub(crate) mod ffi {
    /// Mirror of F-Stack's `struct kevent` without the `ext` words.
    #[derive(Clone, Copy, Debug, Default)]
    struct KEvent {
        ident: u64,
        filter: i16,
        flags: u16,
        fflags: u32,
        data: i64,
        udata: u64,
    }

    #[repr(i32)]
    enum SockOpt {
        ReuseAddr,
        ReusePort,
        NoDelay,
        KeepAlive,
        KeepIdle,
        KeepIntvl,
        KeepCnt,
        RecvBuf,
        SendBuf,
        /// Value is the linger timeout in seconds; negative disables linger.
        Linger,
    }

    extern "Rust" {
        type LoopCtx;
        fn teto_loop_tick(ctx: &mut LoopCtx);
    }

    unsafe extern "C++" {
        include!("fstack_wrapper.h");

        fn init(config_args: &Vec<String>, eal_args: &Vec<String>) -> Result<()>;
        fn run(ctx: &mut LoopCtx);
        fn stop();

        fn sock_tcp() -> i32;
        fn sock_udp() -> i32;
        fn sock_set_nonblocking(fd: i32) -> i32;
        fn sock_set_opt(fd: i32, opt: SockOpt, value: i32) -> i32;
        fn sock_bind_v4(fd: i32, ip: u32, port: u16) -> i32;
        fn sock_listen(fd: i32, backlog: i32) -> i32;
        fn sock_accept_v4(fd: i32, ip: &mut u32, port: &mut u16) -> i32;
        fn sock_local_addr_v4(fd: i32, ip: &mut u32, port: &mut u16) -> i32;
        unsafe fn sock_read(fd: i32, buf: *mut u8, len: usize) -> i64;
        fn sock_write(fd: i32, buf: &[u8]) -> i64;
        unsafe fn sock_recvfrom_v4(
            fd: i32,
            buf: *mut u8,
            len: usize,
            ip: &mut u32,
            port: &mut u16,
        ) -> i64;
        fn sock_sendto_v4(fd: i32, buf: &[u8], ip: u32, port: u16) -> i64;
        fn sock_shutdown(fd: i32, how: i32) -> i32;
        fn sock_close(fd: i32) -> i32;

        fn kq_create() -> i32;
        fn kq_change(kq: i32, change: &KEvent) -> i32;
        fn kq_poll(kq: i32, events: &mut [KEvent]) -> i32;
    }
}

/// Converts a `-errno` style return value into an `io::Result`.
pub(crate) fn cvt(r: i64) -> io::Result<i64> {
    if r < 0 {
        Err(io::Error::from_raw_os_error((-r) as i32))
    } else {
        Ok(r)
    }
}

pub(crate) fn cvt32(r: i32) -> io::Result<i32> {
    cvt(r.into()).map(|v| v as i32)
}

/// State handed through `ff_run`'s `void *arg` back into Rust on every
/// poll-loop iteration.
pub(crate) struct LoopCtx {
    // Lifetime-erased borrow of the closure passed to `FStack::run`; valid for
    // the whole `ff_run` call, which is the only time C++ holds this ctx.
    tick: *mut (dyn FnMut() + 'static),
    panic: Option<Box<dyn Any + Send>>,
}

impl LoopCtx {
    /// # Safety
    /// `tick` must stay valid until the `ffi::run` call using this ctx returns.
    pub(crate) unsafe fn new(tick: &mut dyn FnMut()) -> Self {
        // SAFETY: only the trait-object lifetime changes; the caller guarantees
        // the pointee outlives every use.
        let tick: *mut (dyn FnMut() + 'static) = unsafe { std::mem::transmute(tick) };
        Self { tick, panic: None }
    }

    pub(crate) fn take_panic(&mut self) -> Option<Box<dyn Any + Send>> {
        self.panic.take()
    }
}

fn teto_loop_tick(ctx: &mut LoopCtx) {
    if ctx.panic.is_some() {
        // A previous tick panicked and requested a stop; don't run user code
        // again before ff_run returns.
        return;
    }
    // SAFETY: see `LoopCtx::new`.
    let tick = unsafe { &mut *ctx.tick };
    // Unwinding across the C++ frames of ff_run would abort, so catch here,
    // stop the loop, and re-raise once `FStack::run` is back in Rust.
    if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(tick)) {
        ctx.panic = Some(payload);
        ffi::stop();
    }
}

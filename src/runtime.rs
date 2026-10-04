use std::io;
use std::marker::PhantomData;
use std::panic;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::OnceLock;

use crate::config::FStackConfig;
use crate::sys::{ffi, LoopCtx};

static INIT_ATTEMPTED: AtomicBool = AtomicBool::new(false);
static INIT_OUTPUT: OnceLock<String> = OnceLock::new();

// Lifecycle of the process-wide F-Stack instance. When `ff_run` returns,
// F-Stack tears itself down (config unload, `rte_eal_cleanup`), so any ff_*
// call afterwards — including closing a socket — is use-after-free.
const UNINIT: u8 = 0;
const READY: u8 = 1;
const RUNNING: u8 = 2;
const FINISHED: u8 = 3;
static STATE: AtomicU8 = AtomicU8::new(UNINIT);

/// Whether F-Stack calls are currently allowed.
pub(crate) fn is_alive() -> bool {
    matches!(STATE.load(Ordering::Acquire), READY | RUNNING)
}

pub(crate) fn ensure_alive() -> io::Result<()> {
    if is_alive() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "F-Stack has shut down"))
    }
}

/// Handle to the process-wide F-Stack instance.
///
/// Obtained once per process from [`FStack::init`]. F-Stack is single-threaded:
/// every socket call must happen on the thread that initialised it, so this
/// handle — and every socket created from it — is `!Send` and `!Sync`.
///
/// The poll loop ([`FStack::run`]) runs at most once. When it returns, F-Stack
/// shuts down: from then on socket operations return
/// [`io::ErrorKind::BrokenPipe`] and dropping a socket releases nothing.
#[derive(Clone, Copy, Debug)]
pub struct FStack {
    _not_send: PhantomData<*const ()>,
}

impl FStack {
    /// Initialise DPDK and the F-Stack TCP/IP stack on the calling thread.
    ///
    /// Only the first call in a process does anything. Later calls return
    /// [`io::ErrorKind::AlreadyExists`], including after a failed first call:
    /// DPDK cannot be re-initialised once its EAL init has started.
    ///
    /// Some fatal DPDK errors terminate the process inside DPDK itself
    /// (`rte_exit`); those cannot be turned into an `Err`.
    pub fn init(cfg: &FStackConfig) -> io::Result<FStack> {
        // Checked before anything touches F-Stack, so a wrong path can be
        // fixed and init retried (a failed F-Stack init can't be).
        let path = std::path::Path::new(cfg.config_file());
        if !path.is_file() {
            let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "F-Stack config file {} not found (working directory: {cwd}); \
                     pass a path to FStackConfig::new / with_config_file or set ${}",
                    path.display(),
                    crate::config::CONFIG_ENV
                ),
            ));
        }
        if INIT_ATTEMPTED.swap(true, Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "F-Stack has already been initialised in this process",
            ));
        }
        let mut output = String::new();
        let result = ffi::init(&cfg.config_args(), &cfg.eal_args(), cfg.captures_init_output(), &mut output);
        if let Err(e) = result {
            let mut msg = e.what().to_owned();
            if !output.trim().is_empty() {
                msg.push_str("\n--- F-Stack initialisation output ---\n");
                msg.push_str(output.trim_end());
            }
            return Err(io::Error::other(msg));
        }
        let _ = INIT_OUTPUT.set(output);
        STATE.store(READY, Ordering::Release);
        Ok(FStack { _not_send: PhantomData })
    }

    /// What F-Stack, DPDK and the FreeBSD stack printed during
    /// initialisation, if [`FStackConfig::capture_init_output`] was enabled;
    /// empty otherwise.
    pub fn init_output(&self) -> &'static str {
        INIT_OUTPUT.get().map_or("", String::as_str)
    }

    /// Run the F-Stack poll loop on this thread, calling `tick` once per
    /// iteration, until [`FStack::stop`] is called (normally from inside
    /// `tick`). F-Stack shuts down when this returns; it can't be run again.
    ///
    /// `tick` must not block: the network stack only makes progress between
    /// calls. If `tick` panics, the loop stops and the panic resumes from
    /// `run`. Calling `run` while it is running (from inside `tick`), or after
    /// it has returned, is an error.
    pub fn run<F: FnMut()>(&self, mut tick: F) -> io::Result<()> {
        match STATE.compare_exchange(READY, RUNNING, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {}
            Err(RUNNING) => return Err(io::Error::other("FStack::run is already running")),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the F-Stack poll loop has already finished and cannot be restarted",
                ));
            }
        }

        // SAFETY: `tick` outlives the `ffi::run` call below, and `ctx` is not
        // used after it returns.
        let mut ctx = unsafe { LoopCtx::new(&mut tick) };
        ffi::run(&mut ctx);
        STATE.store(FINISHED, Ordering::Release);
        if let Some(payload) = ctx.take_panic() {
            panic::resume_unwind(payload);
        }
        Ok(())
    }

    /// Ask the poll loop to return after the current iteration. Has no effect
    /// outside [`FStack::run`].
    pub fn stop(&self) {
        if STATE.load(Ordering::Acquire) == RUNNING {
            ffi::stop();
        }
    }
}

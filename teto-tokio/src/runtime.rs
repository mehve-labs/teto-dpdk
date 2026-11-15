use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use teto_dpdk::{FStack, FStackConfig, TcpSocketOptions};

use crate::driver::{AcceptItem, Connected, Driver};
use crate::udp_socket::UdpParts;

/// Requests from tokio tasks to the F-Stack thread.
pub(crate) enum Cmd {
    ListenTcp {
        addr: SocketAddr,
        opts: TcpSocketOptions,
        accept_tx: mpsc::Sender<AcceptItem>,
        reply: oneshot::Sender<io::Result<SocketAddr>>,
    },
    BindUdp {
        addr: SocketAddr,
        parts: UdpParts,
        reply: oneshot::Sender<io::Result<SocketAddr>>,
    },
    Connect {
        local: Option<SocketAddr>,
        addr: SocketAddr,
        opts: TcpSocketOptions,
        reply: oneshot::Sender<io::Result<Connected>>,
    },
    SetOptions {
        id: u64,
        opts: TcpSocketOptions,
        reply: oneshot::Sender<io::Result<()>>,
    },
}

struct Shared {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    init_output: &'static str,
    /// Becomes `true` when the F-Stack thread has finished.
    stopped: watch::Receiver<bool>,
}

/// Handle to the F-Stack runtime: a dedicated OS thread running F-Stack's
/// poll loop, which owns every socket.
///
/// Start it once per process with [`start`](Self::start), then create any
/// number of [`TetoTcpListener`](crate::TetoTcpListener)s,
/// [`TetoUdpSocket`](crate::TetoUdpSocket)s and outbound
/// [`TetoTcpStream`](crate::TetoTcpStream)s from it. Handles are cheap to
/// clone. The F-Stack thread exits once every handle and every socket created
/// from it has been dropped (after giving closing connections time to deliver
/// their data); F-Stack can't be restarted in the same process afterwards.
///
/// Like any socket library, data still being delivered is lost if the
/// process exits first: if `main` returns right after dropping a stream that
/// had unsent data, that data is discarded.
///
/// ```rust,no_run
/// use teto_dpdk::config::{FStackConfig, TcpSocketOptions};
/// use teto_tokio::{TetoRuntime, TetoTcpListener, TetoTcpStream, TetoUdpSocket};
///
/// # async fn demo() -> std::io::Result<()> {
/// let rt = TetoRuntime::start(FStackConfig::for_docker()).await?;
/// let opts = TcpSocketOptions::default().nodelay(true);
/// let listener = TetoTcpListener::bind(&rt, "0.0.0.0:8080".parse().unwrap(), opts.clone()).await?;
/// let udp = TetoUdpSocket::bind(&rt, "0.0.0.0:9000".parse().unwrap()).await?;
/// let upstream = TetoTcpStream::connect(&rt, "10.0.0.2:5432".parse().unwrap(), opts).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct TetoRuntime {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for TetoRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TetoRuntime").finish_non_exhaustive()
    }
}

impl TetoRuntime {
    /// Initialise F-Stack on a new thread and start its poll loop.
    ///
    /// F-Stack can be initialised once per process: a second call returns
    /// [`io::ErrorKind::AlreadyExists`], as does any call after F-Stack itself
    /// failed to initialise or after this future was cancelled once
    /// initialisation had started. (A missing config file is reported before
    /// F-Stack is touched, so that one can be fixed and retried.)
    pub async fn start(cfg: FStackConfig) -> io::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel::<io::Result<&'static str>>();
        let (stopped_tx, stopped) = watch::channel(false);
        std::thread::Builder::new().name("fstack".into()).spawn(move || {
            // Marks the runtime stopped however the thread ends (normal stop,
            // init failure, or a panic unwinding).
            struct Stopped(watch::Sender<bool>);
            impl Drop for Stopped {
                fn drop(&mut self) {
                    let _ = self.0.send(true);
                }
            }
            let _stopped = Stopped(stopped_tx);
            let started = FStack::init(&cfg).and_then(|fs| Ok((fs, Driver::new(fs, cmd_rx)?)));
            let (fs, mut driver) = match started {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(fs.init_output()));
            // Runs until nothing uses the runtime any more (or a tick
            // panics; the panic then resumes here and ends the thread).
            let _ = fs.run(|| {
                if !driver.tick() {
                    fs.stop();
                }
            });
        })?;
        let init_output = ready_rx
            .await
            .map_err(|_| io::Error::other("F-Stack thread exited during initialisation"))??;
        Ok(TetoRuntime { shared: Arc::new(Shared { cmd_tx, init_output, stopped }) })
    }

    /// Drop this handle and wait until the runtime has stopped: after every
    /// other handle and every socket created from the runtime has been
    /// dropped, closing connections have delivered their data (or timed out),
    /// and F-Stack has been torn down.
    ///
    /// Call it at the end of `main` so the process doesn't exit while data is
    /// still being delivered. It waits for *all* sockets, so drop them first;
    /// wrap it in `tokio::time::timeout` to bound the wait.
    pub async fn shutdown(self) {
        let mut stopped = self.shared.stopped.clone();
        drop(self);
        // An error means the thread is gone (its sender was dropped).
        let _ = stopped.wait_for(|stopped| *stopped).await;
    }

    /// What F-Stack and DPDK printed during initialisation, if the config
    /// enabled [`FStackConfig::capture_init_output`]; empty otherwise. (On
    /// failure it is part of the error returned by [`start`](Self::start).)
    pub fn init_output(&self) -> &str {
        self.shared.init_output
    }

    /// Send a command to the F-Stack thread and wait for its reply.
    pub(crate) async fn call<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<io::Result<T>>) -> Cmd,
    ) -> io::Result<T> {
        let (reply, rx) = oneshot::channel();
        self.shared.cmd_tx.send(make(reply)).map_err(|_| runtime_stopped())?;
        rx.await.map_err(|_| runtime_stopped())?
    }
}

pub(crate) fn runtime_stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "F-Stack runtime stopped")
}

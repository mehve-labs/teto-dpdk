use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

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
        addr: SocketAddr,
        opts: TcpSocketOptions,
        reply: oneshot::Sender<io::Result<Connected>>,
    },
}

struct Shared {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
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
    /// F-Stack can be initialised once per process: a second call (or a call
    /// after a failed one) returns [`io::ErrorKind::AlreadyExists`].
    pub async fn start(cfg: FStackConfig) -> io::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel::<io::Result<()>>();
        std::thread::Builder::new().name("fstack".into()).spawn(move || {
            let started = FStack::init(&cfg).and_then(|fs| Ok((fs, Driver::new(fs, cmd_rx)?)));
            let (fs, mut driver) = match started {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            // Runs until nothing uses the runtime any more (or a tick
            // panics; the panic then resumes here and ends the thread).
            let _ = fs.run(|| {
                if !driver.tick() {
                    fs.stop();
                }
            });
        })?;
        ready_rx
            .await
            .map_err(|_| io::Error::other("F-Stack thread exited during initialisation"))??;
        Ok(TetoRuntime { shared: Arc::new(Shared { cmd_tx }) })
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

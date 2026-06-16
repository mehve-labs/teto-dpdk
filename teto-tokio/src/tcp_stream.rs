use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

use crate::runtime::FStackCmd;

/// An async TCP stream backed by an F-Stack connection.
///
/// Implements [`tokio::io::AsyncRead`] and [`tokio::io::AsyncWrite`], so it
/// can be used with all the familiar `tokio::io` utilities (`read`, `write_all`,
/// `copy`, `BufReader`, etc.).
///
/// Reads are fulfilled from data pushed by the F-Stack poll loop via an async
/// channel. Writes are queued as commands and executed on the next F-Stack
/// poll iteration.
pub struct TetoTcpStream {
    fd: i32,
    peer_addr: SocketAddr,
    data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cmd_tx: mpsc::UnboundedSender<FStackCmd>,
    read_buf: BytesMut,
    shutdown_sent: bool,
}

impl TetoTcpStream {
    pub(crate) fn new(
        fd: i32,
        peer_addr: SocketAddr,
        data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        cmd_tx: mpsc::UnboundedSender<FStackCmd>,
    ) -> Self {
        Self {
            fd,
            peer_addr,
            data_rx,
            cmd_tx,
            read_buf: BytesMut::new(),
            shutdown_sent: false,
        }
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
}

impl AsyncRead for TetoTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if !this.read_buf.is_empty() {
            let n = std::cmp::min(this.read_buf.len(), buf.remaining());
            buf.put_slice(&this.read_buf.split_to(n));
            return Poll::Ready(Ok(()));
        }

        match this.data_rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = std::cmp::min(data.len(), buf.remaining());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    this.read_buf.extend_from_slice(&data[n..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for TetoTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.cmd_tx
            .send(FStackCmd::TcpWrite {
                fd: this.fd,
                data: buf.to_vec(),
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "F-Stack runtime shut down"))?;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.shutdown_sent {
            this.shutdown_sent = true;
            let _ = this.cmd_tx.send(FStackCmd::TcpClose { fd: this.fd });
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for TetoTcpStream {
    fn drop(&mut self) {
        if !self.shutdown_sent {
            let _ = self.cmd_tx.send(FStackCmd::TcpClose { fd: self.fd });
        }
    }
}

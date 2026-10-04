//! Low-level API tests against a live F-Stack instance. Needs the project's
//! Docker environment (privileged container running `entrypoint.sh`).

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream as StdTcpStream, UdpSocket as StdUdpSocket};
use std::time::{Duration, Instant};

use teto_dpdk::event::{Events, Interest, Kqueue};
use teto_dpdk::net::{TcpListener, TcpStream, UdpSocket};
use teto_dpdk::{FStack, FStackConfig, TcpSocketOptions};

fn config() -> FStackConfig {
    FStackConfig::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config.ini"))
        .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
        .with_eal_arg("--no-pci")
        .with_eal_arg("--iova-mode=va")
}

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

#[test]
fn lowlevel_suite() {
    let fs = FStack::init(&config()).expect("init");
    assert_eq!(FStack::init(&config()).unwrap_err().kind(), ErrorKind::AlreadyExists);

    // Bind validation and errors (C3, C6).
    let opts = TcpSocketOptions::default().nodelay(true);
    assert_eq!(
        TcpListener::bind(&fs, sa("[::1]:8080"), &opts).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    #[allow(deprecated)]
    let quickack = TcpSocketOptions::default().quickack(true);
    assert_eq!(
        TcpListener::bind(&fs, sa("0.0.0.0:8080"), &quickack).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    let listener = TcpListener::bind(&fs, sa("0.0.0.0:8080"), &opts).expect("bind");
    assert_eq!(
        TcpListener::bind(&fs, sa("0.0.0.0:8080"), &opts).unwrap_err().kind(),
        ErrorKind::AddrInUse
    );
    assert_eq!(listener.local_addr().unwrap().port(), 8080);
    let udp = UdpSocket::bind(&fs, sa("0.0.0.0:9000")).unwrap();
    assert_eq!(udp.send_to(b"x", sa("[::1]:9")).unwrap_err().kind(), ErrorKind::InvalidInput);

    let mut tcp = TcpEcho::new(&fs, &listener);
    let mut udp_echo = UdpEcho { socket: &udp, buf: [0u8; 2048] };
    let tcp_done = spawn_tcp_clients();
    let udp_done = spawn_udp_client();

    let deadline = Instant::now() + Duration::from_secs(180);
    let mut nested = None;
    fs.run(|| {
        // S1: re-entering the loop is refused.
        if nested.is_none() {
            nested = Some(fs.run(|| {}));
        }
        tcp.tick();
        udp_echo.tick();
        let finished = tcp_done.is_finished() && udp_done.is_finished();
        if finished || Instant::now() > deadline {
            fs.stop();
        }
    })
    .unwrap();
    assert!(nested.unwrap().is_err());
    tcp_done.join().expect("tcp clients");
    udp_done.join().expect("udp client");

    // F-Stack has shut down: everything fails cleanly, nothing is closed twice.
    let gone = |e: std::io::Error| assert_eq!(e.kind(), ErrorKind::BrokenPipe, "{e}");
    gone(fs.run(|| {}).unwrap_err());
    gone(listener.accept().unwrap_err());
    gone(udp.recv_from(&mut [0u8; 8]).unwrap_err());
    gone(Kqueue::new(&fs).unwrap_err());
    gone(TcpListener::bind(&fs, sa("0.0.0.0:8081"), &opts).unwrap_err());
    gone(tcp.kq.poll(&mut Events::with_capacity(1)).unwrap_err());
    drop(tcp);
    drop(listener);
    drop(udp);
}

struct Conn {
    stream: TcpStream,
    out: Vec<u8>,
}

struct TcpEcho<'a> {
    listener: &'a TcpListener,
    kq: Kqueue,
    conns: HashMap<u64, Conn>,
    next: u64,
    events: Events,
    buf: Vec<u8>,
}

impl<'a> TcpEcho<'a> {
    fn new(fs: &FStack, listener: &'a TcpListener) -> Self {
        let kq = Kqueue::new(fs).unwrap();
        kq.register(listener, 0, Interest::READABLE).unwrap();
        TcpEcho { listener, kq, conns: HashMap::new(), next: 1, events: Events::with_capacity(64), buf: vec![0u8; 4096] }
    }

    fn tick(&mut self) {
        self.kq.poll(&mut self.events).unwrap();
        for ev in self.events.iter() {
            if ev.token() == 0 {
                while let Ok((stream, _)) = self.listener.accept() {
                    self.kq.register(&stream, self.next, Interest::READABLE).unwrap();
                    self.conns.insert(self.next, Conn { stream, out: Vec::new() });
                    self.next += 1;
                }
                continue;
            }
            let Some(c) = self.conns.get_mut(&ev.token()) else { continue };
            let mut eof = false;
            loop {
                match c.stream.read(&mut self.buf) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => c.out.extend_from_slice(&self.buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => panic!("read: {e}"),
                }
            }
            while !c.out.is_empty() {
                match c.stream.write(&c.out) {
                    Ok(n) => drop(c.out.drain(..n)),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => panic!("write: {e}"),
                }
            }
            let interest = match (eof, c.out.is_empty()) {
                (true, true) => {
                    // Closing (dropping) while handling events is fine (S2).
                    self.conns.remove(&ev.token());
                    continue;
                }
                (true, false) => Interest::WRITABLE,
                (false, false) => Interest::READABLE | Interest::WRITABLE,
                (false, true) => Interest::READABLE,
            };
            self.kq.register(&c.stream, ev.token(), interest).unwrap();
        }
    }
}

fn spawn_tcp_clients() -> std::thread::JoinHandle<()> {
    const CLIENTS: usize = 50;
    std::thread::spawn(move || {
        // Wait for the TAP device to be configured.
        let deadline = Instant::now() + Duration::from_secs(90);
        while StdTcpStream::connect_timeout(&sa("10.0.0.1:8080"), Duration::from_secs(1)).is_err() {
            assert!(Instant::now() < deadline, "F-Stack never became reachable");
            std::thread::sleep(Duration::from_millis(500));
        }
        let handles: Vec<_> = (0..CLIENTS)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut s = StdTcpStream::connect(sa("10.0.0.1:8080")).unwrap();
                    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    let msg = format!("hello-{i}-").repeat(1000);
                    s.write_all(msg.as_bytes()).unwrap();
                    s.shutdown(Shutdown::Write).unwrap();
                    let mut got = Vec::new();
                    s.read_to_end(&mut got).unwrap();
                    assert!(got == msg.as_bytes(), "client {i}: wrong echo");
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    })
}

struct UdpEcho<'a> {
    socket: &'a UdpSocket,
    buf: [u8; 2048],
}

impl UdpEcho<'_> {
    fn tick(&mut self) {
        // P3: drain everything that's queued.
        loop {
            match self.socket.recv_from(&mut self.buf) {
                Ok((n, peer)) => {
                    let _ = self.socket.send_to(&self.buf[..n], peer.into());
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("recv: {e}"),
            }
        }
    }
}

fn spawn_udp_client() -> std::thread::JoinHandle<()> {
    const N: usize = 300;
    std::thread::spawn(move || {
        let c = StdUdpSocket::bind("0.0.0.0:0").unwrap();
        c.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut buf = [0u8; 64];
        let mut received = 0;
        let deadline = Instant::now() + Duration::from_secs(120);
        // Send in bursts until N echoes arrive (the first bursts may be lost
        // while the TAP device is still being configured).
        while received < N && Instant::now() < deadline {
            for i in 0..50 {
                // Fails with ENETUNREACH until the TAP device is configured.
                let _ = c.send_to(format!("d{i}").as_bytes(), "10.0.0.1:9000");
            }
            if received == 0 {
                std::thread::sleep(Duration::from_millis(500));
            }
            while let Ok((n, _)) = c.recv_from(&mut buf) {
                assert_eq!(buf[0], b'd', "{:?}", &buf[..n]);
                received += 1;
            }
        }
        assert!(received >= N, "only {received} echoes");
    })
}

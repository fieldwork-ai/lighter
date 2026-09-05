//! The link: the card's host end, and every host↔guest channel on it.
//!
//! The framework's network card is a datagram socketpair — one Ethernet
//! frame per datagram — and this module owns our end. Three things live on
//! it, all on one thread:
//!
//! - the responder (`net.rs`): DHCP and echo to the world, answered without
//!   a stack;
//! - a smoltcp interface on the gateway address, which terminates every TCP
//!   connection the guest makes to the host (streams, DNS, UDP, memory
//!   offers, the file server) and originates the ones the host makes to the
//!   guest (the Docker socket, the control channel, published ports);
//! - the reactor: a kqueue over the Mac sockets each stream is joined to,
//!   moving bytes straight between a smoltcp socket's ring and a Mac socket
//!   with no buffer of ours in between.
//!
//! Why TCP over the card and not vsock: S2 measured Apple's vsock at 7–8
//! Gbit/s a stream against 82–87 over the card at a 65535 MTU. The guest
//! side is unchanged in shape — the agent connects to the gateway instead of
//! to CID 2 — and the guest kernel's sockmap join works TCP-to-TCP.
//!
//! One thread, because the framework hands us one card. If the share and the
//! network together prove too much for it, the framework will give a second
//! card and this module runs twice; that is measured before it is built.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint};

use crate::net::{GATEWAY, GATEWAY_MAC, GUEST, HOST_ALIAS, Network, Seen};

/// The guest connects to these on the gateway.
pub const STREAM_PORT: u16 = 2377;
pub const DNS_PORT: u16 = 2379;
pub const UDP_PORT: u16 = 2380;
pub const MEMORY_PORT: u16 = 2381;
pub const SHARE_PORT: u16 = 2382;
/// The host connects to these on the guest.
pub const DOCKER_PORT: u16 = 2375;
pub const CONTROL_PORT: u16 = 2376;
pub const INBOUND_PORT: u16 = 2378;

pub const HEADER_LEN: usize = 19;
const READ_CHUNK: usize = 256 * 1024;
/// A socket's rings. The receive ring is the window the guest is offered
/// and the round trip through this loop is what it has to cover: measured
/// on the M5, container→Mac read 36 Gbit/s at 1 MiB, 54 at 4 and 55 at 8;
/// the send ring made no difference to Mac→container (37, 40, 39), so it
/// stays small. `LIGHTER_LINK_RX_KIB` and `LIGHTER_LINK_TX_KIB` override.
const RX_RING: usize = 4 << 20;
const TX_RING: usize = 1 << 20;

fn ring_bytes() -> (usize, usize) {
    let knob = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map(|kib| kib.clamp(64, 65_536) << 10)
            .unwrap_or(default)
    };
    (
        knob("LIGHTER_LINK_RX_KIB", RX_RING),
        knob("LIGHTER_LINK_TX_KIB", TX_RING),
    )
}
/// Listening sockets kept ready per port: each is one connection the guest
/// can open without a round trip of ours in the way.
const STREAM_BACKLOG: usize = 64;
const SMALL_BACKLOG: usize = 2;
const UDP_HEADER: usize = 7;
const UDP_KIND_DATA: u8 = 0;
const UDP_KIND_OPEN: u8 = 1;
const UDP_KIND_CLOSE: u8 = 2;
/// Frames drained from the card per turn before the sockets get a look.
const FRAMES_PER_TURN: usize = 128;

/// What the share side of the link needs from whoever serves shares.
pub trait ShareTransport: Send + Sync {
    /// A connection announced `tag`; true to keep it.
    fn open(&self, conn: ConnId, tag: &str) -> bool;
    /// One whole FUSE request from `conn`. Answered now (the reply returned)
    /// or later, through [`Link::share_reply`] on any thread.
    fn request(&self, conn: ConnId, request: Vec<u8>) -> Option<Vec<u8>>;
    /// The connection went away.
    fn close(&self, conn: ConnId);
}

/// A connection the link thread owns, named for other threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnId(SocketHandle);

/// What the loop's clients hand it; each is a wake and a short command.
enum Command {
    /// A published port accepted a Mac connection: carry it in.
    Inbound(u16, TcpStream),
    /// A host-side fd to join to a guest port (the Docker socket, control).
    Proxy(u16, OwnedFd),
    DnsReply(SocketHandle, u16, Vec<u8>),
    ShareReply(SocketHandle, Vec<u8>),
}

pub struct Hooks {
    /// The guest offered `spare_mib` beyond what it needs, or asked for it
    /// all back (`release`).
    pub memory: Arc<dyn Fn(u64, bool) + Send + Sync>,
    pub shares: Option<Arc<dyn ShareTransport>>,
}

pub struct Link {
    commands: Mutex<Vec<Command>>,
    wake_fd: RawFd,
}

impl Link {
    /// Takes the host end of the card and starts the thread.
    pub fn start(card: OwnedFd, mtu: u16, network: Network, hooks: Hooks) -> io::Result<Arc<Link>> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: a pipe into an array of two.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let (read_end, write_end) = (fds[0], fds[1]);
        set_nonblocking(read_end)?;
        set_nonblocking(write_end)?;
        // The card stays blocking: reads use MSG_DONTWAIT, and a send that
        // finds the machine's end full waits for it rather than dropping
        // the frame (dropped frames were TCP retransmits, and a stream at
        // under a gigabit).
        let link = Arc::new(Link {
            commands: Mutex::new(Vec::new()),
            wake_fd: write_end,
        });
        let looped = link.clone();
        std::thread::Builder::new()
            .name("link".into())
            .spawn(move || run(looped, read_end, card, mtu, network, hooks))?;
        Ok(link)
    }

    pub fn carry_inbound(&self, port: u16, mac: TcpStream) {
        self.push(Command::Inbound(port, mac));
    }

    /// Joins `fd` (a Mac socket we accepted) to a connection to `guest_port`.
    pub fn proxy(&self, guest_port: u16, fd: OwnedFd) {
        self.push(Command::Proxy(guest_port, fd));
    }

    pub fn share_reply(&self, conn: ConnId, reply: Vec<u8>) {
        self.push(Command::ShareReply(conn.0, reply));
    }

    fn dns_reply(&self, handle: SocketHandle, id: u16, reply: Vec<u8>) {
        self.push(Command::DnsReply(handle, id, reply));
    }

    fn push(&self, command: Command) {
        self.commands
            .lock()
            .expect("link commands poisoned")
            .push(command);
        self.wake();
    }

    fn wake(&self) {
        // SAFETY: a one-byte write on a pipe we own; a full pipe is a wake
        // already pending.
        unsafe { libc::write(self.wake_fd, [1u8].as_ptr().cast(), 1) };
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor the caller owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The card as a smoltcp device.

struct Card {
    fd: RawFd,
    mtu: usize,
    rx: VecDeque<Vec<u8>>,
    tx: Vec<u8>,
    sent: u64,
}

struct Rx(Vec<u8>);

struct Tx<'a> {
    fd: RawFd,
    buf: &'a mut Vec<u8>,
    sent: &'a mut u64,
}

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.buf.clear();
        self.buf.resize(len, 0);
        let r = f(self.buf);
        loop {
            // SAFETY: a datagram send of a buffer we own. A full socket
            // blocks briefly rather than dropping: the framework drains it
            // quickly.
            let n = unsafe { libc::send(self.fd, self.buf.as_ptr().cast(), len, 0) };
            if n >= 0 {
                *self.sent += 1;
                break;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                tracing::debug!(%e, "card: a frame could not be sent");
                break;
            }
        }
        r
    }
}

impl Device for Card {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _t: SInstant) -> Option<(Rx, Tx<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((
            Rx(frame),
            Tx {
                fd: self.fd,
                buf: &mut self.tx,
                sent: &mut self.sent,
            },
        ))
    }

    fn transmit(&mut self, _t: SInstant) -> Option<Tx<'_>> {
        Some(Tx {
            fd: self.fd,
            buf: &mut self.tx,
            sent: &mut self.sent,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        use smoltcp::phy::Checksum;
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = self.mtu + 14;
        c.max_burst_size = Some(64);
        // Checksums are computed on the way out, because the guest's driver
        // verifies them (the device negotiates no offload), and not checked
        // on the way in: the link is a memory pipe, and verifying every
        // 64 KiB segment was the top of the profile.
        c.checksum.ipv4 = Checksum::Tx;
        c.checksum.tcp = Checksum::Tx;
        c.checksum.udp = Checksum::Tx;
        c.checksum.icmpv4 = Checksum::Tx;
        c
    }
}

// ---------------------------------------------------------------------------
// Connections.

enum Phase {
    /// A stream from the guest: the nineteen-byte destination comes first.
    AwaitHeader,
    /// The Mac side is connecting.
    Connecting,
    /// A published port's connection: once the guest accepts, it is told
    /// which port.
    AwaitEstablished(u16),
    /// A host-side fd joined to a guest port: open once established.
    AwaitProxied,
    Open,
    Dns,
    UdpMux,
    Memory,
    ShareHello,
    Share,
    /// Nothing more to say to this one; waiting for the socket to close.
    Draining,
}

struct Conn {
    phase: Phase,
    fd: Option<OwnedFd>,
    reading: bool,
    writing: bool,
    mac_eof: bool,
    guest_eof: bool,
    partial: Vec<u8>,
    /// Bytes to the guest that did not fit the ring yet (the small control
    /// messages; bulk data reads straight into the ring).
    pending: Vec<u8>,
    pending_at: usize,
}

impl Conn {
    fn new(phase: Phase) -> Conn {
        Conn {
            phase,
            fd: None,
            reading: false,
            writing: false,
            mac_eof: false,
            guest_eof: false,
            partial: Vec::new(),
            pending: Vec::new(),
            pending_at: 0,
        }
    }
}

#[derive(Default)]
struct Counters {
    iters: u64,
    frames_in: u64,
    wakes: u64,
    events: u64,
    dropped: u64,
}

struct Kq(RawFd);

impl Kq {
    fn new() -> io::Result<Kq> {
        // SAFETY: kqueue() takes nothing.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Kq(fd))
    }

    fn set(&self, fd: RawFd, filter: i16, flags: u16) {
        let ev = libc::kevent {
            ident: fd as usize,
            filter,
            flags,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: a registration with one change and no event buffer.
        unsafe { libc::kevent(self.0, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }

    fn read(&self, fd: RawFd, on: bool) {
        self.set(
            fd,
            libc::EVFILT_READ,
            if on {
                libc::EV_ADD | libc::EV_ENABLE
            } else {
                libc::EV_DISABLE
            },
        );
    }

    fn write(&self, fd: RawFd, on: bool) {
        self.set(
            fd,
            libc::EVFILT_WRITE,
            if on {
                libc::EV_ADD | libc::EV_ENABLE
            } else {
                libc::EV_DISABLE
            },
        );
    }

    fn forget(&self, fd: RawFd) {
        self.set(fd, libc::EVFILT_READ, libc::EV_DELETE);
        self.set(fd, libc::EVFILT_WRITE, libc::EV_DELETE);
    }
}

struct Loop {
    link: Arc<Link>,
    card: Card,
    iface: Interface,
    sockets: SocketSet<'static>,
    conns: HashMap<SocketHandle, Conn>,
    by_fd: HashMap<RawFd, SocketHandle>,
    listeners: HashMap<u16, Vec<SocketHandle>>,
    /// Aborted sockets kept for one more poll, so the reset goes out.
    dying: Vec<SocketHandle>,
    spare: Vec<tcp::Socket<'static>>,
    kq: Kq,
    network: Network,
    hooks: Hooks,
    udp_flows: HashMap<SocketHandle, HashMap<u32, std::net::UdpSocket>>,
    udp_by_fd: HashMap<RawFd, (SocketHandle, u32)>,
    next_local_port: u16,
    started: Instant,
    counters: Counters,
    frame: Vec<u8>,
}

fn run(link: Arc<Link>, wake_read: RawFd, card: OwnedFd, mtu: u16, network: Network, hooks: Hooks) {
    crate::qos::raise_interactive();
    let kq = match Kq::new() {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(%e, "link: no kqueue");
            return;
        }
    };
    kq.read(wake_read, true);
    kq.read(card.as_raw_fd(), true);
    // Echo replies from the world come back on the responder's ICMP socket
    // and go to the guest as frames.
    let icmp_fd = network.icmp_fd();
    if let Some(fd) = icmp_fd {
        let _ = set_nonblocking(fd);
        kq.read(fd, true);
    }
    let started = Instant::now();
    let card_fd = card.into_raw_fd();
    let mut device = Card {
        fd: card_fd,
        mtu: usize::from(mtu),
        rx: VecDeque::new(),
        tx: Vec::with_capacity(usize::from(mtu) + 14),
        sent: 0,
    };
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
    config.random_seed = started.elapsed().as_nanos() as u64 ^ 0x006c_6967_6874_6572;
    let mut iface = Interface::new(
        config,
        &mut device,
        SInstant::from_micros(started.elapsed().as_micros() as i64),
    );
    iface.update_ip_addrs(|addrs| {
        // The gateway, and the alias the guest reaches the Mac itself by:
        // both answer ARP and echo here, so `ping host.docker.internal`
        // works without a responder rule for it.
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(GATEWAY), 24));
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(HOST_ALIAS), 24));
    });
    let mut l = Loop {
        link,
        card: device,
        iface,
        sockets: SocketSet::new(vec![]),
        conns: HashMap::new(),
        by_fd: HashMap::new(),
        listeners: HashMap::new(),
        dying: Vec::new(),
        spare: Vec::new(),
        kq,
        network,
        hooks,
        udp_flows: HashMap::new(),
        udp_by_fd: HashMap::new(),
        next_local_port: 40000,
        started,
        counters: Counters::default(),
        frame: vec![0u8; usize::from(mtu) + 64],
    };
    l.ensure_listeners();

    let mut events: Vec<libc::kevent> = Vec::with_capacity(128);
    let spin = std::env::var("LIGHTER_REACTOR_SPIN_US")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_micros)
        .unwrap_or(Duration::from_micros(50));
    let zero = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let mut spinning = false;
    let mut last_activity = Instant::now();
    let trace = std::env::var("LIGHTER_LINK_TRACE").is_ok_and(|v| v == "1");
    let mut last_trace = Instant::now();
    loop {
        // How long may we sleep: until smoltcp's next timer, unless spinning.
        let now = l.now();
        let delay = l.iface.poll_delay(now, &l.sockets);
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let timeout: *const libc::timespec = if spinning {
            &zero
        } else if let Some(d) = delay {
            let micros = d.total_micros().min(1_000_000);
            ts.tv_sec = (micros / 1_000_000) as libc::time_t;
            ts.tv_nsec = ((micros % 1_000_000) * 1000) as libc::c_long;
            &ts
        } else if trace {
            ts.tv_nsec = 10_000_000;
            &ts
        } else {
            std::ptr::null()
        };
        // SAFETY: kevent into a buffer of the given capacity.
        let n = unsafe {
            libc::kevent(
                l.kq.0,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.capacity() as libc::c_int,
                timeout,
            )
        };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            tracing::error!("link: kevent failed");
            return;
        }
        // SAFETY: kevent wrote `n` entries.
        unsafe { events.set_len(n as usize) };
        l.counters.iters += 1;
        l.counters.events += n as u64;
        let mut woken = false;
        let mut card_readable = false;
        for ev in &events {
            let fd = ev.ident as RawFd;
            if fd == wake_read {
                woken = true;
                continue;
            }
            if fd == card_fd {
                card_readable = true;
                continue;
            }
            if Some(fd) == icmp_fd {
                l.icmp_readable(fd);
                continue;
            }
            if let Some(&(h, flow)) = l.udp_by_fd.get(&fd) {
                l.udp_readable(h, flow);
                continue;
            }
            let Some(&h) = l.by_fd.get(&fd) else { continue };
            if ev.filter == libc::EVFILT_READ {
                l.readable(h);
            } else if ev.filter == libc::EVFILT_WRITE {
                l.writable(h);
            }
        }
        if woken {
            let mut sink = [0u8; 256];
            // SAFETY: draining a non-blocking pipe into a buffer we own.
            while unsafe { libc::read(wake_read, sink.as_mut_ptr().cast(), sink.len()) } > 0 {}
            l.counters.wakes += 1;
            let commands: Vec<Command> =
                std::mem::take(&mut *l.link.commands.lock().expect("link commands poisoned"));
            for command in commands {
                l.command(command);
            }
        }
        // Frames from the guest, then the stack, then every socket that
        // may have moved.
        let frames = l.drain_card(spinning || card_readable);
        let now = l.now();
        l.iface.poll(now, &mut l.card, &mut l.sockets);
        l.promote_listeners();
        let handles: Vec<SocketHandle> = l.conns.keys().copied().collect();
        for h in handles {
            l.progress(h);
        }
        // Whatever the sockets did may have produced frames; send them now
        // rather than on the next wake.
        let now = l.now();
        l.iface.poll(now, &mut l.card, &mut l.sockets);
        // That poll sent the resets of anything aborted this turn.
        let dying = std::mem::take(&mut l.dying);
        for h in dying {
            l.recycle(h);
        }
        l.ensure_listeners();

        if trace && last_trace.elapsed() >= Duration::from_secs(1) {
            last_trace = Instant::now();
            l.trace();
        }
        if spin.is_zero() {
            continue;
        }
        if n > 0 || woken || frames > 0 {
            last_activity = Instant::now();
            spinning = true;
        } else if spinning && last_activity.elapsed() >= spin {
            spinning = false;
        } else if spinning {
            std::hint::spin_loop();
        }
    }
}

impl Loop {
    fn now(&self) -> SInstant {
        SInstant::from_micros(self.started.elapsed().as_micros() as i64)
    }

    fn trace(&self) {
        for (h, conn) in &self.conns {
            let s = self.sockets.get::<tcp::Socket>(*h);
            if matches!(conn.phase, Phase::Dns | Phase::UdpMux | Phase::Memory) {
                continue;
            }
            eprintln!(
                "LINK conn {:?} phase={} state={} rx={} tx={} fd={:?} reading={} writing={} mac_eof={} guest_eof={} pending={}",
                h,
                match conn.phase {
                    Phase::AwaitHeader => "await-header",
                    Phase::Connecting => "connecting",
                    Phase::AwaitEstablished(_) => "await-established",
                    Phase::AwaitProxied => "await-proxied",
                    Phase::Open => "open",
                    Phase::Dns => "dns",
                    Phase::UdpMux => "udp",
                    Phase::Memory => "memory",
                    Phase::ShareHello => "share-hello",
                    Phase::Share => "share",
                    Phase::Draining => "draining",
                },
                s.state(),
                s.recv_queue(),
                s.send_queue(),
                conn.fd.as_ref().map(AsRawFd::as_raw_fd),
                conn.reading,
                conn.writing,
                conn.mac_eof,
                conn.guest_eof,
                conn.pending.len() - conn.pending_at
            );
        }
        let c = &self.counters;
        eprintln!(
            "LINK iters={} frames_in={} frames_out={} wakes={} events={} conns={} listeners={} spare={} dropped={}",
            c.iters,
            c.frames_in,
            self.card.sent,
            c.wakes,
            c.events,
            self.conns.len(),
            self.listeners.values().map(Vec::len).sum::<usize>(),
            self.spare.len(),
            c.dropped
        );
    }

    /// Reads frames off the card: the responder answers what it answers,
    /// ARP and IP go to the stack, the rest is dropped and counted.
    fn drain_card(&mut self, likely: bool) -> usize {
        if !likely {
            return 0;
        }
        let mut count = 0;
        while count < FRAMES_PER_TURN {
            // SAFETY: a non-blocking recv into a buffer we own.
            let n = unsafe {
                libc::recv(
                    self.card.fd,
                    self.frame.as_mut_ptr().cast(),
                    self.frame.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                break;
            }
            count += 1;
            let frame = &self.frame[..n as usize];
            match crate::net::classify(frame) {
                // DHCP, echo to the gateway or the host alias, and echo to
                // the world: the responder's. ARP and TCP are the stack's.
                Some(Seen::Dhcp) | Some(Seen::IcmpLocal) | Some(Seen::IcmpForward) => {
                    if let Some(reply) = self.network.answer(frame) {
                        // SAFETY: a datagram send of a buffer we own.
                        unsafe { libc::send(self.card.fd, reply.as_ptr().cast(), reply.len(), 0) };
                    }
                }
                _ if frame.len() >= 14
                    && matches!(u16::from_be_bytes([frame[12], frame[13]]), 0x0806 | 0x0800) =>
                {
                    self.card.rx.push_back(frame.to_vec());
                }
                _ => {
                    self.counters.dropped += 1;
                }
            }
        }
        self.counters.frames_in += count as u64;
        count
    }

    /// Echo replies from the Mac's ICMP socket, back to the guest as frames.
    fn icmp_readable(&mut self, fd: RawFd) {
        let mut buf = [0u8; 65_536];
        loop {
            // SAFETY: a non-blocking recv into a buffer we own.
            let n =
                unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
            if n <= 0 {
                break;
            }
            if let Some(frame) = crate::net::echo_reply_frame(&buf[..n as usize]) {
                // SAFETY: a datagram send of a buffer we own.
                unsafe { libc::send(self.card.fd, frame.as_ptr().cast(), frame.len(), 0) };
            }
        }
    }

    fn new_socket(&mut self) -> tcp::Socket<'static> {
        if let Some(s) = self.spare.pop() {
            return s;
        }
        let (rx, tx) = ring_bytes();
        let mut s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; rx]),
            tcp::SocketBuffer::new(vec![0u8; tx]),
        );
        s.set_nagle_enabled(false);
        s.set_ack_delay(None);
        s.set_congestion_control(tcp::CongestionControl::Cubic);
        s
    }

    fn recycle(&mut self, handle: SocketHandle) {
        let smoltcp::socket::Socket::Tcp(mut s) = self.sockets.remove(handle);
        s.abort();
        if self.spare.len() < 64 {
            self.spare.push(s);
        }
    }

    /// Keeps a backlog of listening sockets on every port the guest dials.
    fn ensure_listeners(&mut self) {
        for (port, backlog) in [
            (STREAM_PORT, STREAM_BACKLOG),
            (DNS_PORT, SMALL_BACKLOG),
            (UDP_PORT, SMALL_BACKLOG),
            (MEMORY_PORT, SMALL_BACKLOG),
            (SHARE_PORT, 8),
        ] {
            let have = self.listeners.get(&port).map_or(0, Vec::len);
            for _ in have..backlog {
                let mut s = self.new_socket();
                if s.listen(port).is_err() {
                    self.spare.push(s);
                    break;
                }
                let h = self.sockets.add(s);
                self.listeners.entry(port).or_default().push(h);
            }
        }
    }

    /// A listener that saw a SYN is a connection now.
    fn promote_listeners(&mut self) {
        let mut promoted: Vec<(u16, SocketHandle)> = Vec::new();
        for (port, handles) in self.listeners.iter_mut() {
            handles.retain(|&h| {
                let s = self.sockets.get::<tcp::Socket>(h);
                if s.is_listening() {
                    true
                } else {
                    promoted.push((*port, h));
                    false
                }
            });
        }
        // Each promoted listener is replaced at once, so a burst of SYNs
        // within one turn does not run the backlog dry and get resets.
        let promoted_count = promoted.len();
        for (port, h) in promoted {
            let phase = match port {
                STREAM_PORT => Phase::AwaitHeader,
                DNS_PORT => Phase::Dns,
                UDP_PORT => Phase::UdpMux,
                MEMORY_PORT => Phase::Memory,
                SHARE_PORT => Phase::ShareHello,
                _ => Phase::Draining,
            };
            if matches!(phase, Phase::UdpMux) {
                self.udp_flows.insert(h, HashMap::new());
            }
            self.conns.insert(h, Conn::new(phase));
        }
        if promoted_count > 0 {
            self.ensure_listeners();
        }
    }

    fn local_port(&mut self) -> u16 {
        let p = self.next_local_port;
        self.next_local_port = if p >= 65000 { 40000 } else { p + 1 };
        p
    }

    /// Opens a connection to the guest, with `fd` as its Mac side.
    fn open_to_guest(&mut self, guest_port: u16, fd: OwnedFd, phase: Phase) {
        if set_nonblocking(fd.as_raw_fd()).is_err() {
            return;
        }
        let mut s = self.new_socket();
        let remote = IpEndpoint::new(IpAddress::Ipv4(GUEST), guest_port);
        let mut connected = false;
        for _ in 0..8 {
            let local = self.local_port();
            if s.connect(self.iface.context(), remote, local).is_ok() {
                connected = true;
                break;
            }
        }
        if !connected {
            tracing::debug!(
                guest_port,
                "link: no local port for a connection to the guest"
            );
            self.spare.push(s);
            return;
        }
        let h = self.sockets.add(s);
        let raw = fd.as_raw_fd();
        self.by_fd.insert(raw, h);
        let mut conn = Conn::new(phase);
        conn.fd = Some(fd);
        self.conns.insert(h, conn);
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Inbound(port, mac) => {
                let _ = mac.set_nodelay(true);
                crate::sockbuf::widen(&mac);
                self.open_to_guest(
                    INBOUND_PORT,
                    OwnedFd::from(mac),
                    Phase::AwaitEstablished(port),
                );
            }
            Command::Proxy(guest_port, fd) => {
                crate::sockbuf::widen(&fd);
                self.open_to_guest(guest_port, fd, Phase::AwaitProxied);
            }
            Command::DnsReply(h, id, reply) => {
                if self.conns.contains_key(&h) {
                    let mut frame = Vec::with_capacity(4 + reply.len());
                    frame.extend_from_slice(&(reply.len() as u16).to_be_bytes());
                    frame.extend_from_slice(&id.to_be_bytes());
                    frame.extend_from_slice(&reply);
                    self.queue_to_guest(h, frame);
                }
            }
            Command::ShareReply(h, reply) => {
                if self.conns.contains_key(&h) {
                    self.queue_to_guest(h, reply);
                }
            }
        }
    }

    /// Small control messages to the guest: into the ring if it fits, else
    /// kept until it does.
    fn queue_to_guest(&mut self, h: SocketHandle, bytes: Vec<u8>) {
        let Some(conn) = self.conns.get_mut(&h) else {
            return;
        };
        if conn.pending_at < conn.pending.len() {
            conn.pending.extend_from_slice(&bytes);
            return;
        }
        let s = self.sockets.get_mut::<tcp::Socket>(h);
        let sent = s.send_slice(&bytes).unwrap_or(0);
        if sent < bytes.len() {
            conn.pending = bytes;
            conn.pending_at = sent;
        }
    }

    fn flush_pending(&mut self, h: SocketHandle) {
        let Some(conn) = self.conns.get_mut(&h) else {
            return;
        };
        if conn.pending_at >= conn.pending.len() {
            return;
        }
        let s = self.sockets.get_mut::<tcp::Socket>(h);
        let sent = s.send_slice(&conn.pending[conn.pending_at..]).unwrap_or(0);
        conn.pending_at += sent;
        if conn.pending_at >= conn.pending.len() {
            conn.pending.clear();
            conn.pending_at = 0;
        }
    }

    /// Everything the guest has sent on `h` since last time, appended to the
    /// connection's partial buffer (control channels frame from there).
    fn take_from_guest(&mut self, h: SocketHandle) -> bool {
        let Some(conn) = self.conns.get_mut(&h) else {
            return false;
        };
        let s = self.sockets.get_mut::<tcp::Socket>(h);
        let mut got = false;
        while s.can_recv() {
            let r = s.recv(|buf| {
                conn.partial.extend_from_slice(buf);
                (buf.len(), buf.len())
            });
            match r {
                Ok(0) | Err(_) => break,
                Ok(_) => got = true,
            }
        }
        if peer_closed(s) {
            conn.guest_eof = true;
        }
        got
    }

    fn progress(&mut self, h: SocketHandle) {
        let Some(conn) = self.conns.get(&h) else {
            return;
        };
        // A socket that reset or finished under us.
        let closed = !self.sockets.get::<tcp::Socket>(h).is_open();
        match conn.phase {
            Phase::AwaitHeader => {
                self.take_from_guest(h);
                let conn = self.conns.get_mut(&h).expect("present");
                if conn.partial.len() >= HEADER_LEN {
                    let header: Vec<u8> = conn.partial.drain(..HEADER_LEN).collect();
                    let Some(addr) = destination(&header) else {
                        self.close(h);
                        return;
                    };
                    match connect_nonblocking(addr) {
                        Ok(tcp) => {
                            let fd = tcp.as_raw_fd();
                            self.by_fd.insert(fd, h);
                            let conn = self.conns.get_mut(&h).expect("present");
                            conn.fd = Some(OwnedFd::from(tcp));
                            conn.phase = Phase::Connecting;
                            conn.writing = true;
                            self.kq.write(fd, true);
                        }
                        Err(e) => {
                            tracing::debug!(%addr, %e, "stream: connect failed");
                            self.close(h);
                        }
                    }
                } else if closed || conn.guest_eof {
                    self.close(h);
                }
            }
            Phase::Connecting => {
                if closed {
                    self.close(h);
                }
            }
            Phase::AwaitEstablished(port) => {
                let s = self.sockets.get_mut::<tcp::Socket>(h);
                if s.state() == tcp::State::Established {
                    if s.send_slice(&port.to_be_bytes()).unwrap_or(0) == 2 {
                        let conn = self.conns.get_mut(&h).expect("present");
                        conn.phase = Phase::Open;
                        let fd = conn.fd.as_ref().expect("socket").as_raw_fd();
                        conn.reading = true;
                        self.kq.read(fd, true);
                    }
                } else if closed {
                    tracing::debug!(port, "the agent did not accept an inbound stream");
                    self.close(h);
                }
            }
            Phase::AwaitProxied => {
                let s = self.sockets.get_mut::<tcp::Socket>(h);
                if s.state() == tcp::State::Established {
                    let conn = self.conns.get_mut(&h).expect("present");
                    conn.phase = Phase::Open;
                    let fd = conn.fd.as_ref().expect("socket").as_raw_fd();
                    conn.reading = true;
                    self.kq.read(fd, true);
                } else if closed {
                    tracing::debug!("the guest did not accept; is the agent running?");
                    self.close(h);
                }
            }
            Phase::Open => {
                self.pull_from_guest(h);
                self.resume_reading(h);
                self.maybe_close(h);
            }
            Phase::Dns => {
                self.flush_pending(h);
                if self.take_from_guest(h) {
                    self.dns_progress(h);
                }
                if closed || self.conns.get(&h).is_some_and(|c| c.guest_eof) {
                    self.close(h);
                }
            }
            Phase::UdpMux => {
                self.flush_pending(h);
                if self.take_from_guest(h) {
                    self.udp_progress(h);
                }
                if closed || self.conns.get(&h).is_some_and(|c| c.guest_eof) {
                    self.close(h);
                }
            }
            Phase::Memory => {
                if self.take_from_guest(h) {
                    let conn = self.conns.get_mut(&h).expect("present");
                    while conn.partial.len() >= 16 {
                        let bytes: Vec<u8> = conn.partial.drain(..16).collect();
                        let word = |i: usize| {
                            u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
                        };
                        (self.hooks.memory)(u64::from(word(0)), word(12) != 0);
                    }
                }
                if closed || self.conns.get(&h).is_some_and(|c| c.guest_eof) {
                    (self.hooks.memory)(0, true);
                    self.close(h);
                }
            }
            Phase::ShareHello => {
                self.take_from_guest(h);
                let conn = self.conns.get_mut(&h).expect("present");
                // "lighterfs <tag>\n"
                if let Some(nl) = conn.partial.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = conn.partial.drain(..=nl).collect();
                    let text = String::from_utf8_lossy(&line[..nl]).to_string();
                    let tag = text.strip_prefix("lighterfs ").unwrap_or("").to_string();
                    let accepted = self
                        .hooks
                        .shares
                        .as_ref()
                        .is_some_and(|shares| shares.open(ConnId(h), &tag));
                    if accepted {
                        conn.phase = Phase::Share;
                        self.share_progress(h);
                    } else {
                        tracing::warn!(%tag, "a share connection for a tag nobody serves");
                        self.close(h);
                    }
                } else if closed || conn.guest_eof {
                    self.close(h);
                }
            }
            Phase::Share => {
                self.flush_pending(h);
                if self.take_from_guest(h) {
                    self.share_progress(h);
                }
                if closed || self.conns.get(&h).is_some_and(|c| c.guest_eof) {
                    if let Some(shares) = &self.hooks.shares {
                        shares.close(ConnId(h));
                    }
                    self.close(h);
                }
            }
            Phase::Draining => {
                if closed {
                    self.close(h);
                }
            }
        }
    }

    /// FUSE requests are framed by the length in their header.
    fn share_progress(&mut self, h: SocketHandle) {
        let Some(shares) = self.hooks.shares.clone() else {
            return;
        };
        let mut requests: Vec<Vec<u8>> = Vec::new();
        {
            let Some(conn) = self.conns.get_mut(&h) else {
                return;
            };
            let mut at = 0usize;
            while conn.partial.len() - at >= 40 {
                let len = u32::from_le_bytes([
                    conn.partial[at],
                    conn.partial[at + 1],
                    conn.partial[at + 2],
                    conn.partial[at + 3],
                ]) as usize;
                if len < 40 {
                    tracing::warn!(
                        len,
                        "a FUSE request shorter than its header; dropping the connection"
                    );
                    self.close(h);
                    return;
                }
                if conn.partial.len() - at < len {
                    break;
                }
                requests.push(conn.partial[at..at + len].to_vec());
                at += len;
            }
            conn.partial.drain(..at);
        }
        for request in requests {
            if let Some(reply) = shares.request(ConnId(h), request) {
                self.queue_to_guest(h, reply);
            }
        }
    }

    /// Guest → Mac: straight from the ring to the socket.
    fn pull_from_guest(&mut self, h: SocketHandle) {
        let Some(conn) = self.conns.get_mut(&h) else {
            return;
        };
        let Some(fd) = conn.fd.as_ref().map(AsRawFd::as_raw_fd) else {
            return;
        };
        let s = self.sockets.get_mut::<tcp::Socket>(h);
        let mut blocked = false;
        let mut failed = false;
        // Whatever arrived in the same segment as the stream header was taken
        // off the ring with it; it goes first (a TLS ClientHello, typically).
        while !conn.partial.is_empty() && !blocked {
            // SAFETY: a write of bytes we own to a socket we own.
            let n = unsafe { libc::write(fd, conn.partial.as_ptr().cast(), conn.partial.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted {
                    blocked = true;
                } else {
                    failed = true;
                }
            } else {
                conn.partial.drain(..n as usize);
            }
            if failed {
                break;
            }
        }
        while s.can_recv() && !blocked && !failed {
            let r = s.recv(|buf| {
                // SAFETY: a write of the ring's readable span to a socket we own.
                let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted
                    {
                        (0, Ok(0usize))
                    } else {
                        (0, Err(e))
                    }
                } else {
                    (n as usize, Ok(n as usize))
                }
            });
            match r {
                Ok(Ok(0)) => blocked = true,
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            self.close(h);
            return;
        }
        if blocked && !conn.writing {
            conn.writing = true;
            self.kq.write(fd, true);
        } else if !blocked && conn.writing {
            conn.writing = false;
            self.kq.write(fd, false);
        }
        if peer_closed(s) && !conn.guest_eof && !s.can_recv() && conn.partial.is_empty() {
            // The guest is done talking; tell the Mac side once the ring is
            // empty.
            conn.guest_eof = true;
            // SAFETY: shutdown on a socket we own.
            unsafe { libc::shutdown(fd, libc::SHUT_WR) };
        }
    }

    /// Mac → guest: straight from the socket into the ring.
    fn readable(&mut self, h: SocketHandle) {
        let Some(conn) = self.conns.get_mut(&h) else {
            return;
        };
        if !matches!(conn.phase, Phase::Open) {
            return;
        }
        let Some(fd) = conn.fd.as_ref().map(AsRawFd::as_raw_fd) else {
            return;
        };
        let s = self.sockets.get_mut::<tcp::Socket>(h);
        loop {
            if !s.can_send() {
                // No room: stop listening for the Mac until the guest drains.
                if conn.reading {
                    conn.reading = false;
                    self.kq.read(fd, false);
                }
                return;
            }
            let r = s.send(|buf| {
                let want = buf.len().min(READ_CHUNK);
                // SAFETY: a read into the ring's writable span.
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), want) };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted
                    {
                        (0, Ok(None))
                    } else {
                        (0, Err(e))
                    }
                } else if n == 0 {
                    (0, Ok(Some(0usize)))
                } else {
                    (n as usize, Ok(Some(n as usize)))
                }
            });
            match r {
                Ok(Ok(None)) => return,
                Ok(Ok(Some(0))) => {
                    conn.mac_eof = true;
                    conn.reading = false;
                    self.kq.read(fd, false);
                    s.close();
                    return;
                }
                Ok(Ok(Some(_))) => {}
                Ok(Err(_)) | Err(_) => {
                    self.close(h);
                    return;
                }
            }
        }
    }

    /// The guest drained some of the ring: read from the Mac again.
    fn resume_reading(&mut self, h: SocketHandle) {
        let Some(conn) = self.conns.get_mut(&h) else {
            return;
        };
        if conn.reading || conn.mac_eof {
            return;
        }
        let Some(fd) = conn.fd.as_ref().map(AsRawFd::as_raw_fd) else {
            return;
        };
        let s = self.sockets.get::<tcp::Socket>(h);
        if s.can_send() {
            conn.reading = true;
            self.kq.read(fd, true);
            self.readable(h);
        }
    }

    fn writable(&mut self, h: SocketHandle) {
        let Some(conn) = self.conns.get_mut(&h) else {
            return;
        };
        if matches!(conn.phase, Phase::Connecting) {
            let fd = conn.fd.as_ref().expect("socket").as_raw_fd();
            match connect_result(fd) {
                Ok(()) => {
                    conn.phase = Phase::Open;
                    conn.writing = false;
                    self.kq.write(fd, false);
                    conn.reading = true;
                    self.kq.read(fd, true);
                    self.pull_from_guest(h);
                    self.readable(h);
                }
                Err(e) => {
                    tracing::debug!(%e, "stream: connect failed");
                    self.close(h);
                }
            }
            return;
        }
        self.pull_from_guest(h);
    }

    fn maybe_close(&mut self, h: SocketHandle) {
        let Some(conn) = self.conns.get(&h) else {
            return;
        };
        let s = self.sockets.get::<tcp::Socket>(h);
        let ring_empty = !s.can_recv();
        let finished = !s.is_open()
            || (conn.mac_eof && conn.guest_eof && s.state() != tcp::State::Established);
        if finished && ring_empty {
            self.close(h);
        }
    }

    fn close(&mut self, h: SocketHandle) {
        if let Some(flows) = self.udp_flows.remove(&h) {
            for (_, socket) in flows {
                let fd = socket.as_raw_fd();
                self.kq.forget(fd);
                self.udp_by_fd.remove(&fd);
            }
        }
        if let Some(conn) = self.conns.remove(&h) {
            if matches!(conn.phase, Phase::Share)
                && let Some(shares) = &self.hooks.shares
            {
                shares.close(ConnId(h));
            }
            if let Some(fd) = conn.fd {
                let raw = fd.as_raw_fd();
                self.kq.forget(raw);
                self.by_fd.remove(&raw);
                // SAFETY: shutdown before the descriptor is dropped.
                unsafe { libc::shutdown(raw, libc::SHUT_RDWR) };
            }
        }
        let s = self.sockets.get_mut::<tcp::Socket>(h);
        if s.is_open() {
            // A stream ended from our side before the guest was done (a
            // refused connect, a Mac socket error): a reset, which the guest
            // reports as a refusal at once rather than an EOF after a wait.
            s.abort();
            self.dying.push(h);
        } else {
            self.recycle(h);
        }
    }

    // -- DNS ---------------------------------------------------------------

    fn dns_progress(&mut self, h: SocketHandle) {
        let Some(conn) = self.conns.get_mut(&h) else {
            return;
        };
        let mut at = 0usize;
        let mut replies: Vec<(u16, Vec<u8>)> = Vec::new();
        while conn.partial.len() - at >= 4 {
            let len = u16::from_be_bytes([conn.partial[at], conn.partial[at + 1]]) as usize;
            let id = u16::from_be_bytes([conn.partial[at + 2], conn.partial[at + 3]]);
            if conn.partial.len() - at - 4 < len {
                break;
            }
            let query = conn.partial[at + 4..at + 4 + len].to_vec();
            at += 4 + len;
            let link = self.link.clone();
            let deliver: Arc<dyn Fn(u16, Vec<u8>) + Send + Sync> =
                Arc::new(move |id, reply| link.dns_reply(h, id, reply));
            if let Some(reply) = crate::dns::answer(query, id, deliver) {
                replies.push((id, reply));
            }
        }
        conn.partial.drain(..at);
        for (id, reply) in replies {
            self.command(Command::DnsReply(h, id, reply));
        }
    }

    // -- UDP ---------------------------------------------------------------

    fn udp_progress(&mut self, h: SocketHandle) {
        let Some(conn) = self.conns.get_mut(&h) else {
            return;
        };
        let mut at = 0usize;
        while conn.partial.len() - at >= UDP_HEADER {
            let hdr = &conn.partial[at..at + UDP_HEADER];
            let len = u16::from_be_bytes([hdr[0], hdr[1]]) as usize;
            let flow = u32::from_be_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]);
            let kind = hdr[6];
            if conn.partial.len() - at - UDP_HEADER < len {
                break;
            }
            let payload = &conn.partial[at + UDP_HEADER..at + UDP_HEADER + len];
            at += UDP_HEADER + len;
            let flows = self.udp_flows.entry(h).or_default();
            match kind {
                UDP_KIND_OPEN => {
                    let Some(dst) = udp_destination(payload) else {
                        continue;
                    };
                    let bind: SocketAddr = if dst.is_ipv4() {
                        "0.0.0.0:0".parse().expect("addr")
                    } else {
                        "[::]:0".parse().expect("addr")
                    };
                    let Ok(socket) = std::net::UdpSocket::bind(bind) else {
                        continue;
                    };
                    if socket.connect(dst).is_err() || socket.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let fd = socket.as_raw_fd();
                    crate::sockbuf::widen(&socket);
                    self.kq.read(fd, true);
                    self.udp_by_fd.insert(fd, (h, flow));
                    if let Some(old) = flows.insert(flow, socket) {
                        let ofd = old.as_raw_fd();
                        self.kq.forget(ofd);
                        self.udp_by_fd.remove(&ofd);
                    }
                }
                UDP_KIND_DATA => {
                    if let Some(socket) = flows.get(&flow) {
                        let _ = socket.send(payload);
                    }
                }
                UDP_KIND_CLOSE => {
                    if let Some(socket) = flows.remove(&flow) {
                        let fd = socket.as_raw_fd();
                        self.kq.forget(fd);
                        self.udp_by_fd.remove(&fd);
                    }
                }
                _ => {}
            }
        }
        conn.partial.drain(..at);
    }

    fn udp_readable(&mut self, h: SocketHandle, flow: u32) {
        let Some(socket) = self.udp_flows.get(&h).and_then(|f| f.get(&flow)) else {
            return;
        };
        let mut batch: Vec<u8> = Vec::with_capacity(64 * 1500);
        let mut buf = [0u8; 65536];
        for _ in 0..64 {
            match socket.recv(&mut buf) {
                Ok(n) => {
                    batch.extend_from_slice(&(n as u16).to_be_bytes());
                    batch.extend_from_slice(&flow.to_be_bytes());
                    batch.push(UDP_KIND_DATA);
                    batch.extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
        if !batch.is_empty() {
            // Datagrams: what does not fit is dropped, as a network would.
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            let _ = s.send_slice(&batch);
        }
    }
}

// ---------------------------------------------------------------------------

/// Whether the far end has sent its FIN (or the socket is gone). `may_recv`
/// alone is false while a connection is still being set up, which is not
/// the same thing.
fn peer_closed(s: &tcp::Socket) -> bool {
    matches!(
        s.state(),
        tcp::State::CloseWait
            | tcp::State::LastAck
            | tcp::State::Closing
            | tcp::State::TimeWait
            | tcp::State::Closed
    )
}

fn udp_destination(payload: &[u8]) -> Option<SocketAddr> {
    if payload.len() < HEADER_LEN {
        return None;
    }
    let port = u16::from_be_bytes([payload[17], payload[18]]);
    match payload[0] {
        4 => Some(SocketAddr::new(
            std::net::Ipv4Addr::new(payload[1], payload[2], payload[3], payload[4]).into(),
            port,
        )),
        6 => {
            let mut b = [0u8; 16];
            b.copy_from_slice(&payload[1..17]);
            Some(SocketAddr::new(std::net::Ipv6Addr::from(b).into(), port))
        }
        _ => None,
    }
}

/// Where a stream's header says it is going; the gateway and the host alias
/// are the Mac itself.
pub fn destination(header: &[u8]) -> Option<SocketAddr> {
    if header.len() < HEADER_LEN {
        return None;
    }
    let port = u16::from_be_bytes([header[17], header[18]]);
    let ip: std::net::IpAddr = match header[0] {
        4 => {
            let v4 = std::net::Ipv4Addr::new(header[1], header[2], header[3], header[4]);
            if v4 == GATEWAY || v4 == HOST_ALIAS {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                v4.into()
            }
        }
        6 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&header[1..17]);
            std::net::Ipv6Addr::from(octets).into()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

fn connect_nonblocking(addr: SocketAddr) -> io::Result<TcpStream> {
    let (family, sockaddr, len): (libc::c_int, Vec<u8>, libc::socklen_t) = match addr {
        SocketAddr::V4(a) => {
            // SAFETY: a zeroed sockaddr_in, filled below.
            let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sin.sin_len = size_of::<libc::sockaddr_in>() as u8;
            sin.sin_family = libc::AF_INET as u8;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
            // SAFETY: viewing the struct as bytes.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!(sin).cast::<u8>(),
                    size_of::<libc::sockaddr_in>(),
                )
            }
            .to_vec();
            (
                libc::AF_INET,
                bytes,
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(a) => {
            // SAFETY: a zeroed sockaddr_in6, filled below.
            let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sin6.sin6_len = size_of::<libc::sockaddr_in6>() as u8;
            sin6.sin6_family = libc::AF_INET6 as u8;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_addr.s6_addr = a.ip().octets();
            // SAFETY: viewing the struct as bytes.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!(sin6).cast::<u8>(),
                    size_of::<libc::sockaddr_in6>(),
                )
            }
            .to_vec();
            (
                libc::AF_INET6,
                bytes,
                size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    };
    // SAFETY: socket creation and a non-blocking connect on it.
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { TcpStream::from_raw_fd(fd) };
    set_nonblocking(fd)?;
    let _ = stream.set_nodelay(true);
    crate::sockbuf::widen(&stream);
    let rc = unsafe { libc::connect(fd, sockaddr.as_ptr().cast(), len) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
    }
    Ok(stream)
}

fn connect_result(fd: RawFd) -> io::Result<()> {
    let mut err: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt into an int we own.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            std::ptr::addr_of_mut!(err).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if err != 0 {
        return Err(io::Error::from_raw_os_error(err));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_v4_header_names_its_address_and_port() {
        let mut header = [0u8; HEADER_LEN];
        header[0] = 4;
        header[1..5].copy_from_slice(&[93, 184, 216, 34]);
        header[17..19].copy_from_slice(&443u16.to_be_bytes());
        assert_eq!(
            destination(&header),
            Some("93.184.216.34:443".parse().unwrap())
        );
    }

    #[test]
    fn the_gateway_and_the_host_alias_are_the_mac() {
        for ip in [GATEWAY, HOST_ALIAS] {
            let mut header = [0u8; HEADER_LEN];
            header[0] = 4;
            header[1..5].copy_from_slice(&ip.octets());
            header[17..19].copy_from_slice(&8080u16.to_be_bytes());
            assert_eq!(
                destination(&header),
                Some("127.0.0.1:8080".parse().unwrap())
            );
        }
    }

    #[test]
    fn a_v6_header_carries_sixteen_bytes() {
        let mut header = [0u8; HEADER_LEN];
        header[0] = 6;
        header[1..17].copy_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        header[17..19].copy_from_slice(&53u16.to_be_bytes());
        assert_eq!(destination(&header), Some("[::1]:53".parse().unwrap()));
    }

    #[test]
    fn an_unknown_family_is_refused() {
        let header = [9u8; HEADER_LEN];
        assert_eq!(destination(&header), None);
    }

    #[test]
    fn a_nonblocking_connect_to_a_closed_port_reports_the_refusal() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let stream = connect_nonblocking(addr).unwrap();
        let fd = stream.as_raw_fd();
        let mut fds = [libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        }];
        // SAFETY: poll on one descriptor we own.
        unsafe { libc::poll(fds.as_mut_ptr(), 1, 2000) };
        assert!(connect_result(fd).is_err());
    }
}

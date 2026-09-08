//! One thread for every stream, instead of two threads per stream.
//!
//! A stream is a TCP socket on the Mac and a vsock connection to the guest,
//! and bytes go both ways. Two blocking threads per stream did that simply
//! and cost two spawns and a stack each on every connection, which at
//! thousands of connections a second was most of what a connection cost,
//! and a scheduler hop on every hop of every byte. Here a kqueue watches
//! every stream's socket, the vsock side pokes a pipe when anything a stream
//! could be waiting on has changed, and one thread moves bytes for all of
//! them with no wakeup between a byte arriving and its being forwarded.
//!
//! Nothing here blocks: sockets are non-blocking, the vsock API used is the
//! `try_` family, and a direction that cannot make progress waits for the
//! event that lets it. Backpressure is symmetric — a TCP side that cannot
//! be written stops being read on the vsock side and the other way round.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lighter_docker::Published;

use crate::virtio::vsock::{Chunk, ConnKey, Gone, Outbound, Status, VsockShared};

/// Sixteen bytes of address with IPv4 in the first four, a family byte
/// first, the port last: what the guest sends before a stream's bytes.
pub const HEADER_LEN: usize = 19;
/// The vsock port the agent answers inbound streams on.
pub const INBOUND_PORT: u32 = 2378;

/// The card's addresses for the Mac itself, as seen from the guest.
const GATEWAY: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 168, 127, 1);
const HOST_ALIAS: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 168, 127, 254);

/// How much of a TCP socket is read per readiness, into a reused buffer.
const READ_CHUNK: usize = 256 * 1024;

enum Phase {
    /// A DNS stream: framed queries in, framed replies out, no socket.
    Dns,
    /// A guest-opened stream whose header has not all arrived.
    AwaitHeader,
    /// The Mac's socket is connecting; writable means done.
    Connecting,
    /// A host-opened stream (published port) waiting for the guest's accept,
    /// then owed the guest address to dial as its first bytes.
    AwaitEstablished(SocketAddr),
    Open,
    /// The guest's UDP, every flow multiplexed on this one stream; the
    /// flows' sockets are in `Loop::udp_flows`.
    UdpMux,
    /// Published UDP ports, the other way round: every flow from a client
    /// of the Mac to a container, on this one stream to the agent.
    UdpInbound,
}

/// A UDP port Docker published, bound on the Mac: its socket, the address
/// in the guest the agent dials for it, and a flow per client that has
/// spoken, with when it last did.
struct UdpPublish {
    socket: std::net::UdpSocket,
    dst: SocketAddr,
    flows: HashMap<SocketAddr, (u32, Instant)>,
}

/// How long a published port's client may say nothing before its flow is
/// closed; the agent's own sweep of outbound flows uses the same.
const UDP_FLOW_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

struct Stream {
    tcp: Option<TcpStream>,
    phase: Phase,
    /// Bytes read from the socket the guest has not had credit for yet.
    to_guest: Vec<u8>,
    to_guest_at: usize,
    /// Chunks from the guest not yet on the socket, and how far into the
    /// first one the last write got.
    from_guest: VecDeque<Chunk>,
    from_guest_at: usize,
    tcp_eof: bool,
    guest_eof: bool,
    reading: bool,
    writing: bool,
    /// For a DNS stream: bytes of a frame not yet whole.
    partial: Vec<u8>,
}

/// Counters for `LIGHTER_STREAM_TRACE`.
#[derive(Default)]
struct Counters {
    iters: u64,
    wakes: u64,
    events: u64,
    takes_chunks: u64,
    takes_empty: u64,
    writev: u64,
    eagain: u64,
    writable: u64,
}

enum Command {
    Outbound(ConnKey),
    /// A connection accepted on a published port, and the address in the
    /// guest the agent is to dial for it.
    Inbound(SocketAddr, TcpStream),
    Dns(ConnKey),
    /// A DNS reply resolved off-thread, to go out on its stream.
    DnsReply(ConnKey, u16, Vec<u8>),
    /// The guest's UDP stream (see the agent's udp.rs for the framing).
    Udp(ConnKey),
    /// The agent's stream for published UDP ports, same framing, flows
    /// opened from this side.
    UdpInbound(ConnKey),
    /// A UDP port Docker published, bound on the Mac, and where in the
    /// guest the agent dials for it.
    PublishUdp(Published, std::net::UdpSocket, SocketAddr),
    WithdrawUdp(Published),
}

/// The UDP frame: length, flow, kind; then the payload.
const UDP_HEADER: usize = 7;
const UDP_KIND_DATA: u8 = 0;
const UDP_KIND_OPEN: u8 = 1;
const UDP_KIND_CLOSE: u8 = 2;

/// How much a framed stream (DNS replies, the UDP mux) may hold for a guest
/// that has stopped granting credit before new frames are dropped whole.
/// A frame split at the credit boundary and never finished desynchronises
/// the guest's frame reader for good; a frame dropped whole is a datagram
/// lost, which both protocols allow.
const FRAMED_BACKLOG: usize = 1 << 20;

pub struct Reactor {
    shared: Arc<VsockShared>,
    commands: Mutex<Vec<Command>>,
    wake_fd: RawFd,
}

impl Reactor {
    /// Starts the reactor thread.
    pub fn start(shared: Arc<VsockShared>) -> io::Result<Arc<Reactor>> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: a two-int array for pipe to fill.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let (read_end, write_end) = (fds[0], fds[1]);
        set_nonblocking(read_end)?;
        set_nonblocking(write_end)?;
        let reactor = Arc::new(Reactor {
            shared: shared.clone(),
            commands: Mutex::new(Vec::new()),
            wake_fd: write_end,
        });
        let poke = reactor.clone();
        shared.set_stream_waker(move || poke.wake());
        let looped = reactor.clone();
        std::thread::Builder::new()
            .name("streams".into())
            .spawn(move || run(looped, read_end))?;
        Ok(reactor)
    }

    /// A stream the guest opened, established, its header on the way.
    pub fn accept_outbound(&self, key: ConnKey) {
        self.commands
            .lock()
            .expect("reactor commands poisoned")
            .push(Command::Outbound(key));
        self.wake();
    }

    /// A connection accepted on a published port, to carry into the guest,
    /// where the agent dials `dst`.
    pub fn carry_inbound(&self, dst: SocketAddr, mac: TcpStream) {
        self.commands
            .lock()
            .expect("reactor commands poisoned")
            .push(Command::Inbound(dst, mac));
        self.wake();
    }

    /// A DNS stream the guest opened.
    /// The guest's UDP stream: one per boot, every flow on it.
    pub fn accept_udp(&self, key: ConnKey) {
        self.commands
            .lock()
            .expect("reactor commands poisoned")
            .push(Command::Udp(key));
        self.wake();
    }

    /// The agent's stream for published UDP ports.
    pub fn accept_udp_inbound(&self, key: ConnKey) {
        self.commands
            .lock()
            .expect("reactor commands poisoned")
            .push(Command::UdpInbound(key));
        self.wake();
    }

    /// A UDP port Docker published, bound on the Mac: datagrams to it go
    /// into the guest, to `dst`, and replies come back to their senders.
    pub fn publish_udp(&self, published: Published, socket: std::net::UdpSocket, dst: SocketAddr) {
        self.commands
            .lock()
            .expect("reactor commands poisoned")
            .push(Command::PublishUdp(published, socket, dst));
        self.wake();
    }

    pub fn withdraw_udp(&self, published: Published) {
        self.commands
            .lock()
            .expect("reactor commands poisoned")
            .push(Command::WithdrawUdp(published));
        self.wake();
    }

    pub fn accept_dns(&self, key: ConnKey) {
        self.commands
            .lock()
            .expect("reactor commands poisoned")
            .push(Command::Dns(key));
        self.wake();
    }

    fn dns_reply(&self, key: ConnKey, id: u16, reply: Vec<u8>) {
        self.commands
            .lock()
            .expect("reactor commands poisoned")
            .push(Command::DnsReply(key, id, reply));
        self.wake();
    }

    fn wake(&self) {
        // One byte; a full pipe already means a wake is pending.
        // SAFETY: writing one byte from a stack array.
        unsafe { libc::write(self.wake_fd, [1u8].as_ptr().cast(), 1) };
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a live descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A UDP flow's destination from its opening frame: family, sixteen
/// address bytes, port.
fn udp_destination(payload: &[u8]) -> Option<SocketAddr> {
    if payload.len() < 19 {
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

/// Sends a framed stream's backlog and then `frames`, as far as credit
/// allows, keeping whatever the guest could not take in `backlog` from
/// `at`. New frames on a backlog past `FRAMED_BACKLOG` are dropped whole.
/// `Err` when the connection is gone.
fn push_framed(
    shared: &VsockShared,
    key: ConnKey,
    backlog: &mut Vec<u8>,
    at: &mut usize,
    frames: Vec<u8>,
) -> Result<(), Gone> {
    if *at >= backlog.len() {
        backlog.clear();
        *at = 0;
        if frames.is_empty() {
            return Ok(());
        }
        // Nothing waiting: the frames go as they are, no copy.
        if let Some(rest) = shared.try_send_owned(key, frames, false)? {
            *backlog = rest;
        }
        return Ok(());
    }
    if backlog.len() - *at + frames.len() <= FRAMED_BACKLOG {
        backlog.extend_from_slice(&frames);
    } else {
        tracing::debug!(
            len = frames.len(),
            "framed stream: backlog full, frames dropped"
        );
    }
    let n = shared.try_send(key, &backlog[*at..])?;
    *at += n;
    if *at >= backlog.len() {
        backlog.clear();
        *at = 0;
    }
    Ok(())
}

/// The nineteen-byte header naming `addr`: the inverse of `destination`,
/// and what an inbound stream opens with so the agent knows where to dial.
pub(crate) fn header_bytes(addr: SocketAddr) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    match addr.ip() {
        std::net::IpAddr::V4(a) => {
            header[0] = 4;
            header[1..5].copy_from_slice(&a.octets());
        }
        std::net::IpAddr::V6(a) => {
            header[0] = 6;
            header[1..17].copy_from_slice(&a.octets());
        }
    }
    header[17..19].copy_from_slice(&addr.port().to_be_bytes());
    header
}

/// Where the guest's header says to go.
fn destination(header: &[u8]) -> Option<SocketAddr> {
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

/// A non-blocking connect: the socket, connecting or connected.
fn connect_nonblocking(addr: SocketAddr) -> io::Result<TcpStream> {
    let (family, sockaddr, len) = sockaddr_bytes(addr);
    // SAFETY: a plain socket(2) call.
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own.
    let stream = unsafe { TcpStream::from_raw_fd(fd) };
    set_nonblocking(fd)?;
    let _ = stream.set_nodelay(true);
    crate::sockbuf::widen(&stream);
    // SAFETY: the sockaddr bytes are a correctly built sockaddr of `len`.
    let rc = unsafe { libc::connect(fd, sockaddr.as_ptr().cast(), len) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
    }
    Ok(stream)
}

/// The address family and the `sockaddr` bytes for a socket address, with
/// their length, for the libc calls that take one.
pub(crate) fn sockaddr_bytes(addr: SocketAddr) -> (libc::c_int, Vec<u8>, libc::socklen_t) {
    match addr {
        SocketAddr::V4(a) => {
            let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sin.sin_len = size_of::<libc::sockaddr_in>() as u8;
            sin.sin_family = libc::AF_INET as u8;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
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
            let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sin6.sin6_len = size_of::<libc::sockaddr_in6>() as u8;
            sin6.sin6_family = libc::AF_INET6 as u8;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_addr.s6_addr = a.ip().octets();
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
    }
}

/// After a non-blocking connect signals writable: did it succeed?
fn connect_result(fd: RawFd) -> io::Result<()> {
    let mut err: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: an int for SO_ERROR to fill.
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
        // SAFETY: one change, no events requested, no timeout.
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
    reactor: Arc<Reactor>,
    shared: Arc<VsockShared>,
    kq: Kq,
    streams: HashMap<ConnKey, Stream>,
    by_fd: HashMap<RawFd, ConnKey>,
    buf: Vec<u8>,
    memory: Option<Arc<crate::memory::GuestMemory>>,
    counters: Counters,
    /// Per UDP stream, the flows' sockets by the guest's flow id.
    udp_flows: HashMap<ConnKey, HashMap<u32, std::net::UdpSocket>>,
    udp_by_fd: HashMap<RawFd, (ConnKey, u32)>,
    /// The agent's stream for published UDP ports, once it has dialled.
    udp_inbound: Option<ConnKey>,
    udp_published: HashMap<Published, UdpPublish>,
    udp_published_by_fd: HashMap<RawFd, Published>,
    /// Each inbound flow's port and client, by the id this side gave it.
    udp_inbound_flows: HashMap<u32, (Published, SocketAddr)>,
    udp_inbound_next: u32,
    udp_inbound_swept: Instant,
}

fn run(reactor: Arc<Reactor>, wake_read: RawFd) {
    crate::qos::raise_interactive();
    let kq = match Kq::new() {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(%e, "streams: no kqueue");
            return;
        }
    };
    kq.read(wake_read, true);
    let mut l = Loop {
        shared: reactor.shared.clone(),
        reactor,
        kq,
        streams: HashMap::new(),
        by_fd: HashMap::new(),
        buf: Vec::with_capacity(READ_CHUNK),
        memory: None,
        counters: Counters::default(),
        udp_flows: HashMap::new(),
        udp_by_fd: HashMap::new(),
        udp_inbound: None,
        udp_published: HashMap::new(),
        udp_published_by_fd: HashMap::new(),
        udp_inbound_flows: HashMap::new(),
        udp_inbound_next: 0,
        udp_inbound_swept: Instant::now(),
    };
    let mut events: Vec<libc::kevent> = Vec::with_capacity(64);
    // `LIGHTER_STREAM_TRACE=1`: every stream's state every 100 ms, to the log.
    let trace = std::env::var("LIGHTER_STREAM_TRACE")
        .map(|v| v == "1")
        .unwrap_or(false);
    let tick = libc::timespec {
        tv_sec: 0,
        tv_nsec: 10_000_000,
    };
    let mut last_trace = std::time::Instant::now();
    // Spinning before parking: after any activity the loop polls kqueue and
    // the vsock side's flag for `spin` rather than blocking, so a reply
    // that follows a request by a few microseconds — every GET on a kept
    // connection — is picked up without the pipe write on the poller and
    // the scheduler hop here. Fifty microseconds: on the M1 a GET on a kept
    // connection went from 149 µs to 129 (OrbStack's 128), and 200 bought
    // nothing more; the M5 was level (60 → 63 → 59) with the outbound
    // port path a few percent up. `LIGHTER_REACTOR_SPIN_US`, 0 disables.
    let spin = std::env::var("LIGHTER_REACTOR_SPIN_US")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_micros)
        .unwrap_or(std::time::Duration::from_micros(50));
    let zero = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let mut spinning = false;
    let mut last_activity = std::time::Instant::now();
    loop {
        let timeout: *const libc::timespec = if spinning {
            &zero
        } else if trace {
            &tick
        } else {
            std::ptr::null()
        };
        // SAFETY: the events buffer has capacity 64 and no changes are given.
        let n = unsafe {
            libc::kevent(
                l.kq.0,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                64,
                timeout,
            )
        };
        if trace && last_trace.elapsed() >= std::time::Duration::from_millis(10) {
            last_trace = std::time::Instant::now();
            l.trace();
        }
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            tracing::error!("streams: kevent failed");
            return;
        }
        // SAFETY: kevent wrote `n` entries.
        unsafe { events.set_len(n as usize) };
        l.counters.iters += 1;
        l.counters.events += n as u64;
        let mut woken = false;
        for ev in &events {
            let fd = ev.ident as RawFd;
            if fd == wake_read {
                woken = true;
                continue;
            }
            if let Some(&published) = l.udp_published_by_fd.get(&fd) {
                l.udp_published_readable(published);
                continue;
            }
            if let Some(&(key, flow)) = l.udp_by_fd.get(&fd) {
                l.udp_readable(key, flow);
                continue;
            }
            let Some(&key) = l.by_fd.get(&fd) else {
                continue;
            };
            if ev.filter == libc::EVFILT_READ {
                l.readable(key);
            } else if ev.filter == libc::EVFILT_WRITE {
                l.writable(key);
            }
        }
        let pending = l
            .shared
            .stream_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        if woken {
            // Drain the pipe.
            let mut sink = [0u8; 256];
            // SAFETY: reading into a stack buffer on a non-blocking fd.
            while unsafe { libc::read(wake_read, sink.as_mut_ptr().cast(), sink.len()) } > 0 {}
        }
        if woken || pending {
            l.counters.wakes += 1;
            // Take the commands, then look at every stream: the waker says
            // nothing about which one changed.
            let commands: Vec<Command> = std::mem::take(
                &mut *l
                    .reactor
                    .commands
                    .lock()
                    .expect("reactor commands poisoned"),
            );
            for command in commands {
                l.command(command);
            }
            if l.memory.is_none() {
                l.memory = l.shared.memory();
            }
            let keys: Vec<ConnKey> = l.streams.keys().copied().collect();
            for key in keys {
                l.progress(key);
            }
        }
        if spin.is_zero() {
            continue;
        }
        if n > 0 || woken || pending {
            last_activity = std::time::Instant::now();
            if !spinning {
                spinning = true;
                l.shared
                    .reactor_spinning
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        } else if spinning {
            if last_activity.elapsed() < spin {
                std::hint::spin_loop();
                continue;
            }
            spinning = false;
            l.shared
                .reactor_spinning
                .store(false, std::sync::atomic::Ordering::SeqCst);
            // Parked as far as the vsock side can see; anything it flagged
            // in the gap is taken now rather than waited for.
            if l.shared
                .stream_pending
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                spinning = true;
                l.shared
                    .reactor_spinning
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

impl Loop {
    fn trace(&self) {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() % 100_000_000)
            .unwrap_or(0);
        for (key, s) in &self.streams {
            if !matches!(s.phase, Phase::Open) {
                continue;
            }
            let from_guest: usize =
                s.from_guest.iter().map(|c| c.len()).sum::<usize>() - s.from_guest_at;
            eprintln!(
                "TRACE t={t} stream {:?} from_guest={} to_guest={} reading={} writing={} tcp_eof={} guest_eof={}",
                key,
                from_guest,
                s.to_guest.len() - s.to_guest_at,
                s.reading,
                s.writing,
                s.tcp_eof,
                s.guest_eof
            );
        }
        let c = &self.counters;
        eprintln!(
            "TRACE t={t} reactor iters={} wakes={} events={} takes_chunks={} takes_empty={} writev={} eagain={} writable={}",
            c.iters,
            c.wakes,
            c.events,
            c.takes_chunks,
            c.takes_empty,
            c.writev,
            c.eagain,
            c.writable
        );
        for line in self.shared.trace_lines() {
            if line.contains("outbound=0/0") && line.contains("guest_inflight=0 ") {
                continue;
            }
            eprintln!("TRACE t={t} {line}");
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Outbound(key) => {
                self.streams.insert(
                    key,
                    Stream {
                        tcp: None,
                        phase: Phase::AwaitHeader,
                        to_guest: Vec::new(),
                        to_guest_at: 0,
                        from_guest: VecDeque::new(),
                        from_guest_at: 0,
                        tcp_eof: false,
                        guest_eof: false,
                        reading: false,
                        writing: false,
                        partial: Vec::new(),
                    },
                );
            }
            Command::Dns(key) => {
                self.streams.insert(
                    key,
                    Stream {
                        tcp: None,
                        phase: Phase::Dns,
                        to_guest: Vec::new(),
                        to_guest_at: 0,
                        from_guest: VecDeque::new(),
                        from_guest_at: 0,
                        tcp_eof: false,
                        guest_eof: false,
                        reading: false,
                        writing: false,
                        partial: Vec::new(),
                    },
                );
            }
            Command::DnsReply(key, id, reply) => {
                self.dns_send(key, id, &reply);
            }
            Command::Udp(key) => {
                self.streams.insert(
                    key,
                    Stream {
                        tcp: None,
                        phase: Phase::UdpMux,
                        to_guest: Vec::new(),
                        to_guest_at: 0,
                        from_guest: VecDeque::new(),
                        from_guest_at: 0,
                        tcp_eof: false,
                        guest_eof: false,
                        reading: false,
                        writing: false,
                        partial: Vec::new(),
                    },
                );
                self.udp_flows.insert(key, HashMap::new());
            }
            Command::UdpInbound(key) => {
                // A second agent stream (the agent restarted) replaces the
                // first, whose flows died with it.
                if let Some(old) = self.udp_inbound.take() {
                    self.close(old);
                }
                self.streams.insert(
                    key,
                    Stream {
                        tcp: None,
                        phase: Phase::UdpInbound,
                        to_guest: Vec::new(),
                        to_guest_at: 0,
                        from_guest: VecDeque::new(),
                        from_guest_at: 0,
                        tcp_eof: false,
                        guest_eof: false,
                        reading: false,
                        writing: false,
                        partial: Vec::new(),
                    },
                );
                self.udp_inbound = Some(key);
            }
            Command::PublishUdp(published, socket, dst) => {
                let fd = socket.as_raw_fd();
                self.kq.read(fd, true);
                self.udp_published_by_fd.insert(fd, published);
                if let Some(old) = self.udp_published.insert(
                    published,
                    UdpPublish {
                        socket,
                        dst,
                        flows: HashMap::new(),
                    },
                ) {
                    self.retire_udp_publish(old);
                }
            }
            Command::WithdrawUdp(published) => {
                if let Some(old) = self.udp_published.remove(&published) {
                    self.retire_udp_publish(old);
                }
            }
            Command::Inbound(dst, mac) => {
                let _ = mac.set_nodelay(true);
                crate::sockbuf::widen(&mac);
                if set_nonblocking(mac.as_raw_fd()).is_err() {
                    return;
                }
                let Ok(clone) = mac.try_clone() else { return };
                let key = self.shared.open(INBOUND_PORT, clone);
                let fd = mac.as_raw_fd();
                self.by_fd.insert(fd, key);
                self.streams.insert(
                    key,
                    Stream {
                        tcp: Some(mac),
                        phase: Phase::AwaitEstablished(dst),
                        to_guest: Vec::new(),
                        to_guest_at: 0,
                        from_guest: VecDeque::new(),
                        from_guest_at: 0,
                        tcp_eof: false,
                        guest_eof: false,
                        reading: false,
                        writing: false,
                        partial: Vec::new(),
                    },
                );
            }
        }
    }

    /// A framed reply onto a DNS stream: whole, now or when credit returns.
    fn dns_send(&mut self, key: ConnKey, id: u16, reply: &[u8]) {
        let mut frame = Vec::with_capacity(4 + reply.len());
        frame.extend_from_slice(&(reply.len() as u16).to_be_bytes());
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(reply);
        self.queue_framed(key, frame);
    }

    /// Frames onto a DNS or UDP stream. What credit takes goes now; the
    /// tail waits in the stream's backlog for the guest's next grant, so
    /// a frame is never cut and abandoned. A backlog already full drops
    /// the new frames whole instead.
    fn queue_framed(&mut self, key: ConnKey, frames: Vec<u8>) {
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        let result = push_framed(
            &self.shared,
            key,
            &mut stream.to_guest,
            &mut stream.to_guest_at,
            frames,
        );
        if result.is_err() {
            self.close(key);
        }
    }

    /// The backlog of a framed stream, tried again.
    fn flush_framed(&mut self, key: ConnKey) {
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        if stream.to_guest_at >= stream.to_guest.len() {
            return;
        }
        let result = push_framed(
            &self.shared,
            key,
            &mut stream.to_guest,
            &mut stream.to_guest_at,
            Vec::new(),
        );
        if result.is_err() {
            self.close(key);
        }
    }

    /// Queries off a DNS stream: whole frames answered, a partial one kept.
    /// Frames from the guest's UDP stream: flows opened, datagrams sent,
    /// flows closed.
    fn udp_progress(&mut self, key: ConnKey) {
        if !self.gather(key) {
            return;
        }
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        let mut at = 0usize;
        while stream.partial.len() - at >= UDP_HEADER {
            let h = &stream.partial[at..at + UDP_HEADER];
            let len = u16::from_be_bytes([h[0], h[1]]) as usize;
            let flow = u32::from_be_bytes([h[2], h[3], h[4], h[5]]);
            let kind = h[6];
            if stream.partial.len() - at - UDP_HEADER < len {
                break;
            }
            let payload = &stream.partial[at + UDP_HEADER..at + UDP_HEADER + len];
            at += UDP_HEADER + len;
            let flows = self.udp_flows.entry(key).or_default();
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
                    self.udp_by_fd.insert(fd, (key, flow));
                    tracing::debug!(flow, %dst, fd, "udp: flow opened");
                    if let Some(old) = flows.insert(flow, socket) {
                        let ofd = old.as_raw_fd();
                        self.kq.forget(ofd);
                        self.udp_by_fd.remove(&ofd);
                    }
                }
                UDP_KIND_DATA => {
                    if let Some(socket) = flows.get(&flow) {
                        // A datagram the socket cannot take right now is a
                        // datagram lost, which is what UDP promises.
                        let r = socket.send(payload);
                        tracing::debug!(flow, len = payload.len(), ok = r.is_ok(), "udp: sent");
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
        stream.partial.drain(..at);
    }

    /// A flow's socket has datagrams: each framed, the batch to the guest,
    /// whole frames only (`queue_framed`).
    fn udp_readable(&mut self, key: ConnKey, flow: u32) {
        let Some(socket) = self.udp_flows.get(&key).and_then(|f| f.get(&flow)) else {
            return;
        };
        let mut batch: Vec<u8> = Vec::with_capacity(64 * 1500);
        let mut buf = [0u8; 65536];
        for _ in 0..64 {
            match socket.recv(&mut buf) {
                Ok(n) => {
                    if n > u16::MAX as usize {
                        // Longer than the frame's length field can say; no
                        // real datagram is, and one that were would arrive
                        // with a length of zero.
                        continue;
                    }
                    batch.extend_from_slice(&(n as u16).to_be_bytes());
                    batch.extend_from_slice(&flow.to_be_bytes());
                    batch.push(UDP_KIND_DATA);
                    batch.extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
        if !batch.is_empty() {
            tracing::debug!(flow, len = batch.len(), "udp: replies to the guest");
            self.queue_framed(key, batch);
        }
    }

    /// What the guest has sent on a framed stream, appended to the
    /// stream's `partial` and credited: `false` when there was nothing, or
    /// the stream has ended (and been closed here).
    fn gather(&mut self, key: ConnKey) -> bool {
        let memory = self.memory.clone();
        let Some(stream) = self.streams.get_mut(&key) else {
            return false;
        };
        let chunks = match self.shared.try_take_outbound(key) {
            Outbound::Chunks(c) => c,
            Outbound::Empty => return false,
            Outbound::Finished | Outbound::Gone => {
                self.close(key);
                return false;
            }
        };
        let mut heads: Vec<u16> = Vec::new();
        let mut bytes = 0u32;
        for chunk in chunks {
            match chunk {
                Chunk::Owned(v) => {
                    bytes += v.len() as u32;
                    stream.partial.extend_from_slice(&v);
                }
                Chunk::Guest { head, spans } => {
                    if let Some(mem) = &memory {
                        for (gpa, len) in &spans {
                            let start = stream.partial.len();
                            stream.partial.resize(start + len, 0);
                            let _ = mem.read(*gpa, &mut stream.partial[start..]);
                            bytes += *len as u32;
                        }
                    }
                    heads.push(head);
                }
            }
        }
        if !heads.is_empty() {
            self.shared.complete(heads);
        }
        if bytes > 0 {
            self.shared.acknowledge(key, bytes);
        }
        true
    }

    /// Frames from the agent's inbound UDP stream: a container's replies,
    /// each to the client whose flow it names; a flow the agent closed.
    fn udp_inbound_progress(&mut self, key: ConnKey) {
        if !self.gather(key) {
            return;
        }
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        let mut at = 0usize;
        while stream.partial.len() - at >= UDP_HEADER {
            let h = &stream.partial[at..at + UDP_HEADER];
            let len = u16::from_be_bytes([h[0], h[1]]) as usize;
            let flow = u32::from_be_bytes([h[2], h[3], h[4], h[5]]);
            let kind = h[6];
            if stream.partial.len() - at - UDP_HEADER < len {
                break;
            }
            let payload = &stream.partial[at + UDP_HEADER..at + UDP_HEADER + len];
            at += UDP_HEADER + len;
            match kind {
                UDP_KIND_DATA => {
                    if let Some(&(published, client)) = self.udp_inbound_flows.get(&flow)
                        && let Some(publish) = self.udp_published.get(&published)
                    {
                        // A reply the client's socket cannot take is lost,
                        // as UDP allows.
                        let _ = publish.socket.send_to(payload, client);
                    }
                }
                UDP_KIND_CLOSE => {
                    if let Some((published, client)) = self.udp_inbound_flows.remove(&flow)
                        && let Some(publish) = self.udp_published.get_mut(&published)
                    {
                        publish.flows.remove(&client);
                    }
                }
                _ => {}
            }
        }
        stream.partial.drain(..at);
    }

    /// A published UDP port has datagrams: each client's flow found or
    /// opened, each datagram framed, the batch to the agent. Without the
    /// agent's stream yet, or with the backlog full, they are lost, as UDP
    /// allows. Flows nobody has used for a minute are closed on the way.
    fn udp_published_readable(&mut self, published: Published) {
        let Some(mux) = self.udp_inbound else {
            // Drain, or the socket stays readable forever.
            if let Some(publish) = self.udp_published.get(&published) {
                let mut buf = [0u8; 65536];
                while publish.socket.recv_from(&mut buf).is_ok() {}
            }
            return;
        };
        let Some(publish) = self.udp_published.get_mut(&published) else {
            return;
        };
        let now = Instant::now();
        let mut batch: Vec<u8> = Vec::with_capacity(64 * 1500);
        let mut buf = [0u8; 65536];
        for _ in 0..64 {
            let Ok((n, client)) = publish.socket.recv_from(&mut buf) else {
                break;
            };
            if n > u16::MAX as usize {
                continue;
            }
            let flow = match publish.flows.get_mut(&client) {
                Some((flow, last)) => {
                    *last = now;
                    *flow
                }
                None => {
                    let flow = self.udp_inbound_next;
                    self.udp_inbound_next = self.udp_inbound_next.wrapping_add(1);
                    publish.flows.insert(client, (flow, now));
                    self.udp_inbound_flows.insert(flow, (published, client));
                    batch.extend_from_slice(&(HEADER_LEN as u16).to_be_bytes());
                    batch.extend_from_slice(&flow.to_be_bytes());
                    batch.push(UDP_KIND_OPEN);
                    batch.extend_from_slice(&header_bytes(publish.dst));
                    tracing::debug!(flow, %client, %published, "udp: inbound flow opened");
                    flow
                }
            };
            batch.extend_from_slice(&(n as u16).to_be_bytes());
            batch.extend_from_slice(&flow.to_be_bytes());
            batch.push(UDP_KIND_DATA);
            batch.extend_from_slice(&buf[..n]);
        }
        if now.duration_since(self.udp_inbound_swept) >= UDP_FLOW_IDLE {
            self.udp_inbound_swept = now;
            for publish in self.udp_published.values_mut() {
                let stale: Vec<SocketAddr> = publish
                    .flows
                    .iter()
                    .filter(|(_, (_, last))| now.duration_since(*last) >= UDP_FLOW_IDLE)
                    .map(|(client, _)| *client)
                    .collect();
                for client in stale {
                    if let Some((flow, _)) = publish.flows.remove(&client) {
                        self.udp_inbound_flows.remove(&flow);
                        batch.extend_from_slice(&0u16.to_be_bytes());
                        batch.extend_from_slice(&flow.to_be_bytes());
                        batch.push(UDP_KIND_CLOSE);
                    }
                }
            }
        }
        if !batch.is_empty() {
            self.queue_framed(mux, batch);
        }
    }

    /// A published UDP port withdrawn: its socket forgotten, and every flow
    /// it had closed on the agent's side too.
    fn retire_udp_publish(&mut self, publish: UdpPublish) {
        let fd = publish.socket.as_raw_fd();
        self.kq.forget(fd);
        self.udp_published_by_fd.remove(&fd);
        let mut batch = Vec::new();
        for (_, (flow, _)) in publish.flows {
            self.udp_inbound_flows.remove(&flow);
            batch.extend_from_slice(&0u16.to_be_bytes());
            batch.extend_from_slice(&flow.to_be_bytes());
            batch.push(UDP_KIND_CLOSE);
        }
        if !batch.is_empty()
            && let Some(mux) = self.udp_inbound
        {
            self.queue_framed(mux, batch);
        }
    }

    fn dns_progress(&mut self, key: ConnKey) {
        if !self.gather(key) {
            return;
        }
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        let mut at = 0usize;
        let mut replies: Vec<(u16, Vec<u8>)> = Vec::new();
        while stream.partial.len() - at >= 4 {
            let len = u16::from_be_bytes([stream.partial[at], stream.partial[at + 1]]) as usize;
            let id = u16::from_be_bytes([stream.partial[at + 2], stream.partial[at + 3]]);
            if stream.partial.len() - at - 4 < len {
                break;
            }
            let query = stream.partial[at + 4..at + 4 + len].to_vec();
            at += 4 + len;
            let reactor = self.reactor.clone();
            let deliver: Arc<dyn Fn(u16, Vec<u8>) + Send + Sync> =
                Arc::new(move |id, reply| reactor.dns_reply(key, id, reply));
            if let Some(reply) = crate::dns::answer(query, id, deliver) {
                replies.push((id, reply));
            }
        }
        stream.partial.drain(..at);
        for (id, reply) in replies {
            self.dns_send(key, id, &reply);
        }
    }

    /// Whatever a stream can do now.
    fn progress(&mut self, key: ConnKey) {
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        match stream.phase {
            Phase::Dns => {
                self.flush_framed(key);
                self.dns_progress(key);
            }
            Phase::UdpMux => {
                self.flush_framed(key);
                self.udp_progress(key);
            }
            Phase::UdpInbound => {
                self.flush_framed(key);
                self.udp_inbound_progress(key);
            }
            Phase::AwaitHeader => match self.shared.try_read_outbound(key, HEADER_LEN) {
                Ok(Some(header)) => {
                    let Some(addr) = destination(&header) else {
                        self.close(key);
                        return;
                    };
                    match connect_nonblocking(addr) {
                        Ok(tcp) => {
                            let fd = tcp.as_raw_fd();
                            self.by_fd.insert(fd, key);
                            let stream = self.streams.get_mut(&key).expect("present");
                            stream.tcp = Some(tcp);
                            stream.phase = Phase::Connecting;
                            stream.writing = true;
                            self.kq.write(fd, true);
                        }
                        Err(e) => {
                            tracing::debug!(%addr, %e, "stream: connect failed");
                            self.close(key);
                        }
                    }
                }
                Ok(None) => {}
                Err(Gone) => self.close(key),
            },
            Phase::Connecting => {}
            Phase::AwaitEstablished(dst) => match self.shared.status(key) {
                Status::Established => match self.shared.try_send(key, &header_bytes(dst)) {
                    Ok(HEADER_LEN) => {
                        let fd = stream.tcp.as_ref().expect("socket").as_raw_fd();
                        stream.phase = Phase::Open;
                        stream.reading = true;
                        self.kq.read(fd, true);
                    }
                    Ok(0) => {}
                    // A header the guest's credit cut in two cannot be
                    // finished without a second copy of its first bytes;
                    // a fresh connection has its whole window, so this is
                    // a connection that is wrong, not one that is slow.
                    Ok(n) => {
                        tracing::debug!(n, "inbound: the header did not fit the guest's credit");
                        self.close(key);
                    }
                    Err(Gone) => self.close(key),
                },
                Status::Connecting => {}
                Status::Gone => self.close(key),
            },
            Phase::Open => {
                self.push_to_guest(key);
                self.pull_from_guest(key);
            }
        }
    }

    /// Bytes read from the socket that the guest had no credit for, tried again.
    fn push_to_guest(&mut self, key: ConnKey) {
        if self.shared.guest_receive_closed(key) {
            self.stop_host_input(key);
            return;
        }
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        if stream.to_guest_at < stream.to_guest.len() {
            match self
                .shared
                .try_send(key, &stream.to_guest[stream.to_guest_at..])
            {
                Ok(n) => {
                    stream.to_guest_at += n;
                    if stream.to_guest_at == stream.to_guest.len() {
                        stream.to_guest.clear();
                        stream.to_guest_at = 0;
                        if !stream.reading && !stream.tcp_eof {
                            let fd = stream.tcp.as_ref().expect("socket").as_raw_fd();
                            stream.reading = true;
                            self.kq.read(fd, true);
                        }
                    }
                }
                Err(Gone) => {
                    if self.shared.guest_receive_closed(key) {
                        self.stop_host_input(key);
                    } else {
                        self.close(key);
                    }
                }
            }
        }
    }

    /// The guest refuses further input but may still owe the host a response.
    fn stop_host_input(&mut self, key: ConnKey) {
        if let Some(stream) = self.streams.get_mut(&key) {
            stream.to_guest.clear();
            stream.to_guest_at = 0;
            stream.tcp_eof = true;
            stream.reading = false;
            if let Some(tcp) = &stream.tcp {
                self.kq.read(tcp.as_raw_fd(), false);
                let _ = tcp.shutdown(std::net::Shutdown::Read);
            }
        }
    }

    /// Chunks from the guest, onto the socket as far as it takes them, and
    /// again while there are more: what arrived during a write that could
    /// not complete is taken once it has, not on the next wake — the guest
    /// may be waiting on this very write for the credit to send anything.
    fn pull_from_guest(&mut self, key: ConnKey) {
        loop {
            let Some(stream) = self.streams.get_mut(&key) else {
                return;
            };
            if stream.from_guest.is_empty() && !stream.guest_eof {
                match self.shared.try_take_outbound(key) {
                    Outbound::Chunks(chunks) => {
                        self.counters.takes_chunks += 1;
                        stream.from_guest.extend(chunks)
                    }
                    Outbound::Empty => self.counters.takes_empty += 1,
                    Outbound::Finished => stream.guest_eof = true,
                    Outbound::Gone => {
                        self.close(key);
                        return;
                    }
                }
            }
            if stream.from_guest.is_empty() {
                self.flush(key);
                return;
            }
            self.flush(key);
            let Some(stream) = self.streams.get_mut(&key) else {
                return;
            };
            if stream.writing || !stream.from_guest.is_empty() {
                return;
            }
        }
    }

    /// Writes what is pending from the guest; arms the write filter if the
    /// socket would not take it all.
    fn flush(&mut self, key: ConnKey) {
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        let Some(tcp) = stream.tcp.as_ref() else {
            return;
        };
        let fd = tcp.as_raw_fd();
        while !stream.from_guest.is_empty() {
            // One writev over everything pending, from the offset into the
            // first chunk.
            let mut iovs: Vec<libc::iovec> = Vec::with_capacity(stream.from_guest.len() * 2);
            let mut skip = stream.from_guest_at;
            for chunk in &stream.from_guest {
                match chunk {
                    Chunk::Owned(v) => {
                        let s = skip.min(v.len());
                        skip -= s;
                        if v.len() > s {
                            iovs.push(libc::iovec {
                                iov_base: v[s..].as_ptr() as *mut libc::c_void,
                                iov_len: v.len() - s,
                            });
                        }
                    }
                    Chunk::Guest { spans, .. } => {
                        let Some(mem) = &self.memory else { break };
                        for (gpa, len) in spans {
                            let s = skip.min(*len);
                            skip -= s;
                            if *len > s
                                && let Ok(ptr) = mem.host_span(gpa + s as u64, len - s)
                            {
                                iovs.push(libc::iovec {
                                    iov_base: ptr.cast(),
                                    iov_len: len - s,
                                });
                            }
                        }
                    }
                }
                if iovs.len() >= 900 {
                    break;
                }
            }
            if iovs.is_empty() {
                break;
            }
            // SAFETY: every iovec points into a chunk or guest span that
            // outlives this call.
            let n = unsafe { libc::writev(fd, iovs.as_ptr(), iovs.len() as libc::c_int) };
            self.counters.writev += 1;
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    self.counters.eagain += 1;
                    if !stream.writing {
                        stream.writing = true;
                        self.kq.write(fd, true);
                    }
                    return;
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                self.close(key);
                return;
            }
            // Retire what was written whole; credit and complete it.
            let mut left = n as usize;
            let mut done_heads: Vec<u16> = Vec::new();
            let mut recycled: Vec<Chunk> = Vec::new();
            let mut acked = 0u32;
            while left > 0 {
                let Some(front) = stream.from_guest.front() else {
                    break;
                };
                let remaining = front.len() - stream.from_guest_at;
                if left >= remaining {
                    left -= remaining;
                    acked += remaining as u32;
                    let chunk = stream.from_guest.pop_front().expect("front");
                    stream.from_guest_at = 0;
                    match chunk {
                        Chunk::Guest { head, .. } => done_heads.push(head),
                        owned => recycled.push(owned),
                    }
                } else {
                    stream.from_guest_at += left;
                    acked += left as u32;
                    left = 0;
                }
            }
            if !done_heads.is_empty() {
                self.shared.complete(done_heads);
            }
            if acked > 0 {
                self.shared.acknowledge(key, acked);
            }
            if !recycled.is_empty() {
                self.shared.recycle(recycled);
            }
        }
        if stream.writing && stream.from_guest.is_empty() {
            stream.writing = false;
            self.kq.write(fd, false);
        }
        if stream.from_guest.is_empty() && stream.guest_eof {
            // The guest said it is done and everything it sent is on the
            // socket: the Mac's peer is owed an EOF.
            if let Some(tcp) = &stream.tcp {
                let _ = tcp.shutdown(std::net::Shutdown::Write);
            }
            if stream.tcp_eof {
                self.close(key);
            }
        }
    }

    fn readable(&mut self, key: ConnKey) {
        if self.shared.guest_receive_closed(key) {
            self.stop_host_input(key);
            self.pull_from_guest(key);
            return;
        }
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        let Some(tcp) = stream.tcp.as_ref() else {
            return;
        };
        let fd = tcp.as_raw_fd();
        if !matches!(stream.phase, Phase::Open) {
            return;
        }
        loop {
            // Into a payload-sized buffer the packet will own: a read and a
            // copy into a packet was two copies of every inbound byte on the
            // one thread that carries them all.
            if self.buf.capacity() < READ_CHUNK {
                self.buf = self.shared.spare_buffer();
                self.buf.reserve(READ_CHUNK);
            }
            self.buf.clear();
            // SAFETY: reading into the buffer's spare capacity on a
            // non-blocking fd; the length is set from what arrived.
            let n = unsafe { libc::read(fd, self.buf.as_mut_ptr().cast(), READ_CHUNK) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    return;
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                self.close(key);
                return;
            }
            if n == 0 {
                stream.tcp_eof = true;
                stream.reading = false;
                self.kq.read(fd, false);
                self.shared.shutdown_write(key);
                if stream.guest_eof && stream.from_guest.is_empty() {
                    self.close(key);
                }
                return;
            }
            let n = n as usize;
            // SAFETY: `n` bytes were just written into the buffer by read.
            unsafe { self.buf.set_len(n) };
            let buf = std::mem::take(&mut self.buf);
            let bulk = n >= READ_CHUNK / 2;
            match self.shared.try_send_owned(key, buf, bulk) {
                Ok(None) => {}
                Ok(Some(rest)) => {
                    // Out of credit: keep the rest, stop reading until the
                    // waker says the guest has room.
                    stream.to_guest = rest;
                    stream.to_guest_at = 0;
                    stream.reading = false;
                    self.kq.read(fd, false);
                    return;
                }
                Err(Gone) => {
                    if self.shared.guest_receive_closed(key) {
                        self.stop_host_input(key);
                        self.pull_from_guest(key);
                    } else {
                        self.close(key);
                    }
                    return;
                }
            }
        }
    }

    fn writable(&mut self, key: ConnKey) {
        self.counters.writable += 1;
        let Some(stream) = self.streams.get_mut(&key) else {
            return;
        };
        if matches!(stream.phase, Phase::Connecting) {
            let fd = stream.tcp.as_ref().expect("socket").as_raw_fd();
            match connect_result(fd) {
                Ok(()) => {
                    stream.phase = Phase::Open;
                    stream.writing = false;
                    self.kq.write(fd, false);
                    stream.reading = true;
                    self.kq.read(fd, true);
                    self.pull_from_guest(key);
                }
                Err(e) => {
                    tracing::debug!(%e, "stream: connect failed");
                    self.close(key);
                }
            }
            return;
        }
        self.pull_from_guest(key);
    }

    fn close(&mut self, key: ConnKey) {
        if self.udp_inbound == Some(key) {
            // The agent's stream is gone, and every inbound flow with it;
            // the published sockets stay, for the stream that replaces it.
            self.udp_inbound = None;
            self.udp_inbound_flows.clear();
            for publish in self.udp_published.values_mut() {
                publish.flows.clear();
            }
        }
        if let Some(flows) = self.udp_flows.remove(&key) {
            for (_, socket) in flows {
                let fd = socket.as_raw_fd();
                self.kq.forget(fd);
                self.udp_by_fd.remove(&fd);
            }
        }
        if let Some(stream) = self.streams.remove(&key) {
            if let Some(tcp) = stream.tcp {
                let fd = tcp.as_raw_fd();
                self.kq.forget(fd);
                self.by_fd.remove(&fd);
                let _ = tcp.shutdown(std::net::Shutdown::Both);
            }
            // Chains still held are returned by the vsock side on removal.
            for chunk in stream.from_guest {
                if let Chunk::Guest { head, .. } = chunk {
                    self.shared.complete([head]);
                }
            }
        }
        self.shared.shutdown(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn framed_stream(credit: u32) -> (Arc<VsockShared>, ConnKey, std::os::unix::net::UnixStream) {
        let shared = Arc::new(VsockShared::new());
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let key = shared.open(2380, ours);
        shared.establish_for_test(key, credit);
        (shared, key, theirs)
    }

    fn frames(n: usize, len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for flow in 0..n {
            out.extend_from_slice(&(len as u16).to_be_bytes());
            out.extend_from_slice(&(flow as u32).to_be_bytes());
            out.push(UDP_KIND_DATA);
            out.extend(std::iter::repeat_n(flow as u8, len));
        }
        out
    }

    /// The bug this guards against: a batch of frames cut at the guest's
    /// credit and the cut-off tail thrown away, after which the guest's
    /// frame reader takes payload bytes for a header and never recovers.
    #[test]
    fn a_framed_batch_beyond_the_credit_is_finished_when_credit_returns() {
        let (shared, key, _peer) = framed_stream(100);
        let batch = frames(3, 50);
        let mut backlog = Vec::new();
        let mut at = 0;
        push_framed(&shared, key, &mut backlog, &mut at, batch.clone()).unwrap();
        assert_eq!(shared.queued_for_test(key), &batch[..100]);
        assert_eq!(
            &backlog[at..],
            &batch[100..],
            "the tail is kept, not dropped"
        );

        shared.drain_for_test(key);
        push_framed(&shared, key, &mut backlog, &mut at, Vec::new()).unwrap();
        assert_eq!(
            shared.queued_for_test(key),
            &batch[100..],
            "the rest goes when the guest grants credit again"
        );
        assert!(backlog.is_empty() && at == 0);
    }

    #[test]
    fn frames_arriving_behind_a_backlog_queue_behind_it_in_order() {
        let (shared, key, _peer) = framed_stream(10);
        let first = frames(1, 20);
        let second = frames(1, 20);
        let mut backlog = Vec::new();
        let mut at = 0;
        push_framed(&shared, key, &mut backlog, &mut at, first.clone()).unwrap();
        push_framed(&shared, key, &mut backlog, &mut at, second.clone()).unwrap();
        // Ten bytes of credit at a time, as the guest consumes.
        let mut seen = shared.queued_for_test(key);
        for _ in 0..8 {
            shared.drain_for_test(key);
            push_framed(&shared, key, &mut backlog, &mut at, Vec::new()).unwrap();
            seen.extend(shared.queued_for_test(key));
        }
        let mut expected = first;
        expected.extend_from_slice(&second);
        assert_eq!(seen, expected);
        assert!(backlog.is_empty());
    }

    #[test]
    fn a_full_backlog_drops_new_frames_whole() {
        let (shared, key, _peer) = framed_stream(0);
        let mut backlog = vec![0u8; FRAMED_BACKLOG];
        let mut at = 0;
        let before = backlog.len();
        push_framed(&shared, key, &mut backlog, &mut at, frames(1, 8)).unwrap();
        assert_eq!(
            backlog.len(),
            before,
            "nothing of the new frame is appended"
        );
        assert!(shared.queued_for_test(key).is_empty());
    }

    #[test]
    fn a_header_round_trips_through_its_bytes() {
        for addr in ["192.168.127.2:18098", "[::1]:18097", "[fd6c::2]:53"] {
            let addr: SocketAddr = addr.parse().unwrap();
            assert_eq!(destination(&header_bytes(addr)), Some(addr));
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
    fn a_nonblocking_connect_to_a_closed_port_reports_the_refusal() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let stream = connect_nonblocking(addr).unwrap();
        // Poll until the socket settles.
        let fd = stream.as_raw_fd();
        let mut fds = [libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        }];
        unsafe { libc::poll(fds.as_mut_ptr(), 1, 2000) };
        assert!(connect_result(fd).is_err());
    }
}

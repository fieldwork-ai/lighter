//! Every stream the agent carries, on one epoll loop per process.
//!
//! A thread per connection is what failed on 2026-09-29: a guest short of
//! threads refused one spawn, the panic took the whole outbound proxy with it,
//! and every container's TCP went unanswered until a restart. Here a
//! connection is a small state machine on the one thread, each state one of
//! the waits a connection's thread used to sleep in, and anything that runs
//! out refuses that one connection.
//!
//! Three routes share it: containers' outbound TCP to the Mac, the Mac's
//! connections to published ports, and the Mac's `docker` to dockerd's socket.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub enum Route {
    /// A container's connection, redirected here, to the Mac.
    Outbound,
    /// The Mac's connection to a published port, header first.
    Inbound,
    /// The Mac's `docker` to a unix socket in the guest.
    Unix(PathBuf),
}

/// What the loop needs from the guest: how to reach the Mac, and where a
/// redirected connection was going. Stand-ins in the tests.
pub struct Env {
    /// Starts a non-blocking connect to the Mac's stream port.
    pub dial_host: fn() -> io::Result<OwnedFd>,
    pub destination: fn(RawFd) -> Option<SocketAddr>,
    pub joiner: Option<&'static crate::sockmap::Joiner>,
    /// How long a timed-out dial of the Mac keeps being retried.
    pub host_window: Duration,
}

const LISTENER: u64 = u64::MAX;
/// The accelerator gate's verdicts are ready (`accelerator::Checker`).
const GATE: u64 = u64::MAX - 1;
/// In an inbound header's family byte: the client's address follows the
/// destination's (the host's `HEADER_CLIENT`, `reactor.rs`).
const HEADER_CLIENT: u8 = 0x80;
/// A socket's first read of a fresh buffer, and the copying path's buffer.
const BUFFER: usize = 256 * 1024;
const PIPE: usize = 4 << 20;
/// How long a write refused for want of memory keeps being retried: the
/// guest reclaiming under a host that is short.
const REFUSED_WINDOW: Duration = Duration::from_secs(30);
/// How long a v6 dial waits to see whether Docker's proxy hangs up on it.
const V6_HANGUP_WAIT: Duration = Duration::from_millis(50);
/// Keepalive on a container's side of an outbound stream: idle seconds
/// before the first probe, seconds between probes, and probes unanswered
/// before the socket fails. See [`keep_alive`].
const KEEPALIVE: [libc::c_int; 3] = [30, 10, 3];
/// Accepting a continuous backlog must leave time to carry and close the
/// streams already held. The listener is level-triggered, so the next turn
/// sees any connections left queued.
const ACCEPT_BATCH: usize = 256;

pub fn serve(listener: OwnedFd, route: Route, env: Env) -> std::process::ExitCode {
    let mut lp = match Loop::new(listener, route, env) {
        Ok(lp) => lp,
        Err(e) => {
            eprintln!("lighter-agent: cannot start the stream loop: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    loop {
        lp.turn(None);
    }
}

struct Loop {
    epoll: OwnedFd,
    listener: OwnedFd,
    route: Route,
    env: Env,
    conns: HashMap<u64, Conn>,
    next: u64,
    timers: BinaryHeap<Reverse<(Instant, u64)>>,
    /// Given up to accept, and at once close, a connection when the process
    /// is out of descriptors, so a full table sheds load rather than
    /// spinning on a listener that stays readable.
    spare: Option<OwnedFd>,
    listening: bool,
    /// Streams to an accelerator port, waiting for the gate's verdict, and
    /// the gate, started by the first of them.
    gated: HashMap<u64, (OwnedFd, SocketAddr)>,
    gate: Option<crate::accelerator::Checker>,
}

impl Loop {
    fn new(listener: OwnedFd, route: Route, env: Env) -> io::Result<Loop> {
        set_nonblocking(listener.as_raw_fd())?;
        // std listens with a backlog of 128, and a burst of connections past
        // it waits out SYN retries. Linux's own default ceiling instead.
        // SAFETY: a live, bound socket.
        unsafe { libc::listen(listener.as_raw_fd(), 4096) };
        // SAFETY: a plain epoll_create1 call.
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        let epoll = unsafe { OwnedFd::from_raw_fd(raw) };
        let lp = Loop {
            epoll,
            listener,
            route,
            env,
            conns: HashMap::new(),
            next: 0,
            timers: BinaryHeap::new(),
            spare: open_spare(),
            listening: false,
            gated: HashMap::new(),
            gate: None,
        };
        lp.listen(true)?;
        let mut lp = lp;
        lp.listening = true;
        Ok(lp)
    }

    fn listen(&self, on: bool) -> io::Result<()> {
        let mut ev = libc::epoll_event { events: libc::EPOLLIN as u32, u64: LISTENER };
        let op = if on { libc::EPOLL_CTL_ADD } else { libc::EPOLL_CTL_DEL };
        // SAFETY: live descriptors and an event the call reads.
        if unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), op, self.listener.as_raw_fd(), &mut ev) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// One wait and everything it made ready. `limit` bounds the wait, for
    /// the tests.
    fn turn(&mut self, limit: Option<Duration>) {
        let now = Instant::now();
        let mut wait = self.timers.peek().map(|Reverse((at, _))| at.saturating_duration_since(now));
        if let Some(limit) = limit {
            wait = Some(wait.map_or(limit, |w| w.min(limit)));
        }
        let timeout = wait.map_or(-1, |w| w.as_millis().min(i32::MAX as u128) as i32 + i32::from(w.subsec_nanos() % 1_000_000 != 0));
        let mut events: [libc::epoll_event; 256] = [libc::epoll_event { events: 0, u64: 0 }; 256];
        // SAFETY: a live epoll descriptor and a buffer of the length given.
        let n = unsafe { libc::epoll_wait(self.epoll.as_raw_fd(), events.as_mut_ptr(), events.len() as i32, timeout) };
        let now = Instant::now();
        let mut ready: HashSet<u64> = HashSet::new();
        if n > 0 {
            for ev in &events[..n as usize] {
                let token = ev.u64;
                if token == LISTENER {
                    self.accept_all(now);
                } else if token == GATE {
                    self.judged(now);
                } else {
                    ready.insert(token >> 1);
                }
            }
        }
        while let Some(Reverse((at, id))) = self.timers.peek().copied() {
            if at > now {
                break;
            }
            self.timers.pop();
            if id == LISTENER {
                if !self.listening && self.listen(true).is_ok() {
                    self.listening = true;
                }
            } else {
                ready.insert(id);
            }
        }
        for id in ready {
            self.drive(id, now);
        }
    }

    fn accept_all(&mut self, now: Instant) {
        for _ in 0..ACCEPT_BATCH {
            // SAFETY: accepting on a live listener with no interest in the
            // peer address.
            let raw = unsafe {
                libc::accept4(self.listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(),
                    libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
            };
            if raw < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EAGAIN) => return,
                    Some(libc::EINTR) | Some(libc::ECONNABORTED) | Some(libc::EPROTO) => continue,
                    Some(libc::EMFILE) | Some(libc::ENFILE) => {
                        self.shed(&e);
                        // A successful shed does not make room: the spare
                        // was reopened. Carry and close existing streams
                        // before accepting again, even if clients retry
                        // fast enough to keep the listener readable.
                        self.pause_listening(now);
                        return;
                    }
                    _ => {
                        ran_out("memory for a connection", &e);
                        self.pause_listening(now);
                        return;
                    }
                }
            }
            // SAFETY: a fresh accepted descriptor we now own.
            let a = unsafe { OwnedFd::from_raw_fd(raw) };
            let id = self.next;
            self.next += 1;
            match Conn::accepted(a, &self.route, &self.env, now) {
                Some(Accepted::Ready(conn)) => self.admit(id, conn, now),
                Some(Accepted::Gated { a, dst, kind, peer }) => self.hold(id, a, dst, kind, peer),
                None => {}
            }
        }
    }

    fn admit(&mut self, id: u64, mut conn: Conn, now: Instant) {
        conn.watched_a = conn.interest().0;
        if let Err(e) = self.watch(libc::EPOLL_CTL_ADD, conn.a.as_raw_fd(), id << 1, conn.watched_a) {
            ran_out("epoll watches", &e);
            return;
        }
        self.conns.insert(id, conn);
        self.drive(id, now);
    }

    /// Holds a stream to an accelerator port until the gate has decided.
    /// Unwatched meanwhile: a client that hangs up first is found closed
    /// when it is let through, as any stream is.
    fn hold(&mut self, id: u64, a: OwnedFd, dst: SocketAddr, kind: &'static str, peer: std::net::IpAddr) {
        if self.gate.is_none() {
            match crate::accelerator::Checker::start() {
                Ok(gate) => {
                    if let Err(e) = self.watch(libc::EPOLL_CTL_ADD, gate.wake_fd(), GATE, libc::EPOLLIN as u32) {
                        ran_out("epoll watches", &e);
                        return;
                    }
                    self.gate = Some(gate);
                }
                Err(e) => {
                    ran_out("a thread for the accelerator gate", &e);
                    return;
                }
            }
        }
        self.gate.as_ref().expect("just started").ask(id, kind, peer);
        self.gated.insert(id, (a, dst));
    }

    /// The gate's verdicts: a permitted stream goes on as any outbound one,
    /// a refused one is dropped, which the container sees as a reset.
    fn judged(&mut self, now: Instant) {
        let verdicts = self.gate.as_ref().map(|g| g.verdicts()).unwrap_or_default();
        for (id, permitted) in verdicts {
            let Some((a, dst)) = self.gated.remove(&id) else { continue };
            if permitted && let Some(conn) = Conn::outbound(a, dst, &self.env, now) {
                self.admit(id, conn, now);
            }
        }
    }

    /// Accepts and drops one waiting connection with the spare descriptor.
    fn shed(&mut self, error: &io::Error) {
        // Diagnostics also open files. Release the spare before reading
        // /proc, otherwise EMFILE makes every resource count unavailable.
        drop(self.spare.take());
        ran_out("descriptors", error);
        // SAFETY: as in accept_all.
        let raw = unsafe {
            libc::accept4(self.listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC)
        };
        if raw >= 0 {
            // SAFETY: a fresh descriptor, closed at once.
            drop(unsafe { OwnedFd::from_raw_fd(raw) });
        }
        self.spare = open_spare();
    }

    /// Stops accepting for a moment rather than spinning on a failure that
    /// the next accept would meet again.
    fn pause_listening(&mut self, now: Instant) {
        if self.listening && self.listen(false).is_ok() {
            self.listening = false;
            self.timers.push(Reverse((now + Duration::from_millis(50), LISTENER)));
        }
    }

    /// Watches a socket for what its connection's state waits on. A change
    /// of mask (`EPOLL_CTL_MOD`) looks at the socket again, so nothing that
    /// happened meanwhile is missed.
    fn watch(&self, op: libc::c_int, fd: RawFd, token: u64, events: u32) -> io::Result<()> {
        let mut ev = libc::epoll_event { events: events | libc::EPOLLET as u32, u64: token };
        // SAFETY: live descriptors and an event the call reads.
        if unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), op, fd, &mut ev) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Brings the watches up to what the connection now waits on.
    fn rewatch(&mut self, id: u64) -> io::Result<()> {
        let Some(conn) = self.conns.get_mut(&id) else { return Ok(()) };
        let (want_a, want_b) = conn.interest();
        let (a, b) = (conn.a.as_raw_fd(), conn.b.as_ref().map(|b| b.as_raw_fd()));
        let (had_a, had_b) = (conn.watched_a, conn.watched_b);
        conn.watched_a = want_a;
        conn.watched_b = want_b;
        if want_a != had_a {
            self.watch(libc::EPOLL_CTL_MOD, a, id << 1, want_a)?;
        }
        if let Some(b) = b
            && want_b != had_b
        {
            self.watch(libc::EPOLL_CTL_MOD, b, id << 1 | 1, want_b)?;
        }
        Ok(())
    }

    fn drive(&mut self, id: u64, now: Instant) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        let before = conn.b.as_ref().map(|b| b.as_raw_fd());
        let step = conn.drive(&self.route, &self.env, now);
        let after = conn.b.as_ref().map(|b| b.as_raw_fd());
        let armed = conn.at;
        if step == Step::Wait
            && let Some(fd) = after
            && (after != before || conn.b_new)
        {
            conn.b_new = false;
            conn.watched_b = conn.interest().1;
            let events = conn.watched_b;
            if let Err(e) = self.watch(libc::EPOLL_CTL_ADD, fd, id << 1 | 1, events) {
                ran_out("epoll watches", &e);
                self.close(id);
                return;
            }
            // Edges before the watch were missed; look again now.
            return self.drive(id, now);
        }
        match step {
            Step::Wait => {
                if let Err(e) = self.rewatch(id) {
                    ran_out("epoll watches", &e);
                    self.close(id);
                    return;
                }
                if let Some(at) = armed {
                    self.timers.push(Reverse((at, id)));
                }
            }
            Step::Close => self.close(id),
        }
    }

    fn close(&mut self, id: u64) {
        if let Some(conn) = self.conns.remove(&id) {
            conn.close(&self.env);
        }
    }
}

/// What an accepted connection is: one to carry, or a stream to an
/// accelerator port that waits for the gate first.
enum Accepted {
    Ready(Conn),
    Gated { a: OwnedFd, dst: SocketAddr, kind: &'static str, peer: std::net::IpAddr },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Wait,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Inbound: reading the header the Mac sends first.
    Header,
    /// The second socket's connect is in progress.
    Dialing,
    /// Outbound: the Mac timed out; dial again at `at`.
    Retry,
    /// Writing `owed` to the second socket.
    Owed,
    /// Inbound over v6: waiting to see whether Docker's proxy hangs up.
    V6Check,
    /// Waiting for the first byte either way.
    First,
    /// Joined in the kernel; waiting for both ends to finish.
    Joined,
    /// Carried here, a byte at a time.
    Copying,
}

struct Conn {
    /// The accepted side: the container's TCP, or the Mac's stream.
    a: OwnedFd,
    /// The side dialed for it.
    b: Option<OwnedFd>,
    /// Set when `b` is a fresh socket the loop has not watched yet.
    b_new: bool,
    /// The epoll masks each socket is watched with now.
    watched_a: u32,
    watched_b: u32,
    a_tcp: bool,
    b_tcp: bool,
    state: State,
    /// Bytes owed to `b` before anything else: the outbound header, or what
    /// followed the inbound header in the same packet.
    owed: Vec<u8>,
    owed_at: usize,
    /// Inbound: the header so far, and what followed it, kept for a v4 retry.
    head: Vec<u8>,
    queued: Vec<u8>,
    dst: Option<SocketAddr>,
    client: Option<SocketAddr>,
    as_client: bool,
    tried_v4: bool,
    dial_started: Instant,
    pause: Duration,
    attempts: u32,
    /// The state's own deadline, if it has one.
    at: Option<Instant>,
    /// Keeps the vCPUs polling while an accelerator stream is open.
    wide: Option<crate::accelerator::Wide>,
    joined: Option<crate::sockmap::Joined>,
    pumps: Option<Box<[Pump; 2]>>,
}

impl Conn {
    fn new(a: OwnedFd, a_tcp: bool, state: State, now: Instant) -> Conn {
        Conn {
            a,
            b: None,
            b_new: false,
            watched_a: 0,
            watched_b: 0,
            a_tcp,
            b_tcp: false,
            state,
            owed: Vec::new(),
            owed_at: 0,
            head: Vec::new(),
            queued: Vec::new(),
            dst: None,
            client: None,
            as_client: false,
            tried_v4: false,
            dial_started: now,
            pause: Duration::from_millis(250),
            attempts: 0,
            at: None,
            wide: None,
            joined: None,
            pumps: None,
        }
    }

    /// A container's stream, to the Mac.
    fn outbound(a: OwnedFd, dst: SocketAddr, env: &Env, now: Instant) -> Option<Conn> {
        keep_alive(a.as_raw_fd());
        let mut conn = Conn::new(a, true, State::Dialing, now);
        conn.dst = Some(dst);
        conn.wide = crate::accelerator::Wide::open(dst.port());
        conn.dial_host(env).then_some(conn)
    }

    fn accepted(a: OwnedFd, route: &Route, env: &Env, now: Instant) -> Option<Accepted> {
        match route {
            Route::Outbound => {
                let dst = (env.destination)(a.as_raw_fd())?;
                // An accelerator port is reached only by a container that
                // asked for the device, which the gate decides.
                if let Some(kind) = crate::accelerator::kind_of(dst.port()) {
                    let peer = peer_ip(a.as_raw_fd())?;
                    return Some(Accepted::Gated { a, dst, kind, peer });
                }
                Conn::outbound(a, dst, env, now).map(Accepted::Ready)
            }
            Route::Inbound => {
                let _ = crate::vsock::set_buffer(&a, crate::STREAM_WINDOW);
                Some(Accepted::Ready(Conn::new(a, false, State::Header, now)))
            }
            Route::Unix(path) => {
                let mut conn = Conn::new(a, false, State::Dialing, now);
                match unix_socket(path) {
                    Ok(b) => {
                        conn.set_b(b, false);
                        Some(Accepted::Ready(conn))
                    }
                    Err(e) => {
                        // The errno is the whole diagnosis: "no such file"
                        // means the daemon has not made its socket yet,
                        // "connection refused" that it died after making one.
                        eprintln!("lighter-agent: cannot reach {}: {e}", path.display());
                        None
                    }
                }
            }
        }
    }

    /// What each socket is watched for in this state; hang-ups and errors
    /// are reported whatever the mask. A joined stream's bytes move in the
    /// kernel, and a watch for them would wake this thread for every packet
    /// only to find nothing to do: on the M1 that cost joined streams 2 to 4%
    /// of their throughput and a kept-alive request 15 µs.
    fn interest(&self) -> (u32, u32) {
        const READ: u32 = (libc::EPOLLIN | libc::EPOLLRDHUP) as u32;
        const WRITE: u32 = libc::EPOLLOUT as u32;
        const END: u32 = libc::EPOLLRDHUP as u32;
        match self.state {
            State::Header => (READ, 0),
            State::Dialing | State::Owed => (0, WRITE),
            State::Retry => (0, 0),
            State::V6Check => (0, READ),
            State::First => (READ, READ),
            State::Joined => (END, END),
            State::Copying => (READ | WRITE, READ | WRITE),
        }
    }

    fn set_b(&mut self, b: OwnedFd, tcp: bool) {
        self.b = Some(b);
        self.b_new = true;
        self.b_tcp = tcp;
    }

    fn dial_host(&mut self, env: &Env) -> bool {
        match (env.dial_host)() {
            Ok(b) => {
                self.set_b(b, false);
                self.state = State::Dialing;
                true
            }
            Err(e) => self.host_failed(env, e),
        }
    }

    /// A failed dial of the Mac: a timeout is retried for the window, with a
    /// growing pause, because a paging host answers nothing for tens of
    /// seconds and then everything; anything else refuses the connection.
    fn host_failed(&mut self, env: &Env, e: io::Error) -> bool {
        self.b = None;
        let now = Instant::now();
        if e.raw_os_error() == Some(libc::ETIMEDOUT) && now.duration_since(self.dial_started) < env.host_window {
            self.attempts += 1;
            self.at = Some(now + self.pause);
            self.pause = (self.pause * 2).min(Duration::from_secs(4));
            self.state = State::Retry;
            return true;
        }
        eprintln!("lighter-agent: stream to host refused: {e}");
        if e.raw_os_error() == Some(libc::ETIMEDOUT) {
            crate::host_lost();
        }
        false
    }

    /// Runs the connection until it has to wait, or is over.
    fn drive(&mut self, route: &Route, env: &Env, now: Instant) -> Step {
        loop {
            let next = match self.state {
                State::Header => self.header(),
                State::Dialing => self.dialing(route, env, now),
                State::Retry => {
                    if self.at.is_some_and(|at| now < at) {
                        return Step::Wait;
                    }
                    self.at = None;
                    if self.dial_host(env) { Some(Step::Wait) } else { Some(Step::Close) }
                }
                State::Owed => self.owe(route, now),
                State::V6Check => self.v6_check(now),
                State::First => self.first(env),
                State::Joined => self.joined(),
                State::Copying => self.copying(now),
            };
            match next {
                // A transition: run the new state at once, since the edges
                // it waits for may already have passed.
                Some(Step::Wait) if self.b_new => return Step::Wait,
                Some(Step::Wait) => continue,
                None => return Step::Wait,
                Some(Step::Close) => return Step::Close,
            }
        }
    }

    // Each state returns None to wait, Some(Wait) after a transition, and
    // Some(Close) when the connection is over.

    fn header(&mut self) -> Option<Step> {
        let need = if self.head.first().is_some_and(|f| f & HEADER_CLIENT != 0) { 38 } else { 19 };
        while self.head.len() < need {
            let mut buf = [0u8; 38];
            // Only what the header still needs: whatever follows stays queued
            // for the connection.
            let want = need - self.head.len();
            match read(self.a.as_raw_fd(), &mut buf[..want]) {
                Ok(0) => return Some(Step::Close),
                Ok(n) => self.head.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                Err(_) => return Some(Step::Close),
            }
            if self.head.len() == 19 && need == 19 && self.head[0] & HEADER_CLIENT != 0 {
                return Some(Step::Wait);
            }
        }
        let Some(dst) = crate::udp::destination_from(&self.head[..19]) else { return Some(Step::Close) };
        self.dst = Some(dst);
        if need == 38 {
            self.client = crate::udp::destination_from(&self.head[19..38]).filter(|c| c.is_ipv4() == dst.is_ipv4());
        }
        Some(self.dial_target(dst, self.client))
    }

    /// Dials a published port: as the client when the Mac passed one on, so
    /// the container sees who is calling; as the guest otherwise, or when
    /// that fails.
    fn dial_target(&mut self, dst: SocketAddr, client: Option<SocketAddr>) -> Step {
        if let Some(c) = client {
            match crate::udp::as_client(libc::SOCK_STREAM, c)
                .ok_or_else(|| io::Error::other("cannot act as the client"))
                .and_then(|fd| { set_nonblocking(fd.as_raw_fd())?; start_connect(&fd, dst).map(|_| fd) })
            {
                Ok(fd) => {
                    self.as_client = true;
                    self.set_b(fd, true);
                    self.state = State::Dialing;
                    return Step::Wait;
                }
                Err(e) => eprintln!("lighter-agent: inbound to {dst} as {c}: {e}; as the guest"),
            }
        }
        self.as_client = false;
        match tcp_socket(dst).and_then(|fd| start_connect(&fd, dst).map(|_| fd)) {
            Ok(fd) => {
                self.set_b(fd, true);
                self.state = State::Dialing;
                Step::Wait
            }
            Err(e) => self.target_failed(dst, e),
        }
    }

    fn target_failed(&mut self, dst: SocketAddr, e: io::Error) -> Step {
        self.b = None;
        if e.kind() == io::ErrorKind::ConnectionRefused && dst.is_ipv6() {
            return self.dial_v4("refused");
        }
        eprintln!("lighter-agent: inbound to {dst} refused: {e}");
        Step::Close
    }

    /// Docker refuses a v6 publish for a server that binds `0.0.0.0` only,
    /// which is most of them, and on a Mac `localhost` is `::1` first. The
    /// same port on this interface's v4 address is its v4 mapping, where the
    /// server is (`inbound::v4_sibling`).
    fn dial_v4(&mut self, why: &str) -> Step {
        let Some(dst) = self.dst else { return Step::Close };
        if self.tried_v4 {
            return Step::Close;
        }
        self.tried_v4 = true;
        let Some(alt) = crate::inbound::v4_sibling(dst, &crate::interfaces()) else {
            eprintln!("lighter-agent: inbound to {dst} {why}");
            return Step::Close;
        };
        static ANNOUNCED: std::sync::Mutex<std::collections::BTreeSet<u16>> =
            std::sync::Mutex::new(std::collections::BTreeSet::new());
        if ANNOUNCED.lock().map(|mut a| a.insert(dst.port())).unwrap_or(false) {
            println!("AGENT inbound port={} v6 {why}; dialling v4 {}", dst.port(), alt.ip());
        }
        self.dst = Some(alt);
        self.as_client = false;
        match tcp_socket(alt).and_then(|fd| start_connect(&fd, alt).map(|_| fd)) {
            Ok(fd) => {
                self.set_b(fd, true);
                self.state = State::Dialing;
                Step::Wait
            }
            Err(e) => {
                eprintln!("lighter-agent: inbound to {dst} {why}; and to {alt}: {e}");
                Step::Close
            }
        }
    }

    fn dialing(&mut self, route: &Route, env: &Env, now: Instant) -> Option<Step> {
        let b = self.b.as_ref()?.as_raw_fd();
        match connect_result(b) {
            None => None,
            Some(Ok(())) => Some(self.connected(route, env, now)),
            Some(Err(e)) => Some(match route {
                Route::Outbound => {
                    if self.host_failed(env, e) { Step::Wait } else { Step::Close }
                }
                Route::Inbound if self.as_client => {
                    let (dst, c) = (self.dst?, self.client?);
                    eprintln!("lighter-agent: inbound to {dst} as {c}: {e}; as the guest");
                    self.client = None;
                    self.dial_target(dst, None)
                }
                Route::Inbound => {
                    let dst = self.dst?;
                    self.target_failed(dst, e)
                }
                Route::Unix(path) => {
                    eprintln!("lighter-agent: cannot reach {}: {e}", path.display());
                    Step::Close
                }
            }),
        }
    }

    fn connected(&mut self, route: &Route, env: &Env, now: Instant) -> Step {
        let _ = env;
        let b = self.b.as_ref().expect("connected with a socket");
        match route {
            Route::Outbound => {
                if self.attempts > 0 {
                    eprintln!(
                        "lighter-agent: stream to host connected after {} attempts over {:.1}s",
                        self.attempts + 1,
                        now.duration_since(self.dial_started).as_secs_f64()
                    );
                }
                crate::HOST_TIMEOUTS.store(0, std::sync::atomic::Ordering::Relaxed);
                let _ = crate::vsock::set_buffer(b, crate::STREAM_WINDOW);
                self.owed = header(self.dst.expect("outbound has a destination"));
                self.owed_at = 0;
                nodelay(self.a.as_raw_fd());
                crate::ends_with_its_peer(self.a.as_raw_fd());
                if self.wide.is_some() {
                    crate::accelerator::mark(self.a.as_raw_fd());
                    crate::accelerator::mark(b.as_raw_fd());
                }
                self.state = State::Owed;
            }
            Route::Inbound => {
                nodelay(b.as_raw_fd());
                crate::ends_with_its_peer(b.as_raw_fd());
                // Whatever followed the header in the same packet is carried
                // by hand before the join. The join hands sockmap whole
                // socket buffers, and a buffer the header was read out of
                // still holds the header: joined as it stands, the container
                // would see the port bytes ahead of the request (it did, an
                // HTTP server answered 501 to `;\xc7GET`).
                if !self.tried_v4 || self.queued.is_empty() {
                    match take_queued(self.a.as_raw_fd()) {
                        Ok(q) => self.queued.extend_from_slice(&q),
                        Err(_) => return Step::Close,
                    }
                }
                self.owed = self.queued.clone();
                self.owed_at = 0;
                self.state = State::Owed;
            }
            Route::Unix(_) => self.state = State::Copying,
        }
        Step::Wait
    }

    fn owe(&mut self, route: &Route, now: Instant) -> Option<Step> {
        let b = self.b.as_ref()?.as_raw_fd();
        while self.owed_at < self.owed.len() {
            match write(b, &self.owed[self.owed_at..]) {
                Ok(n) => self.owed_at += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                Err(_) => return Some(Step::Close),
            }
        }
        self.owed.clear();
        self.state = match route {
            Route::Inbound if self.dst.is_some_and(|d| d.is_ipv6()) && !self.tried_v4 => {
                self.at = Some(now + V6_HANGUP_WAIT);
                State::V6Check
            }
            _ => State::First,
        };
        Some(Step::Wait)
    }

    /// Whether the peer closed within a moment of connecting, before sending
    /// a byte: Docker's proxy hanging up on a v6 publish it could not
    /// complete. A slow server costs the join fifty milliseconds and its
    /// reply nothing, since the reply waits in the socket.
    fn v6_check(&mut self, now: Instant) -> Option<Step> {
        let b = self.b.as_ref()?.as_raw_fd();
        let ready = poll_now(&[b], libc::POLLIN | libc::POLLRDHUP)[0];
        if ready == 0 {
            if self.at.is_some_and(|at| now < at) {
                return None;
            }
            self.at = None;
            self.state = State::First;
            return Some(Step::Wait);
        }
        self.at = None;
        let mut byte = 0u8;
        // SAFETY: a one-byte peek into a live socket.
        let n = unsafe { libc::recv(b, std::ptr::addr_of_mut!(byte).cast(), 1, libc::MSG_PEEK) };
        let hung_up = n == 0 || (n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::ConnectionReset);
        if hung_up {
            return Some(self.dial_v4("hung up before answering"));
        }
        self.state = State::First;
        Some(Step::Wait)
    }

    /// Nothing is joined until a byte exists to carry: a connection opened
    /// and closed (a probe, a health check) costs the join's ten syscalls
    /// nothing, and one opened and left costs no psock.
    fn first(&mut self, env: &Env) -> Option<Step> {
        let b = self.b.as_ref()?.as_raw_fd();
        let a = self.a.as_raw_fd();
        let r = poll_now(&[a, b], libc::POLLIN | libc::POLLRDHUP);
        if r[0] == 0 && r[1] == 0 {
            return None;
        }
        let ended = libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
        let data = (r[0] | r[1]) & libc::POLLIN != 0;
        // The Mac's end of a stream that ended before the join never reaches
        // the kernel's marker (it is queued only for a socket already joined).
        // With nothing to carry the stream is over; with bytes queued the
        // copying path carries them. The Mac's end is `b` outbound and `a`
        // inbound.
        let host_ended = if self.a_tcp { r[1] & ended != 0 } else { r[0] & ended != 0 };
        if host_ended {
            if !data {
                return Some(Step::Close);
            }
            self.state = State::Copying;
            return Some(Step::Wait);
        }
        let Some(joiner) = env.joiner else {
            self.state = State::Copying;
            return Some(Step::Wait);
        };
        // The TCP socket first. A server that answers and closes at once
        // leaves it in CLOSE_WAIT, which sockmap refuses to attach; refused
        // first, the join fails before any verdict is live and the stream
        // is copied. Refused second, after the vsock's verdict went live, it
        // can only be closed, and the client gets an empty reply (27 in 600
        // requests to a published HTTP/1.0 server, until this order).
        let (tcp, other) = if self.a_tcp { (a, b) } else { (b, a) };
        match joiner.join(tcp, other) {
            Ok(joined) => {
                self.joined = Some(joined);
                self.state = State::Joined;
            }
            Err(failure) => {
                // A peer can close between polling and map insertion. Those
                // ordinary copying fallbacks must not produce a log per
                // request.
                static FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let count = FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if count.is_power_of_two() || !failure.can_fallback {
                    eprintln!("lighter-agent: sockmap join failed (count={count}, copying={}): {}",
                        failure.can_fallback, failure.error);
                }
                if failure.can_fallback {
                    if let Some(joined) = failure.joined {
                        joiner.release(joined);
                    }
                    self.state = State::Copying;
                } else {
                    // Closed before the slots go back (`close`).
                    self.joined = failure.joined;
                    return Some(Step::Close);
                }
            }
        }
        Some(Step::Wait)
    }

    /// A socket reports HUP once both its directions are shut: its peer's end
    /// seen, and its own sent by the kernel behind the redirected bytes. HUP
    /// on both means neither backlog holds anything. An error on either is an
    /// abort, and the other side is closed with whatever it has.
    fn joined(&mut self) -> Option<Step> {
        let b = self.b.as_ref()?.as_raw_fd();
        let r = poll_now(&[self.a.as_raw_fd(), b], libc::POLLRDHUP);
        if (r[0] | r[1]) & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Some(Step::Close);
        }
        if r[0] & libc::POLLHUP != 0 && r[1] & libc::POLLHUP != 0 {
            return Some(Step::Close);
        }
        None
    }

    fn copying(&mut self, now: Instant) -> Option<Step> {
        let b = self.b.as_ref()?.as_raw_fd();
        let a = self.a.as_raw_fd();
        let (a_tcp, b_tcp) = (self.a_tcp, self.b_tcp);
        let pumps = self.pumps.get_or_insert_with(|| Box::new([Pump::new(a_tcp), Pump::new(b_tcp)]));
        let forward = pumps[0].step(a, b, now);
        let back = pumps[1].step(b, a, now);
        if forward == Pumped::Abort || back == Pumped::Abort {
            return Some(Step::Close);
        }
        if forward == Pumped::Done && back == Pumped::Done {
            return Some(Step::Close);
        }
        // One direction done and the other waiting on its reader: if either
        // socket has failed, as a container's does once keepalive finds it
        // gone, nothing more can arrive or be delivered, and waiting would
        // hold the stream as long as the far end keeps its side open.
        if (forward == Pumped::Done || back == Pumped::Done)
            && poll_now(&[a, b], 0).iter().any(|r| r & (libc::POLLERR | libc::POLLNVAL) != 0)
        {
            return Some(Step::Close);
        }
        self.at = [forward, back].iter().filter_map(|p| if let Pumped::RetryAt(t) = p { Some(*t) } else { None }).min();
        None
    }

    fn close(self, env: &Env) {
        let joined = self.joined;
        // Closed before the slots go back: a closed socket has left the maps,
        // and a slot handed out while its last socket is still in one would
        // replace it.
        drop(self.pumps);
        drop(self.b);
        drop(self.a);
        drop(self.wide);
        if let (Some(joined), Some(joiner)) = (joined, env.joiner) {
            joiner.release(joined);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pumped {
    /// Waiting for readiness.
    Blocked,
    /// A write was refused for memory; try again then.
    RetryAt(Instant),
    /// This direction has ended and been passed on.
    Done,
    Abort,
}

/// One direction of a copied stream. From a TCP socket the bytes go through
/// a pipe with `splice` and never enter this process; a vsock or unix socket
/// cannot be spliced from, and goes through a buffer.
struct Pump {
    buf: Vec<u8>,
    lo: usize,
    hi: usize,
    pipe: Option<(OwnedFd, OwnedFd)>,
    in_pipe: usize,
    splice: bool,
    eof: bool,
    done: bool,
    refused_since: Option<Instant>,
    pause: Duration,
    retry_at: Option<Instant>,
    said: bool,
}

impl Pump {
    fn new(from_tcp: bool) -> Pump {
        Pump {
            buf: Vec::new(),
            lo: 0,
            hi: 0,
            pipe: None,
            in_pipe: 0,
            splice: from_tcp,
            eof: false,
            done: false,
            refused_since: None,
            pause: Duration::from_millis(1),
            retry_at: None,
            said: false,
        }
    }

    fn step(&mut self, src: RawFd, dst: RawFd, now: Instant) -> Pumped {
        loop {
            if self.done {
                return Pumped::Done;
            }
            if let Some(at) = self.retry_at {
                if now < at {
                    return Pumped::RetryAt(at);
                }
                self.retry_at = None;
            }
            if self.in_pipe > 0 || self.hi > self.lo {
                match self.flush(dst) {
                    Ok(()) => {
                        self.refused_since = None;
                        self.pause = Duration::from_millis(1);
                        continue;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Pumped::Blocked,
                    Err(e) if e.kind() == io::ErrorKind::OutOfMemory || e.raw_os_error() == Some(libc::ENOBUFS) => {
                        // A write refused for want of memory (a 256 KiB vsock
                        // packet is one linear allocation, and a fragmented
                        // guest refuses those before it is out of memory) is
                        // retried with a growing pause rather than taken as
                        // the end of the stream. Only the peer ends a stream.
                        let since = *self.refused_since.get_or_insert(now);
                        if now.duration_since(since) > REFUSED_WINDOW {
                            eprintln!("lighter-agent: a write refused for {}s; giving the stream up: {e}", REFUSED_WINDOW.as_secs());
                            return Pumped::Abort;
                        }
                        if !self.said {
                            eprintln!("lighter-agent: a write refused for memory; retrying: {e}");
                            self.said = true;
                        }
                        let at = now + self.pause;
                        self.pause = (self.pause * 2).min(Duration::from_millis(100));
                        self.retry_at = Some(at);
                        return Pumped::RetryAt(at);
                    }
                    Err(_) => {
                        // The peer is gone for this direction; the other
                        // direction ends on its own terms.
                        self.finish(dst);
                        return Pumped::Done;
                    }
                }
            }
            if self.eof {
                self.finish(dst);
                return Pumped::Done;
            }
            match self.fill(src) {
                Ok(0) => self.eof = true,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Pumped::Blocked,
                Err(_) => self.eof = true,
            }
        }
    }

    fn finish(&mut self, dst: RawFd) {
        self.done = true;
        // SAFETY: a live descriptor; shutdown of the write half only, so the
        // other direction stays open.
        unsafe { libc::shutdown(dst, libc::SHUT_WR) };
    }

    fn fill(&mut self, src: RawFd) -> io::Result<usize> {
        if self.splice {
            if self.pipe.is_none() {
                match pipe() {
                    Ok(p) => self.pipe = Some(p),
                    Err(e) => {
                        ran_out("pipes", &e);
                        self.splice = false;
                    }
                }
            }
            if let Some((_, w)) = &self.pipe {
                // SAFETY: live descriptors; null offsets for sockets and pipes.
                let n = unsafe {
                    libc::splice(src, std::ptr::null_mut(), w.as_raw_fd(), std::ptr::null_mut(), PIPE,
                        libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK)
                };
                if n >= 0 {
                    self.in_pipe += n as usize;
                    return Ok(n as usize);
                }
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EINVAL) {
                    return Err(e);
                }
                // A socket that refuses splice goes through the buffer.
                self.splice = false;
                self.pipe = None;
            }
        }
        if self.buf.is_empty() {
            self.buf = vec![0u8; BUFFER];
        }
        let n = read(src, &mut self.buf)?;
        self.lo = 0;
        self.hi = n;
        Ok(n)
    }

    fn flush(&mut self, dst: RawFd) -> io::Result<()> {
        while self.in_pipe > 0 {
            let (r, _) = self.pipe.as_ref().expect("bytes in a pipe");
            // SAFETY: as in `fill`.
            let n = unsafe {
                libc::splice(r.as_raw_fd(), std::ptr::null_mut(), dst, std::ptr::null_mut(), self.in_pipe,
                    libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK)
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            self.in_pipe -= n as usize;
        }
        while self.lo < self.hi {
            let n = write(dst, &self.buf[self.lo..self.hi])?;
            self.lo += n;
        }
        Ok(())
    }
}

/// When something runs out, the log names what and how much of everything
/// there was, once a minute at most: the next incident then says its own
/// cause.
fn ran_out(what: &str, e: &io::Error) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    let refused = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    let now = crate::monotonic_ns() as u64 / 1_000_000_000;
    let last = LAST.load(Ordering::Relaxed);
    if last != 0 && now < last + 60 {
        return;
    }
    LAST.store(now.max(1), Ordering::Relaxed);
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default().trim().to_owned();
    let tasks = read("/proc/loadavg").split_whitespace().nth(3).and_then(|t| t.split('/').nth(1)).unwrap_or("?").to_owned();
    let open = std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0);
    eprintln!(
        "lighter-agent: out of {what} ({e}); refusing the connection ({refused} so far). tasks {tasks} of threads-max {}, pid_max {}, this process's descriptors {open}",
        read("/proc/sys/kernel/threads-max"),
        read("/proc/sys/kernel/pid_max"),
    );
}

fn open_spare() -> Option<OwnedFd> {
    std::fs::File::open("/dev/null").ok().map(OwnedFd::from)
}

fn header(dst: SocketAddr) -> Vec<u8> {
    let mut header = vec![0u8; 19];
    match dst.ip() {
        std::net::IpAddr::V4(a) => {
            header[0] = 4;
            header[1..5].copy_from_slice(&a.octets());
        }
        std::net::IpAddr::V6(a) => {
            header[0] = 6;
            header[1..17].copy_from_slice(&a.octets());
        }
    }
    header[17..19].copy_from_slice(&dst.port().to_be_bytes());
    header
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a live descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn nodelay(fd: RawFd) {
    let one: libc::c_int = 1;
    // SAFETY: an int-sized option on a live socket.
    unsafe {
        libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, std::ptr::addr_of!(one).cast(),
            size_of::<libc::c_int>() as libc::socklen_t)
    };
}

/// Probes a container's side of an outbound stream once it falls quiet. A
/// container that closes its end leaves the stream half closed, which is
/// also what a container that only shut down its writes looks like, and the
/// far end may never close its own (#57: Home Assistant's polls of devices
/// that keep idle connections open, until the agent ran out of descriptors).
/// A socket shut for writing still answers the probes; a closed one stops
/// existing once its kernel stops waiting for this side's FIN (a minute),
/// the next probe draws a reset, and the error ends the stream.
fn keep_alive(fd: RawFd) {
    let one: libc::c_int = 1;
    let [idle, interval, count] = KEEPALIVE;
    // SAFETY: int-sized options on a live socket.
    unsafe {
        for (level, name, value) in [
            (libc::SOL_SOCKET, libc::SO_KEEPALIVE, one),
            (libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, idle),
            (libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, interval),
            (libc::IPPROTO_TCP, libc::TCP_KEEPCNT, count),
        ] {
            libc::setsockopt(fd, level, name, std::ptr::addr_of!(value).cast(),
                size_of::<libc::c_int>() as libc::socklen_t);
        }
    }
}

fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: reading into a buffer we own, with its true length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    loop {
        // SAFETY: writing from a buffer we own, with its true length.
        let n = unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ENOTSOCK) {
            // SAFETY: as above, for a descriptor that is not a socket.
            let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
            return if n >= 0 { Ok(n as usize) } else { Err(io::Error::last_os_error()) };
        }
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Each descriptor's current readiness, without waiting.
fn poll_now<const N: usize>(fds: &[RawFd; N], events: libc::c_short) -> [libc::c_short; N] {
    let mut p = fds.map(|fd| libc::pollfd { fd, events, revents: 0 });
    // SAFETY: live descriptors in a pollfd array of the length given.
    unsafe { libc::poll(p.as_mut_ptr(), N as libc::nfds_t, 0) };
    p.map(|p| p.revents)
}

/// Whether a non-blocking connect has finished: None while it is still in
/// progress.
fn connect_result(fd: RawFd) -> Option<io::Result<()>> {
    let mut err: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: an int for SO_ERROR to fill.
    unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, std::ptr::addr_of_mut!(err).cast(), &mut len) };
    if err != 0 {
        return Some(Err(io::Error::from_raw_os_error(err)));
    }
    if poll_now(&[fd], libc::POLLOUT)[0] & (libc::POLLOUT | libc::POLLHUP) != 0 {
        return Some(Ok(()));
    }
    None
}

fn peer_ip(fd: RawFd) -> Option<std::net::IpAddr> {
    // SAFETY: a zeroed sockaddr_storage is a valid out-parameter.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: the buffer and its length.
    if unsafe { libc::getpeername(fd, std::ptr::addr_of_mut!(addr).cast(), &mut len) } < 0 {
        return None;
    }
    sockaddr_to(&addr).map(|a| a.ip())
}

fn sockaddr_to(addr: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match addr.ss_family as libc::c_int {
        libc::AF_INET => {
            // SAFETY: the family says which sockaddr this is.
            let a = unsafe { &*(addr as *const libc::sockaddr_storage as *const libc::sockaddr_in) };
            Some(SocketAddr::new(std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)).into(), u16::from_be(a.sin_port)))
        }
        libc::AF_INET6 => {
            // SAFETY: as above.
            let a = unsafe { &*(addr as *const libc::sockaddr_storage as *const libc::sockaddr_in6) };
            let ip = std::net::Ipv6Addr::from(a.sin6_addr.s6_addr);
            Some(SocketAddr::new(ip.to_ipv4_mapped().map_or(ip.into(), Into::into), u16::from_be(a.sin6_port)))
        }
        _ => None,
    }
}

fn tcp_socket(dst: SocketAddr) -> io::Result<OwnedFd> {
    let family = if dst.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    // SAFETY: a plain socket(2) call.
    let raw = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        let e = io::Error::last_os_error();
        ran_out("sockets", &e);
        return Err(e);
    }
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Starts a non-blocking connect; its end is read with [`connect_result`].
fn start_connect(fd: &OwnedFd, dst: SocketAddr) -> io::Result<()> {
    let (storage, len) = sockaddr_from(dst);
    // SAFETY: a sockaddr of the length given, living for the call.
    let rc = unsafe { libc::connect(fd.as_raw_fd(), std::ptr::addr_of!(storage).cast(), len) };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EINPROGRESS) {
        return Ok(());
    }
    Err(e)
}

fn sockaddr_from(dst: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: zeroed storage is valid for either family.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match dst {
        SocketAddr::V4(a) => {
            // SAFETY: storage is large enough for a sockaddr_in.
            let s = unsafe { &mut *(std::ptr::addr_of_mut!(storage) as *mut libc::sockaddr_in) };
            s.sin_family = libc::AF_INET as libc::sa_family_t;
            s.sin_port = a.port().to_be();
            s.sin_addr.s_addr = u32::from(*a.ip()).to_be();
            size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            // SAFETY: storage is large enough for a sockaddr_in6.
            let s = unsafe { &mut *(std::ptr::addr_of_mut!(storage) as *mut libc::sockaddr_in6) };
            s.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            s.sin6_port = a.port().to_be();
            s.sin6_addr.s6_addr = a.ip().octets();
            s.sin6_scope_id = a.scope_id();
            size_of::<libc::sockaddr_in6>()
        }
    };
    (storage, len as libc::socklen_t)
}

fn unix_socket(path: &std::path::Path) -> io::Result<OwnedFd> {
    // SAFETY: a plain socket(2) call.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        let e = io::Error::last_os_error();
        ran_out("sockets", &e);
        return Err(e);
    }
    // SAFETY: a fresh descriptor we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: zeroed storage is a valid sockaddr_un.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    // SAFETY: a sockaddr_un living for the call, with its size.
    let rc = unsafe {
        libc::connect(fd.as_raw_fd(), std::ptr::addr_of!(addr).cast(), size_of::<libc::sockaddr_un>() as libc::socklen_t)
    };
    if rc < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
    }
    Ok(fd)
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: a two-int array for pipe2 to fill.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors we own.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // The pipe's capacity bounds each splice; four megabytes keeps the two
    // calls per chunk from being the cost.
    // SAFETY: F_SETPIPE_SZ with an int argument.
    unsafe { libc::fcntl(w.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE as libc::c_int) };
    Ok((r, w))
}

/// The bytes queued on `from` right now, taken off it.
fn take_queued(from: RawFd) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match read(from, &mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(out),
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};

    fn run(mut lp: Loop) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || loop {
            lp.turn(Some(Duration::from_millis(50)));
        })
    }

    fn listener() -> (OwnedFd, u16) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        (OwnedFd::from(l), port)
    }

    /// The Mac, for the outbound route: reads the 19-byte header, then echoes.
    static HOST_PORT: AtomicU16 = AtomicU16::new(0);
    static REFUSE_FIRST: AtomicU32 = AtomicU32::new(0);

    fn fake_host() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                std::thread::spawn(move || {
                    let mut head = [0u8; 19];
                    if s.read_exact(&mut head).is_err() {
                        return;
                    }
                    assert_eq!(&head[..5], &[4, 10, 1, 2, 3]);
                    assert_eq!(u16::from_be_bytes([head[17], head[18]]), 8080);
                    let mut buf = [0u8; 4096];
                    loop {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if s.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = s.shutdown(std::net::Shutdown::Write);
                });
            }
        });
        port
    }

    fn dial_fake_host() -> io::Result<OwnedFd> {
        if REFUSE_FIRST.load(Ordering::SeqCst) > 0 {
            REFUSE_FIRST.fetch_sub(1, Ordering::SeqCst);
            return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
        }
        let dst: SocketAddr = format!("127.0.0.1:{}", HOST_PORT.load(Ordering::SeqCst)).parse().unwrap();
        let fd = tcp_socket(dst)?;
        start_connect(&fd, dst)?;
        Ok(fd)
    }

    fn to_10_1_2_3(_: RawFd) -> Option<SocketAddr> {
        Some("10.1.2.3:8080".parse().unwrap())
    }

    fn outbound_env() -> Env {
        Env { dial_host: dial_fake_host, destination: to_10_1_2_3, joiner: None, host_window: Duration::from_secs(5) }
    }

    fn round_trip(port: u16, message: &[u8]) -> Vec<u8> {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        c.write_all(message).unwrap();
        c.shutdown(std::net::Shutdown::Write).unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        got
    }

    /// One test for the fake host's port and the refusal counter, which are
    /// process-wide: the outbound cases run in order.
    #[test]
    fn outbound_streams_carry_the_header_and_both_directions() {
        HOST_PORT.store(fake_host(), Ordering::SeqCst);
        let (l, port) = listener();
        let _t = run(Loop::new(l, Route::Outbound, outbound_env()).unwrap());

        // A request and its reply, half-closed from the container's side.
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(round_trip(port, &big), big, "a 3 MB stream both ways");

        // A connection opened and closed before its first byte.
        drop(TcpStream::connect(("127.0.0.1", port)).unwrap());

        // Many at once, all on the one loop thread.
        let clients: Vec<_> = (0..500)
            .map(|i| std::thread::spawn(move || round_trip(port, format!("hello {i}").as_bytes())))
            .collect();
        for (i, c) in clients.into_iter().enumerate() {
            assert_eq!(c.join().unwrap(), format!("hello {i}").as_bytes());
        }

        // A Mac that times out twice is dialled again, and the stream carries.
        REFUSE_FIRST.store(2, Ordering::SeqCst);
        let started = Instant::now();
        assert_eq!(round_trip(port, b"late"), b"late");
        assert!(started.elapsed() >= Duration::from_millis(700), "retried after 250 and 500 ms");
    }

    /// The Mac's side of an inbound stream: the header naming a published
    /// port, then the request, in one write as a packet would bring them.
    #[test]
    fn inbound_streams_dial_the_header_and_keep_what_followed_it() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let dst = server.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in server.incoming() {
                let Ok(mut s) = s else { continue };
                let mut got = Vec::new();
                let _ = s.read_to_end(&mut got);
                let _ = s.write_all(&got);
            }
        });
        let (l, port) = listener();
        let env = Env { dial_host: dial_fake_host, destination: to_10_1_2_3, joiner: None, host_window: Duration::from_secs(1) };
        let _t = run(Loop::new(l, Route::Inbound, env).unwrap());
        let mut message = header(dst);
        message.extend_from_slice(b"GET / HTTP/1.0\r\n\r\n");
        assert_eq!(round_trip(port, &message), b"GET / HTTP/1.0\r\n\r\n");
    }

    #[test]
    fn unix_streams_reach_the_socket_and_half_close() {
        let dir = std::env::temp_dir().join(format!("streams-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("docker.sock");
        let _ = std::fs::remove_file(&path);
        let server = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for s in server.incoming() {
                let Ok(mut s) = s else { continue };
                let mut got = Vec::new();
                let _ = s.read_to_end(&mut got);
                let _ = s.write_all(b"reply to ");
                let _ = s.write_all(&got);
            }
        });
        let (l, port) = listener();
        let env = Env { dial_host: dial_fake_host, destination: to_10_1_2_3, joiner: None, host_window: Duration::from_secs(1) };
        let _t = run(Loop::new(l, Route::Unix(path), env).unwrap());
        assert_eq!(round_trip(port, b"ping"), b"reply to ping");
    }

    /// File limits are per process: isolate the shortage from the other
    /// tests and put a timeout around a loop that used to shed indefinitely.
    #[test]
    fn descriptor_exhaustion_drains_finished_streams_before_shedding_more_clients() {
        use std::process::{Command, Stdio};
        const CHILD: &str = "LIGHTER_TEST_EMFILE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "streams::tests::descriptor_exhaustion_drains_finished_streams_before_shedding_more_clients", "--nocapture"])
                .env(CHILD, "1").stdout(Stdio::piped()).stderr(Stdio::piped())
                .spawn().unwrap();
            let started = Instant::now();
            while child.try_wait().unwrap().is_none() {
                if started.elapsed() > Duration::from_secs(10) {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!("stream loop did not recover: {}", String::from_utf8_lossy(&output.stderr));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{}\n{}",
                String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            let log = String::from_utf8_lossy(&output.stderr);
            assert!(log.contains("out of descriptors"), "the fixture must actually reach EMFILE");
            assert!(!log.contains("tasks ?"), "diagnostics need room to read /proc");
            assert!(!log.contains("this process's descriptors 0"), "descriptor count must remain available");
            return;
        }

        use std::os::unix::net::{UnixListener, UnixStream};
        let path = std::env::temp_dir().join(format!("lighter-emfile-{}.sock", std::process::id()));
        let server = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut request = Vec::new();
            socket.read_to_end(&mut request).unwrap();
            socket.write_all(b"reply to ").unwrap();
            socket.write_all(&request).unwrap();
        });
        let (listener, port) = listener();
        let mut lp = Loop::new(listener, Route::Unix(path.clone()), outbound_env()).unwrap();

        // A finished stream still owns two descriptors until the loop
        // drives it. Its peers have closed, so both directions can finish.
        let (a, peer_a) = UnixStream::pair().unwrap();
        let (b, peer_b) = UnixStream::pair().unwrap();
        drop(peer_a);
        drop(peer_b);
        set_nonblocking(a.as_raw_fd()).unwrap();
        set_nonblocking(b.as_raw_fd()).unwrap();
        let mut finished = Conn::new(a.into(), false, State::Copying, Instant::now());
        finished.b = Some(b.into());
        (finished.watched_a, finished.watched_b) = finished.interest();
        lp.watch(libc::EPOLL_CTL_ADD, finished.a.as_raw_fd(), 0, finished.watched_a).unwrap();
        lp.watch(libc::EPOLL_CTL_ADD, finished.b.as_ref().unwrap().as_raw_fd(), 1, finished.watched_b).unwrap();
        lp.conns.insert(0, finished);
        lp.next = 1;

        // Only one queued client must be refused. The next can be served
        // once the finished stream releases its descriptors.
        let _refused = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut queued = TcpStream::connect(("127.0.0.1", port)).unwrap();
        queued.write_all(b"ping").unwrap();
        queued.shutdown(std::net::Shutdown::Write).unwrap();
        queued.set_nonblocking(true).unwrap();

        // SAFETY: this test runs alone in its child process. Preserve the
        // hard limit; the child exits without affecting the parent.
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
        limit.rlim_cur = 64;
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut held = Vec::new();
        loop {
            match std::fs::File::open("/dev/null") {
                Ok(file) => held.push(file),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                    break;
                }
            }
        }
        lp.turn(Some(Duration::ZERO));
        assert!(lp.conns.is_empty(), "the finished stream must be closed under EMFILE");
        // Leave room for the fixture server's accept and copying path.
        for _ in 0..10 { held.pop(); }
        let started = Instant::now();
        let mut response = Vec::new();
        loop {
            lp.turn(Some(Duration::from_millis(10)));
            let mut buf = [0; 64];
            match queued.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("queued client was needlessly refused: {error}"),
            }
            assert!(started.elapsed() < Duration::from_secs(3), "queued client did not recover");
        }
        assert_eq!(response, b"reply to ping");
        server.join().unwrap();
        drop(lp);
        std::fs::remove_file(path).unwrap();
    }

    fn int_option(fd: RawFd, level: libc::c_int, name: libc::c_int) -> libc::c_int {
        let mut value: libc::c_int = -1;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: an int for the option to fill, on a live socket.
        assert_eq!(unsafe { libc::getsockopt(fd, level, name, std::ptr::addr_of_mut!(value).cast(), &mut len) }, 0);
        value
    }

    #[test]
    fn a_containers_side_is_probed_once_quiet() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let _c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (a, _) = l.accept().unwrap();
        keep_alive(a.as_raw_fd());
        let fd = a.as_raw_fd();
        assert_eq!(int_option(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE), 1);
        assert_eq!(int_option(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE), KEEPALIVE[0]);
        assert_eq!(int_option(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL), KEEPALIVE[1]);
        assert_eq!(int_option(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT), KEEPALIVE[2]);
    }

    /// A container that shuts down its writes keeps its stream: the far end
    /// may still answer. One whose socket then fails, as it does once the
    /// probes find it gone, loses the stream even though the far end never
    /// closes its own side (#57: four descriptors held per such stream,
    /// until the agent had none).
    #[test]
    fn a_half_closed_stream_ends_when_the_containers_side_fails() {
        use std::os::unix::net::UnixStream;
        let (listener, _) = listener();
        let mut lp = Loop::new(listener, Route::Outbound, outbound_env()).unwrap();

        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let container = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (a, _) = l.accept().unwrap();
        let (b, far) = UnixStream::pair().unwrap();
        set_nonblocking(a.as_raw_fd()).unwrap();
        set_nonblocking(b.as_raw_fd()).unwrap();
        let mut conn = Conn::new(a.into(), true, State::Copying, Instant::now());
        conn.b = Some(b.into());
        (conn.watched_a, conn.watched_b) = conn.interest();
        lp.watch(libc::EPOLL_CTL_ADD, conn.a.as_raw_fd(), 0, conn.watched_a).unwrap();
        lp.watch(libc::EPOLL_CTL_ADD, conn.b.as_ref().unwrap().as_raw_fd(), 1, conn.watched_b).unwrap();
        lp.conns.insert(0, conn);
        lp.next = 1;

        container.shutdown(std::net::Shutdown::Write).unwrap();
        for _ in 0..20 {
            lp.turn(Some(Duration::from_millis(10)));
        }
        assert_eq!(lp.conns.len(), 1, "a half-closed stream is still a stream");
        let mut eof = [0u8; 1];
        far.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!((&far).read(&mut eof).unwrap(), 0, "the far end is told the container is done");

        // Reset, as the container's kernel answers a probe once its socket
        // is gone.
        let linger = libc::linger { l_onoff: 1, l_linger: 0 };
        // SAFETY: a linger struct on a live socket.
        unsafe {
            libc::setsockopt(container.as_raw_fd(), libc::SOL_SOCKET, libc::SO_LINGER,
                std::ptr::addr_of!(linger).cast(), size_of::<libc::linger>() as libc::socklen_t);
        }
        drop(container);
        let started = Instant::now();
        while !lp.conns.is_empty() {
            lp.turn(Some(Duration::from_millis(10)));
            assert!(started.elapsed() < Duration::from_secs(3), "the stream outlived its container's socket");
        }
        drop(far);
    }
}

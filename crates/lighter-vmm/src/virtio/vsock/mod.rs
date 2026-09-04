//! virtio-vsock.
//!
//! A socket family between host and guest that does not go through the network.
//! That independence is the whole reason it exists here: the Docker socket has
//! to work before DHCP, regardless of what the guest's routing table says, and
//! without occupying a TCP port that a container might want.
//!
//! # Shape
//!
//! Three queues. The guest posts empty buffers on RX for us to fill, sends us
//! packets on TX, and EVENT carries CID changes that only matter under live
//! migration, which we do not do.
//!
//! # Threading
//!
//! Two kinds of thread reach the connection table: a vCPU thread servicing a
//! queue notification, and one host thread per open connection reading from its
//! socket. They share [`VsockShared`].
//!
//! **The lock order is transport, then shared, and never the reverse.** A vCPU
//! arrives holding the transport lock and takes the shared lock inside
//! [`VsockDevice::notify`]. A connection thread therefore takes the shared
//! lock, enqueues, *drops it*, and only then pokes the transport. Doing those
//! two under one lock deadlocks the machine the first time a container writes
//! to the Docker socket while the guest is also writing back, which is to say
//! immediately and only under load.

pub mod credit;
pub mod packet;

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};

use crate::memory::GuestMemory;
use crate::virtio::mmio::COMMON_FEATURES;
use crate::virtio::queue::{Descriptor, Virtqueue};
use crate::virtio::{Serviced, VirtioDevice, device_type};

use credit::Credit;
use packet::{GUEST_CID, HDR_LEN, MAX_PAYLOAD, Op, Packet, shutdown};

/// Queue indices.
pub const RX_QUEUE: u16 = 0;
pub const TX_QUEUE: u16 = 1;
pub const EVENT_QUEUE: u16 = 2;

/// Where host-side ephemeral ports start.
///
/// High enough not to collide with the well-known guest ports we dial.
const FIRST_EPHEMERAL_PORT: u32 = 1 << 30;

/// How many packets may wait for the guest to post receive buffers.
///
/// Bounded for the same reason the network's backlog is: the producers are host
/// threads and the consumer is a guest that may be busy.
const OUTBOX_LIMIT: usize = 1024;

/// A connection's half of the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// We sent a REQUEST and are waiting for the guest to accept.
    Connecting,
    Established,
    /// Torn down; the entry survives only until its thread notices.
    Closed,
}

struct Conn {
    guest_port: u32,
    state: State,
    credit: Credit,
    /// Guest data waiting to be written to the host socket.
    ///
    /// **Nothing writes to a host socket from a vCPU thread.** A blocking
    /// `write_all` there stops the core, so the guest stops running, so it
    /// stops consuming and stops issuing credit — while the host peer is itself
    /// blocked writing to us because our receive path has stalled. Both sides
    /// then wait for each other forever. Buffering here and writing from a
    /// dedicated thread is what breaks that cycle.
    ///
    /// It needs no size limit of its own: the guest may only send what our
    /// advertised credit allows, and credit advances only as the writer thread
    /// drains this. The protocol is the bound.
    ///
    /// Payloads are kept whole, moved in and moved out. The first version
    /// was a deque of bytes and pushed each packet's payload into it one
    /// byte at a time, on the vCPU thread, under both locks: 64 KiB of
    /// `push_back` per packet, which was the whole of a 7 Gbit/s ceiling.
    /// A payload the guest sent stays in the guest's own buffers until the
    /// writer has put it on the socket (`Chunk::Guest`): copying it out
    /// first was a second memcpy of every outbound byte, on a core of its
    /// own, and with the writer's own copy into the kernel that was the
    /// whole of a 62 Gbit/s ceiling.
    outbound: VecDeque<Chunk>,
    /// The guest has half-closed: it will send nothing further, but it is still
    /// willing to receive. The host is owed an EOF once `outbound` drains.
    guest_done: bool,
    /// Bytes consumed since the guest was last told. A credit update per
    /// batch written was a packet, a transport lock and a guest interrupt
    /// for every few hundred kilobytes; the guest only needs to hear when
    /// a useful fraction of its window has come back.
    unreported: u32,
    /// The host end, kept for shutdown. The writer thread holds its own
    /// clone. None until a pump attaches one.
    socket: Option<Box<dyn Closable>>,
}

/// What the host end of a connection has to be: readable, writable,
/// clonable for a writer thread, and closable. A unix socket for the Docker
/// proxy, a TCP socket for a stream to the network — the pump does not
/// care which, and it matters that it does not, because a socket pair in
/// between was a copy and a thread per connection that bought nothing.
pub trait Socket: Read + Write + AsRawFd + Send + 'static {
    fn try_clone(&self) -> std::io::Result<Self>
    where
        Self: Sized;
    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()>;
}

impl Socket for UnixStream {
    fn try_clone(&self) -> std::io::Result<Self> {
        UnixStream::try_clone(self)
    }
    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        UnixStream::shutdown(self, how)
    }
}

impl Socket for std::net::TcpStream {
    fn try_clone(&self) -> std::io::Result<Self> {
        std::net::TcpStream::try_clone(self)
    }
    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        std::net::TcpStream::shutdown(self, how)
    }
}

/// The one thing the connection table does with a socket: close it when
/// the guest is done. Held type-erased so the table need not know which
/// kind it is; a guest-opened connection has none until its pump starts.
trait Closable: Send {
    fn close(&self);
}

impl<S: Socket> Closable for S {
    fn close(&self) {
        let _ = Socket::shutdown(self, std::net::Shutdown::Both);
    }
}

/// A connection as a reactor sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Established,
    Gone,
}

/// What [`VsockShared::try_take_outbound`] found.
pub enum Outbound {
    Chunks(Vec<Chunk>),
    /// Nothing now; the guest may send more.
    Empty,
    /// The guest is done and everything it sent has been taken.
    Finished,
    /// No such connection.
    Gone,
}

/// Bytes on their way from the guest to the host socket.
pub enum Chunk {
    /// Owned by us: a control payload, or a copy made for a reader.
    Owned(Vec<u8>),
    /// Still in the guest's transmit buffers. `head` is the chain to return
    /// once the bytes are on the socket; `spans` are the payload's host
    /// addresses, valid until then.
    Guest { head: u16, spans: Vec<(u64, usize)> },
}

impl Chunk {
    pub fn len(&self) -> usize {
        match self {
            Chunk::Owned(v) => v.len(),
            Chunk::Guest { spans, .. } => spans.iter().map(|(_, n)| n).sum(),
        }
    }
}

/// A connection: our port and the guest's, which together are what every
/// packet carries. Host-opened connections have a unique port of ours and a
/// well-known one of the guest's; guest-opened ones share the port they
/// dialed and differ in theirs, which is why one port was never enough.
pub type ConnKey = (u32, u32);

struct Inner {
    conns: HashMap<ConnKey, Conn>,
    outbox: VecDeque<Packet>,
    next_port: u32,
    /// Payload buffers the writers have finished with, for the ring reader
    /// to fill again. A fresh 256 KiB Vec per packet is a page-faulted,
    /// zeroed mapping and an unmap on every packet — the host's writer
    /// thread sat at a core in `writev` and the copies never got faster.
    spare: Vec<Vec<u8>>,
    /// Transmit chains whose bytes are on a socket now, waiting to go back
    /// on the used ring the next time the transport looks.
    done: VecDeque<u16>,
    /// Ports the host answers: a REQUEST to one is accepted and the new
    /// connection handed to the listener, one end of a socket pair for the
    /// device's pump and the other for whoever listens.
    listeners: HashMap<u32, std::sync::mpsc::Sender<Accepted>>,
}

/// A connection the guest opened, as handed to its listener: established,
/// with whatever the guest sent first waiting in its queue. The listener
/// reads what it needs with [`VsockShared::read_outbound_exact`] and then
/// runs [`pump`] on the socket it chose.
pub struct Accepted {
    pub key: ConnKey,
}

/// State shared between the device and the host threads driving connections.
pub struct VsockShared {
    inner: Mutex<Inner>,
    /// Guest memory, for a writer reading a payload where the guest left
    /// it. Set when the driver activates the device.
    memory: std::sync::OnceLock<Arc<GuestMemory>>,
    /// Signalled when credit frees up or a connection changes state, so a
    /// blocked writer wakes without polling.
    progress: Condvar,
    /// Pokes the transport to drain the outbox into the guest.
    ///
    /// Set after construction because the transport does not exist yet when the
    /// device is built. Queuing a packet is not delivering it, and **whoever
    /// queues must be the one to wake**: a sender that queues up to the credit
    /// limit and then blocks waiting for more credit is waiting on the guest
    /// receiving exactly the packets it just queued. Leave the wake to the
    /// caller and that is a stall which only resolves when the guest happens to
    /// notify for its own reasons — a megabyte took minutes.
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Pokes whoever drives non-blocking streams (a reactor) when anything
    /// a stream might be waiting on has changed: credit, outbound bytes, a
    /// connection established or gone. Set once by the reactor.
    stream_waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for VsockShared {
    fn default() -> Self {
        VsockShared::new()
    }
}

impl VsockShared {
    pub fn new() -> VsockShared {
        VsockShared {
            memory: std::sync::OnceLock::new(),
            stream_waker: Mutex::new(None),
            inner: Mutex::new(Inner {
                conns: HashMap::new(),
                outbox: VecDeque::new(),
                next_port: FIRST_EPHEMERAL_PORT,
                spare: Vec::new(),
                done: VecDeque::new(),
                listeners: HashMap::new(),
            }),
            progress: Condvar::new(),
            waker: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("vsock state poisoned")
    }

    /// Installs the callback that drains the outbox into the guest.
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self.waker.lock().expect("vsock waker poisoned") = Some(Arc::new(waker));
    }

    /// Registers the reactor's waker.
    pub fn set_stream_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self.stream_waker.lock().expect("stream waker poisoned") = Some(Arc::new(waker));
    }

    /// Something a waiter may care about changed: blocking waiters are
    /// woken through the condvar, the reactor through its waker.
    fn progressed(&self) {
        self.progress.notify_all();
        let waker = self
            .stream_waker
            .lock()
            .expect("stream waker poisoned")
            .as_ref()
            .map(Arc::clone);
        if let Some(waker) = waker {
            waker();
        }
    }

    /// Asks the transport to deliver whatever is queued.
    ///
    /// The callback is cloned out and the lock dropped before calling it,
    /// because it takes the transport lock and nothing may hold two of these at
    /// once — see this module's note on lock order.
    fn wake(&self) {
        let waker = self
            .waker
            .lock()
            .expect("vsock waker poisoned")
            .as_ref()
            .map(Arc::clone);
        if let Some(waker) = waker {
            waker();
        }
    }

    /// Opens a connection to `guest_port`, proxying `socket` over it.
    ///
    /// Returns our port, which identifies the connection for the rest of its
    /// life. The REQUEST is only queued here: the guest answers on its own
    /// schedule, and [`VsockShared::await_established`] is where that is waited
    /// for.
    pub fn open<S: Socket>(&self, guest_port: u32, socket: S) -> ConnKey {
        let mut inner = self.lock();
        let host_port = inner.next_port;
        inner.next_port = inner.next_port.wrapping_add(1).max(FIRST_EPHEMERAL_PORT);

        inner.conns.insert(
            (host_port, guest_port),
            Conn {
                guest_port,
                state: State::Connecting,
                credit: Credit::new(),
                outbound: VecDeque::new(),
                guest_done: false,
                unreported: 0,
                socket: Some(Box::new(socket)),
            },
        );
        let mut request = Packet::control(Op::Request, host_port, guest_port);
        request.buf_alloc = credit::BUF_ALLOC;
        inner.outbox.push_back(request);
        drop(inner);

        self.wake();
        (host_port, guest_port)
    }

    /// Answers connections the guest opens to `host_port`.
    ///
    /// Each accepted connection arrives on the returned channel already
    /// established; the listener reads the guest's opening bytes from the
    /// queue and runs [`pump`] on whatever socket they call for.
    pub fn listen(&self, host_port: u32) -> std::sync::mpsc::Receiver<Accepted> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.lock().listeners.insert(host_port, tx);
        rx
    }

    /// Gives a connection the socket its pump will use, for closing.
    fn attach<S: Socket>(&self, key: ConnKey, socket: S) {
        if let Some(conn) = self.lock().conns.get_mut(&key) {
            conn.socket = Some(Box::new(socket));
        }
    }

    /// The connection's state as a reactor wants it.
    pub fn status(&self, key: ConnKey) -> Status {
        match self.lock().conns.get(&key).map(|c| c.state) {
            Some(State::Connecting) => Status::Connecting,
            Some(State::Established) => Status::Established,
            Some(State::Closed) | None => Status::Gone,
        }
    }

    /// Queues as much of `data` as credit and the outbox allow, without
    /// waiting: the count accepted, or `Err` for a connection that is not
    /// there to take it. A reactor keeps the rest and comes back when its
    /// waker fires.
    pub fn try_send(&self, key: ConnKey, data: &[u8]) -> Result<usize, ()> {
        let mut inner = self.lock();
        let Some(conn) = inner.conns.get(&key) else {
            return Err(());
        };
        if conn.state != State::Established {
            return if conn.state == State::Connecting { Ok(0) } else { Err(()) };
        }
        let mut allowed = conn.credit.available() as usize;
        let mut room = OUTBOX_LIMIT.saturating_sub(inner.outbox.len());
        let (host_port, guest_port) = key;
        let fwd_cnt = conn.credit.fwd_cnt();
        let mut offset = 0;
        while allowed > 0 && room > 0 && offset < data.len() {
            let take = allowed.min(MAX_PAYLOAD).min(data.len() - offset);
            let mut packet = Packet::control(Op::Rw, host_port, guest_port);
            let mut payload = match inner.spare.pop() {
                Some(mut buf) => {
                    buf.clear();
                    buf
                }
                None => Vec::with_capacity(MAX_PAYLOAD),
            };
            payload.extend_from_slice(&data[offset..offset + take]);
            packet.payload = payload;
            packet.buf_alloc = credit::BUF_ALLOC;
            packet.fwd_cnt = fwd_cnt;
            inner.outbox.push_back(packet);
            if let Some(conn) = inner.conns.get_mut(&key) {
                conn.credit.sent(take as u32);
            }
            offset += take;
            allowed -= take;
            room -= 1;
        }
        drop(inner);
        if offset > 0 {
            self.wake();
        }
        Ok(offset)
    }

    /// What the guest has sent, without waiting.
    pub fn try_take_outbound(&self, key: ConnKey) -> Outbound {
        let mut inner = self.lock();
        let Some(conn) = inner.conns.get_mut(&key) else {
            return Outbound::Gone;
        };
        if !conn.outbound.is_empty() {
            return Outbound::Chunks(conn.outbound.drain(..).collect());
        }
        if conn.state == State::Closed || conn.guest_done {
            return Outbound::Finished;
        }
        Outbound::Empty
    }

    /// Exactly `n` of the guest's first bytes if they have arrived, credited;
    /// `Ok(None)` when not yet; `Err` for a connection that ended first.
    pub fn try_read_outbound(&self, key: ConnKey, n: usize) -> Result<Option<Vec<u8>>, ()> {
        let memory = self.memory.get().cloned();
        let mut inner = self.lock();
        let Some(conn) = inner.conns.get_mut(&key) else {
            return Err(());
        };
        let have: usize = conn.outbound.iter().map(Chunk::len).sum();
        if have < n {
            if conn.state == State::Closed || conn.guest_done {
                return Err(());
            }
            return Ok(None);
        }
        let mut out = Vec::with_capacity(n);
        let mut finished: Vec<u16> = Vec::new();
        while out.len() < n {
            let Some(chunk) = conn.outbound.pop_front() else { break };
            let (mut bytes, head) = match chunk {
                Chunk::Owned(v) => (v, None),
                Chunk::Guest { head, spans } => {
                    let mut v = Vec::new();
                    if let Some(mem) = &memory {
                        for (gpa, len) in &spans {
                            let start = v.len();
                            v.resize(start + len, 0);
                            let _ = mem.read(*gpa, &mut v[start..]);
                        }
                    }
                    (v, Some(head))
                }
            };
            if let Some(head) = head {
                finished.push(head);
            }
            let take = bytes.len().min(n - out.len());
            if take < bytes.len() {
                let rest = bytes.split_off(take);
                conn.outbound.push_front(Chunk::Owned(rest));
            }
            out.extend_from_slice(&bytes[..take]);
        }
        inner.done.extend(finished);
        drop(inner);
        self.wake();
        self.acknowledge(key, n as u32);
        Ok(Some(out))
    }

    /// Takes exactly `n` bytes from what the guest has sent, blocking for
    /// them, and credits the guest for them. `None` if the connection ends
    /// first. For a listener reading the header a stream starts with.
    pub fn read_outbound_exact(&self, key: ConnKey, n: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        let mut inner = self.lock();
        loop {
            let memory = self.memory.get().cloned();
            let conn = inner.conns.get_mut(&key)?;
            let mut finished: Vec<u16> = Vec::new();
            while out.len() < n {
                let Some(chunk) = conn.outbound.pop_front() else {
                    break;
                };
                // Whole chunk as bytes; a guest chunk is copied out here,
                // which is fine for the few bytes a header is.
                let (mut bytes, head) = match chunk {
                    Chunk::Owned(v) => (v, None),
                    Chunk::Guest { head, spans } => {
                        let mut v = Vec::new();
                        if let Some(mem) = &memory {
                            for (gpa, len) in &spans {
                                let start = v.len();
                                v.resize(start + len, 0);
                                let _ = mem.read(*gpa, &mut v[start..]);
                            }
                        }
                        (v, Some(head))
                    }
                };
                if let Some(head) = head {
                    finished.push(head);
                }
                let take = bytes.len().min(n - out.len());
                if take < bytes.len() {
                    let rest = bytes.split_off(take);
                    conn.outbound.push_front(Chunk::Owned(rest));
                }
                out.extend_from_slice(&bytes[..take]);
            }
            let done_now = out.len() == n;
            let ended = conn.state == State::Closed || conn.guest_done;
            inner.done.extend(finished.iter().copied());
            if done_now {
                break;
            }
            if ended {
                return None;
            }
            let (guard, _) = self
                .progress
                .wait_timeout(inner, std::time::Duration::from_millis(100))
                .expect("vsock state poisoned");
            inner = guard;
        }
        drop(inner);
        self.wake();
        self.acknowledge(key, n as u32);
        Some(out)
    }

    /// Chains whose bytes are on a socket: back to the guest on the next
    /// look at the ring.
    pub fn complete(&self, heads: impl IntoIterator<Item = u16>) {
        let mut inner = self.lock();
        inner.done.extend(heads);
        // Returned on the next look at either ring, which the credit update
        // or the guest's next packet brings; a wake per batch was a guest
        // interrupt for every few hundred kilobytes. A pile of them is
        // pushed out rather than left: the ring is finite.
        let pile = inner.done.len() >= 32;
        drop(inner);
        if pile {
            self.wake();
        }
    }

    /// Guest memory, once the device is active.
    pub fn memory(&self) -> Option<Arc<GuestMemory>> {
        self.memory.get().cloned()
    }

    /// Blocks until the guest accepts or refuses. `false` means refused.
    pub fn await_established(&self, key: ConnKey, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            match inner.conns.get(&key).map(|c| c.state) {
                Some(State::Established) => return true,
                // A refused connection is removed outright, so a missing entry
                // and an explicitly closed one mean the same thing.
                Some(State::Closed) | None => return false,
                Some(State::Connecting) => {}
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, _) = self
                .progress
                .wait_timeout(inner, remaining)
                .expect("vsock state poisoned");
            inner = guard;
        }
    }

    /// Sends `data` to the guest, blocking while the peer has no credit.
    ///
    /// Returns false once the connection is gone, which is the signal for the
    /// calling thread to stop.
    pub fn send(&self, key: ConnKey, data: &[u8]) -> bool {
        let mut offset = 0;

        while offset < data.len() {
            let mut inner = self.lock();

            let Some(conn) = inner.conns.get(&key) else {
                return false;
            };
            if conn.state != State::Established {
                return false;
            }

            let allowed = conn.credit.available() as usize;
            // Also bounded by the outbox, or a fast host writer against a slow
            // guest would grow it without limit.
            let room = OUTBOX_LIMIT.saturating_sub(inner.outbox.len());

            if allowed == 0 || room == 0 {
                // Out of credit. What frees it is the guest receiving what is
                // already queued, so deliver that *before* waiting — waiting
                // first means waiting on an event only this call can cause.
                drop(inner);
                self.wake();

                let inner = self.lock();
                let _unused = self
                    .progress
                    .wait_timeout(inner, std::time::Duration::from_millis(50))
                    .expect("vsock state poisoned");
                continue;
            }

            // As many packets as credit and the outbox allow under one lock,
            // and one wake for all of them: a wake per 64 KiB was a transport
            // lock and a guest interrupt per packet, which at 8 Gbit/s was
            // most of what a packet cost.
            let (host_port, guest_port) = key;
            let fwd_cnt = conn.credit.fwd_cnt();
            let mut allowed = allowed;
            let mut room = room;
            while allowed > 0 && room > 0 && offset < data.len() {
                let take = allowed.min(MAX_PAYLOAD).min(data.len() - offset);
                let mut packet = Packet::control(Op::Rw, host_port, guest_port);
                let mut payload = match inner.spare.pop() {
                    Some(mut buf) => {
                        buf.clear();
                        buf
                    }
                    None => Vec::with_capacity(MAX_PAYLOAD),
                };
                payload.extend_from_slice(&data[offset..offset + take]);
                packet.payload = payload;
                packet.buf_alloc = credit::BUF_ALLOC;
                packet.fwd_cnt = fwd_cnt;
                inner.outbox.push_back(packet);
                if let Some(conn) = inner.conns.get_mut(&key) {
                    conn.credit.sent(take as u32);
                }
                offset += take;
                allowed -= take;
                room -= 1;
            }

            drop(inner);
            self.wake();
        }
        true
    }

    /// Tells the guest we will send no more, while staying open to receive.
    ///
    /// This is what a peer that has finished its request but still wants the
    /// reply does — `docker run` attaching with no stdin is the case that
    /// matters. Sending a full close here instead loses the reply.
    pub fn shutdown_write(&self, key: ConnKey) {
        let mut inner = self.lock();
        if !inner.conns.contains_key(&key) {
            return;
        }
        let (host_port, guest_port) = key;
        let mut packet = Packet::control(Op::Shutdown, host_port, guest_port);
        packet.flags = shutdown::SEND;
        packet.buf_alloc = credit::BUF_ALLOC;
        inner.outbox.push_back(packet);
        drop(inner);

        self.progressed();
        self.wake();
    }

    /// Closes our end, telling the guest we will write no more.
    pub fn shutdown(&self, key: ConnKey) {
        let mut inner = self.lock();
        if !inner.conns.contains_key(&key) {
            return;
        }
        let (host_port, guest_port) = key;
        let mut packet = Packet::control(Op::Shutdown, host_port, guest_port);
        packet.flags = shutdown::BOTH;
        packet.buf_alloc = credit::BUF_ALLOC;
        inner.outbox.push_back(packet);
        if let Some(conn) = inner.conns.get_mut(&key) {
            conn.state = State::Closed;
        }
        drop(inner);
        self.progressed();
        self.wake();
    }

    /// Takes buffered guest data for the writer thread.
    ///
    /// Returns `None` once the connection is gone. Blocks while the connection
    /// is alive and has nothing pending, so the writer thread does not spin.
    fn take_outbound(&self, key: ConnKey) -> Option<Vec<Chunk>> {
        let mut inner = self.lock();
        loop {
            let conn = inner.conns.get_mut(&key)?;
            if !conn.outbound.is_empty() {
                return Some(conn.outbound.drain(..).collect());
            }
            // Nothing buffered and nothing more coming: the writer is done, and
            // returning None is what tells it to give the host its EOF.
            if conn.state == State::Closed || conn.guest_done {
                return None;
            }
            let (guard, _) = self
                .progress
                .wait_timeout(inner, std::time::Duration::from_millis(100))
                .expect("vsock state poisoned");
            inner = guard;
        }
    }

    /// Records that `bytes` reached the host application, freeing the guest to
    /// send that much more.
    pub fn acknowledge(&self, key: ConnKey, bytes: u32) {
        let mut inner = self.lock();
        let Some(conn) = inner.conns.get_mut(&key) else {
            return;
        };
        conn.credit.consumed(bytes);
        conn.unreported = conn.unreported.saturating_add(bytes);
        // A quarter of the window at a time: the guest always has at least
        // three quarters in hand, and a small transfer that never reaches the
        // threshold has nothing more to send anyway. A CREDIT_REQUEST is
        // answered at once regardless.
        if conn.unreported < credit::BUF_ALLOC / 4 {
            return;
        }
        conn.unreported = 0;
        let fwd_cnt = conn.credit.fwd_cnt();

        let (host_port, guest_port) = key;
        let mut update = Packet::control(Op::CreditUpdate, host_port, guest_port);
        update.buf_alloc = credit::BUF_ALLOC;
        update.fwd_cnt = fwd_cnt;
        inner.outbox.push_back(update);
        drop(inner);

        // The guest is waiting on this to send more, so it goes now rather than
        // whenever something else happens to wake the transport.
        self.wake();
    }

    /// A buffer to read a payload into: one a writer finished with, or a
    /// new one. Capacity is kept; length is zero.
    fn spare_buffer(&self) -> Vec<u8> {
        let mut inner = self.lock();
        match inner.spare.pop() {
            Some(mut buf) => {
                buf.clear();
                buf
            }
            None => Vec::with_capacity(MAX_PAYLOAD),
        }
    }

    /// Hands payload buffers back for the ring reader to fill again. Bounded,
    /// so a burst does not leave a pile of them behind.
    pub fn recycle(&self, chunks: Vec<Chunk>) {
        const KEEP: usize = 64;
        let mut inner = self.lock();
        for chunk in chunks {
            if inner.spare.len() >= KEEP {
                break;
            }
            if let Chunk::Owned(buf) = chunk
                && buf.capacity() >= MAX_PAYLOAD / 4
            {
                inner.spare.push(buf);
            }
        }
    }

    /// Whether anything is waiting to go to the guest.
    pub fn has_pending(&self) -> bool {
        !self.lock().outbox.is_empty()
    }
}

/// The device model.
pub struct Vsock {
    shared: Arc<VsockShared>,
}

impl Vsock {
    pub fn new(shared: Arc<VsockShared>) -> Vsock {
        Vsock { shared }
    }

    /// Handles one packet from the guest.
    ///
    /// Takes no `self`: everything a packet can affect lives in the shared
    /// state, and saying so lets the caller hold the lock across a batch.
    fn handle(inner: &mut Inner, packet: Packet) {
        // Our port is the packet's destination and the guest's its source;
        // together they name the connection.
        let host_port = packet.dst_port;
        let key: ConnKey = (packet.dst_port, packet.src_port);

        // Credit rides on every packet, including ones we otherwise ignore.
        if let Some(conn) = inner.conns.get_mut(&key) {
            conn.credit.observe(packet.buf_alloc, packet.fwd_cnt);
        }

        match packet.op {
            Op::Response => {
                if let Some(conn) = inner.conns.get_mut(&key) {
                    conn.state = State::Established;
                    tracing::debug!(host_port, guest_port = conn.guest_port, "vsock established");
                }
            }

            Op::Rst => {
                // Refused, or torn down. Either way the connection is over, and
                // dropping the entry closes the host socket with it. Chains it
                // still held go back to the guest.
                if let Some(conn) = inner.conns.remove(&key) {
                    Vsock::retire(inner, conn);
                    tracing::debug!(host_port, "vsock reset by guest");
                }
                // Running under the transport lock already: the retired
                // chains go back in this very pass.
            }

            Op::Shutdown => {
                let Some(conn) = inner.conns.get_mut(&key) else {
                    return;
                };

                // A half-close is not a close, and treating it as one is how
                // `docker run` loses all of its output: the CLI shuts its write
                // side once it has sent the attach request and has no stdin to
                // forward, and tearing the connection down there kills the
                // direction the container's output was going to arrive on.
                //
                // SEND alone means the guest will write no more. The host is
                // owed an EOF, which the writer thread delivers once it has
                // drained what is already buffered — but the other direction
                // stays open.
                if packet.flags & shutdown::RCV == 0 && packet.flags & shutdown::SEND != 0 {
                    conn.guest_done = true;
                    return;
                }

                conn.state = State::Closed;
                if let Some(socket) = &conn.socket {
                    socket.close();
                }
                let mut rst = Packet::control(Op::Rst, host_port, packet.src_port);
                rst.buf_alloc = credit::BUF_ALLOC;
                inner.outbox.push_back(rst);
                if let Some(conn) = inner.conns.remove(&key) {
                    Vsock::retire(inner, conn);
                }
            }

            Op::Rw => {
                let Some(conn) = inner.conns.get_mut(&key) else {
                    // Data for a connection we do not have. Tell the guest, or
                    // it will wait for a reply that is never coming.
                    let mut rst = Packet::control(Op::Rst, host_port, packet.src_port);
                    rst.buf_alloc = credit::BUF_ALLOC;
                    inner.outbox.push_back(rst);
                    return;
                };

                // Queued, not written: see `Conn::outbound`. Credit is NOT
                // advanced here — the bytes are not with the host application
                // yet, and claiming they are would invite the guest to send
                // more than we can hold.
                conn.outbound.push_back(Chunk::Owned(packet.payload));
            }

            Op::CreditRequest => {
                if let Some(conn) = inner.conns.get_mut(&key) {
                    conn.unreported = 0;
                    let guest_port = conn.guest_port;
                    let fwd_cnt = conn.credit.fwd_cnt();
                    let mut update = Packet::control(Op::CreditUpdate, host_port, guest_port);
                    update.buf_alloc = credit::BUF_ALLOC;
                    update.fwd_cnt = fwd_cnt;
                    inner.outbox.push_back(update);
                }
            }

            // Already recorded above; that is all a credit update is.
            Op::CreditUpdate => {}

            Op::Request => {
                // A guest-initiated connection: accepted when something on
                // the host listens on the port, refused explicitly otherwise
                // (a silent drop would hang the guest). The new connection is
                // one end of a socket pair for the pump and the other for
                // the listener, which the listener threads together itself;
                // nothing is spawned under this lock.
                let accepted = inner
                    .listeners
                    .get(&host_port)
                    .is_some_and(|listener| listener.send(Accepted { key }).is_ok());
                match accepted {
                    true => {
                        let mut conn = Conn {
                            guest_port: packet.src_port,
                            state: State::Established,
                            credit: Credit::new(),
                            outbound: VecDeque::new(),
                            guest_done: false,
                            unreported: 0,
                            socket: None,
                        };
                        conn.credit.observe(packet.buf_alloc, packet.fwd_cnt);
                        inner.conns.insert(key, conn);
                        let mut response =
                            Packet::control(Op::Response, host_port, packet.src_port);
                        response.buf_alloc = credit::BUF_ALLOC;
                        inner.outbox.push_back(response);
                    }
                    false => {
                        let mut rst = Packet::control(Op::Rst, host_port, packet.src_port);
                        rst.buf_alloc = credit::BUF_ALLOC;
                        inner.outbox.push_back(rst);
                    }
                }
            }

            Op::Invalid => {
                tracing::debug!(host_port, "vsock packet with an unknown operation");
            }
        }
    }

    /// Returns to the guest any chains a departing connection still held.
    fn retire(inner: &mut Inner, conn: Conn) {
        for chunk in conn.outbound {
            if let Chunk::Guest { head, .. } = chunk {
                inner.done.push_back(head);
            }
        }
    }

    /// An RW packet whose payload stays in the guest: queued as a
    /// [`Chunk::Guest`] if the connection is there to take it, in which
    /// case the chain is held until the writer is done with it. Returns
    /// whether the chain was held.
    fn handle_guest_rw(inner: &mut Inner, packet: &Packet, head: u16, spans: Vec<(u64, usize)>) -> bool {
        let key: ConnKey = (packet.dst_port, packet.src_port);
        match inner.conns.get_mut(&key) {
            Some(conn) => {
                conn.credit.observe(packet.buf_alloc, packet.fwd_cnt);
                conn.outbound.push_back(Chunk::Guest { head, spans });
                true
            }
            None => {
                let mut rst = Packet::control(Op::Rst, packet.dst_port, packet.src_port);
                rst.buf_alloc = credit::BUF_ALLOC;
                inner.outbox.push_back(rst);
                false
            }
        }
    }

    /// Returns the chains a writer has finished with to the guest.
    fn complete_done(inner: &mut Inner, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut any = false;
        while let Some(head) = inner.done.pop_front() {
            queue.push_used(mem, head, 0);
            any = true;
        }
        any
    }

    /// Reads packets the guest has queued for us.
    fn drain_tx(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;
        // Each entry: the packet, and for an RW one the chain and the
        // payload's spans in guest memory, which the writer reads in place.
        let mut packets: Vec<(Packet, Option<(u16, Vec<(u64, usize)>)>)> = Vec::new();

        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            // The header into a stack array; the payload stays where it is,
            // as spans of guest memory, unless the packet is not RW.
            let mut header = [0u8; HDR_LEN];
            let mut have = 0usize;
            let mut spans: Vec<(u64, usize)> = Vec::new();
            let mut declared = 0usize;
            let mut ok = true;
            for desc in chain {
                if desc.is_write_only() {
                    continue;
                }
                let mut addr = desc.addr;
                let mut len = desc.len as usize;
                if have < HDR_LEN {
                    let take = len.min(HDR_LEN - have);
                    if mem.read(addr, &mut header[have..have + take]).is_err() {
                        ok = false;
                        break;
                    }
                    have += take;
                    addr += take as u64;
                    len -= take;
                    if have == HDR_LEN {
                        declared = u32::from_le_bytes([
                            header[24], header[25], header[26], header[27],
                        ]) as usize;
                        if declared > MAX_PAYLOAD {
                            ok = false;
                            break;
                        }
                    }
                }
                if len == 0 {
                    continue;
                }
                let so_far: usize = spans.iter().map(|(_, n)| n).sum();
                if so_far >= declared {
                    continue;
                }
                let take = len.min(declared - so_far);
                if mem.host_span(addr, take).is_err() {
                    ok = false;
                    break;
                }
                spans.push((addr, take));
            }
            let total: usize = spans.iter().map(|(_, n)| n).sum();
            let parsed = if ok && have == HDR_LEN && total == declared {
                Packet::header_only(&header).map(|mut p| {
                    // A payload on a packet that is not RW is small and
                    // rare (nothing in the protocol has one); copy it so the
                    // packet can be handled whole.
                    if p.op != Op::Rw && declared > 0 {
                        let mut payload = Vec::with_capacity(declared);
                        for (gpa, len) in &spans {
                            let start = payload.len();
                            payload.resize(start + len, 0);
                            let _ = mem.read(*gpa, &mut payload[start..]);
                        }
                        p.payload = payload;
                        (p, None)
                    } else if declared > 0 {
                        (p, Some((head, std::mem::take(&mut spans))))
                    } else {
                        (p, None)
                    }
                })
            } else {
                None
            };
            match parsed {
                Some((packet, guest)) => {
                    let held = guest.is_some();
                    packets.push((packet, guest));
                    if !held {
                        queue.push_used(mem, head, 0);
                        used_any = true;
                    }
                }
                None => {
                    tracing::debug!("malformed vsock packet from guest");
                    queue.push_used(mem, head, 0);
                    used_any = true;
                }
            }
        }

        let mut inner = self.shared.lock();
        for (packet, guest) in packets {
            match guest {
                Some((head, spans)) => {
                    if !Vsock::handle_guest_rw(&mut inner, &packet, head, spans) {
                        queue.push_used(mem, head, 0);
                        used_any = true;
                    }
                }
                None => Vsock::handle(&mut inner, packet),
            }
        }
        // Chains a writer finished with since the last look.
        if Vsock::complete_done(&mut inner, queue, mem) {
            used_any = true;
        }
        drop(inner);
        // Credit may have freed up, and a connection may have been
        // established or torn down; either way somebody may be blocked.
        self.shared.progressed();

        used_any
    }

    /// Fills the guest's receive buffers from the outbox.
    fn drain_rx(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;
        let mut inner = self.shared.lock();

        while !inner.outbox.is_empty() {
            let Some(chain) = queue.pop(mem) else {
                // No buffers posted; leave the packet queued. The guest
                // notifies us when it posts more.
                break;
            };
            let head = chain.head();

            // Measure the chain before taking a packet. Linux posts receive
            // buffers of a fixed 4 KiB — header included — and nothing in the
            // protocol negotiates that size, so a packet is only ever as large
            // as the buffer that happens to turn up. Deciding the payload size
            // when we send would be guessing.
            let writable: Vec<_> = chain.filter(Descriptor::is_write_only).collect();
            let capacity: usize = writable.iter().map(|d| d.len as usize).sum();

            if capacity <= HDR_LEN {
                // Not even room for a header. Returning it unused is the only
                // honest answer; keeping the packet means we retry when a
                // usable buffer arrives.
                tracing::warn!(capacity, "vsock receive buffer cannot hold a header");
                queue.push_used(mem, head, 0);
                used_any = true;
                continue;
            }

            let mut packet = inner.outbox.pop_front().expect("checked non-empty above");
            let room = capacity - HDR_LEN;

            // Too big for this buffer: send what fits and put the rest back at
            // the front. A stream has no message boundaries, so splitting is
            // invisible to the guest — and it is what keeps a 1 MB write from
            // being silently truncated to the first buffer's worth.
            if packet.payload.len() > room {
                let tail = packet.payload.split_off(room);
                let mut rest = Packet::control(packet.op, packet.src_port, packet.dst_port);
                rest.payload = tail;
                rest.buf_alloc = packet.buf_alloc;
                rest.fwd_cnt = packet.fwd_cnt;
                inner.outbox.push_front(rest);
            }

            // Header, then payload, straight from where they are into the
            // guest's buffers: assembling them into one Vec first was a
            // second copy of every byte that reached the guest.
            let header = packet.header_bytes();
            let total = HDR_LEN + packet.payload.len();
            let mut offset = 0usize;
            for desc in writable {
                if offset >= total {
                    break;
                }
                let mut at = desc.addr;
                let mut room = desc.len as usize;
                if offset < HDR_LEN {
                    let take = room.min(HDR_LEN - offset);
                    if mem.write(at, &header[offset..offset + take]).is_err() {
                        break;
                    }
                    offset += take;
                    at += take as u64;
                    room -= take;
                }
                if room > 0 && offset >= HDR_LEN && offset < total {
                    let start = offset - HDR_LEN;
                    let take = room.min(total - offset);
                    if mem.write(at, &packet.payload[start..start + take]).is_err() {
                        break;
                    }
                    offset += take;
                }
            }

            queue.push_used(mem, head, offset as u32);
            used_any = true;
            // The payload is in the guest now; its buffer goes back for the
            // next one rather than to the allocator.
            if packet.payload.capacity() >= MAX_PAYLOAD / 4 && inner.spare.len() < 64 {
                let mut buf = std::mem::take(&mut packet.payload);
                buf.clear();
                inner.spare.push(buf);
            }
        }

        drop(inner);
        if used_any {
            self.shared.progressed();
        }
        used_any
    }
}

impl VirtioDevice for Vsock {
    fn device_type(&self) -> u32 {
        device_type::VSOCK
    }

    fn name(&self) -> &'static str {
        "virtio-vsock"
    }

    fn features(&self) -> u64 {
        COMMON_FEATURES
    }

    fn queue_count(&self) -> usize {
        3
    }

    /// `struct virtio_vsock_config`: just the guest's CID, as a u64.
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let config = GUEST_CID.to_le_bytes();
        let start = offset as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(start + i).copied().unwrap_or(0);
        }
    }

    fn activate(&mut self, mem: Arc<GuestMemory>) {
        let _ = self.shared.memory.set(mem);
    }

    fn notify(&mut self, queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        let serviced = match queue {
            TX_QUEUE => match queues.get_mut(TX_QUEUE as usize) {
                Some(q) => Serviced::queue_if(TX_QUEUE, self.drain_tx(q, mem)),
                None => Serviced::NONE,
            },
            RX_QUEUE => match queues.get_mut(RX_QUEUE as usize) {
                Some(q) => Serviced::queue_if(RX_QUEUE, self.drain_rx(q, mem)),
                None => Serviced::NONE,
            },
            EVENT_QUEUE => Serviced::NONE,
            other => {
                tracing::debug!(queue = other, "vsock notified on an unknown queue");
                Serviced::NONE
            }
        };

        // Chains a writer finished with go back whichever queue was notified:
        // the writer's wake services RX, and a guest whose transmit ring is
        // full of held chains is waiting for exactly this before it will
        // kick TX again.
        let serviced = match queues.get_mut(TX_QUEUE as usize) {
            Some(tx) => {
                let mut inner = self.shared.lock();
                if Vsock::complete_done(&mut inner, tx, mem) {
                    serviced.and(Serviced::queue(TX_QUEUE))
                } else {
                    serviced
                }
            }
            None => serviced,
        };

        // A packet from the guest usually produces a reply — a RESPONSE, a
        // credit update, an RST — and the guest will not notify RX just because
        // it sent us something. Draining RX here is what delivers it.
        //
        // The reply's queue is reported separately, which is the whole reason
        // `Serviced` is a mask: interrupting on TX's suppression state would
        // leave the reply in the ring with the guest asleep.
        if queue == TX_QUEUE
            && let Some(rx) = queues.get_mut(RX_QUEUE as usize)
        {
            return serviced.with(Serviced::queue_if(RX_QUEUE, self.drain_rx(rx, mem)));
        }
        serviced
    }

    fn reset(&mut self) {
        let mut inner = self.shared.lock();
        inner.conns.clear();
        inner.outbox.clear();
        drop(inner);
        self.shared.progressed();
    }
}

/// Proxies a host socket over an open vsock connection until either end closes.
///
/// Runs on its own thread per connection. That is a real cost at a thousand
/// connections and no cost at the dozen a Docker client opens, and it buys a
/// blocking read with no readiness machinery anywhere.
pub fn pump<S: Socket>(shared: Arc<VsockShared>, key: ConnKey, mut socket: S) {
    // Both threads of a stream do the work a user is waiting on, and a
    // thread at default QoS is a thread Apple silicon may put on an
    // efficiency core: the same transfer read 45 Gbit/s one run and 63 the
    // next until the pump's threads were raised.
    crate::qos::raise_interactive();
    if let Ok(clone) = socket.try_clone() {
        shared.attach(key, clone);
    }
    // Guest to host, on its own thread. The blocking write has to happen
    // somewhere that is not a vCPU thread; this is that somewhere.
    // The writer's end is signalled by the channel closing, since a cached
    // thread has no join handle.
    let (done_tx, writer_done) = std::sync::mpsc::channel::<()>();
    {
        let shared = shared.clone();
        let Ok(mut socket) = socket.try_clone() else {
            shared.shutdown(key);
            return;
        };
        crate::workers::run("vsock-write", crate::qos::CONNECTION_STACK, move || {
                let _done = done_tx;
                let memory = shared.memory();
                while let Some(chunks) = shared.take_outbound(key) {
                    if write_all_chunks(&mut socket, &chunks, memory.as_deref()).is_err() {
                        break;
                    }
                    // Only now are the bytes the host application's, so only
                    // now may the guest be told it has room for more — and
                    // only now may the guest have its buffers back.
                    let bytes: usize = chunks.iter().map(Chunk::len).sum();
                    shared.complete(chunks.iter().filter_map(|c| match c {
                        Chunk::Guest { head, .. } => Some(*head),
                        Chunk::Owned(_) => None,
                    }));
                    shared.acknowledge(key, bytes as u32);
                    shared.recycle(chunks);
                }
                let _ = socket.flush();
                // The guest will send no more, so the host peer is owed an
                // end-of-stream. Without it a client reading to EOF — which is
                // most of them — waits forever on a connection with nothing
                // left to say.
                let _ = Socket::shutdown(&socket, std::net::Shutdown::Write);
            });
    }

    // A megabyte per read: what the socket holds arrives in one call and
    // goes to the guest as one batch of packets under one wake.
    let mut buf = vec![0u8; 1 << 20];
    let mut host_closed = false;
    loop {
        let read = match socket.read(&mut buf) {
            Ok(0) => {
                host_closed = true;
                break;
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        // `send` delivers as it goes; there is nothing to poke afterwards.
        if !shared.send(key, &buf[..read]) {
            break;
        }
    }

    // A host peer that closed its write side has half-closed, not closed:
    // `docker run` sends its attach request, shuts down writing because it has
    // no stdin to forward, and then waits for the container's output. Reporting
    // a full close here takes that output with it.
    if host_closed {
        shared.shutdown_write(key);
        let _ = writer_done.recv();
        shared.shutdown(key);
        return;
    }

    shared.shutdown(key);
    let _ = writer_done.recv();
}

/// One vectored write per batch, completed across partial writes. Guest
/// chunks are written from where they are: the kernel copies them out of
/// guest memory itself.
pub fn write_all_chunks(
    socket: &mut impl Socket,
    chunks: &[Chunk],
    memory: Option<&GuestMemory>,
) -> std::io::Result<()> {
    let mut iovs: Vec<libc::iovec> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        match chunk {
            Chunk::Owned(v) => iovs.push(libc::iovec {
                iov_base: v.as_ptr() as *mut libc::c_void,
                iov_len: v.len(),
            }),
            Chunk::Guest { spans, .. } => {
                let Some(mem) = memory else {
                    return Err(std::io::Error::other("no guest memory for a guest chunk"));
                };
                for (gpa, len) in spans {
                    let ptr = mem
                        .host_span(*gpa, *len)
                        .map_err(|_| std::io::Error::other("guest span out of bounds"))?;
                    iovs.push(libc::iovec {
                        iov_base: ptr.cast(),
                        iov_len: *len,
                    });
                }
            }
        }
    }
    for batch in iovs.chunks_mut(1024) {
        crate::net::write_all_vectored(socket.as_raw_fd(), batch)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(chunks: Option<Vec<Chunk>>) -> Option<Vec<u8>> {
        chunks.map(|c| {
            c.into_iter()
                .flat_map(|chunk| match chunk {
                    Chunk::Owned(v) => v,
                    Chunk::Guest { .. } => panic!("a guest chunk in a unit test"),
                })
                .collect()
        })
    }

    fn shared_with_connection() -> (Arc<VsockShared>, ConnKey, UnixStream) {
        let shared = Arc::new(VsockShared::new());
        let (ours, theirs) = UnixStream::pair().unwrap();
        let port = shared.open(2375, ours);
        (shared, port, theirs)
    }

    #[test]
    fn opening_a_connection_queues_a_request() {
        let (shared, port, _peer) = shared_with_connection();
        let inner = shared.lock();
        assert_eq!(inner.outbox.len(), 1);
        let request = &inner.outbox[0];
        assert_eq!(request.op, Op::Request);
        assert_eq!(request.src_port, port.0);
        assert_eq!(request.dst_port, 2375);
        assert_eq!(
            request.buf_alloc,
            credit::BUF_ALLOC,
            "the guest must be told our buffer size in the opening packet, or it \
             cannot send us anything"
        );
    }

    #[test]
    fn a_response_establishes_the_connection() {
        let (shared, port, _peer) = shared_with_connection();
        let mut response = Packet::control(Op::Response, 2375, port.0);
        response.buf_alloc = 4096;

        let mut inner = shared.lock();
        Vsock::handle(&mut inner, response);
        assert_eq!(inner.conns[&port].state, State::Established);
        assert_eq!(inner.conns[&port].credit.available(), 4096);
    }

    /// A guest that says only "I will send no more" has half-closed. Tearing
    /// the connection down there kills the direction the reply travels on,
    /// which is how `docker run` came to produce a correct exit code and no
    /// output whatsoever.
    #[test]
    fn a_send_only_shutdown_keeps_the_connection_open() {
        let (shared, port, _peer) = shared_with_connection();
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Response, 2375, port.0));

        let mut half = Packet::control(Op::Shutdown, 2375, port.0);
        half.flags = shutdown::SEND;
        Vsock::handle(&mut inner, half);

        assert!(
            inner.conns.contains_key(&port),
            "a half-close must not remove the connection"
        );
        assert!(inner.conns[&port].guest_done);
        assert_eq!(
            inner.conns[&port].state,
            State::Established,
            "the receive direction is still live"
        );
    }

    #[test]
    fn a_full_shutdown_closes_the_connection() {
        let (shared, port, _peer) = shared_with_connection();
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Response, 2375, port.0));

        let mut full = Packet::control(Op::Shutdown, 2375, port.0);
        full.flags = shutdown::BOTH;
        Vsock::handle(&mut inner, full);

        assert!(!inner.conns.contains_key(&port));
    }

    /// Once the guest has half-closed and the buffer is empty there is nothing
    /// further to hand the writer, and that `None` is what makes it deliver the
    /// EOF the host peer is waiting for.
    #[test]
    fn the_writer_is_told_to_finish_after_a_half_close() {
        let (shared, port, _peer) = shared_with_connection();
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Response, 2375, port.0));

        let mut data = Packet::control(Op::Rw, 2375, port.0);
        data.payload = b"tail".to_vec();
        Vsock::handle(&mut inner, data);

        let mut half = Packet::control(Op::Shutdown, 2375, port.0);
        half.flags = shutdown::SEND;
        Vsock::handle(&mut inner, half);
        drop(inner);

        // Buffered data first — a half-close must not discard it.
        assert_eq!(
            flat(shared.take_outbound(port)).as_deref(),
            Some(&b"tail"[..])
        );
        assert!(shared.take_outbound(port).is_none(), "then end of stream");
    }

    #[test]
    fn a_reset_removes_the_connection() {
        let (shared, port, _peer) = shared_with_connection();
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Rst, 2375, port.0));
        assert!(!inner.conns.contains_key(&port));
    }

    /// Guest data is buffered, never written from the caller's thread — that
    /// caller is a vCPU, and a blocking write there stops the machine.
    #[test]
    fn guest_data_is_buffered_rather_than_written_inline() {
        let (shared, port, _peer) = shared_with_connection();

        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Response, 2375, port.0));
        inner.outbox.clear();

        let mut data = Packet::control(Op::Rw, 2375, port.0);
        data.payload = b"hello".to_vec();
        Vsock::handle(&mut inner, data);

        assert_eq!(
            inner.conns[&port]
                .outbound
                .iter()
                .map(Chunk::len)
                .sum::<usize>(),
            5,
            "the payload should be queued for the writer thread"
        );
        assert!(
            !inner.outbox.iter().any(|p| p.op == Op::CreditUpdate),
            "credit must not advance until the bytes actually reach the host; \
             promising room we do not have is how the buffer overruns"
        );
    }

    /// The other half of the contract: once the writer thread has delivered the
    /// bytes, the guest is told, or a transfer larger than one credit window
    /// stops and never resumes.
    #[test]
    fn acknowledging_delivery_advances_credit_and_tells_the_guest() {
        let (shared, port, _peer) = shared_with_connection();
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Response, 2375, port.0));
        inner.outbox.clear();
        drop(inner);

        // A few bytes are consumed silently: the guest has most of its
        // window in hand and an update per batch was a packet, a lock and
        // an interrupt for every few hundred kilobytes.
        shared.acknowledge(port, 5);
        assert!(
            !shared.lock().outbox.iter().any(|p| p.op == Op::CreditUpdate),
            "a few bytes should not produce a credit update"
        );

        // A quarter of the window is when the guest is told.
        shared.acknowledge(port, credit::BUF_ALLOC / 4);

        let inner = shared.lock();
        let update = inner
            .outbox
            .iter()
            .find(|p| p.op == Op::CreditUpdate)
            .expect("delivery of a quarter window should produce a credit update");
        assert_eq!(update.fwd_cnt, 5 + credit::BUF_ALLOC / 4);
        assert_eq!(update.buf_alloc, credit::BUF_ALLOC);
    }

    #[test]
    fn the_writer_receives_what_the_guest_sent() {
        let (shared, port, _peer) = shared_with_connection();
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Response, 2375, port.0));
        let mut data = Packet::control(Op::Rw, 2375, port.0);
        data.payload = b"hello".to_vec();
        Vsock::handle(&mut inner, data);
        drop(inner);

        assert_eq!(
            flat(shared.take_outbound(port)).as_deref(),
            Some(&b"hello"[..])
        );
    }

    #[test]
    fn data_for_an_unknown_connection_is_reset_rather_than_dropped() {
        let shared = Arc::new(VsockShared::new());
        let mut inner = shared.lock();
        let mut stray = Packet::control(Op::Rw, 2375, 999);
        stray.payload = b"x".to_vec();
        Vsock::handle(&mut inner, stray);
        assert_eq!(inner.outbox.len(), 1);
        assert_eq!(inner.outbox[0].op, Op::Rst);
    }

    /// Nothing on the host listens for guest-initiated connections yet. A
    /// silent drop would leave the guest waiting forever.
    #[test]
    fn a_guest_initiated_connection_is_refused_explicitly() {
        let shared = Arc::new(VsockShared::new());
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Request, 1234, 5678));
        assert_eq!(inner.outbox[0].op, Op::Rst);
    }

    #[test]
    fn sending_respects_the_peers_credit() {
        let (shared, port, _peer) = shared_with_connection();
        let mut response = Packet::control(Op::Response, 2375, port.0);
        response.buf_alloc = 4;
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, response);
        inner.outbox.clear();
        drop(inner);

        // Four bytes fit; the call returns once they are queued.
        assert!(shared.send(port, b"abcd"));
        let inner = shared.lock();
        assert_eq!(inner.outbox.len(), 1);
        assert_eq!(inner.outbox[0].payload, b"abcd");
        assert_eq!(
            inner.conns[&port].credit.available(),
            0,
            "the peer's buffer is now full"
        );
    }

    #[test]
    fn sending_on_a_closed_connection_reports_failure() {
        let (shared, port, _peer) = shared_with_connection();
        shared.shutdown(port);
        assert!(!shared.send(port, b"anything"));
    }

    #[test]
    fn config_space_reports_the_guest_cid() {
        let device = Vsock::new(Arc::new(VsockShared::new()));
        let mut config = [0u8; 8];
        device.config_read(0, &mut config);
        assert_eq!(u64::from_le_bytes(config), GUEST_CID);
        assert_eq!(GUEST_CID, 3, "0 and 1 are reserved and 2 is the host");
        assert_eq!(packet::HOST_CID, 2);
    }

    /// Linux posts 4 KiB receive buffers, header included, and nothing in the
    /// protocol negotiates that. A device that emits a 64 KiB packet into one
    /// of them writes the first 4 KiB and loses the rest — silently, because
    /// the descriptor still comes back looking serviced. That is what made a
    /// 1 MB transfer never finish.
    ///
    /// The unit here is the arithmetic that decides the split; the gate proves
    /// the whole path against a real guest.
    #[test]
    fn a_payload_larger_than_the_buffer_is_split_not_truncated() {
        const GUEST_RX_BUF: usize = 4096;
        let room = GUEST_RX_BUF - HDR_LEN;

        let mut packet = Packet::control(Op::Rw, 1, 2);
        packet.payload = vec![0xab; 10_000];

        // What drain_rx does when the chain is smaller than the packet.
        let mut delivered = Vec::new();
        let tail = packet.payload.split_off(room);
        delivered.extend_from_slice(&packet.payload);
        assert_eq!(
            packet.to_bytes().len(),
            GUEST_RX_BUF,
            "the first packet must fill the buffer exactly, not overrun it"
        );

        let mut rest = Packet::control(Op::Rw, 1, 2);
        rest.payload = tail;
        delivered.extend_from_slice(&rest.payload);

        assert_eq!(delivered.len(), 10_000, "no bytes may be lost in the split");
        assert!(delivered.iter().all(|&b| b == 0xab));
    }
}

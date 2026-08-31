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
    outbound: VecDeque<u8>,
    /// The host end, kept for shutdown. The writer thread holds its own clone.
    socket: UnixStream,
}

struct Inner {
    /// Keyed by our own port, which is what the guest addresses us by.
    conns: HashMap<u32, Conn>,
    outbox: VecDeque<Packet>,
    next_port: u32,
}

/// State shared between the device and the host threads driving connections.
pub struct VsockShared {
    inner: Mutex<Inner>,
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
}

impl Default for VsockShared {
    fn default() -> Self {
        VsockShared::new()
    }
}

impl VsockShared {
    pub fn new() -> VsockShared {
        VsockShared {
            inner: Mutex::new(Inner {
                conns: HashMap::new(),
                outbox: VecDeque::new(),
                next_port: FIRST_EPHEMERAL_PORT,
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
    pub fn open(&self, guest_port: u32, socket: UnixStream) -> u32 {
        let mut inner = self.lock();
        let host_port = inner.next_port;
        inner.next_port = inner.next_port.wrapping_add(1).max(FIRST_EPHEMERAL_PORT);

        inner.conns.insert(
            host_port,
            Conn {
                guest_port,
                state: State::Connecting,
                credit: Credit::new(),
                outbound: VecDeque::new(),
                socket,
            },
        );
        let mut request = Packet::control(Op::Request, host_port, guest_port);
        request.buf_alloc = credit::BUF_ALLOC;
        inner.outbox.push_back(request);
        drop(inner);

        self.wake();
        host_port
    }

    /// Blocks until the guest accepts or refuses. `false` means refused.
    pub fn await_established(&self, host_port: u32, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            match inner.conns.get(&host_port).map(|c| c.state) {
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
    pub fn send(&self, host_port: u32, data: &[u8]) -> bool {
        let mut offset = 0;

        while offset < data.len() {
            let mut inner = self.lock();

            let Some(conn) = inner.conns.get(&host_port) else {
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

            let take = allowed.min(MAX_PAYLOAD).min(data.len() - offset);
            let guest_port = conn.guest_port;
            let fwd_cnt = conn.credit.fwd_cnt();

            let mut packet = Packet::control(Op::Rw, host_port, guest_port);
            packet.payload = data[offset..offset + take].to_vec();
            packet.buf_alloc = credit::BUF_ALLOC;
            packet.fwd_cnt = fwd_cnt;
            inner.outbox.push_back(packet);

            if let Some(conn) = inner.conns.get_mut(&host_port) {
                conn.credit.sent(take as u32);
            }
            offset += take;

            drop(inner);
            self.wake();
        }
        true
    }

    /// Closes our end, telling the guest we will write no more.
    pub fn shutdown(&self, host_port: u32) {
        let mut inner = self.lock();
        let Some(conn) = inner.conns.get(&host_port) else {
            return;
        };
        let guest_port = conn.guest_port;
        let mut packet = Packet::control(Op::Shutdown, host_port, guest_port);
        packet.flags = shutdown::BOTH;
        packet.buf_alloc = credit::BUF_ALLOC;
        inner.outbox.push_back(packet);
        if let Some(conn) = inner.conns.get_mut(&host_port) {
            conn.state = State::Closed;
        }
        drop(inner);
        self.progress.notify_all();
        self.wake();
    }

    /// Takes buffered guest data for the writer thread.
    ///
    /// Returns `None` once the connection is gone. Blocks while the connection
    /// is alive and has nothing pending, so the writer thread does not spin.
    fn take_outbound(&self, host_port: u32) -> Option<Vec<u8>> {
        let mut inner = self.lock();
        loop {
            let conn = inner.conns.get_mut(&host_port)?;
            if !conn.outbound.is_empty() {
                return Some(conn.outbound.drain(..).collect());
            }
            if conn.state == State::Closed {
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
    fn acknowledge(&self, host_port: u32, bytes: u32) {
        let mut inner = self.lock();
        let Some(conn) = inner.conns.get_mut(&host_port) else {
            return;
        };
        conn.credit.consumed(bytes);
        let guest_port = conn.guest_port;
        let fwd_cnt = conn.credit.fwd_cnt();

        let mut update = Packet::control(Op::CreditUpdate, host_port, guest_port);
        update.buf_alloc = credit::BUF_ALLOC;
        update.fwd_cnt = fwd_cnt;
        inner.outbox.push_back(update);
        drop(inner);

        // The guest is waiting on this to send more, so it goes now rather than
        // whenever something else happens to wake the transport.
        self.wake();
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
        // Our port is the packet's destination; the guest addresses us by the
        // port we chose when we opened the connection.
        let host_port = packet.dst_port;

        // Credit rides on every packet, including ones we otherwise ignore.
        if let Some(conn) = inner.conns.get_mut(&host_port) {
            conn.credit.observe(packet.buf_alloc, packet.fwd_cnt);
        }

        match packet.op {
            Op::Response => {
                if let Some(conn) = inner.conns.get_mut(&host_port) {
                    conn.state = State::Established;
                    tracing::debug!(host_port, guest_port = conn.guest_port, "vsock established");
                }
            }

            Op::Rst => {
                // Refused, or torn down. Either way the connection is over, and
                // dropping the entry closes the host socket with it.
                if inner.conns.remove(&host_port).is_some() {
                    tracing::debug!(host_port, "vsock reset by guest");
                }
            }

            Op::Shutdown => {
                if let Some(conn) = inner.conns.get_mut(&host_port) {
                    conn.state = State::Closed;
                    // Half-closes are legal, but nothing we proxy uses one, and
                    // shutting the socket down is what makes the reader thread
                    // return rather than block forever on a dead connection.
                    let _ = conn.socket.shutdown(std::net::Shutdown::Both);
                }
                let mut rst = Packet::control(Op::Rst, host_port, packet.src_port);
                rst.buf_alloc = credit::BUF_ALLOC;
                inner.outbox.push_back(rst);
                inner.conns.remove(&host_port);
            }

            Op::Rw => {
                let Some(conn) = inner.conns.get_mut(&host_port) else {
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
                conn.outbound.extend(packet.payload.iter().copied());
            }

            Op::CreditRequest => {
                if let Some(conn) = inner.conns.get(&host_port) {
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
                // A guest-initiated connection. Nothing listens on the host
                // side yet, and a silent drop would hang the guest, so refuse
                // it explicitly.
                let mut rst = Packet::control(Op::Rst, packet.dst_port, packet.src_port);
                rst.buf_alloc = credit::BUF_ALLOC;
                inner.outbox.push_back(rst);
            }

            Op::Invalid => {
                tracing::debug!(host_port, "vsock packet with an unknown operation");
            }
        }
    }

    /// Reads packets the guest has queued for us.
    fn drain_tx(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;
        let mut packets = Vec::new();

        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let mut bytes = Vec::with_capacity(HDR_LEN);
            for desc in chain {
                if desc.is_write_only() {
                    continue;
                }
                let len = desc.len as usize;
                if bytes.len() + len > HDR_LEN + MAX_PAYLOAD {
                    break;
                }
                let start = bytes.len();
                bytes.resize(start + len, 0);
                if mem.read(desc.addr, &mut bytes[start..]).is_err() {
                    bytes.truncate(start);
                    break;
                }
            }

            match Packet::parse(&bytes) {
                Some(packet) => packets.push(packet),
                None => tracing::debug!(len = bytes.len(), "malformed vsock packet from guest"),
            }
            queue.push_used(mem, head, 0);
            used_any = true;
        }

        if !packets.is_empty() {
            let mut inner = self.shared.lock();
            for packet in packets {
                Vsock::handle(&mut inner, packet);
            }
            drop(inner);
            // Credit may have freed up, and a connection may have been
            // established or torn down; either way somebody may be blocked.
            self.shared.progress.notify_all();
        }

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

            let bytes = packet.to_bytes();
            let mut offset = 0usize;
            for desc in writable {
                if offset >= bytes.len() {
                    break;
                }
                let take = (desc.len as usize).min(bytes.len() - offset);
                if mem.write(desc.addr, &bytes[offset..offset + take]).is_err() {
                    break;
                }
                offset += take;
            }

            queue.push_used(mem, head, offset as u32);
            used_any = true;
        }

        drop(inner);
        if used_any {
            self.shared.progress.notify_all();
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
        self.shared.progress.notify_all();
    }
}

/// Proxies a host socket over an open vsock connection until either end closes.
///
/// Runs on its own thread per connection. That is a real cost at a thousand
/// connections and no cost at the dozen a Docker client opens, and it buys a
/// blocking read with no readiness machinery anywhere.
pub fn pump(shared: Arc<VsockShared>, host_port: u32, mut socket: UnixStream) {
    // Guest to host, on its own thread. The blocking write has to happen
    // somewhere that is not a vCPU thread; this is that somewhere.
    let writer = {
        let shared = shared.clone();
        let Ok(mut socket) = socket.try_clone() else {
            shared.shutdown(host_port);
            return;
        };
        std::thread::Builder::new()
            .name("vsock-write".into())
            .spawn(move || {
                while let Some(data) = shared.take_outbound(host_port) {
                    if socket.write_all(&data).is_err() {
                        break;
                    }
                    // Only now are the bytes the host application's, so only
                    // now may the guest be told it has room for more.
                    shared.acknowledge(host_port, data.len() as u32);
                }
                let _ = socket.flush();
            })
    };

    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = match socket.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        // `send` delivers as it goes; there is nothing to poke afterwards.
        if !shared.send(host_port, &buf[..read]) {
            break;
        }
    }

    shared.shutdown(host_port);
    if let Ok(writer) = writer {
        let _ = writer.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_with_connection() -> (Arc<VsockShared>, u32, UnixStream) {
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
        assert_eq!(request.src_port, port);
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
        let mut response = Packet::control(Op::Response, 2375, port);
        response.buf_alloc = 4096;

        let mut inner = shared.lock();
        Vsock::handle(&mut inner, response);
        assert_eq!(inner.conns[&port].state, State::Established);
        assert_eq!(inner.conns[&port].credit.available(), 4096);
    }

    #[test]
    fn a_reset_removes_the_connection() {
        let (shared, port, _peer) = shared_with_connection();
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Rst, 2375, port));
        assert!(!inner.conns.contains_key(&port));
    }

    /// Guest data is buffered, never written from the caller's thread — that
    /// caller is a vCPU, and a blocking write there stops the machine.
    #[test]
    fn guest_data_is_buffered_rather_than_written_inline() {
        let (shared, port, _peer) = shared_with_connection();

        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Response, 2375, port));
        inner.outbox.clear();

        let mut data = Packet::control(Op::Rw, 2375, port);
        data.payload = b"hello".to_vec();
        Vsock::handle(&mut inner, data);

        assert_eq!(
            inner.conns[&port].outbound.len(),
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
        Vsock::handle(&mut inner, Packet::control(Op::Response, 2375, port));
        inner.outbox.clear();
        drop(inner);

        shared.acknowledge(port, 5);

        let inner = shared.lock();
        let update = inner
            .outbox
            .iter()
            .find(|p| p.op == Op::CreditUpdate)
            .expect("delivery should produce a credit update");
        assert_eq!(update.fwd_cnt, 5);
        assert_eq!(update.buf_alloc, credit::BUF_ALLOC);
    }

    #[test]
    fn the_writer_receives_what_the_guest_sent() {
        let (shared, port, _peer) = shared_with_connection();
        let mut inner = shared.lock();
        Vsock::handle(&mut inner, Packet::control(Op::Response, 2375, port));
        let mut data = Packet::control(Op::Rw, 2375, port);
        data.payload = b"hello".to_vec();
        Vsock::handle(&mut inner, data);
        drop(inner);

        assert_eq!(shared.take_outbound(port).as_deref(), Some(&b"hello"[..]));
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
        let mut response = Packet::control(Op::Response, 2375, port);
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

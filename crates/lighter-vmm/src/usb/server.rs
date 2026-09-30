//! The USB/IP server: every attached device's stream on one thread.
//!
//! Each device is a session: a unix socket whose other end the reactor
//! carries to the guest's `vhci-hcd`, and a [`Device`] that does the USB
//! work. Nothing here waits on a device. Every operation starts at once and
//! completes through the session's [`Sink`], from whatever thread the device
//! finishes on, so a bulk in that waits hours for a Zigbee frame costs a map
//! entry and no thread. A kqueue watches every session's socket; completions
//! are handed over through a queue and a pipe that wakes the loop.
//!
//! The protocol is Linux's own server's (`drivers/usb/usbip/stub_rx.c`,
//! `stub_tx.c`), including its unlink rules: a URB stopped by an unlink is
//! answered by the unlink's reply alone, with the URB's status, and never by
//! a submit reply; an unlink that finds its URB already answered is answered
//! with status 0.
//!
//! macOS can abort only a whole pipe, never one request, so an unlink stops
//! every URB queued on its endpoint. While that abort is outstanding, new
//! URBs for the pipe wait; when it is done, the URBs it caught that nobody
//! unlinked go back first, in order, then the ones that waited, so the bytes
//! on a pipe keep their order. A caught URB that had moved data is answered
//! short, as Linux would; a caught control request is never repeated, since
//! one with no data stage cannot say whether the device acted on it.
//!
//! Commands are always read and acted on, so an unlink is never stuck behind
//! the traffic it would stop. What is bounded is the replies waiting for the
//! guest to read them: past the mark, new submits wait, in order, and start
//! when the guest reads again. Only a guest that stops reading altogether,
//! with more waiting than that, stops being read.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::usbip::{self, Command, Special, Submit, status};
use crate::reactor::Kq;

/// Replies waiting for the guest past this defer new submits; under the low
/// mark they start again.
const TX_HIGH: usize = 4 << 20;
const TX_LOW: usize = 1 << 20;
/// Submits waiting past this stop the socket being read at all: only a
/// guest that has stopped reading its replies gets here.
const DEFERRED_MAX: usize = 16 << 20;
const READ_CHUNK: usize = 64 * 1024;

/// Linux's URB transfer flags that change what a transfer means.
const URB_SHORT_NOT_OK: u32 = 0x0001;
const URB_ZERO_PACKET: u32 = 0x0040;

/// What a device is asked to do. Every one completes through the sink,
/// under the tag it was started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A control transfer on the default pipe: `out` for a host-to-device
    /// request, `in_len` bytes back for a device-to-host one.
    Control {
        setup: [u8; 8],
        out: Vec<u8>,
        in_len: usize,
    },
    /// A bulk or interrupt transfer on `endpoint` (its address, direction bit
    /// included). `zero_packet`: an out transfer whose length is a whole
    /// number of packets ends with a zero-length one.
    Transfer {
        endpoint: u8,
        out: Vec<u8>,
        in_len: usize,
        zero_packet: bool,
    },
    SetConfiguration(u8),
    SetInterface {
        interface: u8,
        alternate: u8,
    },
    ClearHalt(u8),
    Reset,
}

/// A device's side of a session.
pub trait Device: Send {
    fn start(&mut self, tag: u32, op: Op);
    /// Stops every transfer queued on `endpoint` (0 for the default pipe).
    /// Each completes with [`status::ECONNRESET`] and whatever moved.
    fn abort(&mut self, endpoint: u8);
    /// Gives the device back to macOS. Its sink's [`Sink::closed`] is called
    /// when that is done, and not before: until then the session counts as
    /// served, so nothing seizes the device again while it is being released.
    fn close(&mut self);
}

/// A finished operation: a Linux URB status, and for an in transfer the
/// bytes received (`actual` of them; for an out transfer, the bytes sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub tag: u32,
    pub status: i32,
    pub actual: usize,
    pub data: Vec<u8>,
}

enum Event {
    Done(Done),
    /// The device went away (unplugged, or taken back by macOS).
    Gone,
    /// Detached on the Mac's side: the guest sees the device unplugged.
    End,
    /// The device is back with macOS; the session is over.
    Closed,
    /// An abort of this pipe could not be started.
    AbortFailed(u8),
}

/// Where a session's device reports, from any thread.
#[derive(Clone)]
pub struct Sink {
    id: u64,
    shared: Arc<Shared>,
}

impl Sink {
    pub fn done(&self, done: Done) {
        self.shared.post(self.id, Event::Done(done));
    }

    pub fn gone(&self) {
        self.shared.post(self.id, Event::Gone);
    }

    /// An abort asked for could not be carried out.
    pub fn abort_failed(&self, endpoint: u8) {
        self.shared.post(self.id, Event::AbortFailed(endpoint));
    }

    /// The device has been given back: the last word from a device.
    pub fn closed(&self) {
        self.shared.post(self.id, Event::Closed);
    }
}

/// A session handed to the loop: its id, its socket and its device.
type Arrival = (u64, UnixStream, Box<dyn Device>);

struct Shared {
    events: Mutex<Vec<(u64, Event)>>,
    arrivals: Mutex<Vec<Arrival>>,
    wake_write: RawFd,
    next: AtomicU64,
    /// Sessions from `serve` until their device is given back.
    live: Mutex<HashSet<u64>>,
    /// Called whenever a device has been given back.
    on_closed: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Shared {
    fn post(&self, id: u64, event: Event) {
        self.events
            .lock()
            .expect("usb events poisoned")
            .push((id, event));
        self.wake();
    }

    fn wake(&self) {
        let b = 1u8;
        // SAFETY: a one-byte write to our own non-blocking pipe; a full pipe
        // already means a wake is pending.
        unsafe { libc::write(self.wake_write, std::ptr::addr_of!(b).cast(), 1) };
    }

    fn is_live(&self, id: u64) -> bool {
        self.live.lock().expect("usb live poisoned").contains(&id)
    }
}

pub struct Server {
    shared: Arc<Shared>,
}

impl Server {
    pub fn start() -> io::Result<Server> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: a two-int array for pipe(2).
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            crate::reactor::set_nonblocking(fd)?;
            // SAFETY: fcntl on a live descriptor.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        let shared = Arc::new(Shared {
            events: Mutex::new(Vec::new()),
            arrivals: Mutex::new(Vec::new()),
            wake_write: fds[1],
            next: AtomicU64::new(1),
            live: Mutex::new(HashSet::new()),
            on_closed: Mutex::new(None),
        });
        let kq = Kq::new()?;
        kq.read(fds[0], true);
        let looped = shared.clone();
        std::thread::Builder::new()
            .name("usb".into())
            .spawn(move || Loop::new(looped, kq, fds[0]).run())?;
        Ok(Server { shared })
    }

    /// Calls `f`, from the server's thread, whenever a device has been given
    /// back to macOS: what waits on one need not wait for its next tick.
    pub fn on_closed(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.shared.on_closed.lock().expect("usb poisoned") = Some(f);
    }

    /// A sink for a session about to start: the device is opened with it,
    /// then handed to [`Server::serve`].
    pub fn sink(&self) -> Sink {
        Sink {
            id: self.shared.next.fetch_add(1, Ordering::Relaxed),
            shared: self.shared.clone(),
        }
    }

    /// Serves `device` on `socket` until either ends. Served from now: a
    /// caller asking straight after sees it so.
    pub fn serve(&self, sink: &Sink, socket: UnixStream, device: Box<dyn Device>) {
        self.shared
            .live
            .lock()
            .expect("usb live poisoned")
            .insert(sink.id);
        self.shared
            .arrivals
            .lock()
            .expect("usb arrivals poisoned")
            .push((sink.id, socket, device));
        self.shared.wake();
    }

    /// Ends a session: the guest sees its device unplugged, and the device
    /// goes back to macOS.
    pub fn end(&self, sink: &Sink) {
        self.shared.post(sink.id, Event::End);
    }

    /// Whether a session is still being served, or its device not yet given
    /// back.
    pub fn serving(&self, sink: &Sink) -> bool {
        self.shared.is_live(sink.id)
    }
}

struct Pending {
    /// The pipe the URB is queued on: its endpoint address, 0 for control.
    pipe: u8,
    submit: Submit,
    /// The unlink that asked to stop it, whose reply answers it.
    unlink: Option<u32>,
}

/// A pipe while an abort on it is outstanding.
#[derive(Default)]
struct Aborting {
    /// Its URBs the abort has yet to hand back.
    outstanding: HashSet<u32>,
    /// URBs it caught that nobody unlinked and that moved nothing: they go
    /// back on the pipe, by sequence number, when the abort is done.
    caught: BTreeMap<u32, Submit>,
    /// Submits for the pipe that came while it was aborting, in order.
    held: VecDeque<Submit>,
}

struct Session {
    socket: UnixStream,
    device: Box<dyn Device>,
    rx: Vec<u8>,
    tx: VecDeque<Vec<u8>>,
    tx_at: usize,
    tx_bytes: usize,
    reading: bool,
    writing: bool,
    /// URBs started on the device, by sequence number.
    pending: HashMap<u32, Pending>,
    /// Pipes with an abort outstanding.
    aborting: HashMap<u8, Aborting>,
    /// Submits waiting for the guest to read its replies, in order.
    deferred: VecDeque<Submit>,
    deferred_bytes: usize,
    /// The guest's end has closed.
    closing: bool,
}

impl Session {
    fn reply(&mut self, reply: Vec<u8>) {
        self.tx_bytes += reply.len();
        self.tx.push_back(reply);
    }

    /// Starts a submit on the device, or holds it behind its pipe's abort.
    fn start(&mut self, submit: Submit) {
        let (pipe, op) = operation(&submit);
        if let Some(a) = self.aborting.get_mut(&pipe) {
            a.held.push_back(submit);
            return;
        }
        let seqnum = submit.seqnum;
        self.pending.insert(
            seqnum,
            Pending {
                pipe,
                submit,
                unlink: None,
            },
        );
        self.device.start(seqnum, op);
    }

    fn submit(&mut self, submit: Submit) {
        if submit.is_iso() {
            // Isochronous transfers (audio, video, a Bluetooth dongle's
            // voice channel) are not carried yet; the URB fails and the
            // driver sees why.
            self.reply(usbip::ret_submit(submit.seqnum, status::EOPNOTSUPP, 0, &[]));
            return;
        }
        // A control request's setup packet and its header must agree on its
        // direction, or its reply would carry data the guest does not read.
        if submit.ep == 0 && (submit.setup[0] & 0x80 != 0) != submit.is_in() {
            self.reply(usbip::ret_submit(submit.seqnum, status::EINVAL, 0, &[]));
            return;
        }
        if self.tx_bytes >= TX_HIGH || !self.deferred.is_empty() {
            self.deferred_bytes += deferred_weight(&submit);
            self.deferred.push_back(submit);
            return;
        }
        self.start(submit);
    }

    fn unlink(&mut self, seqnum: u32, victim: u32) {
        // Not on the device yet: stopped here, as Linux stops a URB it has
        // not submitted.
        if let Some(i) = self.deferred.iter().position(|s| s.seqnum == victim) {
            let s = self.deferred.remove(i).expect("found");
            self.deferred_bytes -= deferred_weight(&s);
            self.reply(usbip::ret_unlink(seqnum, status::ECONNRESET));
            return;
        }
        for a in self.aborting.values_mut() {
            if let Some(i) = a.held.iter().position(|s| s.seqnum == victim) {
                a.held.remove(i);
                self.reply(usbip::ret_unlink(seqnum, status::ECONNRESET));
                return;
            }
            if a.caught.remove(&victim).is_some() {
                self.reply(usbip::ret_unlink(seqnum, status::ECONNRESET));
                return;
            }
        }
        match self.pending.get_mut(&victim) {
            Some(p) if p.unlink.is_none() => {
                p.unlink = Some(seqnum);
                let pipe = p.pipe;
                if !self.aborting.contains_key(&pipe) {
                    let outstanding = self
                        .pending
                        .iter()
                        .filter(|(_, q)| q.pipe == pipe)
                        .map(|(&s, _)| s)
                        .collect();
                    self.aborting.insert(
                        pipe,
                        Aborting {
                            outstanding,
                            ..Aborting::default()
                        },
                    );
                    self.device.abort(pipe);
                }
            }
            // Answered already, or already being stopped by an earlier
            // unlink: nothing of this one's to stop.
            _ => self.reply(usbip::ret_unlink(seqnum, status::OK)),
        }
    }

    fn done(&mut self, done: Done) {
        let Some(p) = self.pending.remove(&done.tag) else {
            return;
        };
        let pipe = p.pipe;
        let caught_by_abort = self
            .aborting
            .get_mut(&pipe)
            .is_some_and(|a| a.outstanding.remove(&done.tag));
        let is_in = p.submit.is_in();
        let actual = if is_in {
            done.actual.min(done.data.len())
        } else {
            done.actual
        };
        let collateral =
            p.unlink.is_none() && caught_by_abort && pipe != 0 && done.status == status::ECONNRESET;
        if let Some(unlink) = p.unlink {
            self.reply(usbip::ret_unlink(unlink, done.status));
        } else if collateral && actual == 0 && p.submit.length > 0 {
            // Caught by the abort another URB's unlink asked for, with
            // nothing moved: it goes back on the pipe when the abort is done.
            self.aborting
                .get_mut(&pipe)
                .expect("aborting")
                .caught
                .insert(done.tag, p.submit);
        } else {
            let mut status = done.status;
            // Caught by another URB's abort after moving data: short, not
            // failed. Unless it cannot say whether it finished: a
            // zero-length out transfer, or one whose closing zero-length
            // packet may not have gone, which is answered with the abort.
            let complete = actual == p.submit.length;
            let unsure = p.submit.length == 0
                || (complete && p.submit.transfer_flags & URB_ZERO_PACKET != 0);
            if collateral && !unsure {
                status = status::OK;
            }
            if status == status::OK
                && is_in
                && p.submit.transfer_flags & URB_SHORT_NOT_OK != 0
                && actual < p.submit.length
            {
                status = status::EREMOTEIO;
            }
            let data: &[u8] = if is_in { &done.data[..actual] } else { &[] };
            self.reply(usbip::ret_submit(done.tag, status, actual, data));
        }
        if self
            .aborting
            .get(&pipe)
            .is_some_and(|a| a.outstanding.is_empty())
        {
            self.abort_done(pipe);
        }
    }

    /// The abort is done when every URB it caught is back: the caught go
    /// back on the pipe first, in order, then what waited behind them.
    fn abort_done(&mut self, pipe: u8) {
        let a = self.aborting.remove(&pipe).expect("aborting");
        for (_, submit) in a.caught {
            self.start(submit);
        }
        for submit in a.held {
            self.start(submit);
        }
    }

    /// macOS could not abort the pipe, so its URBs may never come back. The
    /// ones unlinked are answered now, as gone (a completion that comes
    /// later finds nothing to answer); the rest carry on, and so does the
    /// pipe.
    fn abort_failed(&mut self, pipe: u8) {
        let Some(a) = self.aborting.get_mut(&pipe) else {
            return;
        };
        let outstanding: Vec<u32> = a.outstanding.drain().collect();
        for tag in outstanding {
            if let Some(unlink) = self.pending.get(&tag).and_then(|p| p.unlink) {
                self.pending.remove(&tag);
                self.reply(usbip::ret_unlink(unlink, status::ECONNRESET));
            }
        }
        self.abort_done(pipe);
    }

    /// Acts on every whole command received.
    fn commands(&mut self) -> Result<(), usbip::ParseError> {
        let mut at = 0;
        let result = loop {
            match usbip::parse(&self.rx[at..]) {
                Ok(Some((command, used))) => {
                    at += used;
                    match command {
                        Command::Submit(submit) => self.submit(submit),
                        Command::Unlink { seqnum, victim } => self.unlink(seqnum, victim),
                    }
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        self.rx.drain(..at);
        result
    }

    /// Starts deferred submits once the guest has read its replies down.
    fn resume(&mut self) {
        if self.tx_bytes >= TX_LOW {
            return;
        }
        while let Some(submit) = self.deferred.pop_front() {
            self.deferred_bytes -= deferred_weight(&submit);
            self.start(submit);
        }
    }
}

struct Loop {
    shared: Arc<Shared>,
    kq: Kq,
    wake_read: RawFd,
    sessions: HashMap<u64, Session>,
    by_fd: HashMap<RawFd, u64>,
    /// Sessions ended or gone before the loop took them up, and sessions
    /// ended but not yet closed.
    ended_early: HashSet<u64>,
    buf: Vec<u8>,
}

impl Loop {
    fn new(shared: Arc<Shared>, kq: Kq, wake_read: RawFd) -> Loop {
        Loop {
            shared,
            kq,
            wake_read,
            sessions: HashMap::new(),
            by_fd: HashMap::new(),
            ended_early: HashSet::new(),
            buf: vec![0u8; READ_CHUNK],
        }
    }

    fn run(mut self) {
        let mut events: [libc::kevent; 64] = [libc::kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        }; 64];
        loop {
            // SAFETY: a live kqueue and a buffer of the length given; no timeout.
            let n = unsafe {
                libc::kevent(
                    self.kq.0,
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    std::ptr::null(),
                )
            };
            if n < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                tracing::error!("usb: kevent failed: {}", io::Error::last_os_error());
                return;
            }
            // Completions first: a reply the device finished before the
            // guest's next command counts against that command.
            let mut readable = Vec::new();
            for ev in &events[..n as usize] {
                let fd = ev.ident as RawFd;
                if fd == self.wake_read {
                    self.drain_wake();
                } else if let Some(&id) = self.by_fd.get(&fd) {
                    readable.push((id, ev.filter == libc::EVFILT_READ));
                }
            }
            self.take_arrivals();
            let mut touched: HashSet<u64> = self.take_events().into_iter().collect();
            for (id, read) in readable {
                if read {
                    self.read(id);
                }
                touched.insert(id);
            }
            for id in touched {
                self.pace(id);
            }
        }
    }

    fn drain_wake(&self) {
        let mut sink = [0u8; 256];
        // SAFETY: reading our own non-blocking pipe into a buffer we own.
        while unsafe { libc::read(self.wake_read, sink.as_mut_ptr().cast(), sink.len()) } > 0 {}
    }

    fn take_arrivals(&mut self) {
        let arrivals =
            std::mem::take(&mut *self.shared.arrivals.lock().expect("usb arrivals poisoned"));
        for (id, socket, mut device) in arrivals {
            if self.ended_early.remove(&id) || socket.set_nonblocking(true).is_err() {
                let _ = socket.shutdown(std::net::Shutdown::Both);
                device.close();
                continue;
            }
            let fd = socket.as_raw_fd();
            self.kq.read(fd, true);
            self.by_fd.insert(fd, id);
            self.sessions.insert(
                id,
                Session {
                    socket,
                    device,
                    rx: Vec::new(),
                    tx: VecDeque::new(),
                    tx_at: 0,
                    tx_bytes: 0,
                    reading: true,
                    writing: false,
                    pending: HashMap::new(),
                    aborting: HashMap::new(),
                    deferred: VecDeque::new(),
                    deferred_bytes: 0,
                    closing: false,
                },
            );
        }
    }

    /// Hands out the devices' events; returns the sessions they touched.
    fn take_events(&mut self) -> Vec<u64> {
        let events = std::mem::take(&mut *self.shared.events.lock().expect("usb events poisoned"));
        let mut touched = Vec::new();
        for (id, event) in events {
            match event {
                Event::Closed => {
                    self.ended_early.remove(&id);
                    self.shared
                        .live
                        .lock()
                        .expect("usb live poisoned")
                        .remove(&id);
                    if let Some(f) = &*self.shared.on_closed.lock().expect("usb poisoned") {
                        f();
                    }
                }
                Event::AbortFailed(pipe) => {
                    tracing::warn!(session = id, pipe, "usb: macOS could not abort a pipe");
                    if let Some(session) = self.sessions.get_mut(&id) {
                        session.abort_failed(pipe);
                        touched.push(id);
                    }
                }
                Event::Done(done) => {
                    if let Some(session) = self.sessions.get_mut(&id) {
                        session.done(done);
                        touched.push(id);
                    }
                }
                Event::Gone | Event::End => {
                    if matches!(event, Event::Gone) {
                        tracing::info!(session = id, "usb: the device went away");
                    }
                    if self.sessions.contains_key(&id) {
                        self.end(id);
                    } else if matches!(event, Event::Gone) || self.shared.is_live(id) {
                        // Not yet taken up (a device can go before it is
                        // served): ended when it is. Forgotten at `Closed`,
                        // which a device reports after `Gone` and a served
                        // session after `End`; an `End` for a session
                        // already closed is dropped.
                        self.ended_early.insert(id);
                    }
                }
            }
        }
        touched
    }

    /// Reads and acts on the guest's commands, a chunk at a time, until
    /// the socket is empty or the guest has stopped reading its replies.
    fn read(&mut self, id: u64) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        while session.deferred_bytes < DEFERRED_MAX {
            match session.socket.read(&mut self.buf) {
                Ok(0) => {
                    // The guest detached, or the machine is stopping.
                    session.closing = true;
                    return;
                }
                Ok(n) => session.rx.extend_from_slice(&self.buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    session.closing = true;
                    return;
                }
            }
            if let Err(e) = session.commands() {
                tracing::warn!(session = id, "usb: {e}; ending the device's session");
                self.end(id);
                return;
            }
        }
    }

    /// Starts what may start, writes what it can, and sets the socket's
    /// watches: writable while replies wait, readable unless the guest has
    /// stopped reading.
    fn pace(&mut self, id: u64) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        loop {
            session.resume();
            let before = session.tx_bytes;
            while let Some(front) = session.tx.front() {
                match session.socket.write(&front[session.tx_at..]) {
                    Ok(n) => {
                        session.tx_at += n;
                        session.tx_bytes -= n;
                        if session.tx_at == front.len() {
                            session.tx.pop_front();
                            session.tx_at = 0;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        self.end(id);
                        return;
                    }
                }
            }
            // Writing may have brought the replies under the low mark with
            // submits still deferred: start them now, not at the next event.
            if session.deferred.is_empty()
                || session.tx_bytes >= TX_LOW
                || session.tx_bytes == before
            {
                break;
            }
        }
        if session.closing {
            self.end(id);
            return;
        }
        let fd = session.socket.as_raw_fd();
        let want_write = !session.tx.is_empty();
        if want_write != session.writing {
            session.writing = want_write;
            self.kq.write(fd, want_write);
        }
        let want_read = session.deferred_bytes < DEFERRED_MAX;
        if want_read != session.reading {
            session.reading = want_read;
            self.kq.read(fd, want_read);
        }
    }

    fn end(&mut self, id: u64) {
        let Some(mut session) = self.sessions.remove(&id) else {
            return;
        };
        let fd = session.socket.as_raw_fd();
        self.kq.forget(fd);
        self.by_fd.remove(&fd);
        let _ = session.socket.shutdown(std::net::Shutdown::Both);
        // Served until the device says it is back with macOS (`Closed`).
        session.device.close();
    }
}

/// What a waiting submit costs: its data, and its header, so in transfers,
/// which carry none, count too.
fn deferred_weight(submit: &Submit) -> usize {
    submit.data.len() + usbip::HEADER_LEN
}

/// What a submit asks of the device, and the pipe it queues on.
fn operation(submit: &Submit) -> (u8, Op) {
    if submit.ep == 0 {
        let op = match usbip::special(&submit.setup) {
            Some(Special::SetConfiguration(value)) => Op::SetConfiguration(value),
            Some(Special::SetInterface {
                interface,
                alternate,
            }) => Op::SetInterface {
                interface,
                alternate,
            },
            Some(Special::ClearHalt { endpoint }) => Op::ClearHalt(endpoint),
            Some(Special::ResetDevice) => Op::Reset,
            None => Op::Control {
                setup: submit.setup,
                out: submit.data.clone(),
                in_len: if submit.is_in() { submit.length } else { 0 },
            },
        };
        return (0, op);
    }
    let endpoint = submit.endpoint_address();
    (
        endpoint,
        Op::Transfer {
            endpoint,
            out: submit.data.clone(),
            in_len: if submit.is_in() { submit.length } else { 0 },
            zero_packet: !submit.is_in() && submit.transfer_flags & URB_ZERO_PACKET != 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::usbip::{DIR_IN, DIR_OUT, Reply, encode, parse_reply};
    use std::sync::mpsc;
    use std::time::Duration;

    /// A device the test finishes by hand: every start and abort is reported
    /// on a channel, and the test completes operations through the sink.
    struct Mock {
        log: mpsc::Sender<Call>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Start(u32, Op),
        Abort(u8),
        Close,
    }

    impl Device for Mock {
        fn start(&mut self, tag: u32, op: Op) {
            let _ = self.log.send(Call::Start(tag, op));
        }
        fn abort(&mut self, endpoint: u8) {
            let _ = self.log.send(Call::Abort(endpoint));
        }
        fn close(&mut self) {
            let _ = self.log.send(Call::Close);
        }
    }

    struct Rig {
        guest: UnixStream,
        sink: Sink,
        calls: mpsc::Receiver<Call>,
        server: Server,
        buf: Vec<u8>,
        ins: std::collections::HashSet<u32>,
    }

    fn rig() -> Rig {
        let server = Server::start().unwrap();
        let (guest, host) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (tx, calls) = mpsc::channel();
        let sink = server.sink();
        server.serve(&sink, host, Box::new(Mock { log: tx }));
        Rig {
            guest,
            sink,
            calls,
            server,
            buf: Vec::new(),
            ins: Default::default(),
        }
    }

    impl Rig {
        fn send(&mut self, command: Command) {
            if let Command::Submit(s) = &command
                && s.is_in()
            {
                self.ins.insert(s.seqnum);
            }
            self.guest.write_all(&encode(&command)).unwrap();
        }

        fn call(&self) -> Call {
            self.calls
                .recv_timeout(Duration::from_secs(5))
                .expect("a call on the device")
        }

        fn no_call(&self) {
            assert_eq!(
                self.calls.recv_timeout(Duration::from_millis(100)).ok(),
                None
            );
        }

        fn reply(&mut self) -> Reply {
            loop {
                let ins = self.ins.clone();
                if let Some((reply, used)) = parse_reply(&self.buf, |s| ins.contains(&s)) {
                    self.buf.drain(..used);
                    return reply;
                }
                let mut chunk = [0u8; 4096];
                let n = self.guest.read(&mut chunk).expect("a reply");
                assert!(n > 0, "the session ended");
                self.buf.extend_from_slice(&chunk[..n]);
            }
        }

        fn until_not_serving(&self) {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while self.server.serving(&self.sink) {
                assert!(std::time::Instant::now() < deadline, "still served");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn done(&self, tag: u32, status: i32, actual: usize, data: &[u8]) {
            self.sink.done(Done {
                tag,
                status,
                actual,
                data: data.to_vec(),
            });
        }

        fn no_reply(&mut self) {
            self.guest
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut chunk = [0u8; 64];
            let got = self.guest.read(&mut chunk);
            assert!(
                matches!(got, Err(ref e) if e.kind() == io::ErrorKind::WouldBlock),
                "unexpected {got:?}"
            );
            self.guest
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
        }
    }

    fn submit(seqnum: u32, direction: u32, ep: u8, length: usize, data: Vec<u8>) -> Command {
        Command::Submit(Submit {
            seqnum,
            devid: 1,
            direction,
            ep,
            transfer_flags: 0,
            length,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0; 8],
            data,
            iso: Vec::new(),
        })
    }

    fn control(seqnum: u32, direction: u32, setup: [u8; 8], data: Vec<u8>) -> Command {
        let Command::Submit(mut s) = submit(
            seqnum,
            direction,
            0,
            u16::from_le_bytes([setup[6], setup[7]]) as usize,
            data,
        ) else {
            unreachable!()
        };
        s.setup = setup;
        Command::Submit(s)
    }

    #[test]
    fn transfers_go_to_their_pipe_and_are_answered() {
        let mut r = rig();
        r.send(submit(1, DIR_OUT, 2, 3, b"abc".to_vec()));
        assert_eq!(
            r.call(),
            Call::Start(
                1,
                Op::Transfer {
                    endpoint: 0x02,
                    out: b"abc".to_vec(),
                    in_len: 0,
                    zero_packet: false
                }
            )
        );
        r.sink.done(Done {
            tag: 1,
            status: 0,
            actual: 3,
            data: Vec::new(),
        });
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 1,
                status: 0,
                actual: 3,
                data: Vec::new()
            }
        );

        r.send(submit(2, DIR_IN, 1, 64, Vec::new()));
        assert_eq!(
            r.call(),
            Call::Start(
                2,
                Op::Transfer {
                    endpoint: 0x81,
                    out: Vec::new(),
                    in_len: 64,
                    zero_packet: false
                }
            )
        );
        r.sink.done(Done {
            tag: 2,
            status: 0,
            actual: 5,
            data: b"hello".to_vec(),
        });
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 2,
                status: 0,
                actual: 5,
                data: b"hello".to_vec()
            }
        );
    }

    #[test]
    fn control_requests_the_host_owns_are_carried_out_not_forwarded() {
        let mut r = rig();
        r.send(control(1, DIR_OUT, [0x00, 9, 1, 0, 0, 0, 0, 0], Vec::new()));
        assert_eq!(r.call(), Call::Start(1, Op::SetConfiguration(1)));
        r.send(control(
            2,
            DIR_OUT,
            [0x01, 11, 1, 0, 2, 0, 0, 0],
            Vec::new(),
        ));
        assert_eq!(
            r.call(),
            Call::Start(
                2,
                Op::SetInterface {
                    interface: 2,
                    alternate: 1
                }
            )
        );
        r.send(control(
            3,
            DIR_OUT,
            [0x02, 1, 0, 0, 0x81, 0, 0, 0],
            Vec::new(),
        ));
        assert_eq!(r.call(), Call::Start(3, Op::ClearHalt(0x81)));
        // CDC-ACM's SET_LINE_CODING goes to the device as it is.
        let coding = vec![0x00, 0xc2, 0x01, 0x00, 0, 0, 8];
        r.send(control(
            4,
            DIR_OUT,
            [0x21, 0x20, 0, 0, 0, 0, 7, 0],
            coding.clone(),
        ));
        assert_eq!(
            r.call(),
            Call::Start(
                4,
                Op::Control {
                    setup: [0x21, 0x20, 0, 0, 0, 0, 7, 0],
                    out: coding,
                    in_len: 0
                }
            )
        );
        // A descriptor read comes back with its bytes.
        r.send(control(5, DIR_IN, [0x80, 6, 0, 1, 0, 0, 18, 0], Vec::new()));
        assert_eq!(
            r.call(),
            Call::Start(
                5,
                Op::Control {
                    setup: [0x80, 6, 0, 1, 0, 0, 18, 0],
                    out: Vec::new(),
                    in_len: 18
                }
            )
        );
        r.sink.done(Done {
            tag: 5,
            status: 0,
            actual: 18,
            data: vec![18; 18],
        });
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 5,
                status: 0,
                actual: 18,
                data: vec![18; 18]
            }
        );
    }

    #[test]
    fn an_unlinked_urb_is_answered_by_its_unlink_alone() {
        let mut r = rig();
        r.send(submit(1, DIR_IN, 1, 64, Vec::new()));
        r.call();
        r.send(Command::Unlink {
            seqnum: 2,
            victim: 1,
        });
        assert_eq!(r.call(), Call::Abort(0x81));
        r.sink.done(Done {
            tag: 1,
            status: status::ECONNRESET,
            actual: 0,
            data: Vec::new(),
        });
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 2,
                status: status::ECONNRESET
            }
        );
        r.no_reply();
    }

    #[test]
    fn an_unlink_that_finds_its_urb_answered_says_so() {
        let mut r = rig();
        r.send(submit(1, DIR_OUT, 2, 1, vec![1]));
        r.call();
        r.sink.done(Done {
            tag: 1,
            status: 0,
            actual: 1,
            data: Vec::new(),
        });
        assert!(matches!(r.reply(), Reply::Submit { seqnum: 1, .. }));
        r.send(Command::Unlink {
            seqnum: 2,
            victim: 1,
        });
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 2,
                status: 0
            }
        );
        r.no_call();
    }

    #[test]
    fn a_urb_that_finishes_as_it_is_unlinked_is_answered_by_the_unlink_with_its_status() {
        let mut r = rig();
        r.send(submit(1, DIR_IN, 1, 64, Vec::new()));
        r.call();
        r.send(Command::Unlink {
            seqnum: 2,
            victim: 1,
        });
        r.call();
        r.sink.done(Done {
            tag: 1,
            status: 0,
            actual: 4,
            data: b"late".to_vec(),
        });
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 2,
                status: 0
            }
        );
        r.no_reply();
    }

    /// macOS aborts a whole pipe: a URB beside the unlinked one is submitted
    /// again when nothing moved, and answered short when something did.
    #[test]
    fn urbs_caught_by_anothers_abort_are_resubmitted_or_answered_short() {
        let mut r = rig();
        r.send(submit(1, DIR_IN, 1, 64, Vec::new()));
        r.call();
        r.send(submit(2, DIR_IN, 1, 64, Vec::new()));
        r.call();
        r.send(submit(3, DIR_IN, 1, 64, Vec::new()));
        r.call();
        r.send(Command::Unlink {
            seqnum: 4,
            victim: 1,
        });
        assert_eq!(r.call(), Call::Abort(0x81));
        r.sink.done(Done {
            tag: 1,
            status: status::ECONNRESET,
            actual: 0,
            data: Vec::new(),
        });
        r.sink.done(Done {
            tag: 2,
            status: status::ECONNRESET,
            actual: 0,
            data: Vec::new(),
        });
        r.sink.done(Done {
            tag: 3,
            status: status::ECONNRESET,
            actual: 2,
            data: b"hi".to_vec(),
        });
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 4,
                status: status::ECONNRESET
            }
        );
        assert_eq!(
            r.call(),
            Call::Start(
                2,
                Op::Transfer {
                    endpoint: 0x81,
                    out: Vec::new(),
                    in_len: 64,
                    zero_packet: false
                }
            )
        );
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 3,
                status: 0,
                actual: 2,
                data: b"hi".to_vec()
            }
        );
        r.no_reply();
    }

    #[test]
    fn isochronous_urbs_fail_rather_than_hang() {
        let mut r = rig();
        let Command::Submit(mut s) = submit(1, DIR_IN, 3, 192, Vec::new()) else {
            unreachable!()
        };
        s.number_of_packets = 1;
        s.iso = vec![0; 16];
        r.send(Command::Submit(s));
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 1,
                status: status::EOPNOTSUPP,
                actual: 0,
                data: Vec::new()
            }
        );
        r.no_call();
    }

    #[test]
    fn a_device_that_goes_away_ends_the_stream_and_is_closed() {
        let mut r = rig();
        r.send(submit(1, DIR_IN, 1, 64, Vec::new()));
        r.call();
        r.sink.gone();
        assert_eq!(r.call(), Call::Close);
        let mut chunk = [0u8; 16];
        assert_eq!(
            r.guest.read(&mut chunk).unwrap(),
            0,
            "the guest sees the stream end"
        );
        // Served until the device is back with macOS, so nothing seizes it
        // again while it is being released.
        std::thread::sleep(Duration::from_millis(100));
        assert!(r.server.serving(&r.sink));
        r.sink.closed();
        r.until_not_serving();
    }

    #[test]
    fn ending_a_session_unplugs_it_for_the_guest_and_gives_the_device_back() {
        let mut r = rig();
        r.server.end(&r.sink);
        assert_eq!(r.call(), Call::Close);
        let mut chunk = [0u8; 16];
        assert_eq!(r.guest.read(&mut chunk).unwrap(), 0);
    }

    #[test]
    fn the_guest_detaching_gives_the_device_back() {
        let r = rig();
        r.guest.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(r.call(), Call::Close);
    }

    #[test]
    fn a_corrupt_stream_ends_the_session() {
        let mut r = rig();
        let mut bytes = encode(&submit(1, DIR_IN, 1, 64, Vec::new()));
        bytes[3] = 99;
        r.guest.write_all(&bytes).unwrap();
        assert_eq!(r.call(), Call::Close);
    }

    /// A guest that stops reading its replies stops having its submits
    /// started: what the host holds for it is bounded.
    #[test]
    fn replies_that_back_up_hold_new_submits_until_they_are_read() {
        let mut r = rig();
        let big = 256 * 1024;
        let answer = |r: &Rig, tag| r.done(tag, 0, big, &vec![0; big]);
        // Each submit is answered with a quarter megabyte the guest does not
        // read, until one is held.
        let mut seq = 0u32;
        let held = loop {
            seq += 1;
            assert!(
                seq <= 100,
                "every submit was started while no reply was read"
            );
            r.send(submit(seq, DIR_IN, 1, big, Vec::new()));
            match r.calls.recv_timeout(Duration::from_millis(300)) {
                Ok(Call::Start(tag, _)) => answer(&r, tag),
                Ok(other) => panic!("{other:?}"),
                Err(_) => break seq,
            }
        };
        assert!(
            (held as usize - 1) * big >= TX_HIGH,
            "it held a submit early ({held})"
        );
        // Commands are still read while it is held.
        r.send(Command::Unlink {
            seqnum: 1000,
            victim: 1,
        });
        // Reading the replies starts the held one, and the rest run.
        let reader = std::thread::spawn({
            let mut guest = r.guest.try_clone().unwrap();
            move || {
                let mut chunk = vec![0u8; 1 << 20];
                let mut total = 0usize;
                let want = 100 * (big + usbip::HEADER_LEN) + usbip::HEADER_LEN;
                while total < want {
                    match guest.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => total += n,
                    }
                }
                total
            }
        });
        assert_eq!(r.call(), Call::Start(held, transfer(0x81, big)));
        answer(&r, held);
        for seq in held + 1..=100 {
            r.send(submit(seq, DIR_IN, 1, big, Vec::new()));
            assert_eq!(r.call(), Call::Start(seq, transfer(0x81, big)));
            answer(&r, seq);
        }
        assert_eq!(
            reader.join().unwrap(),
            100 * (big + usbip::HEADER_LEN) + usbip::HEADER_LEN
        );
    }

    fn with_flags(command: Command, flags: u32) -> Command {
        let Command::Submit(mut s) = command else {
            unreachable!()
        };
        s.transfer_flags = flags;
        Command::Submit(s)
    }

    fn transfer(endpoint: u8, in_len: usize) -> Op {
        Op::Transfer {
            endpoint,
            out: Vec::new(),
            in_len,
            zero_packet: false,
        }
    }

    /// An unlink is acted on while replies back up: it is what frees them.
    #[test]
    fn unlinks_are_acted_on_while_replies_back_up() {
        let mut r = rig();
        r.send(submit(1, DIR_IN, 2, 64, Vec::new()));
        assert_eq!(r.call(), Call::Start(1, transfer(0x82, 64)));
        let big = 8 << 20;
        r.send(submit(2, DIR_IN, 1, big, Vec::new()));
        r.call();
        r.done(2, 0, big, &vec![7; big]);
        // Past the mark: a new submit waits, and is not started.
        r.send(submit(3, DIR_IN, 1, 64, Vec::new()));
        r.no_call();
        // Unlinking a URB on the device still aborts its pipe.
        r.send(Command::Unlink {
            seqnum: 4,
            victim: 1,
        });
        assert_eq!(r.call(), Call::Abort(0x82));
        // Unlinking the one that waits stops it here.
        r.send(Command::Unlink {
            seqnum: 5,
            victim: 3,
        });
        r.no_call();
        r.done(1, status::ECONNRESET, 0, &[]);
        assert!(matches!(r.reply(), Reply::Submit { seqnum: 2, actual, .. } if actual == big));
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 5,
                status: status::ECONNRESET
            }
        );
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 4,
                status: status::ECONNRESET
            }
        );
        r.no_reply();
        r.no_call();
    }

    /// Bytes on a pipe keep their order through another URB's abort: what
    /// it caught goes back first, then what came while it ran.
    #[test]
    fn a_pipe_keeps_its_order_through_an_abort() {
        let mut r = rig();
        r.send(submit(1, DIR_OUT, 2, 1, vec![1]));
        r.call();
        r.send(submit(2, DIR_OUT, 2, 1, vec![2]));
        r.call();
        r.send(Command::Unlink {
            seqnum: 3,
            victim: 1,
        });
        assert_eq!(r.call(), Call::Abort(0x02));
        r.send(submit(4, DIR_OUT, 2, 1, vec![4]));
        // Another pipe is not held.
        r.send(submit(5, DIR_IN, 1, 64, Vec::new()));
        assert_eq!(r.call(), Call::Start(5, transfer(0x81, 64)));
        r.no_call();
        r.done(1, status::ECONNRESET, 0, &[]);
        r.no_call();
        r.done(2, status::ECONNRESET, 0, &[]);
        let out = |b: u8| Op::Transfer {
            endpoint: 0x02,
            out: vec![b],
            in_len: 0,
            zero_packet: false,
        };
        assert_eq!(r.call(), Call::Start(2, out(2)));
        assert_eq!(r.call(), Call::Start(4, out(4)));
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 3,
                status: status::ECONNRESET
            }
        );
    }

    /// A control request is never repeated: one with no data stage cannot
    /// say whether the device acted on it.
    #[test]
    fn control_requests_caught_by_an_abort_are_answered_not_repeated() {
        let mut r = rig();
        r.send(control(1, DIR_IN, [0x80, 6, 0, 1, 0, 0, 18, 0], Vec::new()));
        r.call();
        r.send(control(
            2,
            DIR_OUT,
            [0x21, 0x22, 3, 0, 0, 0, 0, 0],
            Vec::new(),
        ));
        r.call();
        r.send(Command::Unlink {
            seqnum: 3,
            victim: 1,
        });
        assert_eq!(r.call(), Call::Abort(0));
        r.done(1, status::ECONNRESET, 0, &[]);
        r.done(2, status::ECONNRESET, 0, &[]);
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 3,
                status: status::ECONNRESET
            }
        );
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 2,
                status: status::ECONNRESET,
                actual: 0,
                data: Vec::new()
            }
        );
        r.no_call();
    }

    #[test]
    fn a_device_gone_before_it_is_served_ends_its_session() {
        let server = Server::start().unwrap();
        let (mut guest, host) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (tx, calls) = mpsc::channel();
        let sink = server.sink();
        sink.gone();
        std::thread::sleep(Duration::from_millis(50));
        server.serve(&sink, host, Box::new(Mock { log: tx }));
        assert_eq!(
            calls.recv_timeout(Duration::from_secs(5)).unwrap(),
            Call::Close
        );
        let mut chunk = [0u8; 16];
        assert_eq!(guest.read(&mut chunk).unwrap(), 0);
    }

    #[test]
    fn transfer_flags_are_honored() {
        let mut r = rig();
        r.send(with_flags(
            submit(1, DIR_OUT, 2, 64, vec![0; 64]),
            URB_ZERO_PACKET,
        ));
        assert_eq!(
            r.call(),
            Call::Start(
                1,
                Op::Transfer {
                    endpoint: 0x02,
                    out: vec![0; 64],
                    in_len: 0,
                    zero_packet: true
                }
            )
        );
        r.send(with_flags(
            submit(2, DIR_IN, 1, 64, Vec::new()),
            URB_SHORT_NOT_OK,
        ));
        r.call();
        r.done(2, 0, 5, b"short");
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 2,
                status: status::EREMOTEIO,
                actual: 5,
                data: b"short".to_vec()
            }
        );
    }

    #[test]
    fn a_control_request_whose_setup_and_header_disagree_is_refused() {
        let mut r = rig();
        // A host-to-device setup packet in a device-to-host URB.
        r.send(control(
            1,
            DIR_IN,
            [0x21, 0x22, 3, 0, 0, 0, 0, 0],
            Vec::new(),
        ));
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 1,
                status: status::EINVAL,
                actual: 0,
                data: Vec::new()
            }
        );
        r.no_call();
    }

    #[test]
    fn a_device_reporting_more_than_it_sent_is_clamped() {
        let mut r = rig();
        r.send(submit(1, DIR_IN, 1, 64, Vec::new()));
        r.call();
        r.done(1, 0, 10, b"abc");
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 1,
                status: 0,
                actual: 3,
                data: b"abc".to_vec()
            }
        );
    }

    /// An abort macOS could not start answers the unlink at once, and the
    /// pipe carries on; a completion that comes later is not answered twice.
    #[test]
    fn an_abort_that_fails_answers_the_unlink_and_frees_the_pipe() {
        let mut r = rig();
        r.send(submit(1, DIR_IN, 1, 64, Vec::new()));
        r.call();
        r.send(submit(2, DIR_IN, 1, 64, Vec::new()));
        r.call();
        r.send(Command::Unlink {
            seqnum: 3,
            victim: 1,
        });
        assert_eq!(r.call(), Call::Abort(0x81));
        r.send(submit(4, DIR_IN, 1, 64, Vec::new()));
        r.no_call();
        r.sink.abort_failed(0x81);
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 3,
                status: status::ECONNRESET
            }
        );
        assert_eq!(r.call(), Call::Start(4, transfer(0x81, 64)));
        r.done(1, 0, 3, b"old");
        r.done(2, 0, 2, b"ok");
        assert_eq!(
            r.reply(),
            Reply::Submit {
                seqnum: 2,
                status: 0,
                actual: 2,
                data: b"ok".to_vec()
            }
        );
        r.no_reply();
    }

    /// A transfer caught by another's abort that cannot say whether it
    /// finished is answered with the abort, never sent again nor called
    /// complete.
    #[test]
    fn transfers_that_cannot_say_whether_they_finished_are_not_repeated() {
        let mut r = rig();
        r.send(submit(1, DIR_OUT, 2, 1, vec![1]));
        r.call();
        // A zero-length packet: nothing moved either way.
        r.send(submit(2, DIR_OUT, 2, 0, Vec::new()));
        r.call();
        // Its data sent, its closing zero-length packet perhaps not.
        r.send(with_flags(
            submit(3, DIR_OUT, 2, 64, vec![0; 64]),
            URB_ZERO_PACKET,
        ));
        r.call();
        r.send(Command::Unlink {
            seqnum: 4,
            victim: 1,
        });
        assert_eq!(r.call(), Call::Abort(0x02));
        r.done(1, status::ECONNRESET, 0, &[]);
        r.done(2, status::ECONNRESET, 0, &[]);
        r.done(3, status::ECONNRESET, 64, &[]);
        assert_eq!(
            r.reply(),
            Reply::Unlink {
                seqnum: 4,
                status: status::ECONNRESET
            }
        );
        for seqnum in [2, 3] {
            assert_eq!(
                r.reply(),
                Reply::Submit {
                    seqnum,
                    status: status::ECONNRESET,
                    actual: if seqnum == 3 { 64 } else { 0 },
                    data: Vec::new()
                }
            );
        }
        r.no_call();
    }

    /// Ending a session already closed leaves nothing behind to be served.
    #[test]
    fn ending_a_closed_session_is_a_no_op() {
        let r = rig();
        r.sink.gone();
        assert_eq!(r.call(), Call::Close);
        r.sink.closed();
        r.until_not_serving();
        r.server.end(&r.sink);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!r.server.serving(&r.sink));
    }
}

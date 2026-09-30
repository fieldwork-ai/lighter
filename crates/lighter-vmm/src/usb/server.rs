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
//! with status 0. macOS can abort only a whole pipe, never one request, so an
//! abort stops every URB queued on the endpoint. The ones nobody unlinked
//! are then submitted again when nothing had moved, and answered with what
//! did move otherwise; a guest sees what it would on Linux.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::usbip::{self, Command, Special, Submit, status};
use crate::reactor::Kq;

/// What a session may hold at once: replies not yet written, plus what its
/// pending URBs carry (an out transfer's data) or will bring back (an in
/// transfer's length). Past the high mark the session stops taking commands,
/// and its socket stops being read, until it is under the low mark: a guest
/// that submits faster than it reads cannot grow the host without bound.
const COMMITTED_HIGH: usize = 4 << 20;
const COMMITTED_LOW: usize = 1 << 20;
const READ_CHUNK: usize = 64 * 1024;

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
    /// included).
    Transfer {
        endpoint: u8,
        out: Vec<u8>,
        in_len: usize,
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
    /// Ends the session's hold on the device, giving it back to macOS.
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
}

/// A session handed to the loop: its id, its socket and its device.
type Arrival = (u64, UnixStream, Box<dyn Device>);

struct Shared {
    events: Mutex<Vec<(u64, Event)>>,
    arrivals: Mutex<Vec<Arrival>>,
    wake_write: RawFd,
    next: AtomicU64,
    /// The sessions the loop holds now, for callers that ask.
    live: Mutex<Vec<u64>>,
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
            live: Mutex::new(Vec::new()),
        });
        let kq = Kq::new()?;
        kq.read(fds[0], true);
        let looped = shared.clone();
        std::thread::Builder::new()
            .name("usb".into())
            .spawn(move || Loop::new(looped, kq, fds[0]).run())?;
        Ok(Server { shared })
    }

    /// A sink for a session about to start: the device is opened with it,
    /// then handed to [`Server::serve`].
    pub fn sink(&self) -> Sink {
        Sink {
            id: self.shared.next.fetch_add(1, Ordering::Relaxed),
            shared: self.shared.clone(),
        }
    }

    /// Serves `device` on `socket` until either ends.
    pub fn serve(&self, sink: &Sink, socket: UnixStream, device: Box<dyn Device>) {
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

    /// Whether a session is still being served.
    pub fn serving(&self, sink: &Sink) -> bool {
        self.shared
            .live
            .lock()
            .expect("usb live poisoned")
            .contains(&sink.id)
    }
}

struct Pending {
    /// The pipe the URB is queued on: its endpoint address, 0 for control.
    pipe: u8,
    submit: Submit,
    /// The unlink that asked to stop it, whose reply answers it.
    unlink: Option<u32>,
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
    pending: HashMap<u32, Pending>,
    /// Bytes of replies queued plus bytes the pending URBs hold or await.
    committed: usize,
    /// Taking commands; off while `committed` is above the low mark again.
    taking: bool,
    /// The guest's end has closed: finish writing, then end.
    closing: bool,
}

impl Session {
    fn hold(&mut self, seqnum: u32, pending: Pending) {
        self.committed += weight(&pending.submit);
        self.pending.insert(seqnum, pending);
    }

    fn release(&mut self, seqnum: u32) -> Option<Pending> {
        let p = self.pending.remove(&seqnum)?;
        self.committed -= weight(&p.submit);
        Some(p)
    }
}

/// What a pending URB counts against its session.
fn weight(submit: &Submit) -> usize {
    submit.data.len() + if submit.is_in() { submit.length } else { 0 }
}

struct Loop {
    shared: Arc<Shared>,
    kq: Kq,
    wake_read: RawFd,
    sessions: HashMap<u64, Session>,
    by_fd: HashMap<RawFd, u64>,
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
            for ev in &events[..n as usize] {
                let fd = ev.ident as RawFd;
                if fd == self.wake_read {
                    self.drain_wake();
                    continue;
                }
                let Some(&id) = self.by_fd.get(&fd) else {
                    continue;
                };
                if ev.filter == libc::EVFILT_READ {
                    self.read(id);
                } else if ev.filter == libc::EVFILT_WRITE {
                    self.pace(id);
                }
            }
            self.take_arrivals();
            self.take_events();
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
        for (id, socket, device) in arrivals {
            if socket.set_nonblocking(true).is_err() {
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
                    committed: 0,
                    taking: true,
                    closing: false,
                },
            );
            self.shared.live.lock().expect("usb live poisoned").push(id);
        }
    }

    fn take_events(&mut self) {
        let events = std::mem::take(&mut *self.shared.events.lock().expect("usb events poisoned"));
        for (id, event) in events {
            match event {
                Event::Done(done) => self.done(id, done),
                Event::Gone => {
                    tracing::info!(session = id, "usb: the device went away");
                    self.end(id);
                }
                Event::End => self.end(id),
            }
        }
    }

    fn read(&mut self, id: u64) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        loop {
            match session.socket.read(&mut self.buf) {
                Ok(0) => {
                    // The guest detached, or the machine is stopping.
                    session.closing = true;
                    break;
                }
                Ok(n) => session.rx.extend_from_slice(&self.buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    session.closing = true;
                    break;
                }
            }
            if session.committed >= COMMITTED_HIGH {
                break;
            }
        }
        self.process(id);
    }

    /// Takes the commands the session has read, while it may.
    fn process(&mut self, id: u64) {
        let mut at = 0;
        let mut corrupt = false;
        loop {
            let Some(session) = self.sessions.get(&id) else {
                return;
            };
            if session.committed >= COMMITTED_HIGH {
                break;
            }
            match usbip::parse(&session.rx[at..]) {
                Ok(Some((command, used))) => {
                    at += used;
                    self.command(id, command);
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(session = id, "usb: {e}; ending the device's session");
                    corrupt = true;
                    break;
                }
            }
        }
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.rx.drain(..at);
        if corrupt {
            self.end(id);
            return;
        }
        self.pace(id);
    }

    fn command(&mut self, id: u64, command: Command) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        match command {
            Command::Submit(submit) => {
                if submit.is_iso() {
                    // Isochronous transfers (audio, video, a Bluetooth
                    // dongle's voice channel) are not carried yet; the URB
                    // fails and the driver sees why.
                    queue(
                        session,
                        usbip::ret_submit(submit.seqnum, status::EOPNOTSUPP, 0, &[]),
                    );
                    self.pace(id);
                    return;
                }
                let (pipe, op) = operation(&submit);
                let seqnum = submit.seqnum;
                session.hold(
                    seqnum,
                    Pending {
                        pipe,
                        submit,
                        unlink: None,
                    },
                );
                session.device.start(seqnum, op);
            }
            Command::Unlink { seqnum, victim } => {
                match session.pending.get_mut(&victim) {
                    Some(p) if p.unlink.is_none() => {
                        p.unlink = Some(seqnum);
                        let pipe = p.pipe;
                        session.device.abort(pipe);
                    }
                    // Answered already, or already being stopped by an
                    // earlier unlink: nothing of this one's to stop.
                    _ => queue(session, usbip::ret_unlink(seqnum, status::OK)),
                }
                self.pace(id);
            }
        }
    }

    fn done(&mut self, id: u64, done: Done) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let Some(p) = session.release(done.tag) else {
            return;
        };
        if let Some(unlink) = p.unlink {
            queue(session, usbip::ret_unlink(unlink, done.status));
        } else if done.status == status::ECONNRESET && done.actual == 0 {
            // Caught by an abort another URB's unlink asked for: nothing
            // moved, so it goes back on its pipe as if nothing happened.
            let (pipe, op) = operation(&p.submit);
            session.hold(
                done.tag,
                Pending {
                    pipe,
                    submit: p.submit,
                    unlink: None,
                },
            );
            session.device.start(done.tag, op);
            return;
        } else {
            // Answered with what moved, including a transfer another
            // URB's abort cut short, which is then short, not failed.
            let status = if done.status == status::ECONNRESET {
                status::OK
            } else {
                done.status
            };
            let data: &[u8] = if p.submit.is_in() {
                &done.data[..done.actual.min(done.data.len())]
            } else {
                &[]
            };
            queue(
                session,
                usbip::ret_submit(done.tag, status, done.actual, data),
            );
        }
        self.pace(id);
    }

    /// Writes what it can, and sets the socket's watches for what the session
    /// waits on: writable while replies wait, readable while it takes
    /// commands. A session back under its low mark takes the commands it had
    /// already read.
    fn pace(&mut self, id: u64) {
        self.flush(id);
        let resume = self
            .sessions
            .get(&id)
            .is_some_and(|s| !s.taking && s.committed < COMMITTED_LOW);
        if resume {
            if let Some(s) = self.sessions.get_mut(&id) {
                s.taking = true;
            }
            self.process(id);
        }
    }

    fn flush(&mut self, id: u64) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        while let Some(front) = session.tx.front() {
            match session.socket.write(&front[session.tx_at..]) {
                Ok(n) => {
                    session.tx_at += n;
                    session.tx_bytes -= n;
                    session.committed -= n;
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
        let fd = session.socket.as_raw_fd();
        let want_write = !session.tx.is_empty();
        if want_write != session.writing {
            session.writing = want_write;
            self.kq.write(fd, want_write);
        }
        if session.committed >= COMMITTED_HIGH {
            session.taking = false;
        }
        let want_read = !session.closing && session.taking;
        if want_read != session.reading {
            session.reading = want_read;
            self.kq.read(fd, want_read);
        }
        if session.closing && session.tx.is_empty() {
            self.end(id);
        }
    }

    fn end(&mut self, id: u64) {
        let Some(mut session) = self.sessions.remove(&id) else {
            return;
        };
        let fd = session.socket.as_raw_fd();
        self.kq.forget(fd);
        self.by_fd.remove(&fd);
        self.shared
            .live
            .lock()
            .expect("usb live poisoned")
            .retain(|&l| l != id);
        let _ = session.socket.shutdown(std::net::Shutdown::Both);
        session.device.close();
    }
}

fn queue(session: &mut Session, reply: Vec<u8>) {
    session.tx_bytes += reply.len();
    session.committed += reply.len();
    session.tx.push_back(reply);
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
                    in_len: 0
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
                    in_len: 64
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
                    in_len: 64
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
        assert!(!r.server.serving(&r.sink));
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

    /// A guest that stops reading stops being read: its replies cannot grow
    /// without bound.
    #[test]
    fn replies_that_back_up_stop_the_commands_being_read() {
        let r = rig();
        let big = 256 * 1024;
        let mut started = 0u32;
        // Each submit is answered at once with a quarter megabyte the guest
        // never reads; the server must stop taking submits well before a
        // hundred of them are answered.
        let writer = r.guest.try_clone().unwrap();
        writer.set_nonblocking(true).unwrap();
        let mut writer = writer;
        for seq in 1..=100u32 {
            let bytes = encode(&submit(seq, DIR_IN, 1, big, Vec::new()));
            let _ = writer.write(&bytes);
        }
        while let Ok(Call::Start(tag, _)) = r.calls.recv_timeout(Duration::from_millis(300)) {
            started += 1;
            r.sink.done(Done {
                tag,
                status: 0,
                actual: big,
                data: vec![0; big],
            });
        }
        assert!(
            started < 100,
            "every submit was taken while no reply was read ({started})"
        );
        assert!(
            started as usize * big >= COMMITTED_HIGH,
            "it stopped early ({started})"
        );
        // Reading the replies lets it take the rest. (The writer's clone
        // shares the socket's non-blocking flag; the reader wants blocking.)
        r.guest.set_nonblocking(false).unwrap();
        let mut replies = 0;
        let reader = std::thread::spawn({
            let mut guest = r.guest.try_clone().unwrap();
            move || {
                let mut chunk = vec![0u8; 1 << 20];
                let mut total = 0usize;
                while total < 100 * (big + usbip::HEADER_LEN) {
                    match guest.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => total += n,
                    }
                }
                total
            }
        });
        while let Ok(Call::Start(tag, _)) = r.calls.recv_timeout(Duration::from_secs(2)) {
            replies += 1;
            r.sink.done(Done {
                tag,
                status: 0,
                actual: big,
                data: vec![0; big],
            });
        }
        assert_eq!(
            started + replies,
            100,
            "every submit is taken once replies are read"
        );
        assert_eq!(reader.join().unwrap(), 100 * (big + usbip::HEADER_LEN));
    }
}

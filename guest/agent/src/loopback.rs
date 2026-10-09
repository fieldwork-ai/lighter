//! A host-network container's `localhost`, where the guest has nothing.
//!
//! On the Mac a container's `localhost` would be the Mac's, so a port
//! nothing in the guest listens on is carried to the Mac's own loopback
//! (init's `local_tcp` and the redirect after it). Carried blindly, a
//! connection to a port nobody listens on anywhere would be accepted here
//! and then reset, which `nc -z` and every wait-for-port loop reads as up.
//! So each such connection's SYN waits in an nft queue while the Mac is
//! asked whether its loopback has the port: yes, and it goes on to the
//! redirect; no, and init's rule refuses it with a reset, which the client
//! sees as "connection refused", as it would on the Mac.
//!
//! The Mac is asked through the DNS stream the guest already has: a
//! question for `<port>.tcp.loopback.lighter.internal`, A for 127.0.0.1 and
//! AAAA for ::1, answered by the VMM from a bind of that address (in use
//! means something holds it) with the address, or with nothing.
//!
//! Without this thread the queue's `bypass` lets the SYN through, and the
//! connection is carried as before.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// init's `queue num`.
pub const QUEUE: u16 = 7;
/// init's mark for "refuse this one".
const REFUSE: u32 = 0x4c4f_4f52;

const NETLINK_NETFILTER: libc::c_int = 12;
const NFNL_SUBSYS_QUEUE: u16 = 3;
const NFQNL_MSG_PACKET: u16 = 0;
const NFQNL_MSG_VERDICT: u16 = 1;
const NFQNL_MSG_CONFIG: u16 = 2;
const NFQA_CFG_CMD: u16 = 1;
const NFQA_CFG_PARAMS: u16 = 2;
const NFQNL_CFG_CMD_BIND: u8 = 1;
const NFQNL_COPY_PACKET: u8 = 2;
const NFQA_PACKET_HDR: u16 = 1;
const NFQA_VERDICT_HDR: u16 = 2;
const NFQA_MARK: u16 = 3;
const NFQA_PAYLOAD: u16 = 10;
const NF_ACCEPT: u32 = 1;
const NF_REPEAT: u32 = 4;

/// How long the Mac has to say; past it the connection goes on as before.
const ASK_TIMEOUT: Duration = Duration::from_millis(500);

/// Starts the thread that answers the queue.
pub fn start(dns: SocketAddr) {
    let spawned = std::thread::Builder::new().name("loopback".into()).spawn(move || {
        let queue = match Queue::bind(QUEUE) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("lighter-agent: no loopback queue, localhost to the Mac is not checked: {e}");
                return;
            }
        };
        let mut buf = vec![0u8; 4096];
        loop {
            let (id, packet) = match queue.next(&mut buf) {
                Ok(Some(p)) => p,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("lighter-agent: the loopback queue stopped: {e}");
                    return;
                }
            };
            let open = match destination(&packet) {
                Some((v6, port)) => ask(dns, v6, port).unwrap_or(true),
                None => true,
            };
            let sent = if open { queue.verdict(id, NF_ACCEPT, None) } else { queue.verdict(id, NF_REPEAT, Some(REFUSE)) };
            if let Err(e) = sent {
                eprintln!("lighter-agent: loopback verdict: {e}");
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("lighter-agent: no thread for the loopback queue: {e}");
    }
}

/// The family and port a queued SYN is for.
pub fn destination(packet: &[u8]) -> Option<(bool, u16)> {
    let (v6, l4) = match packet.first()? >> 4 {
        4 => (false, usize::from(packet[0] & 0x0f) * 4),
        6 => (true, 40),
        _ => return None,
    };
    let tcp = packet.get(l4..l4 + 4)?;
    Some((v6, u16::from_be_bytes([tcp[2], tcp[3]])))
}

/// The question for the VMM: `<port>.tcp.loopback.lighter.internal`, A or
/// AAAA, with `id`.
pub fn question(id: u16, v6: bool, port: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(64);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in [port.to_string().as_str(), "tcp", "loopback", "lighter", "internal"] {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&(if v6 { 28u16 } else { 1u16 }).to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes());
    q
}

/// Whether a reply to [`question`] `id` says the port is open: an answer
/// at all.
pub fn says_open(reply: &[u8], id: u16) -> Option<bool> {
    if reply.len() < 12 || reply[0..2] != id.to_be_bytes() || reply[2] & 0x80 == 0 {
        return None;
    }
    Some(u16::from_be_bytes([reply[6], reply[7]]) > 0)
}

/// Asks the Mac; `None` when it did not say in time.
fn ask(dns: SocketAddr, v6: bool, port: u16) -> Option<bool> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.set_read_timeout(Some(ASK_TIMEOUT)).ok()?;
    let id = (std::process::id() as u16) ^ port;
    socket.send_to(&question(id, v6, port), dns).ok()?;
    let mut buf = [0u8; 512];
    loop {
        let (n, _) = socket.recv_from(&mut buf).ok()?;
        if let Some(open) = says_open(&buf[..n], id) {
            return Some(open);
        }
    }
}

/// An nfnetlink queue, bound.
struct Queue {
    fd: OwnedFd,
    num: u16,
}

impl Queue {
    fn bind(num: u16) -> io::Result<Queue> {
        // SAFETY: a plain socket call.
        let raw = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_NETFILTER) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        let queue = Queue { fd: unsafe { OwnedFd::from_raw_fd(raw) }, num };
        let mut cmd = vec![NFQNL_CFG_CMD_BIND, 0];
        cmd.extend_from_slice(&(libc::AF_INET as u16).to_be_bytes());
        queue.send(NFQNL_MSG_CONFIG, &[(NFQA_CFG_CMD, cmd)])?;
        let mut params = 128u32.to_be_bytes().to_vec();
        params.push(NFQNL_COPY_PACKET);
        queue.send(NFQNL_MSG_CONFIG, &[(NFQA_CFG_PARAMS, params)])?;
        Ok(queue)
    }

    /// One nfnetlink message to the queue subsystem, acknowledged.
    fn send(&self, kind: u16, attrs: &[(u16, Vec<u8>)]) -> io::Result<()> {
        let mut body = vec![libc::AF_UNSPEC as u8, 0];
        body.extend_from_slice(&self.num.to_be_bytes());
        for (kind, value) in attrs {
            body.extend_from_slice(&((4 + value.len()) as u16).to_ne_bytes());
            body.extend_from_slice(&kind.to_ne_bytes());
            body.extend_from_slice(value);
            while body.len() % 4 != 0 {
                body.push(0);
            }
        }
        let mut msg = Vec::with_capacity(16 + body.len());
        msg.extend_from_slice(&((16 + body.len()) as u32).to_ne_bytes());
        msg.extend_from_slice(&((NFNL_SUBSYS_QUEUE << 8) | kind).to_ne_bytes());
        msg.extend_from_slice(&(libc::NLM_F_REQUEST as u16).to_ne_bytes());
        msg.extend_from_slice(&[0; 8]);
        msg.extend_from_slice(&body);
        // SAFETY: a buffer of its length, to a netlink socket.
        if unsafe { libc::send(self.fd.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn verdict(&self, id: u32, verdict: u32, mark: Option<u32>) -> io::Result<()> {
        let mut header = verdict.to_be_bytes().to_vec();
        header.extend_from_slice(&id.to_be_bytes());
        let mut attrs = vec![(NFQA_VERDICT_HDR, header)];
        if let Some(mark) = mark {
            attrs.push((NFQA_MARK, mark.to_be_bytes().to_vec()));
        }
        self.send(NFQNL_MSG_VERDICT, &attrs)
    }

    /// The next queued packet's id and bytes; `None` for a message that is
    /// not one (an acknowledgement).
    fn next(&self, buf: &mut [u8]) -> io::Result<Option<(u32, Vec<u8>)>> {
        // SAFETY: a buffer of the length given.
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            let e = io::Error::last_os_error();
            // A burst the socket could not hold: those SYNs are retried by
            // their senders, and queued again.
            return if e.raw_os_error() == Some(libc::ENOBUFS) { Ok(None) } else { Err(e) };
        }
        Ok(packet(&buf[..n as usize]))
    }
}

/// A queued packet in one nfnetlink message: its id and its bytes.
pub fn packet(msg: &[u8]) -> Option<(u32, Vec<u8>)> {
    if msg.len() < 20 {
        return None;
    }
    let kind = u16::from_ne_bytes([msg[4], msg[5]]);
    if kind != (NFNL_SUBSYS_QUEUE << 8) | NFQNL_MSG_PACKET {
        return None;
    }
    let len = (u32::from_ne_bytes(msg[0..4].try_into().ok()?) as usize).min(msg.len());
    let mut attrs = &msg[20..len];
    let (mut id, mut payload) = (None, None);
    while attrs.len() >= 4 {
        let alen = usize::from(u16::from_ne_bytes([attrs[0], attrs[1]]));
        let akind = u16::from_ne_bytes([attrs[2], attrs[3]]) & 0x7fff;
        if alen < 4 || alen > attrs.len() {
            break;
        }
        let value = &attrs[4..alen];
        match akind {
            NFQA_PACKET_HDR if value.len() >= 4 => id = Some(u32::from_be_bytes(value[0..4].try_into().ok()?)),
            NFQA_PAYLOAD => payload = Some(value.to_vec()),
            _ => {}
        }
        let next = (alen + 3) & !3;
        if next >= attrs.len() {
            break;
        }
        attrs = &attrs[next..];
    }
    Some((id?, payload?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_syns_family_and_port() {
        let mut v4 = vec![0x45u8; 40];
        v4[20..24].copy_from_slice(&[0xc3, 0x50, 0x15, 0x38]); // 50000 -> 5432
        assert_eq!(destination(&v4), Some((false, 5432)));
        let mut v6 = vec![0u8; 60];
        v6[0] = 0x60;
        v6[40..44].copy_from_slice(&[0xc3, 0x50, 0x1f, 0x90]);
        assert_eq!(destination(&v6), Some((true, 8080)));
        assert_eq!(destination(&[0x45; 10]), None);
    }

    #[test]
    fn the_question_names_the_port_and_the_answer_is_whether_there_is_one() {
        let q = question(0x1234, false, 5432);
        let name: Vec<u8> = q[12..q.len() - 4].to_vec();
        assert_eq!(name, b"\x045432\x03tcp\x08loopback\x07lighter\x08internal\x00");
        assert_eq!(&q[q.len() - 4..], &[0, 1, 0, 1]);
        let v6 = question(1, true, 1);
        assert_eq!(&v6[v6.len() - 4..], &[0, 28, 0, 1]);
        let mut reply = q.clone();
        reply[2] |= 0x80;
        assert_eq!(says_open(&reply, 0x1234), Some(false));
        reply[7] = 1;
        assert_eq!(says_open(&reply, 0x1234), Some(true));
        assert_eq!(says_open(&reply, 0x4321), None, "another question's");
        assert_eq!(says_open(&q, 0x1234), None, "not a reply");
    }

    #[test]
    fn a_queued_packet_is_read_from_its_message() {
        let payload = [0x45u8, 0, 0, 40];
        let mut attrs = Vec::new();
        attrs.extend_from_slice(&11u16.to_ne_bytes());
        attrs.extend_from_slice(&NFQA_PACKET_HDR.to_ne_bytes());
        attrs.extend_from_slice(&[0, 0, 0, 9, 0x08, 0x00, 3, 0]);
        attrs.extend_from_slice(&((4 + payload.len()) as u16).to_ne_bytes());
        attrs.extend_from_slice(&NFQA_PAYLOAD.to_ne_bytes());
        attrs.extend_from_slice(&payload);
        let mut msg = Vec::new();
        msg.extend_from_slice(&((20 + attrs.len()) as u32).to_ne_bytes());
        msg.extend_from_slice(&((NFNL_SUBSYS_QUEUE << 8) | NFQNL_MSG_PACKET).to_ne_bytes());
        msg.extend_from_slice(&[0; 10]);
        msg.extend_from_slice(&[0, 0, 0, 7]);
        msg.extend_from_slice(&attrs);
        assert_eq!(packet(&msg), Some((9, payload.to_vec())));
    }
}

//! UDP carried to the host as datagrams over one vsock stream.
//!
//! netfilter diverts every UDP datagram that would leave through eth0 to the
//! socket here by TPROXY (DNS and DHCP excepted; the first is answered by the
//! DNS forwarder, the second by the VMM's responder on the network device), so it
//! arrives with its destination intact; the socket is transparent, and reads
//! that destination with each datagram (`IP_RECVORIGDSTADDR`), which with the
//! source names a flow. The host keeps one socket per flow and the two ends
//! exchange frames over a single stream:
//!
//!     len u16 | flow u32 | kind u8 | payload
//!
//! `kind` is 0 for data, 1 for a flow opening (the payload is the destination:
//! family, sixteen address bytes, port), 2 for a flow closing. A reply is sent
//! to the container from a transparent socket bound to the destination it
//! spoke to, so that is the source it hears from.
//!
//! Datagrams are taken in batches (`recvmmsg`) and written as one frame batch,
//! so a stream of small datagrams is a stream of large vsock packets rather
//! than a packet apiece.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The host's vsock port for UDP.
pub const UDP_PORT: u32 = 2380;

pub const KIND_DATA: u8 = 0;
pub const KIND_OPEN: u8 = 1;
pub const KIND_CLOSE: u8 = 2;
const HEADER: usize = 7;
const BATCH: usize = 64;
const DATAGRAM: usize = 65536;
const IDLE: Duration = Duration::from_secs(60);

struct Flow {
    src: SocketAddr,
    /// Bound to the flow's destination, transparently: replies come from
    /// where the container sent to.
    reply: Option<UdpSocket>,
    last: Instant,
}

const IP_TRANSPARENT: libc::c_int = 19;

fn transparent(fd: i32) -> std::io::Result<()> {
    let one: libc::c_int = 1;
    // SAFETY: a live socket and an int-sized option value.
    if unsafe {
        libc::setsockopt(fd, libc::SOL_IP, IP_TRANSPARENT, std::ptr::addr_of!(one).cast(), size_of::<libc::c_int>() as libc::socklen_t)
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A socket that answers as the flow's destination.
fn bound_reply_socket(dst: SocketAddr) -> Option<UdpSocket> {
    // SAFETY: plain socket creation.
    let fd = unsafe { libc::socket(if dst.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 }, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a fresh descriptor we own.
    let socket: UdpSocket = unsafe { <UdpSocket as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    transparent(fd).ok()?;
    let one: libc::c_int = 1;
    // SAFETY: as above; the port may be one a local service also holds.
    unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, std::ptr::addr_of!(one).cast(), size_of::<libc::c_int>() as libc::socklen_t) };
    let sa = sockaddr_of(dst);
    // SAFETY: a sockaddr filled for the family, of the length given.
    if unsafe { libc::bind(fd, sa.0.as_ptr().cast(), sa.1) } < 0 {
        return None;
    }
    Some(socket)
}

fn sockaddr_of(addr: SocketAddr) -> ([u8; size_of::<libc::sockaddr_storage>()], libc::socklen_t) {
    let mut buf = [0u8; size_of::<libc::sockaddr_storage>()];
    match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr { s_addr: u32::from(*a.ip()).to_be() },
                sin_zero: [0; 8],
            };
            // SAFETY: copying a plain struct into a buffer of at least its size.
            unsafe { std::ptr::write_unaligned(buf.as_mut_ptr().cast(), sin) };
            (buf, size_of::<libc::sockaddr_in>() as libc::socklen_t)
        }
        SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr { s6_addr: a.ip().octets() },
                sin6_scope_id: 0,
            };
            // SAFETY: as above.
            unsafe { std::ptr::write_unaligned(buf.as_mut_ptr().cast(), sin6) };
            (buf, size_of::<libc::sockaddr_in6>() as libc::socklen_t)
        }
    }
}

/// Flows by id, for the reply thread; and by (source, destination), for the
/// forwarder.
#[derive(Default)]
struct Flows {
    by_id: HashMap<u32, Flow>,
    ids: HashMap<(SocketAddr, SocketAddr), u32>,
    next: u32,
}

pub fn frame(out: &mut Vec<u8>, flow: u32, kind: u8, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(&flow.to_be_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
}

fn destination_bytes(dst: SocketAddr) -> [u8; 19] {
    let mut b = [0u8; 19];
    match dst.ip() {
        IpAddr::V4(a) => {
            b[0] = 4;
            b[1..5].copy_from_slice(&a.octets());
        }
        IpAddr::V6(a) => {
            b[0] = 6;
            b[1..17].copy_from_slice(&a.octets());
        }
    }
    b[17..19].copy_from_slice(&dst.port().to_be_bytes());
    b
}

pub fn serve(port: u16, host: crate::Fd) -> std::io::Result<()> {
    // Transparent before bind: what TPROXY diverts is delivered to it
    // whatever the destination says.
    // SAFETY: plain socket creation.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own.
    let socket: UdpSocket = unsafe { <UdpSocket as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    transparent(raw)?;
    let sa = sockaddr_of(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port));
    // SAFETY: a sockaddr_in in a buffer of its size.
    if unsafe { libc::bind(raw, sa.0.as_ptr().cast(), sa.1) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = socket.as_raw_fd();
    // A container sends at line rate into this one socket; the default
    // receive buffer dropped seven datagrams in eight at 13 Gbit/s.
    let buf: libc::c_int = 16 << 20;
    // SAFETY: a live socket and an int-sized option value.
    unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, std::ptr::addr_of!(buf).cast(), size_of::<libc::c_int>() as libc::socklen_t) };
    // The bridge's netfilter call runs the IP prerouting hooks in the
    // bridge's context for traffic off docker0, where a socket TPROXY
    // assigns does not survive into routing: every datagram counted as
    // "no port". dockerd turns it on at start; off again here, and in init.
    let _ = std::fs::write("/proc/sys/net/bridge/bridge-nf-call-iptables", "0");
    let one: libc::c_int = 1;
    // SAFETY: a live socket and an int-sized option value.
    if unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_IP,
            libc::IP_RECVORIGDSTADDR,
            std::ptr::addr_of!(one).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let flows = Arc::new(Mutex::new(Flows::default()));
    let mut host_write = host;
    let mut host_read = host_write.try_clone()?;
    let reply_socket = socket.try_clone()?;
    let reply_flows = flows.clone();
    std::thread::Builder::new()
        .name("udp-replies".into())
        .spawn(move || {
            let mut header = [0u8; HEADER];
            let mut payload = vec![0u8; DATAGRAM];
            loop {
                if host_read.read_exact(&mut header).is_err() {
                    return;
                }
                let len = u16::from_be_bytes([header[0], header[1]]) as usize;
                let flow = u32::from_be_bytes([header[2], header[3], header[4], header[5]]);
                let kind = header[6];
                if host_read.read_exact(&mut payload[..len]).is_err() {
                    return;
                }
                let mut flows = reply_flows.lock().expect("udp flows poisoned");
                match kind {
                    KIND_DATA => {
                        if let Some(f) = flows.by_id.get_mut(&flow) {
                            f.last = Instant::now();
                            let _ = match &f.reply {
                                Some(s) => s.send_to(&payload[..len], f.src),
                                None => reply_socket.send_to(&payload[..len], f.src),
                            };
                        }
                    }
                    KIND_CLOSE => {
                        if let Some(f) = flows.by_id.remove(&flow) {
                            flows.ids.retain(|_, id| *id != flow);
                            let _ = f;
                        }
                    }
                    _ => {}
                }
            }
        })?;
    println!("AGENT udp-proxy port={port}");

    // recvmmsg's tables: names, control buffers, one iovec each.
    let mut names = vec![[0u8; size_of::<libc::sockaddr_storage>()]; BATCH];
    let mut controls = vec![[0u8; 64]; BATCH];
    let mut buffers = vec![vec![0u8; DATAGRAM]; BATCH];
    let mut iovs: Vec<libc::iovec> = (0..BATCH)
        .map(|i| libc::iovec {
            iov_base: buffers[i].as_mut_ptr().cast(),
            iov_len: DATAGRAM,
        })
        .collect();
    let mut msgs: Vec<libc::mmsghdr> = (0..BATCH)
        .map(|i| {
            // SAFETY: zeroed is a valid mmsghdr.
            let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
            m.msg_hdr.msg_name = names[i].as_mut_ptr().cast();
            m.msg_hdr.msg_namelen = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            m.msg_hdr.msg_iov = &mut iovs[i];
            m.msg_hdr.msg_iovlen = 1;
            m.msg_hdr.msg_control = controls[i].as_mut_ptr().cast();
            m.msg_hdr.msg_controllen = 64;
            m
        })
        .collect();
    let mut out: Vec<u8> = Vec::with_capacity(BATCH * 1500);
    let mut swept = Instant::now();
    loop {
        for m in msgs.iter_mut() {
            m.msg_hdr.msg_namelen = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            m.msg_hdr.msg_controllen = 64;
            m.msg_hdr.msg_flags = 0;
        }
        // SAFETY: the tables above outlive the call and describe BATCH
        // messages with buffers of DATAGRAM bytes each.
        let n = unsafe { libc::recvmmsg(fd, msgs.as_mut_ptr(), BATCH as libc::c_uint, libc::MSG_WAITFORONE as libc::c_uint, std::ptr::null_mut()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(std::io::Error::last_os_error());
        }
        out.clear();
        {
            let mut flows = flows.lock().expect("udp flows poisoned");
            for i in 0..n as usize {
                let len = msgs[i].msg_len as usize;
                let Some(src) = sockaddr_from(&names[i], msgs[i].msg_hdr.msg_namelen) else { continue };
                let Some(dst) = original_destination(&msgs[i].msg_hdr) else { continue };
                let id = match flows.ids.get(&(src, dst)) {
                    Some(&id) => {
                        if let Some(f) = flows.by_id.get_mut(&id) {
                            f.last = Instant::now();
                        }
                        id
                    }
                    None => {
                        let id = flows.next;
                        flows.next = flows.next.wrapping_add(1);
                        flows.ids.insert((src, dst), id);
                        flows.by_id.insert(id, Flow { src, reply: bound_reply_socket(dst), last: Instant::now() });
                        frame(&mut out, id, KIND_OPEN, &destination_bytes(dst));
                        id
                    }
                };
                frame(&mut out, id, KIND_DATA, &buffers[i][..len]);
            }
            if swept.elapsed() >= IDLE {
                swept = Instant::now();
                let stale: Vec<u32> = flows
                    .by_id
                    .iter()
                    .filter(|(_, f)| f.last.elapsed() >= IDLE)
                    .map(|(id, _)| *id)
                    .collect();
                for id in stale {
                    flows.by_id.remove(&id);
                    flows.ids.retain(|_, v| *v != id);
                    frame(&mut out, id, KIND_CLOSE, &[]);
                }
            }
        }
        if !out.is_empty() && host_write.write_all(&out).is_err() {
            return Err(std::io::Error::other("udp stream to host closed"));
        }
    }
}

fn sockaddr_from(raw: &[u8], len: libc::socklen_t) -> Option<SocketAddr> {
    let family = u16::from_ne_bytes([raw[0], raw[1]]) as i32;
    if (len as usize) >= size_of::<libc::sockaddr_in>() && family == libc::AF_INET {
        // SAFETY: the kernel filled at least a sockaddr_in.
        let a: libc::sockaddr_in = unsafe { std::ptr::read_unaligned(raw.as_ptr().cast()) };
        return Some(SocketAddr::new(Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)).into(), u16::from_be(a.sin_port)));
    }
    if (len as usize) >= size_of::<libc::sockaddr_in6>() && family == libc::AF_INET6 {
        // SAFETY: the kernel filled a sockaddr_in6.
        let a: libc::sockaddr_in6 = unsafe { std::ptr::read_unaligned(raw.as_ptr().cast()) };
        return Some(SocketAddr::new(std::net::Ipv6Addr::from(a.sin6_addr.s6_addr).into(), u16::from_be(a.sin6_port)));
    }
    None
}

/// The destination the datagram was sent to before netfilter redirected it,
/// from the `IP_ORIGDSTADDR` control message.
fn original_destination(msg: &libc::msghdr) -> Option<SocketAddr> {
    // SAFETY: CMSG_FIRSTHDR/NXTHDR walk the control buffer the kernel filled,
    // bounded by msg_controllen.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_IP && (*c).cmsg_type == libc::IP_ORIGDSTADDR {
                let a: libc::sockaddr_in = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast());
                return Some(SocketAddr::new(Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)).into(), u16::from_be(a.sin_port)));
            }
            c = libc::CMSG_NXTHDR(msg, c);
        }
    }
    None
}

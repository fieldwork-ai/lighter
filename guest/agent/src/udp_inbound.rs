//! Published UDP ports: the host's datagrams carried in, replies carried out.
//!
//! The mirror of `udp.rs`. The host binds the port Docker published, and
//! every client that speaks to it is a flow on one vsock stream to here,
//! in the same frames (`len u16 | flow u32 | kind u8 | payload`); a flow's
//! opening frame names where in this guest to dial (family, sixteen address
//! bytes, port: eth0's address and the port, where Docker's proxy answers).
//! Each flow is a socket here connected to that address, so what the
//! container answers comes back to the flow's socket and goes to the host
//! as data frames, and the host sends it on to the client.
//!
//! One thread reads the host's frames and keeps the flows; one waits on
//! every flow's socket with epoll and frames what arrives. The host closes
//! a flow whose client has said nothing for a minute, and every flow of a
//! port Docker has withdrawn.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

use crate::udp::{KIND_CLOSE, KIND_DATA, KIND_OPEN, destination_from, frame};

/// The host's vsock port for published UDP ports.
pub const UDP_INBOUND_PORT: u32 = 2383;

const HEADER: usize = 7;
const DATAGRAM: usize = 65536;
const BATCH: usize = 64;

pub fn serve(host: crate::Fd) -> std::io::Result<()> {
    let mut host_read = host;
    let host_write = Arc::new(Mutex::new(host_read.try_clone()?));
    let flows: Arc<Mutex<HashMap<u32, UdpSocket>>> = Arc::new(Mutex::new(HashMap::new()));
    // SAFETY: plain epoll creation.
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let reply_flows = flows.clone();
    let reply_host = host_write.clone();
    std::thread::Builder::new()
        .name("udp-inbound-replies".into())
        .spawn(move || {
            let mut events: Vec<libc::epoll_event> = vec![unsafe { std::mem::zeroed() }; BATCH];
            let mut buf = vec![0u8; DATAGRAM];
            let mut out: Vec<u8> = Vec::with_capacity(BATCH * 1500);
            loop {
                // SAFETY: the events table has BATCH entries; no timeout.
                let n = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), BATCH as libc::c_int, -1) };
                if n < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return;
                }
                out.clear();
                {
                    let flows = reply_flows.lock().expect("inbound flows poisoned");
                    for ev in &events[..n as usize] {
                        let flow = ev.u64 as u32;
                        let Some(socket) = flows.get(&flow) else { continue };
                        for _ in 0..BATCH {
                            match socket.recv(&mut buf) {
                                Ok(len) => frame(&mut out, flow, KIND_DATA, &buf[..len]),
                                // A refusal from the container (ICMP port
                                // unreachable) surfaces here and is not the
                                // flow's end: the next datagram may be taken.
                                Err(_) => break,
                            }
                        }
                    }
                }
                if !out.is_empty() && reply_host.lock().expect("host poisoned").write_all(&out).is_err() {
                    return;
                }
            }
        })?;
    println!("AGENT udp-inbound");

    let mut header = [0u8; HEADER];
    let mut payload = vec![0u8; DATAGRAM];
    loop {
        if host_read.read_exact(&mut header).is_err() {
            return Err(std::io::Error::other("udp inbound stream from host closed"));
        }
        let len = u16::from_be_bytes([header[0], header[1]]) as usize;
        let flow = u32::from_be_bytes([header[2], header[3], header[4], header[5]]);
        let kind = header[6];
        if host_read.read_exact(&mut payload[..len]).is_err() {
            return Err(std::io::Error::other("udp inbound stream from host closed"));
        }
        match kind {
            KIND_OPEN => {
                let Some(dst) = destination_from(&payload[..len]) else { continue };
                let Some(socket) = connected(dst) else { continue };
                let mut ev = libc::epoll_event { events: libc::EPOLLIN as u32, u64: u64::from(flow) };
                // SAFETY: a live epoll and a live socket; the event names the flow.
                if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, socket.as_raw_fd(), &mut ev) } < 0 {
                    continue;
                }
                // A flow id reused before its close arrived: the old socket
                // is dropped, which takes it out of the epoll set.
                flows.lock().expect("inbound flows poisoned").insert(flow, socket);
            }
            KIND_DATA => {
                if let Some(socket) = flows.lock().expect("inbound flows poisoned").get(&flow) {
                    // A datagram the socket cannot take is lost, as UDP allows.
                    let _ = socket.send(&payload[..len]);
                }
            }
            KIND_CLOSE => {
                flows.lock().expect("inbound flows poisoned").remove(&flow);
            }
            _ => {}
        }
    }
}

/// A non-blocking socket connected to `dst`, so the container's replies are
/// what it receives and nothing else.
fn connected(dst: SocketAddr) -> Option<UdpSocket> {
    let bind: SocketAddr = if dst.is_ipv4() { "0.0.0.0:0".parse().ok()? } else { "[::]:0".parse().ok()? };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(dst).ok()?;
    socket.set_nonblocking(true).ok()?;
    Some(socket)
}

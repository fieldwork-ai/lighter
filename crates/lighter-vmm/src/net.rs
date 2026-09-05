//! The host side of the guest's network card: a responder, not a stack.
//!
//! Nothing a container sends crosses to the Mac as packets it has to route.
//! TCP and UDP leave the guest as streams to the gateway, terminated on the
//! link (`link.rs`), DNS is answered on the host over the same channel, and
//! published ports are bound on the Mac by the VMM itself. What the card
//! still carries besides those streams is what the guest needs to believe it
//! is on a network at all: one DHCP lease, and ICMP echo to the world. This
//! module answers those inside the process; ARP and echo to the gateway the
//! link's own stack answers.
//!
//! It replaces `gvproxy`, a userspace TCP/IP stack that ran as a Go sidecar:
//! a second process, a 25 MB binary in every tarball with its own signing and
//! notarization, a socket protocol between the two, and a copy of every frame
//! into it. Once the streams took the traffic, all of that was carrying ARP
//! and DHCP.
//!
//! The addresses are the ones gvproxy used, so nothing that agreed with them
//! moves: the guest's routes, the gates, `host.docker.internal`.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The guest's address, its gateway, and the alias the guest reaches the Mac
/// itself by (`host.docker.internal`). The streams map the last two to
/// loopback on the Mac.
pub const GATEWAY_IP: &str = "192.168.127.1";
pub const GUEST_IP: &str = "192.168.127.2";
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 1);
pub const GUEST: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 2);
pub const HOST_ALIAS: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 254);
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);

/// The guest's MAC, which the device advertises and the lease is keyed on.
pub const GUEST_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];
/// The gateway's, which is what the guest's ARP table holds for `.1`.
pub const GATEWAY_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xdd];

const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_IPV6: u16 = 0x86dd;
const PROTO_ICMP: u8 = 1;
const PROTO_UDP: u8 = 17;
const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// The largest frame the card carries. As large as Ethernet's length field
/// allows: every byte to the host is a TCP segment on this link, and a
/// 65535-byte MTU is what makes one datagram of it (S2: 82 Gbit/s of such
/// frames against 21 of 1400-byte ones). `LIGHTER_NET_MTU` overrides it.
pub const DEFAULT_MTU: u16 = 65_535;

pub fn link_mtu() -> u16 {
    std::env::var("LIGHTER_NET_MTU")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|m| (1280..=65_535).contains(m))
        .unwrap_or(DEFAULT_MTU)
}

/// What the card saw and what it did with it. Diagnostics: a rule that lets
/// a flow escape the redirects shows up here as a dropped protocol.
#[derive(Default)]
pub struct Counters {
    pub arp: AtomicU64,
    pub dhcp: AtomicU64,
    pub icmp_local: AtomicU64,
    pub icmp_forwarded: AtomicU64,
    pub icmp_replied: AtomicU64,
    pub dropped: AtomicU64,
}

/// The card's other end.
pub struct Network {
    mtu: u16,
    /// An unprivileged ICMP socket, for echo to the world. macOS lets any
    /// process open `SOCK_DGRAM`/`IPPROTO_ICMP` and send echo requests on
    /// it; the replies come back with their IP header. `None` when the
    /// kernel refused, in which case `ping` from a container reaches the
    /// gateway and nothing beyond it.
    icmp: Option<Arc<OwnedFd>>,
    counters: Arc<Counters>,
}

impl Network {
    /// Opens the ICMP socket; the rest of the card needs nothing from the
    /// host at all.
    pub fn start(mtu: u16) -> Result<Network, NetError> {
        let icmp = match icmp_socket() {
            Ok(fd) => Some(Arc::new(fd)),
            Err(e) => {
                tracing::warn!(%e, "no ICMP socket; ping from a container stops at the gateway");
                None
            }
        };
        tracing::info!(
            gateway = GATEWAY_IP,
            guest = GUEST_IP,
            mtu,
            "network started"
        );
        Ok(Network {
            mtu,
            icmp,
            counters: Arc::new(Counters::default()),
        })
    }

    /// The link's MTU, for the device to advertise.
    pub const fn mtu(&self) -> u16 {
        self.mtu
    }

    /// Answers one frame the guest transmitted, or drops it (counted). For
    /// a card whose frames arrive on a socket rather than a ring: the caller
    /// reads a frame, asks, and writes whatever comes back.
    pub fn answer(&self, frame: &[u8]) -> Option<Vec<u8>> {
        Responder {
            icmp: self.icmp.clone(),
            counters: self.counters.clone(),
        }
        .answer(frame)
    }

    /// The host's ICMP datagram socket, whose replies [`echo_reply_frame`]
    /// turns back into frames; `None` when the Mac refused one.
    pub fn icmp_fd(&self) -> Option<std::os::fd::RawFd> {
        self.icmp.as_ref().map(|fd| fd.as_raw_fd())
    }

    pub fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }

}

/// Answers a frame the guest transmitted, or drops it.
struct Responder {
    icmp: Option<Arc<OwnedFd>>,
    counters: Arc<Counters>,
}

impl Responder {
    fn answer(&self, frame: &[u8]) -> Option<Vec<u8>> {
        match classify(frame) {
            Some(Seen::Arp) => {
                self.counters.arp.fetch_add(1, Ordering::Relaxed);
                arp_reply(frame)
            }
            Some(Seen::Dhcp) => {
                self.counters.dhcp.fetch_add(1, Ordering::Relaxed);
                dhcp_reply(frame)
            }
            Some(Seen::IcmpLocal) => {
                self.counters.icmp_local.fetch_add(1, Ordering::Relaxed);
                echo_reply_local(frame)
            }
            Some(Seen::IcmpForward) => {
                if let Some(icmp) = &self.icmp
                    && forward_echo(icmp.as_raw_fd(), frame)
                {
                    self.counters.icmp_forwarded.fetch_add(1, Ordering::Relaxed);
                }
                None
            }
            None => {
                let dropped = self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                // The first few say what they were: a flow escaping the
                // redirects would show here as TCP or UDP, and be a bug.
                if dropped < 8 {
                    tracing::debug!(
                        ethertype = ethertype(frame).map(|t| format!("{t:#06x}")),
                        proto = ipv4_proto(frame),
                        "frame with no answer dropped"
                    );
                }
                None
            }
        }
    }
}

/// What a transmitted frame is, as far as the card cares.
#[derive(Debug, PartialEq, Eq)]
pub enum Seen {
    Arp,
    Dhcp,
    /// Echo to the gateway or the host alias: answered here.
    IcmpLocal,
    /// Echo to anywhere else: sent on the host's ICMP socket.
    IcmpForward,
}

fn ethertype(frame: &[u8]) -> Option<u16> {
    frame.get(12..14).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

/// The IPv4 packet inside a frame, if that is what it carries.
fn ipv4(frame: &[u8]) -> Option<&[u8]> {
    if ethertype(frame)? != ETHERTYPE_IPV4 {
        return None;
    }
    let packet = frame.get(14..)?;
    let ihl = (packet.first()? & 0x0f) as usize * 4;
    if packet.first()? >> 4 != 4 || ihl < 20 || packet.len() < ihl {
        return None;
    }
    let total = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if total < ihl || total > packet.len() {
        return None;
    }
    Some(&packet[..total])
}

fn ipv4_proto(frame: &[u8]) -> Option<u8> {
    ipv4(frame).map(|p| p[9])
}

fn ipv4_header_len(packet: &[u8]) -> usize {
    (packet[0] & 0x0f) as usize * 4
}

fn ipv4_dst(packet: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19])
}

fn ipv4_src(packet: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15])
}

pub fn classify(frame: &[u8]) -> Option<Seen> {
    match ethertype(frame)? {
        ETHERTYPE_ARP => {
            let arp = frame.get(14..42)?;
            // Ethernet/IPv4 request for one of our two addresses.
            let request = arp[0..8] == [0, 1, 8, 0, 6, 4, 0, 1];
            let target = Ipv4Addr::new(arp[24], arp[25], arp[26], arp[27]);
            (request && (target == GATEWAY || target == HOST_ALIAS)).then_some(Seen::Arp)
        }
        ETHERTYPE_IPV4 => {
            let packet = ipv4(frame)?;
            let ihl = ipv4_header_len(packet);
            match packet[9] {
                PROTO_UDP => {
                    let udp = packet.get(ihl..)?;
                    let src = u16::from_be_bytes([udp[0], udp[1]]);
                    let dst = u16::from_be_bytes([udp[2], udp[3]]);
                    (src == DHCP_CLIENT_PORT && dst == DHCP_SERVER_PORT).then_some(Seen::Dhcp)
                }
                PROTO_ICMP => {
                    let icmp = packet.get(ihl..)?;
                    if icmp.first()? != &8 {
                        return None;
                    }
                    let dst = ipv4_dst(packet);
                    Some(if dst == GATEWAY || dst == HOST_ALIAS {
                        Seen::IcmpLocal
                    } else {
                        Seen::IcmpForward
                    })
                }
                _ => None,
            }
        }
        ETHERTYPE_IPV6 => None,
        _ => None,
    }
}

// --- ARP ---------------------------------------------------------------------

fn arp_reply(frame: &[u8]) -> Option<Vec<u8>> {
    let arp = frame.get(14..42)?;
    let sender_mac = &arp[8..14];
    let sender_ip = &arp[14..18];
    let target_ip = &arp[24..28];
    let mut out = Vec::with_capacity(42);
    out.extend_from_slice(sender_mac);
    out.extend_from_slice(&GATEWAY_MAC);
    out.extend_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    out.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
    out.extend_from_slice(&GATEWAY_MAC);
    out.extend_from_slice(target_ip);
    out.extend_from_slice(sender_mac);
    out.extend_from_slice(sender_ip);
    Some(out)
}

// --- DHCP --------------------------------------------------------------------

const DHCP_DISCOVER: u8 = 1;
const DHCP_OFFER: u8 = 2;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;
const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];

/// The lease: `.2/24`, router `.1`, DNS `.2` (the agent's resolver — what
/// init writes to `resolv.conf` regardless), and no expiry, so the guest
/// never wakes to renew it.
fn dhcp_reply(frame: &[u8]) -> Option<Vec<u8>> {
    let packet = ipv4(frame)?;
    let ihl = ipv4_header_len(packet);
    let udp = packet.get(ihl..)?;
    let bootp = udp.get(8..)?;
    if bootp.len() < 240 || bootp[0] != 1 || bootp[236..240] != DHCP_MAGIC {
        return None;
    }
    let xid = &bootp[4..8];
    let broadcast = bootp[10] & 0x80 != 0;
    let chaddr = &bootp[28..34];
    let mut kind = None;
    let mut at = 240;
    while at + 1 < bootp.len() {
        let (code, len) = (bootp[at], bootp[at + 1] as usize);
        if code == 255 {
            break;
        }
        if code == 53 && len == 1 {
            kind = bootp.get(at + 2).copied();
        }
        at += 2 + len;
    }
    let reply_kind = match kind? {
        DHCP_DISCOVER => DHCP_OFFER,
        DHCP_REQUEST => DHCP_ACK,
        _ => return None,
    };

    let mut b = Vec::with_capacity(300);
    b.push(2); // BOOTREPLY
    b.extend_from_slice(&[1, 6, 0]); // Ethernet, 6-byte address, hops 0
    b.extend_from_slice(xid);
    b.extend_from_slice(&[0, 0]); // secs
    b.extend_from_slice(&[if broadcast { 0x80 } else { 0 }, 0]); // flags
    b.extend_from_slice(&[0; 4]); // ciaddr
    b.extend_from_slice(&GUEST.octets()); // yiaddr
    b.extend_from_slice(&GATEWAY.octets()); // siaddr
    b.extend_from_slice(&[0; 4]); // giaddr
    b.extend_from_slice(chaddr);
    b.extend_from_slice(&[0; 10]); // chaddr padding
    b.extend_from_slice(&[0; 64]); // sname
    b.extend_from_slice(&[0; 128]); // file
    b.extend_from_slice(&DHCP_MAGIC);
    b.extend_from_slice(&[53, 1, reply_kind]);
    b.extend_from_slice(&[54, 4]);
    b.extend_from_slice(&GATEWAY.octets());
    b.extend_from_slice(&[51, 4, 0xff, 0xff, 0xff, 0xff]);
    b.extend_from_slice(&[1, 4]);
    b.extend_from_slice(&NETMASK.octets());
    b.extend_from_slice(&[3, 4]);
    b.extend_from_slice(&GATEWAY.octets());
    b.extend_from_slice(&[6, 4]);
    b.extend_from_slice(&GUEST.octets());
    b.push(255);

    // Unicast to the lease's holder unless it asked for broadcast: a client
    // with no address yet reads its own MAC off a raw socket either way.
    let (dst_mac, dst_ip) = if broadcast {
        ([0xff; 6], Ipv4Addr::BROADCAST)
    } else {
        (chaddr.try_into().ok()?, GUEST)
    };
    Some(udp_frame(
        dst_mac,
        GATEWAY,
        dst_ip,
        DHCP_SERVER_PORT,
        DHCP_CLIENT_PORT,
        &b,
    ))
}

// --- ICMP --------------------------------------------------------------------

/// Echo to the gateway itself: the request turned around.
fn echo_reply_local(frame: &[u8]) -> Option<Vec<u8>> {
    let packet = ipv4(frame)?;
    let ihl = ipv4_header_len(packet);
    let icmp = &packet[ihl..];
    let mut reply = icmp.to_vec();
    reply[0] = 0;
    reply[2] = 0;
    reply[3] = 0;
    let sum = checksum(&reply);
    reply[2..4].copy_from_slice(&sum.to_be_bytes());
    let src: [u8; 6] = frame[6..12].try_into().ok()?;
    Some(ipv4_frame(
        src,
        ipv4_dst(packet),
        ipv4_src(packet),
        PROTO_ICMP,
        64,
        &reply,
    ))
}

/// Sends the guest's echo request out of the host's ICMP socket, to the
/// address the guest named. The kernel fills the IP header; the identifier
/// and sequence travel as they are, which is how the reply finds its way
/// back into a frame.
fn forward_echo(fd: libc::c_int, frame: &[u8]) -> bool {
    let Some(packet) = ipv4(frame) else {
        return false;
    };
    let ihl = ipv4_header_len(packet);
    let icmp = &packet[ihl..];
    if icmp.len() < 8 {
        return false;
    }
    let dst = ipv4_dst(packet);
    let addr = libc::sockaddr_in {
        sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
        sin_family: libc::AF_INET as u8,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(dst.octets()),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: the buffer and the address are valid for the call's duration.
    let n = unsafe {
        libc::sendto(
            fd,
            icmp.as_ptr().cast(),
            icmp.len(),
            0,
            (&addr as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    n == icmp.len() as isize
}

/// A reply read off the host's ICMP socket, as the frame the guest sees.
/// macOS hands the datagram back with its IP header on; the source is the
/// host that answered, the destination becomes the guest.
pub fn echo_reply_frame(datagram: &[u8]) -> Option<Vec<u8>> {
    let (src, icmp) = if datagram.first()? >> 4 == 4 {
        let ihl = ipv4_header_len(datagram);
        (ipv4_src(datagram), datagram.get(ihl..)?)
    } else {
        return None;
    };
    if icmp.len() < 8 || icmp[0] != 0 {
        return None;
    }
    Some(ipv4_frame(GUEST_MAC, src, GUEST, PROTO_ICMP, 64, icmp))
}

fn icmp_socket() -> io::Result<OwnedFd> {
    // SAFETY: a socket call with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is ours and open.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// --- frames ------------------------------------------------------------------

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in bytes.chunks(2) {
        let word = if pair.len() == 2 {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], 0])
        };
        sum += u32::from(word);
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// An Ethernet frame from the gateway carrying one IPv4 packet.
fn ipv4_frame(
    dst_mac: [u8; 6],
    src: Ipv4Addr,
    dst: Ipv4Addr,
    proto: u8,
    ttl: u8,
    payload: &[u8],
) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut out = Vec::with_capacity(14 + total);
    out.extend_from_slice(&dst_mac);
    out.extend_from_slice(&GATEWAY_MAC);
    out.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    let header_at = out.len();
    out.extend_from_slice(&[0x45, 0]);
    out.extend_from_slice(&(total as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0x40, 0]); // id 0, don't fragment
    out.push(ttl);
    out.push(proto);
    out.extend_from_slice(&[0, 0]); // checksum
    out.extend_from_slice(&src.octets());
    out.extend_from_slice(&dst.octets());
    let sum = checksum(&out[header_at..header_at + 20]);
    out[header_at + 10..header_at + 12].copy_from_slice(&sum.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// An Ethernet frame from the gateway carrying one UDP datagram, with the
/// UDP checksum computed over the pseudo-header the receiver will use.
fn udp_frame(
    dst_mac: [u8; 6],
    src: Ipv4Addr,
    dst: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let len = (8 + payload.len()) as u16;
    let mut udp = Vec::with_capacity(len as usize);
    udp.extend_from_slice(&src_port.to_be_bytes());
    udp.extend_from_slice(&dst_port.to_be_bytes());
    udp.extend_from_slice(&len.to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(payload);
    let mut pseudo = Vec::with_capacity(12 + udp.len());
    pseudo.extend_from_slice(&src.octets());
    pseudo.extend_from_slice(&dst.octets());
    pseudo.extend_from_slice(&[0, PROTO_UDP]);
    pseudo.extend_from_slice(&len.to_be_bytes());
    pseudo.extend_from_slice(&udp);
    let sum = match checksum(&pseudo) {
        0 => 0xffff,
        s => s,
    };
    udp[6..8].copy_from_slice(&sum.to_be_bytes());
    ipv4_frame(dst_mac, src, dst, PROTO_UDP, 64, &udp)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn eth(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&dst);
        f.extend_from_slice(&src);
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    fn arp_request(target: Ipv4Addr) -> Vec<u8> {
        let mut p = vec![0, 1, 8, 0, 6, 4, 0, 1];
        p.extend_from_slice(&GUEST_MAC);
        p.extend_from_slice(&GUEST.octets());
        p.extend_from_slice(&[0; 6]);
        p.extend_from_slice(&target.octets());
        eth([0xff; 6], GUEST_MAC, ETHERTYPE_ARP, &p)
    }

    fn guest_ipv4(dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
        // Built with the same helper the responder uses, from the guest's side.
        let mut f = ipv4_frame(GATEWAY_MAC, GUEST, dst, proto, 64, payload);
        f[6..12].copy_from_slice(&GUEST_MAC);
        f
    }

    fn echo_request(dst: Ipv4Addr, id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
        let mut icmp = vec![8, 0, 0, 0];
        icmp.extend_from_slice(&id.to_be_bytes());
        icmp.extend_from_slice(&seq.to_be_bytes());
        icmp.extend_from_slice(data);
        let sum = checksum(&icmp);
        icmp[2..4].copy_from_slice(&sum.to_be_bytes());
        guest_ipv4(dst, PROTO_ICMP, &icmp)
    }

    fn dhcp(kind: u8, broadcast: bool) -> Vec<u8> {
        let mut b = vec![
            1,
            1,
            6,
            0,
            0xde,
            0xad,
            0xbe,
            0xef,
            0,
            0,
            if broadcast { 0x80 } else { 0 },
            0,
        ];
        b.extend_from_slice(&[0; 16]);
        b.extend_from_slice(&GUEST_MAC);
        b.extend_from_slice(&[0; 10 + 64 + 128]);
        b.extend_from_slice(&DHCP_MAGIC);
        b.extend_from_slice(&[53, 1, kind, 255]);
        let mut udp = Vec::new();
        udp.extend_from_slice(&DHCP_CLIENT_PORT.to_be_bytes());
        udp.extend_from_slice(&DHCP_SERVER_PORT.to_be_bytes());
        udp.extend_from_slice(&((8 + b.len()) as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(&b);
        let mut f = ipv4_frame(
            [0xff; 6],
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            PROTO_UDP,
            64,
            &udp,
        );
        f[6..12].copy_from_slice(&GUEST_MAC);
        f
    }

    #[test]
    fn the_gateway_and_the_host_alias_answer_arp_and_nothing_else_does() {
        let reply = arp_reply(&arp_request(GATEWAY)).unwrap();
        assert_eq!(&reply[0..6], &GUEST_MAC, "to the asker");
        assert_eq!(&reply[6..12], &GATEWAY_MAC);
        assert_eq!(reply[20..22], [0, 2], "an ARP reply");
        assert_eq!(&reply[22..28], &GATEWAY_MAC, "sender hardware address");
        assert_eq!(&reply[28..32], &GATEWAY.octets(), "sender protocol address");
        assert_eq!(classify(&arp_request(HOST_ALIAS)), Some(Seen::Arp));
        assert_eq!(classify(&arp_request(GUEST)), None, "the guest's own probe");
        assert_eq!(
            classify(&arp_request(Ipv4Addr::new(192, 168, 127, 9))),
            None
        );
    }

    #[test]
    fn a_discover_gets_an_offer_and_a_request_an_ack_with_the_lease() {
        for (kind, expected) in [(DHCP_DISCOVER, DHCP_OFFER), (DHCP_REQUEST, DHCP_ACK)] {
            let frame = dhcp(kind, false);
            assert_eq!(classify(&frame), Some(Seen::Dhcp));
            let reply = dhcp_reply(&frame).unwrap();
            assert_eq!(&reply[0..6], &GUEST_MAC, "unicast to the lease holder");
            let packet = ipv4(&reply).unwrap();
            assert_eq!(packet[9], PROTO_UDP);
            assert_eq!(ipv4_src(packet), GATEWAY);
            assert_eq!(ipv4_dst(packet), GUEST);
            let udp = &packet[20..];
            assert_eq!(u16::from_be_bytes([udp[0], udp[1]]), DHCP_SERVER_PORT);
            assert_eq!(u16::from_be_bytes([udp[2], udp[3]]), DHCP_CLIENT_PORT);
            let bootp = &udp[8..];
            assert_eq!(bootp[0], 2, "BOOTREPLY");
            assert_eq!(&bootp[4..8], &[0xde, 0xad, 0xbe, 0xef], "the client's xid");
            assert_eq!(&bootp[16..20], &GUEST.octets(), "yiaddr");
            assert_eq!(&bootp[28..34], &GUEST_MAC);
            let options = &bootp[240..];
            assert_eq!(&options[0..3], &[53, 1, expected]);
            assert!(
                options.windows(6).any(|w| w == [3, 4, 192, 168, 127, 1]),
                "router"
            );
            assert!(
                options.windows(6).any(|w| w == [1, 4, 255, 255, 255, 0]),
                "mask"
            );
            assert!(
                options.windows(6).any(|w| w == [6, 4, 192, 168, 127, 2]),
                "dns is the agent"
            );
            assert!(
                options.windows(6).any(|w| w == [51, 4, 255, 255, 255, 255]),
                "no expiry"
            );
        }
        let reply = dhcp_reply(&dhcp(DHCP_DISCOVER, true)).unwrap();
        assert_eq!(&reply[0..6], &[0xff; 6], "broadcast when asked");
    }

    #[test]
    fn echo_to_the_gateway_comes_straight_back_with_its_payload() {
        let frame = echo_request(GATEWAY, 0x1234, 7, b"hello, gateway");
        assert_eq!(classify(&frame), Some(Seen::IcmpLocal));
        let reply = echo_reply_local(&frame).unwrap();
        assert_eq!(&reply[0..6], &GUEST_MAC);
        let packet = ipv4(&reply).unwrap();
        assert_eq!(ipv4_src(packet), GATEWAY);
        assert_eq!(ipv4_dst(packet), GUEST);
        let icmp = &packet[20..];
        assert_eq!(icmp[0], 0, "echo reply");
        assert_eq!(&icmp[4..8], &[0x12, 0x34, 0, 7]);
        assert_eq!(&icmp[8..], b"hello, gateway");
        assert_eq!(checksum(icmp), 0, "a valid checksum sums to zero");
        assert_eq!(
            classify(&echo_request(HOST_ALIAS, 1, 1, b"")),
            Some(Seen::IcmpLocal)
        );
        assert_eq!(
            classify(&echo_request(Ipv4Addr::new(1, 1, 1, 1), 1, 1, b"")),
            Some(Seen::IcmpForward)
        );
    }

    #[test]
    fn a_reply_off_the_host_socket_becomes_a_frame_to_the_guest() {
        // What macOS hands back: the IP header, then the echo reply.
        let mut icmp = vec![0, 0, 0, 0, 0x12, 0x34, 0, 7];
        icmp.extend_from_slice(b"pong");
        let sum = checksum(&icmp);
        icmp[2..4].copy_from_slice(&sum.to_be_bytes());
        let datagram = ipv4_frame(
            [0; 6],
            Ipv4Addr::new(1, 1, 1, 1),
            GUEST,
            PROTO_ICMP,
            57,
            &icmp,
        );
        let frame = echo_reply_frame(&datagram[14..]).unwrap();
        assert_eq!(&frame[0..6], &GUEST_MAC);
        let packet = ipv4(&frame).unwrap();
        assert_eq!(ipv4_src(packet), Ipv4Addr::new(1, 1, 1, 1));
        assert_eq!(ipv4_dst(packet), GUEST);
        assert_eq!(&packet[20..], &icmp[..]);
    }

    #[test]
    fn what_has_a_stream_is_dropped_and_ipv6_is_ignored() {
        let tcp = guest_ipv4(Ipv4Addr::new(93, 184, 216, 34), 6, &[0; 20]);
        assert_eq!(classify(&tcp), None);
        let udp = guest_ipv4(
            Ipv4Addr::new(8, 8, 8, 8),
            PROTO_UDP,
            &[0, 53, 0, 53, 0, 8, 0, 0],
        );
        assert_eq!(classify(&udp), None, "UDP that is not DHCP");
        let v6 = eth(
            [0x33, 0x33, 0, 0, 0, 2],
            GUEST_MAC,
            ETHERTYPE_IPV6,
            &[0x60; 48],
        );
        assert_eq!(classify(&v6), None);
        assert_eq!(classify(&[0u8; 10]), None, "a runt");
    }

    #[test]
    fn the_guest_mac_is_the_one_the_lease_is_keyed_on() {
        assert_eq!(GUEST_MAC, [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee]);
    }
}

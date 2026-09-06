//! The host side of the guest's network card: a responder, not a stack.
//!
//! Nothing a container sends crosses to the Mac as packets any more. TCP and
//! UDP leave the guest as streams over vsock (`streams`, `reactor`), DNS is
//! answered on the host over the same channel, and published ports are bound
//! on the Mac by the VMM itself. What still reaches the virtual network card
//! is what has no stream form and what the guest needs to believe it is on a
//! network at all: ARP for its gateway, one DHCP lease, and ICMP. This module
//! answers those three inside the process and drops the rest.
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
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::virtio::net::{Inbox, Net, Outbox};

/// The guest's address, its gateway, and the alias the guest reaches the Mac
/// itself by (`host.docker.internal`). The streams map the last two to
/// loopback on the Mac.
pub const GATEWAY_IP: &str = "192.168.127.1";
pub const GUEST_IP: &str = "192.168.127.2";
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 1);
pub const GUEST: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 2);
pub const HOST_ALIAS: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 254);
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);

/// The link's IPv6 addresses: a unique-local prefix of lighter's own, the
/// gateway and the guest. The guest's init gives eth0 its address, the
/// gateway's link-layer address and a default route statically, as the
/// lease has no expiry, so the card sends no router advertisements; it
/// answers neighbour solicitations for the gateway and echo to it, and
/// forwards echo elsewhere. Docker's containers get the sibling prefix
/// `fd6c:6967:6874:d0c::/64`.
pub const GATEWAY6: Ipv6Addr = Ipv6Addr::new(0xfd6c, 0x6967, 0x6874, 0, 0, 0, 0, 1);
pub const GUEST6: Ipv6Addr = Ipv6Addr::new(0xfd6c, 0x6967, 0x6874, 0, 0, 0, 0, 2);

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
const PROTO_HOPOPTS: u8 = 0;
const PROTO_ICMPV6: u8 = 58;
const ICMP6_ECHO_REQUEST: u8 = 128;
const ICMP6_ECHO_REPLY: u8 = 129;
const ICMP6_NEIGHBOUR_SOLICIT: u8 = 135;
const ICMP6_NEIGHBOUR_ADVERT: u8 = 136;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// The MTU the guest is told the link carries. Nothing on the far side of
/// the card sees it — every flow is a stream — so it only sizes the frames
/// the three protocols above travel in. `LIGHTER_NET_MTU` overrides it.
pub fn link_mtu() -> u16 {
    std::env::var("LIGHTER_NET_MTU")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|m| (68..=65_520).contains(m))
        .unwrap_or(crate::virtio::net::DEFAULT_MTU)
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
    /// Neighbour solicitations for the gateway, answered.
    pub ndp: AtomicU64,
    pub icmp6_local: AtomicU64,
    pub icmp6_forwarded: AtomicU64,
    pub icmp6_replied: AtomicU64,
    /// What a v6 host says to a link nobody listens on: multicast listener
    /// reports, duplicate address detection. Taken, not dropped.
    pub ipv6_housekeeping: AtomicU64,
    pub dropped: AtomicU64,
}

/// The card's other end.
pub struct Network {
    outbox: Arc<Outbox>,
    mtu: u16,
    /// An unprivileged ICMP socket, for echo to the world. macOS lets any
    /// process open `SOCK_DGRAM`/`IPPROTO_ICMP` and send echo requests on
    /// it; the replies come back with their IP header. `None` when the
    /// kernel refused, in which case `ping` from a container reaches the
    /// gateway and nothing beyond it.
    icmp: Option<Arc<OwnedFd>>,
    /// The same for ICMPv6; its replies come back without an IP header,
    /// the source from `recvfrom`.
    icmp6: Option<Arc<OwnedFd>>,
    counters: Arc<Counters>,
}

impl Network {
    /// Opens the ICMP sockets; the rest of the card needs nothing from the
    /// host at all.
    pub fn start(mtu: u16) -> Result<Network, NetError> {
        let icmp = match icmp_socket(libc::AF_INET) {
            Ok(fd) => Some(Arc::new(fd)),
            Err(e) => {
                tracing::warn!(%e, "no ICMP socket; ping from a container stops at the gateway");
                None
            }
        };
        let icmp6 = match icmp_socket(libc::AF_INET6) {
            Ok(fd) => Some(Arc::new(fd)),
            Err(e) => {
                tracing::warn!(%e, "no ICMPv6 socket; ping6 from a container stops at the gateway");
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
            outbox: Outbox::new(),
            mtu,
            icmp,
            icmp6,
            counters: Arc::new(Counters::default()),
        })
    }

    /// The link's MTU, for the device to advertise.
    pub const fn mtu(&self) -> u16 {
        self.mtu
    }

    /// Where the device puts frames the guest transmitted.
    pub fn outbox(&self) -> Arc<Outbox> {
        self.outbox.clone()
    }

    pub fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }

    /// Starts the two threads of the card. The responder takes what the guest
    /// transmitted and answers it into `inbox`; `wake_rx` is called once per
    /// batch of answers so the device delivers them under one interrupt, and
    /// `wake_tx` when the device had parked on a full outbox and there is
    /// room again. The ICMP reader turns the host's echo replies into frames
    /// the same way.
    pub fn spawn(
        &self,
        inbox: Inbox,
        wake_rx: impl Fn() + Send + Sync + 'static,
        wake_tx: impl Fn() + Send + 'static,
    ) -> io::Result<()> {
        let wake_rx = Arc::new(wake_rx);
        let outbox = self.outbox.clone();
        let responder = Responder {
            icmp: self.icmp.clone(),
            icmp6: self.icmp6.clone(),
            counters: self.counters.clone(),
        };
        let deliver_inbox = inbox.clone();
        let deliver_wake = wake_rx.clone();
        std::thread::Builder::new()
            .name("net-lan".into())
            .spawn(move || {
                crate::qos::raise_interactive();
                while let Some((frames, parked)) = outbox.take() {
                    let mut answered = 0usize;
                    for frame in &frames {
                        if let Some(reply) = responder.answer(frame)
                            && Net::enqueue_received(&deliver_inbox, reply)
                        {
                            answered += 1;
                        }
                    }
                    if answered > 0 {
                        deliver_wake();
                    }
                    if parked {
                        wake_tx();
                    }
                }
                tracing::debug!("network responder stopped");
            })?;
        if let Some(icmp) = &self.icmp {
            let icmp = icmp.clone();
            let counters = self.counters.clone();
            let inbox = inbox.clone();
            let wake_rx = wake_rx.clone();
            std::thread::Builder::new()
                .name("net-icmp".into())
                .spawn(move || {
                    let mut buf = vec![0u8; 65_536];
                    loop {
                        // SAFETY: a read into a buffer we own, of its length.
                        let n = unsafe {
                            libc::recv(icmp.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0)
                        };
                        if n <= 0 {
                            if n < 0
                                && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
                            {
                                continue;
                            }
                            break;
                        }
                        if let Some(frame) = echo_reply_frame(&buf[..n as usize]) {
                            counters.icmp_replied.fetch_add(1, Ordering::Relaxed);
                            if Net::enqueue_received(&inbox, frame) {
                                wake_rx();
                            }
                        }
                    }
                    tracing::debug!("ICMP reader stopped");
                })?;
        }
        if let Some(icmp6) = &self.icmp6 {
            let icmp6 = icmp6.clone();
            let counters = self.counters.clone();
            let inbox = inbox.clone();
            let wake_rx = wake_rx.clone();
            std::thread::Builder::new()
                .name("net-icmp6".into())
                .spawn(move || {
                    let mut buf = vec![0u8; 65_536];
                    loop {
                        // SAFETY: zeroed is a valid sockaddr_in6 to fill.
                        let mut from: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
                        let mut from_len =
                            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
                        // SAFETY: a read into a buffer we own, of its length,
                        // and an address of the length given.
                        let n = unsafe {
                            libc::recvfrom(
                                icmp6.as_raw_fd(),
                                buf.as_mut_ptr().cast(),
                                buf.len(),
                                0,
                                (&mut from as *mut libc::sockaddr_in6).cast(),
                                &mut from_len,
                            )
                        };
                        if n <= 0 {
                            if n < 0
                                && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
                            {
                                continue;
                            }
                            break;
                        }
                        let src = Ipv6Addr::from(from.sin6_addr.s6_addr);
                        if let Some(frame) = echo6_reply_frame(src, &buf[..n as usize]) {
                            counters.icmp6_replied.fetch_add(1, Ordering::Relaxed);
                            if Net::enqueue_received(&inbox, frame) {
                                wake_rx();
                            }
                        }
                    }
                    tracing::debug!("ICMPv6 reader stopped");
                })?;
        }
        Ok(())
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        self.outbox.close();
    }
}

/// Answers a frame the guest transmitted, or drops it.
struct Responder {
    icmp: Option<Arc<OwnedFd>>,
    icmp6: Option<Arc<OwnedFd>>,
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
            Some(Seen::NeighbourSolicit) => {
                self.counters.ndp.fetch_add(1, Ordering::Relaxed);
                neighbour_advertisement(frame)
            }
            Some(Seen::Icmp6Local) => {
                self.counters.icmp6_local.fetch_add(1, Ordering::Relaxed);
                echo6_reply_local(frame)
            }
            Some(Seen::Icmp6Forward) => {
                if let Some(icmp6) = &self.icmp6
                    && forward_echo6(icmp6.as_raw_fd(), frame)
                {
                    self.counters
                        .icmp6_forwarded
                        .fetch_add(1, Ordering::Relaxed);
                }
                None
            }
            Some(Seen::Ipv6Housekeeping) => {
                self.counters
                    .ipv6_housekeeping
                    .fetch_add(1, Ordering::Relaxed);
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
enum Seen {
    Arp,
    Dhcp,
    /// Echo to the gateway or the host alias: answered here.
    IcmpLocal,
    /// Echo to anywhere else: sent on the host's ICMP socket.
    IcmpForward,
    /// A neighbour solicitation for the v6 gateway: answered here.
    NeighbourSolicit,
    /// ICMPv6 echo to the v6 gateway: answered here.
    Icmp6Local,
    /// ICMPv6 echo to anywhere else: sent on the host's ICMPv6 socket.
    Icmp6Forward,
    /// What a v6 host says to its link unprompted (multicast listener
    /// reports, duplicate address detection, a solicitation for anything
    /// but the gateway): heard, unanswered.
    Ipv6Housekeeping,
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

fn classify(frame: &[u8]) -> Option<Seen> {
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
        ETHERTYPE_IPV6 => {
            let packet = ipv6(frame)?;
            match packet[6] {
                // Multicast listener reports ride a hop-by-hop header, the
                // only thing on this link that does.
                PROTO_HOPOPTS => Some(Seen::Ipv6Housekeeping),
                PROTO_ICMPV6 => {
                    let icmp = packet.get(40..)?;
                    match *icmp.first()? {
                        ICMP6_NEIGHBOUR_SOLICIT => {
                            let target = ipv6_at(icmp, 8)?;
                            Some(if target == GATEWAY6 {
                                Seen::NeighbourSolicit
                            } else {
                                Seen::Ipv6Housekeeping
                            })
                        }
                        ICMP6_ECHO_REQUEST => Some(if ipv6_dst(packet) == GATEWAY6 {
                            Seen::Icmp6Local
                        } else {
                            Seen::Icmp6Forward
                        }),
                        // MLD queries and reports, router solicitations,
                        // neighbour advertisements.
                        130..=133 | 136 | 143 => Some(Seen::Ipv6Housekeeping),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
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
fn echo_reply_frame(datagram: &[u8]) -> Option<Vec<u8>> {
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

/// An unprivileged ICMP socket of the family: `SOCK_DGRAM` over
/// `IPPROTO_ICMP` or `IPPROTO_ICMPV6`, which macOS opens for any process.
fn icmp_socket(family: libc::c_int) -> io::Result<OwnedFd> {
    let proto = if family == libc::AF_INET6 {
        libc::IPPROTO_ICMPV6
    } else {
        libc::IPPROTO_ICMP
    };
    // SAFETY: a socket call with constant arguments.
    let fd = unsafe { libc::socket(family, libc::SOCK_DGRAM, proto) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is ours and open.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// --- IPv6 --------------------------------------------------------------------

/// The IPv6 packet in a frame, if it is one with a whole header.
fn ipv6(frame: &[u8]) -> Option<&[u8]> {
    let packet = frame.get(14..)?;
    (packet.len() >= 40 && packet[0] >> 4 == 6).then_some(packet)
}

fn ipv6_at(bytes: &[u8], at: usize) -> Option<Ipv6Addr> {
    let octets: [u8; 16] = bytes.get(at..at + 16)?.try_into().ok()?;
    Some(Ipv6Addr::from(octets))
}

fn ipv6_src(packet: &[u8]) -> Ipv6Addr {
    ipv6_at(packet, 8).expect("a whole header")
}

fn ipv6_dst(packet: &[u8]) -> Ipv6Addr {
    ipv6_at(packet, 24).expect("a whole header")
}

/// The ICMPv6 checksum: over the pseudo-header (both addresses, the
/// message length, the next header) and the message.
fn icmp6_checksum(src: Ipv6Addr, dst: Ipv6Addr, icmp: &[u8]) -> u16 {
    let mut pseudo = Vec::with_capacity(40 + icmp.len());
    pseudo.extend_from_slice(&src.octets());
    pseudo.extend_from_slice(&dst.octets());
    pseudo.extend_from_slice(&(icmp.len() as u32).to_be_bytes());
    pseudo.extend_from_slice(&[0, 0, 0, PROTO_ICMPV6]);
    pseudo.extend_from_slice(icmp);
    checksum(&pseudo)
}

/// An Ethernet frame from the gateway carrying one IPv6 packet.
fn ipv6_frame(
    dst_mac: [u8; 6],
    src: Ipv6Addr,
    dst: Ipv6Addr,
    next: u8,
    hop_limit: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + 40 + payload.len());
    out.extend_from_slice(&dst_mac);
    out.extend_from_slice(&GATEWAY_MAC);
    out.extend_from_slice(&ETHERTYPE_IPV6.to_be_bytes());
    out.extend_from_slice(&[0x60, 0, 0, 0]);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.push(next);
    out.push(hop_limit);
    out.extend_from_slice(&src.octets());
    out.extend_from_slice(&dst.octets());
    out.extend_from_slice(payload);
    out
}

/// An ICMPv6 message from the gateway to `dst`, its checksum filled in.
fn icmp6_frame(dst_mac: [u8; 6], dst: Ipv6Addr, hop_limit: u8, mut icmp: Vec<u8>) -> Vec<u8> {
    icmp[2] = 0;
    icmp[3] = 0;
    let sum = icmp6_checksum(GATEWAY6, dst, &icmp);
    icmp[2..4].copy_from_slice(&sum.to_be_bytes());
    ipv6_frame(dst_mac, GATEWAY6, dst, PROTO_ICMPV6, hop_limit, &icmp)
}

/// The gateway's answer to a neighbour solicitation for it: a solicited
/// advertisement with the router and override flags, carrying its
/// link-layer address, hop limit 255 as the protocol requires.
fn neighbour_advertisement(frame: &[u8]) -> Option<Vec<u8>> {
    let packet = ipv6(frame)?;
    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
    let src = ipv6_src(packet);
    // A solicitation from the unspecified address is duplicate address
    // detection for the gateway's own address, which nothing on this link
    // may take; answered to all nodes as the protocol says.
    let (dst, dst_mac) = if src.is_unspecified() {
        (
            Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1),
            [0x33, 0x33, 0, 0, 0, 1],
        )
    } else {
        (src, src_mac)
    };
    let mut na = vec![ICMP6_NEIGHBOUR_ADVERT, 0, 0, 0];
    // Router, solicited (unless for DAD), override.
    na.push(if src.is_unspecified() { 0xa0 } else { 0xe0 });
    na.extend_from_slice(&[0, 0, 0]);
    na.extend_from_slice(&GATEWAY6.octets());
    // Target link-layer address option.
    na.extend_from_slice(&[2, 1]);
    na.extend_from_slice(&GATEWAY_MAC);
    Some(icmp6_frame(dst_mac, dst, 255, na))
}

/// ICMPv6 echo to the gateway itself: the request turned around.
fn echo6_reply_local(frame: &[u8]) -> Option<Vec<u8>> {
    let packet = ipv6(frame)?;
    let icmp = packet.get(40..)?;
    if icmp.len() < 8 {
        return None;
    }
    let mut reply = icmp.to_vec();
    reply[0] = ICMP6_ECHO_REPLY;
    let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
    Some(icmp6_frame(src_mac, ipv6_src(packet), 64, reply))
}

/// Sends the guest's ICMPv6 echo request out of the host's socket to the
/// address the guest named; the kernel fills the header and the checksum.
fn forward_echo6(fd: libc::c_int, frame: &[u8]) -> bool {
    let Some(packet) = ipv6(frame) else {
        return false;
    };
    let Some(icmp) = packet.get(40..) else {
        return false;
    };
    if icmp.len() < 8 {
        return false;
    }
    let dst = ipv6_dst(packet);
    // SAFETY: zeroed is a valid sockaddr_in6 to fill.
    let mut addr: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    addr.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
    addr.sin6_family = libc::AF_INET6 as u8;
    addr.sin6_addr.s6_addr = dst.octets();
    // SAFETY: the buffer and the address are valid for the call's duration.
    let n = unsafe {
        libc::sendto(
            fd,
            icmp.as_ptr().cast(),
            icmp.len(),
            0,
            (&addr as *const libc::sockaddr_in6).cast(),
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
        )
    };
    n == icmp.len() as isize
}

/// A reply read off the host's ICMPv6 socket, as the frame the guest sees:
/// no IP header on this family, the answering host is `src`, the
/// destination becomes the guest, and the checksum is redone for it.
fn echo6_reply_frame(src: Ipv6Addr, icmp: &[u8]) -> Option<Vec<u8>> {
    if icmp.len() < 8 || icmp[0] != ICMP6_ECHO_REPLY {
        return None;
    }
    let mut reply = icmp.to_vec();
    reply[2] = 0;
    reply[3] = 0;
    let sum = icmp6_checksum(src, GUEST6, &reply);
    reply[2..4].copy_from_slice(&sum.to_be_bytes());
    Some(ipv6_frame(GUEST_MAC, src, GUEST6, PROTO_ICMPV6, 64, &reply))
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

/// `writev` until every iovec is on the socket, advancing past whatever a
/// partial write took. The vsock writer's, kept here from the days the card
/// had a socket of its own.
pub(crate) fn write_all_vectored(fd: libc::c_int, iovs: &mut [libc::iovec]) -> io::Result<()> {
    let mut first = 0usize;
    while first < iovs.len() {
        let rest = &iovs[first..];
        // SAFETY: every iovec points into a buffer that outlives this call,
        // and the count is what the slice holds.
        let n = unsafe { libc::writev(fd, rest.as_ptr(), rest.len() as libc::c_int) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let mut n = n as usize;
        while first < iovs.len() && n >= iovs[first].iov_len {
            n -= iovs[first].iov_len;
            first += 1;
        }
        if first < iovs.len() && n > 0 {
            // SAFETY: advancing within the same buffer by fewer bytes than
            // its length.
            iovs[first].iov_base = unsafe { iovs[first].iov_base.add(n) };
            iovs[first].iov_len -= n;
        }
    }
    Ok(())
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
    fn what_has_a_stream_is_dropped_in_either_family() {
        let tcp = guest_ipv4(Ipv4Addr::new(93, 184, 216, 34), 6, &[0; 20]);
        assert_eq!(classify(&tcp), None);
        let udp = guest_ipv4(
            Ipv4Addr::new(8, 8, 8, 8),
            PROTO_UDP,
            &[0, 53, 0, 53, 0, 8, 0, 0],
        );
        assert_eq!(classify(&udp), None, "UDP that is not DHCP");
        let tcp6 = guest_ipv6("2606:4700::1111".parse().unwrap(), 6, 64, &[0; 20]);
        assert_eq!(classify(&tcp6), None, "TCP over v6 escaping the redirect");
        assert_eq!(classify(&[0u8; 10]), None, "a runt");
    }

    fn guest_ipv6(dst: Ipv6Addr, next: u8, hop_limit: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = ipv6_frame(GATEWAY_MAC, GUEST6, dst, next, hop_limit, payload);
        f[6..12].copy_from_slice(&GUEST_MAC);
        f
    }

    fn icmp6_from_guest(dst: Ipv6Addr, mut icmp: Vec<u8>) -> Vec<u8> {
        let sum = icmp6_checksum(GUEST6, dst, &icmp);
        icmp[2..4].copy_from_slice(&sum.to_be_bytes());
        guest_ipv6(dst, PROTO_ICMPV6, 255, &icmp)
    }

    /// The guest asks who has the v6 gateway; the card says it does, with
    /// its link-layer address, and the checksum a receiver will verify.
    #[test]
    fn a_neighbour_solicitation_for_the_gateway_is_advertised() {
        let mut ns = vec![ICMP6_NEIGHBOUR_SOLICIT, 0, 0, 0, 0, 0, 0, 0];
        ns.extend_from_slice(&GATEWAY6.octets());
        ns.extend_from_slice(&[1, 1]);
        ns.extend_from_slice(&GUEST_MAC);
        let solicited = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 1);
        let frame = icmp6_from_guest(solicited, ns);
        assert_eq!(classify(&frame), Some(Seen::NeighbourSolicit));
        let reply = neighbour_advertisement(&frame).expect("an advertisement");
        assert_eq!(&reply[0..6], &GUEST_MAC, "to the asker");
        let packet = ipv6(&reply).unwrap();
        assert_eq!(packet[7], 255, "hop limit 255 or the guest discards it");
        assert_eq!(ipv6_src(packet), GATEWAY6);
        assert_eq!(ipv6_dst(packet), GUEST6);
        let icmp = &packet[40..];
        assert_eq!(icmp[0], ICMP6_NEIGHBOUR_ADVERT);
        assert_eq!(icmp[4], 0xe0, "router, solicited, override");
        assert_eq!(ipv6_at(icmp, 8), Some(GATEWAY6));
        assert_eq!(&icmp[24..26], &[2, 1]);
        assert_eq!(&icmp[26..32], &GATEWAY_MAC);
        assert_eq!(icmp6_checksum(GATEWAY6, GUEST6, icmp), 0, "verifies");
    }

    #[test]
    fn a_solicitation_for_another_address_is_housekeeping() {
        let mut ns = vec![ICMP6_NEIGHBOUR_SOLICIT, 0, 0, 0, 0, 0, 0, 0];
        ns.extend_from_slice(&GUEST6.octets());
        let frame = icmp6_from_guest(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 2), ns);
        assert_eq!(classify(&frame), Some(Seen::Ipv6Housekeeping));
    }

    /// A multicast listener report carries a hop-by-hop header first.
    #[test]
    fn a_listener_report_is_housekeeping_not_a_drop() {
        let mut payload = vec![PROTO_ICMPV6, 0, 5, 2, 0, 0, 1, 0];
        payload.extend_from_slice(&[143, 0, 0, 0, 0, 0, 0, 1]);
        let frame = guest_ipv6(
            Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x16),
            PROTO_HOPOPTS,
            1,
            &payload,
        );
        assert_eq!(classify(&frame), Some(Seen::Ipv6Housekeeping));
    }

    #[test]
    fn echo_to_the_v6_gateway_is_answered_and_elsewhere_forwarded() {
        let mut echo = vec![ICMP6_ECHO_REQUEST, 0, 0, 0, 0x12, 0x34, 0, 1];
        echo.extend_from_slice(b"hello");
        let local = icmp6_from_guest(GATEWAY6, echo.clone());
        assert_eq!(classify(&local), Some(Seen::Icmp6Local));
        let reply = echo6_reply_local(&local).expect("a reply");
        let packet = ipv6(&reply).unwrap();
        assert_eq!(ipv6_src(packet), GATEWAY6);
        assert_eq!(ipv6_dst(packet), GUEST6);
        let icmp = &packet[40..];
        assert_eq!(icmp[0], ICMP6_ECHO_REPLY);
        assert_eq!(
            &icmp[4..8],
            &[0x12, 0x34, 0, 1],
            "identifier and sequence kept"
        );
        assert_eq!(&icmp[8..], b"hello");
        assert_eq!(icmp6_checksum(GATEWAY6, GUEST6, icmp), 0);

        let far = icmp6_from_guest("2606:4700::1111".parse().unwrap(), echo);
        assert_eq!(classify(&far), Some(Seen::Icmp6Forward));
    }

    /// What the host's socket hands back has no IP header and was addressed
    /// to the Mac; the frame the guest sees is addressed to the guest, with
    /// the checksum redone for that.
    #[test]
    fn a_forwarded_v6_reply_is_readdressed_to_the_guest() {
        let answerer: Ipv6Addr = "2606:4700::1111".parse().unwrap();
        let mut reply = vec![ICMP6_ECHO_REPLY, 0, 0xab, 0xcd, 0x12, 0x34, 0, 1];
        reply.extend_from_slice(b"hello");
        let frame = echo6_reply_frame(answerer, &reply).expect("a frame");
        assert_eq!(&frame[0..6], &GUEST_MAC);
        let packet = ipv6(&frame).unwrap();
        assert_eq!(ipv6_src(packet), answerer);
        assert_eq!(ipv6_dst(packet), GUEST6);
        assert_eq!(icmp6_checksum(answerer, GUEST6, &packet[40..]), 0);
        assert!(echo6_reply_frame(answerer, &[ICMP6_ECHO_REQUEST; 8]).is_none());
    }

    #[test]
    fn the_guest_mac_is_the_one_the_lease_is_keyed_on() {
        assert_eq!(GUEST_MAC, [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee]);
    }
}

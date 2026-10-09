//! DNS for the guest, answered by the Mac's resolver.
//!
//! The agent serves DNS inside the guest and carries every query here over
//! one vsock stream, framed `[len u16][id u16][query]`; replies go back the
//! same way. Address questions are answered through `getaddrinfo`, which is
//! the Mac's own resolver with its cache, its scoped resolvers and whatever
//! a VPN configured — the same answer a Mac process gets, at the same
//! speed. The names Docker promises, `host.docker.internal` and the
//! gateway, are answered here directly. Anything else (MX, TXT, SRV) is
//! forwarded raw to the first nameserver in resolv.conf, since the system
//! resolver has no API for those.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::virtio::vsock::{Accepted, VsockShared};

/// The vsock port the agent dials for DNS.
pub const DNS_PORT: u32 = 2379;

/// The card's addresses for the Mac, which the stream host maps to loopback.
/// dockerd's `--host-gateway-ip` in `guest/rootfs/init` repeats HOST_ALIAS.
const HOST_ALIAS: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 254);
const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 1);

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

/// The first nameserver in the Mac's resolver configuration, for record
/// types the system resolver cannot answer.
fn nameserver() -> SocketAddr {
    let text = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() == Some("nameserver")
            && let Some(addr) = words.next()
            && let Ok(ip) = addr.parse::<IpAddr>()
        {
            return SocketAddr::new(ip, 53);
        }
    }
    SocketAddr::new(Ipv4Addr::new(1, 1, 1, 1).into(), 53)
}

/// Starts answering the agent's DNS stream: each accepted stream goes to
/// the reactor, which answers cache hits inline and misses off-thread.
pub fn start(
    shared: Arc<VsockShared>,
    reactor: Arc<crate::reactor::Reactor>,
) -> std::io::Result<()> {
    let accepted = shared.listen(DNS_PORT);
    std::thread::Builder::new()
        .name("dns-accept".into())
        .spawn(move || {
            for Accepted { key } in accepted {
                reactor.accept_dns(key);
            }
        })?;
    Ok(())
}

/// A short cache in front of the system resolver. The resolver has its own,
/// but asking it is an IPC round trip of 150 µs; a hit here is a hash
/// lookup. Ten seconds is under any TTL that matters and what a stub
/// resolver keeps anyway.
struct Cached {
    addrs: Vec<IpAddr>,
    until: std::time::Instant,
}
static CACHE: Mutex<Option<std::collections::HashMap<(String, bool), Cached>>> = Mutex::new(None);
const CACHE_TTL: Duration = Duration::from_secs(10);
const CACHE_MAX: usize = 4096;

fn cache_get(name: &str, want_v6: bool) -> Option<Vec<IpAddr>> {
    let mut guard = CACHE.lock().expect("dns cache poisoned");
    let cache = guard.get_or_insert_with(std::collections::HashMap::new);
    let entry = cache.get(&(name.to_ascii_lowercase(), want_v6))?;
    if entry.until < std::time::Instant::now() {
        return None;
    }
    Some(entry.addrs.clone())
}

fn cache_put(name: &str, want_v6: bool, addrs: &[IpAddr]) {
    let mut guard = CACHE.lock().expect("dns cache poisoned");
    let cache = guard.get_or_insert_with(std::collections::HashMap::new);
    if cache.len() >= CACHE_MAX {
        cache.clear();
    }
    cache.insert(
        (name.to_ascii_lowercase(), want_v6),
        Cached {
            addrs: addrs.to_vec(),
            until: std::time::Instant::now() + CACHE_TTL,
        },
    );
}

/// One query, as the reactor handles it: `Some(reply)` now (a cache hit, a
/// Docker name, or a type the resolver cannot answer being forwarded
/// elsewhere returns None too), or None with the answer to come through
/// `deliver` later.
pub fn answer(
    query: Vec<u8>,
    id: u16,
    deliver: Arc<dyn Fn(u16, Vec<u8>) + Send + Sync>,
) -> Option<Vec<u8>> {
    let q = parse_question(&query)?;
    if q.qclass != CLASS_IN || (q.qtype != TYPE_A && q.qtype != TYPE_AAAA) {
        // Asked where the Mac would ask: the nameserver its configuration
        // picks for the name (a VPN's, an /etc/resolver file's, the
        // default), whose reply is exact, a missing name included; or, for
        // `.local`, the networks themselves, by multicast DNS.
        crate::workers::run("dns-records", crate::qos::CONNECTION_STACK, move || {
            let route = crate::sysdns::route(&q.name);
            tracing::debug!(name = %q.name, qtype = q.qtype, ?route, "a question for the Mac's resolvers");
            match route {
                Some(crate::sysdns::Route::Mdns) => {
                    let answer = crate::sysdns::multicast(&q.name, q.qtype, Duration::from_secs(1));
                    deliver(id, records_reply(&query, &q, answer));
                }
                Some(crate::sysdns::Route::Server(server)) => match forward_to(&query, server) {
                    Some(reply) => deliver(id, reply),
                    None => forward_raw(query, id, deliver),
                },
                None => forward_raw(query, id, deliver),
            }
        });
        return None;
    }
    let want_v6 = q.qtype == TYPE_AAAA;
    if let Ok(addrs) = resolve_local(&q.name, want_v6) {
        return Some(reply(&query, &q, &addrs, 0));
    }
    if let Some(addrs) = cache_get(&q.name, want_v6) {
        return Some(reply(&query, &q, &addrs, 0));
    }
    crate::workers::run("dns-lookup", crate::qos::CONNECTION_STACK, move || {
        let out = match resolve(&q.name, want_v6) {
            Ok(addrs) => {
                cache_put(&q.name, want_v6, &addrs);
                reply(&query, &q, &addrs, 0)
            }
            Err(()) => reply(&query, &q, &[], 3),
        };
        deliver(id, out);
    });
    None
}

/// The names answered here without a resolver.
fn resolve_local(name: &str, want_v6: bool) -> Result<Vec<IpAddr>, ()> {
    match name.to_ascii_lowercase().as_str() {
        "host.docker.internal" | "host.lima.internal" | "host.lighter.internal" => Ok(if want_v6 {
            Vec::new()
        } else {
            vec![HOST_ALIAS.into()]
        }),
        "gateway.docker.internal" => Ok(if want_v6 {
            Vec::new()
        } else {
            vec![GATEWAY.into()]
        }),
        name => match loopback_question(name) {
            Some((port, udp)) => Ok(if loopback_in_use(port, want_v6, udp) {
                vec![if want_v6 {
                    std::net::Ipv6Addr::LOCALHOST.into()
                } else {
                    Ipv4Addr::LOCALHOST.into()
                }]
            } else {
                Vec::new()
            }),
            None => Err(()),
        },
    }
}

/// The guest agent's question about the Mac's loopback (its `loopback.rs`):
/// `<port>.tcp.loopback.lighter.internal`, or `udp`; the port and whether
/// it is UDP's.
fn loopback_question(name: &str) -> Option<(u16, bool)> {
    let rest = name.strip_suffix(".loopback.lighter.internal")?;
    let (port, proto) = rest.split_once('.')?;
    let udp = match proto {
        "tcp" => false,
        "udp" => true,
        _ => return None,
    };
    port.parse()
        .ok()
        .filter(|port| *port != 0)
        .map(|port| (port, udp))
}

/// Whether something on the Mac holds its loopback address at `port`, of
/// TCP or UDP, which is what a listener or a bound socket there does: a bind of the address, without
/// `SO_REUSEADDR`, fails with "in use". Nothing connects, so a server sees
/// only the connection the guest then makes. A listener on the wildcard
/// holds loopback too, and a dual-stack one both families'.
fn loopback_in_use(port: u16, v6: bool, udp: bool) -> bool {
    let (family, addr): (libc::c_int, SocketAddr) = if v6 {
        (libc::AF_INET6, (std::net::Ipv6Addr::LOCALHOST, port).into())
    } else {
        (libc::AF_INET, (Ipv4Addr::LOCALHOST, port).into())
    };
    // SAFETY: a socket call with constant arguments.
    let kind = if udp {
        libc::SOCK_DGRAM
    } else {
        libc::SOCK_STREAM
    };
    let fd = unsafe { libc::socket(family, kind, 0) };
    if fd < 0 {
        return true;
    }
    // SAFETY: the descriptor is ours.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let bound = match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                sin_family: libc::AF_INET as u8,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(a.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: a sockaddr_in of its size, on a live socket.
            unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&sin as *const libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                sin6_family: libc::AF_INET6 as u8,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr {
                    s6_addr: a.ip().octets(),
                },
                sin6_scope_id: 0,
            };
            // SAFETY: a sockaddr_in6 of its size, on a live socket.
            unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&sin6 as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    bound != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EADDRINUSE)
}

/// A question sent as it is to `server`, and its reply if one comes within
/// three seconds.
fn forward_to(query: &[u8], server: SocketAddr) -> Option<Vec<u8>> {
    let local: SocketAddr = if server.is_ipv6() {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let udp = UdpSocket::bind(local).ok()?;
    udp.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    udp.connect(server).ok()?;
    udp.send(query).ok()?;
    let mut buf = vec![0u8; 4096];
    loop {
        let n = udp.recv(&mut buf).ok()?;
        // The reply to this question: its id, and a response.
        if n >= 12 && buf[0..2] == query[0..2] && buf[2] & 0x80 != 0 {
            buf.truncate(n);
            return Some(buf);
        }
    }
}

/// A non-address question, sent raw to the Mac's nameserver on a socket of
/// its own; its reply, if one comes within a few seconds, is delivered.
fn forward_raw(mut query: Vec<u8>, id: u16, deliver: Arc<dyn Fn(u16, Vec<u8>) + Send + Sync>) {
    crate::workers::run("dns-forward", crate::qos::CONNECTION_STACK, move || {
        let Ok(udp) = UdpSocket::bind("0.0.0.0:0") else {
            return;
        };
        let _ = udp.set_read_timeout(Some(Duration::from_secs(5)));
        let original = [query[0], query[1]];
        query[0..2].copy_from_slice(&id.to_be_bytes());
        if udp.send_to(&query, nameserver()).is_err() {
            return;
        }
        let mut buf = vec![0u8; 4096];
        if let Ok((n, _)) = udp.recv_from(&mut buf) {
            buf.truncate(n);
            if n >= 2 {
                buf[0..2].copy_from_slice(&original);
            }
            deliver(id, buf);
        }
    });
}

/// A parsed question: the name and what is asked about it.
struct Question {
    name: String,
    qtype: u16,
    qclass: u16,
    /// Where the question ends in the query, so a reply can copy it whole.
    end: usize,
}

fn parse_question(query: &[u8]) -> Option<Question> {
    if query.len() < 12 || u16::from_be_bytes([query[4], query[5]]) != 1 {
        return None;
    }
    let mut at = 12;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = *query.get(at)? as usize;
        at += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 || at + len > query.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&query[at..at + len]).to_string());
        at += len;
    }
    let qtype = u16::from_be_bytes([*query.get(at)?, *query.get(at + 1)?]);
    let qclass = u16::from_be_bytes([*query.get(at + 2)?, *query.get(at + 3)?]);
    Some(Question {
        name: labels.join("."),
        qtype,
        qclass,
        end: at + 4,
    })
}

/// A reply with the given addresses (or none, with the given rcode).
fn reply(query: &[u8], q: &Question, addrs: &[IpAddr], rcode: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(q.end + addrs.len() * 28);
    out.extend_from_slice(&query[..2]);
    // Flags: response, recursion desired as asked, recursion available.
    let rd = query[2] & 0x01;
    out.push(0x80 | rd);
    out.push(0x80 | (rcode & 0x0f));
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&(addrs.len() as u16).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&query[12..q.end]);
    for addr in addrs {
        // A pointer to the question's name, then type, class, TTL, data.
        out.extend_from_slice(&[0xc0, 0x0c]);
        match addr {
            IpAddr::V4(v4) => {
                out.extend_from_slice(&TYPE_A.to_be_bytes());
                out.extend_from_slice(&CLASS_IN.to_be_bytes());
                out.extend_from_slice(&60u32.to_be_bytes());
                out.extend_from_slice(&4u16.to_be_bytes());
                out.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                out.extend_from_slice(&TYPE_AAAA.to_be_bytes());
                out.extend_from_slice(&CLASS_IN.to_be_bytes());
                out.extend_from_slice(&60u32.to_be_bytes());
                out.extend_from_slice(&16u16.to_be_bytes());
                out.extend_from_slice(&v6.octets());
            }
        }
    }
    out
}

/// A reply carrying what the Mac's resolver said about a question of any
/// type: its records, each under its own name (a CNAME's target's records
/// are not the question's), or none.
fn records_reply(query: &[u8], q: &Question, answer: crate::sysdns::Answer) -> Vec<u8> {
    let (records, rcode) = match answer {
        crate::sysdns::Answer::Records(r) => (r, 0),
        crate::sysdns::Answer::NoData => (Vec::new(), 0),
        crate::sysdns::Answer::NoName => (Vec::new(), 3),
    };
    let mut answers = Vec::new();
    let mut count = 0u16;
    for r in &records {
        let Some(name) = crate::sysdns::wire_name(&r.name) else {
            continue;
        };
        if r.rdata.len() > u16::MAX as usize {
            continue;
        }
        answers.extend_from_slice(&name);
        answers.extend_from_slice(&r.rtype.to_be_bytes());
        answers.extend_from_slice(&q.qclass.to_be_bytes());
        answers.extend_from_slice(&r.ttl.to_be_bytes());
        answers.extend_from_slice(&(r.rdata.len() as u16).to_be_bytes());
        answers.extend_from_slice(&r.rdata);
        count += 1;
    }
    let mut out = Vec::with_capacity(q.end + answers.len());
    out.extend_from_slice(&query[..2]);
    out.push(0x80 | (query[2] & 0x01));
    out.push(0x80 | (rcode & 0x0f));
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&query[12..q.end]);
    out.extend_from_slice(&answers);
    out
}

/// The Mac's answer for a name, through its own resolver.
fn resolve(name: &str, want_v6: bool) -> Result<Vec<IpAddr>, ()> {
    if let Ok(local) = resolve_local(name, want_v6) {
        return Ok(local);
    }
    let addrs = (name, 0u16).to_socket_addrs().map_err(|_| ())?;
    Ok(family_of(addrs.map(|a| a.ip()), want_v6, host_has_v6()))
}

/// The addresses of one family, each once; and none over v6 while the Mac
/// has no v6 route, so a container never tries an address the stream
/// could not reach before falling back to one it could.
fn family_of(addrs: impl Iterator<Item = IpAddr>, want_v6: bool, v6_route: bool) -> Vec<IpAddr> {
    if want_v6 && !v6_route {
        return Vec::new();
    }
    let mut out: Vec<IpAddr> = Vec::new();
    for ip in addrs {
        if ip.is_ipv6() == want_v6 && !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// Whether the Mac has a route to the IPv6 internet: asked of the kernel by
/// connecting a datagram socket (no packet is sent), remembered for as long
/// as an answer is, so a network change is noticed within seconds.
fn host_has_v6() -> bool {
    static LAST: std::sync::Mutex<Option<(std::time::Instant, bool)>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, up)) = *last
        && at.elapsed() < std::time::Duration::from_secs(10)
    {
        return up;
    }
    let up = std::net::UdpSocket::bind("[::]:0")
        .and_then(|s| s.connect("[2001:4860:4860::8888]:53"))
        .is_ok();
    *last = Some((std::time::Instant::now(), up));
    up
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name with both families answers only in the family asked, once
    /// per address; and AAAA is empty on a Mac with no v6 route, so the
    /// container's first attempt is one the stream can carry.
    #[test]
    fn aaaa_is_answered_only_where_the_mac_can_route_it() {
        let addrs: Vec<IpAddr> = vec![
            "93.184.216.34".parse().unwrap(),
            "2606:2800:21f:cb07:6820:80da:af6b:8b2c".parse().unwrap(),
            "93.184.216.34".parse().unwrap(),
        ];
        assert_eq!(
            family_of(addrs.iter().copied(), false, false),
            vec!["93.184.216.34".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(
            family_of(addrs.iter().copied(), true, true),
            vec![
                "2606:2800:21f:cb07:6820:80da:af6b:8b2c"
                    .parse::<IpAddr>()
                    .unwrap()
            ]
        );
        assert!(family_of(addrs.iter().copied(), true, false).is_empty());
    }

    fn query_for(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    #[test]
    fn a_question_is_parsed_whole() {
        let q = query_for("example.com", TYPE_A);
        let parsed = parse_question(&q).unwrap();
        assert_eq!(parsed.name, "example.com");
        assert_eq!(parsed.qtype, TYPE_A);
        assert_eq!(parsed.end, q.len());
    }

    #[test]
    fn a_reply_carries_the_id_the_question_and_the_addresses() {
        let q = query_for("host.docker.internal", TYPE_A);
        let parsed = parse_question(&q).unwrap();
        let addrs = resolve("host.docker.internal", false).unwrap();
        let r = reply(&q, &parsed, &addrs, 0);
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80, "a response");
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1, "one answer");
        assert_eq!(&r[r.len() - 4..], &HOST_ALIAS.octets());
    }

    #[test]
    fn the_docker_names_are_the_mac() {
        assert_eq!(
            resolve("host.docker.internal", false).unwrap(),
            vec![IpAddr::V4(HOST_ALIAS)]
        );
        assert_eq!(
            resolve("gateway.docker.internal", false).unwrap(),
            vec![IpAddr::V4(GATEWAY)]
        );
        assert!(resolve("host.docker.internal", true).unwrap().is_empty());
    }

    #[test]
    fn the_macs_loopback_is_in_use_where_something_listens() {
        assert_eq!(
            loopback_question("5432.tcp.loopback.lighter.internal"),
            Some((5432, false))
        );
        assert_eq!(
            loopback_question("8125.udp.loopback.lighter.internal"),
            Some((8125, true))
        );
        assert_eq!(loopback_question("1.sctp.loopback.lighter.internal"), None);
        assert_eq!(loopback_question("0.tcp.loopback.lighter.internal"), None);
        assert_eq!(loopback_question("x.tcp.loopback.lighter.internal"), None);
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        assert!(loopback_in_use(port, false, false));
        assert!(
            !loopback_in_use(port, true, false),
            "a v4 listener does not hold ::1"
        );
        drop(v4);
        assert!(!loopback_in_use(port, false, false));
        let wild = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        assert!(
            loopback_in_use(wild.local_addr().unwrap().port(), false, false),
            "the wildcard holds loopback"
        );
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(loopback_in_use(
            udp.local_addr().unwrap().port(),
            false,
            true
        ));
    }

    #[test]
    fn records_of_any_type_are_answered_under_their_own_names() {
        let mut query = vec![0xab, 0xcd, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        query.extend_from_slice(b"\x04mail\x07example\x03com\x00");
        query.extend_from_slice(&[0, 15, 0, 1]); // MX
        let q = parse_question(&query).unwrap();
        let answer = crate::sysdns::Answer::Records(vec![
            crate::sysdns::Record {
                name: "mail.example.com".into(),
                rtype: 5,
                ttl: 30,
                rdata: b"\x02mx\x07example\x03com\x00".to_vec(),
            },
            crate::sysdns::Record {
                name: "mx.example.com".into(),
                rtype: 15,
                ttl: 60,
                rdata: b"\x00\x0a\x02mx\x07example\x03com\x00".to_vec(),
            },
        ]);
        let reply = records_reply(&query, &q, answer);
        assert_eq!(&reply[..2], &[0xab, 0xcd]);
        assert_eq!(reply[3] & 0x0f, 0);
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 2);
        let second = reply
            .windows(16)
            .position(|w| w == b"\x02mx\x07example\x03com\x00\x00\x0f".get(..16).unwrap());
        assert!(second.is_some(), "the MX under the CNAME's target");
        let none = records_reply(&query, &q, crate::sysdns::Answer::NoName);
        assert_eq!(
            (none[3] & 0x0f, u16::from_be_bytes([none[6], none[7]])),
            (3, 0)
        );
    }
}

//! DNS for the guest, answered by the Mac's resolver.
//!
//! The agent serves DNS inside the guest and carries every query here over
//! one vsock stream, framed `[len u16][id u16][query]`; replies go back the
//! same way. Address questions are answered through `getaddrinfo`, which is
//! the Mac's own resolver with its cache, its scoped resolvers and whatever
//! a VPN configured — the same answer a Mac process gets, at the same
//! speed. The names Docker promises, `host.docker.internal` and the
//! gateway, are answered here directly. Anything else (MX, TXT, SRV) is
//! asked of the nameservers the Mac's configuration picks for the name, since
//! the system resolver has no API for those: in order, the next each second
//! the last has not answered, as the Mac's own resolver moves past one that
//! is down (`ask`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// The nameservers in the Mac's resolver configuration, for a name its
/// configuration routes nowhere in particular.
fn nameservers() -> Vec<SocketAddr> {
    let text = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let servers: Vec<SocketAddr> = text
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("nameserver"))
                .then(|| words.next()?.parse::<IpAddr>().ok())
                .flatten()
        })
        .map(|ip| SocketAddr::new(ip, 53))
        .collect();
    if servers.is_empty() {
        return vec![SocketAddr::new(Ipv4Addr::new(1, 1, 1, 1).into(), 53)];
    }
    servers
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
                    let answer = crate::sysdns::local(&q.name, q.qtype, Duration::from_secs(1));
                    deliver(id, records_reply(&query, &q, answer));
                }
                Some(crate::sysdns::Route::Servers(servers)) => deliver(
                    id,
                    ask(&query, &servers).unwrap_or_else(|| failure(&query, &q)),
                ),
                None => deliver(
                    id,
                    ask(&query, &nameservers()).unwrap_or_else(|| failure(&query, &q)),
                ),
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
    // A `.local` name the merged way, as for any other type: the Mac's
    // resolver alone, through `getaddrinfo`, gave a container nothing on the
    // M1, 0.12.5 included, while the Mac itself resolved the same name.
    if is_local(&q.name) {
        crate::workers::run("dns-local", crate::qos::CONNECTION_STACK, move || {
            let rtype = if want_v6 { TYPE_AAAA } else { TYPE_A };
            let addrs = local_addrs(
                crate::sysdns::local(&q.name, rtype, Duration::from_secs(1)),
                want_v6,
            );
            if !addrs.is_empty() {
                cache_put(&q.name, want_v6, &addrs);
            }
            deliver(id, reply(&query, &q, &addrs, 0));
        });
        return None;
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

/// Whether a name is multicast DNS's.
fn is_local(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "local" || name.ends_with(".local")
}

/// The addresses of one family in an answer, without IPv6 link-local ones,
/// which a container could not reach through the Mac.
fn local_addrs(answer: crate::sysdns::Answer, want_v6: bool) -> Vec<IpAddr> {
    let crate::sysdns::Answer::Records(records) = answer else {
        return Vec::new();
    };
    let mut out: Vec<IpAddr> = Vec::new();
    for r in records {
        let ip: Option<IpAddr> = match (r.rtype, r.rdata.len()) {
            (TYPE_A, 4) => <[u8; 4]>::try_from(&r.rdata[..])
                .ok()
                .map(|b| Ipv4Addr::from(b).into()),
            (TYPE_AAAA, 16) => <[u8; 16]>::try_from(&r.rdata[..])
                .ok()
                .map(std::net::Ipv6Addr::from)
                .filter(|v6| !v6.is_unicast_link_local())
                .map(IpAddr::from),
            _ => None,
        };
        if let Some(ip) = ip
            && ip.is_ipv6() == want_v6
            && !out.contains(&ip)
        {
            out.push(ip);
        }
    }
    out
}

/// How long a nameserver has to answer before the next is asked as well,
/// and how long all of them have: past a second the Mac's resolver moves on
/// too, and the whole stays inside the five seconds a container's resolver
/// waits for an answer, so a SERVFAIL reaches it rather than a timeout.
const NEXT_SERVER: Duration = Duration::from_secs(1);
const ALL_SERVERS: Duration = Duration::from_secs(4);

/// Asks `servers` in order, the next each [`NEXT_SERVER`] the ones asked
/// have not answered, and returns the first reply to this question from
/// any of them. A first nameserver that is down is what a Mac's network
/// can have for hours: the Mac's resolver moves past it, and one that waited
/// on it alone answered a container nothing (m3-streams, 2026-10-25).
fn ask(query: &[u8], servers: &[SocketAddr]) -> Option<Vec<u8>> {
    ask_within(query, servers, NEXT_SERVER, ALL_SERVERS)
}

fn ask_within(
    query: &[u8],
    servers: &[SocketAddr],
    next: Duration,
    all: Duration,
) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let bind = |ip: IpAddr| {
        let socket = UdpSocket::bind((ip, 0)).ok()?;
        socket.set_nonblocking(true).ok()?;
        Some(socket)
    };
    let v4 = bind(Ipv4Addr::UNSPECIFIED.into());
    let v6 = bind(Ipv6Addr::UNSPECIFIED.into());
    let sockets: Vec<&UdpSocket> = [v4.as_ref(), v6.as_ref()].into_iter().flatten().collect();
    let start = Instant::now();
    let deadline = start + all;
    let mut asked: Vec<SocketAddr> = Vec::new();
    let mut pending = servers.iter();
    let mut next_at = start;
    let mut buf = vec![0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        // A server that cannot be sent to, a family the Mac has no route
        // for, costs no wait: the next is asked at once.
        while now >= next_at {
            let Some(&server) = pending.next() else {
                break;
            };
            let socket = if server.is_ipv6() {
                v6.as_ref()
            } else {
                v4.as_ref()
            };
            if socket.is_some_and(|s| s.send_to(query, server).is_ok()) {
                asked.push(server);
                next_at = now + next;
            }
        }
        if asked.is_empty() && pending.len() == 0 {
            return None;
        }
        let until = if pending.len() > 0 {
            next_at.min(deadline)
        } else {
            deadline
        };
        let mut fds: Vec<libc::pollfd> = sockets
            .iter()
            .map(|s| libc::pollfd {
                fd: s.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let wait = until
            .saturating_duration_since(now)
            .as_millis()
            .clamp(1, 60_000) as libc::c_int;
        // SAFETY: a valid array of pollfds for sockets alive for the call.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait) } <= 0 {
            continue;
        }
        for socket in &sockets {
            while let Ok((n, from)) = socket.recv_from(&mut buf) {
                // The reply to this question, from a server it was asked of:
                // its id, and a response.
                let ours = asked
                    .iter()
                    .any(|s| s.ip() == from.ip() && s.port() == from.port());
                if ours && n >= 12 && buf[0..2] == query[0..2] && buf[2] & 0x80 != 0 {
                    buf.truncate(n);
                    return Some(buf);
                }
            }
        }
    }
}

/// The answer when no nameserver gave one: SERVFAIL, what a resolver says
/// when its upstream fails, so the container is told rather than left to
/// time out.
fn failure(query: &[u8], q: &Question) -> Vec<u8> {
    let mut reply = records_reply(query, q, crate::sysdns::Answer::NoData);
    reply[3] = (reply[3] & 0xf0) | 2;
    reply
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

    /// A nameserver on loopback that answers each question `answers` times,
    /// first with whatever `stray` makes of it, then with the reply; or never.
    struct Nameserver {
        addr: SocketAddr,
        asked: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// Makes a reply to another question, or something that is no reply.
    type Stray = fn(&[u8]) -> Vec<u8>;

    fn nameserver(answering: bool, stray: Option<Stray>) -> Nameserver {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = asked.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = socket.recv_from(&mut buf) {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if !answering {
                    continue;
                }
                if let Some(stray) = stray {
                    let _ = socket.send_to(&stray(&buf[..n]), from);
                }
                let mut reply = buf[..n].to_vec();
                reply[2] |= 0x80;
                reply[3] = 0x83;
                let _ = socket.send_to(&reply, from);
            }
        });
        Nameserver { addr, asked }
    }

    fn asked(server: &Nameserver) -> usize {
        server.asked.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn a_nameserver_that_is_down_is_passed_over_for_the_next() {
        let (down, up) = (nameserver(false, None), nameserver(true, None));
        let query = query_for("lighter-nosuchname.example.com", 16);
        let started = Instant::now();
        let reply = ask_within(
            &query,
            &[down.addr, up.addr],
            Duration::from_millis(300),
            Duration::from_secs(4),
        )
        .expect("the second nameserver's answer");
        let took = started.elapsed();
        assert_eq!(reply[3] & 0x0f, 3, "its NXDOMAIN, as it said it");
        assert!(
            took >= Duration::from_millis(300),
            "the first had its turn: {took:?}"
        );
        assert!(
            took < Duration::from_secs(2),
            "and no more than its turn: {took:?}"
        );
        assert_eq!((asked(&down), asked(&up)), (1, 1));
    }

    #[test]
    fn a_nameserver_that_answers_is_the_only_one_asked() {
        let (first, second) = (nameserver(true, None), nameserver(true, None));
        let query = query_for("example.com", 16);
        let started = Instant::now();
        assert!(
            ask_within(
                &query,
                &[first.addr, second.addr],
                Duration::from_millis(500),
                Duration::from_secs(4)
            )
            .is_some()
        );
        assert!(started.elapsed() < Duration::from_millis(400));
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!((asked(&first), asked(&second)), (1, 0));
    }

    #[test]
    fn only_the_reply_to_this_question_is_taken() {
        // Another question's reply, then a query rather than a reply, before the answer.
        fn other_id(q: &[u8]) -> Vec<u8> {
            let mut r = q.to_vec();
            r[0] ^= 0xff;
            r[2] |= 0x80;
            r
        }
        fn not_a_reply(q: &[u8]) -> Vec<u8> {
            q.to_vec()
        }
        for stray in [other_id as Stray, not_a_reply] {
            let server = nameserver(true, Some(stray));
            let query = query_for("example.com", 16);
            let reply = ask_within(
                &query,
                &[server.addr],
                Duration::from_millis(300),
                Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(&reply[0..2], &query[0..2]);
            assert_eq!(reply[3] & 0x0f, 3);
        }
    }

    #[test]
    fn a_reply_from_a_server_not_asked_is_not_taken() {
        // The server asked stays silent; a socket beside it, on another
        // port, answers the question exactly, to the address that asked.
        let asked = UdpSocket::bind("127.0.0.1:0").unwrap();
        let impostor = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = asked.local_addr().unwrap();
        let answered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sent = answered.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            if let Ok((n, from)) = asked.recv_from(&mut buf) {
                let mut reply = buf[..n].to_vec();
                reply[2] |= 0x80;
                if impostor.send_to(&reply, from).is_ok() {
                    sent.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        });
        let query = query_for("example.com", 16);
        assert!(
            ask_within(
                &query,
                &[addr],
                Duration::from_millis(100),
                Duration::from_millis(600)
            )
            .is_none()
        );
        assert!(
            answered.load(std::sync::atomic::Ordering::SeqCst),
            "the impostor did answer"
        );
    }

    #[test]
    fn no_answer_from_any_is_a_servfail_in_time() {
        let (a, b) = (nameserver(false, None), nameserver(false, None));
        let query = query_for("example.com", 16);
        let started = Instant::now();
        assert!(
            ask_within(
                &query,
                &[a.addr, b.addr],
                Duration::from_millis(100),
                Duration::from_millis(500)
            )
            .is_none()
        );
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(500) && took < Duration::from_millis(900),
            "{took:?}"
        );
        assert_eq!((asked(&a), asked(&b)), (1, 1));
        let q = parse_question(&query).unwrap();
        let reply = failure(&query, &q);
        assert_eq!(&reply[0..2], &query[0..2]);
        assert_eq!(reply[2] & 0x80, 0x80, "a response");
        assert_eq!(reply[3] & 0x0f, 2, "SERVFAIL");
        assert_eq!(&reply[12..], &query[12..], "to the question asked");
    }

    #[test]
    fn the_real_budget_fits_inside_a_containers_wait() {
        // musl and dig wait five seconds for the agent's answer.
        assert!(ALL_SERVERS < Duration::from_secs(5));
        assert!(NEXT_SERVER < ALL_SERVERS);
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

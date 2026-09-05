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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::virtio::vsock::{Accepted, VsockShared};

/// The vsock port the agent dials for DNS.
pub const DNS_PORT: u32 = 2379;

/// The card's addresses for the Mac, which the stream host maps to loopback.
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
        // Forwarded raw to the Mac's nameserver, answered whenever it does.
        forward_raw(query, id, deliver);
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
        _ => Err(()),
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

/// The Mac's answer for a name, through its own resolver.
fn resolve(name: &str, want_v6: bool) -> Result<Vec<IpAddr>, ()> {
    if let Ok(local) = resolve_local(name, want_v6) {
        return Ok(local);
    }
    let addrs = (name, 0u16).to_socket_addrs().map_err(|_| ())?;
    let mut out: Vec<IpAddr> = Vec::new();
    for a in addrs {
        let ip = a.ip();
        if ip.is_ipv6() == want_v6 && !out.contains(&ip) {
            out.push(ip);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

//! Any record type, answered where the Mac would ask.
//!
//! Address questions go through `getaddrinfo`, which is the Mac's resolver
//! with everything configured into it: per-domain resolvers in
//! `/etc/resolver`, the scoped resolvers a VPN installs, `.local` by
//! multicast DNS. The other types (TXT, SRV, MX, PTR, CAA…) were sent raw
//! to the first nameserver in `resolv.conf`, which knows none of that: a
//! VPN's internal SRV record, or a `.local` service a container browses
//! for, was not found. Now each goes where the Mac's configuration sends
//! its name ([`route`]): to that nameserver, whose reply is exact, or, for
//! `.local`, out by multicast DNS from here ([`multicast`]).
//!
//! Not through `DNSServiceQueryRecord`, which is the Mac's resolver for any
//! type: a process with an app's identity, as the machine's is, was told
//! nothing at all by it on the M1, where a plain tool asking the same got
//! thirty answers in 4 ms. Its "no such record" also stands for a missing
//! name and a missing type alike.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// One record of an answer: its owner name (presentation form, without the
/// final dot), type, TTL and data as it goes on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub rtype: u16,
    pub ttl: u32,
    pub rdata: Vec<u8>,
}

/// What the resolver said.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Records(Vec<Record>),
    /// The name exists without records of the type, or does not exist.
    NoData,
    NoName,
}

/// Asks the networks the Mac is on, by multicast DNS, for `name`'s records
/// of `rtype`, and gathers every answer that comes in `wait`. A one-shot
/// query from a port other than 5353 (RFC 6762's legacy unicast) is
/// answered straight back to it, by each responder. No answer is no data:
/// multicast DNS has no "no such name".
pub fn multicast(name: &str, rtype: u16, wait: Duration) -> Answer {
    let Some(qname) = wire_name(name) else {
        return Answer::NoData;
    };
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Answer::NoData;
    };
    let mut id = [0u8; 2];
    // SAFETY: arc4random_buf fills the buffer it is given.
    unsafe { libc::arc4random_buf(id.as_mut_ptr().cast(), id.len()) };
    let id = u16::from_be_bytes(id);
    let mut query = id.to_be_bytes().to_vec();
    query.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    query.extend_from_slice(&qname);
    query.extend_from_slice(&rtype.to_be_bytes());
    query.extend_from_slice(&1u16.to_be_bytes());
    let group: SocketAddr = (Ipv4Addr::new(224, 0, 0, 251), 5353).into();
    for ip in interfaces() {
        set_multicast_if(&socket, ip);
        let _ = socket.send_to(&query, group);
    }
    let mut records: Vec<Record> = Vec::new();
    let deadline = Instant::now() + wait;
    let mut buf = vec![0u8; 9000];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || socket.set_read_timeout(Some(left)).is_err() {
            break;
        }
        let Ok((n, _)) = socket.recv_from(&mut buf) else {
            break;
        };
        for record in answers(&buf[..n], id).unwrap_or_default() {
            if !records.iter().any(|r| {
                r.rtype == record.rtype
                    && r.rdata == record.rdata
                    && r.name.eq_ignore_ascii_case(&record.name)
            }) {
                records.push(record);
            }
        }
    }
    if records.is_empty() {
        Answer::NoData
    } else {
        Answer::Records(records)
    }
}

/// The Mac's interfaces' IPv4 addresses that carry multicast, other than
/// loopback.
fn interfaces() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: an out-pointer, freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return out;
    }
    let mut at = list;
    while !at.is_null() {
        // SAFETY: a node of the list getifaddrs returned.
        let ifa = unsafe { &*at };
        let up = ifa.ifa_flags & (libc::IFF_UP as u32) != 0
            && ifa.ifa_flags & (libc::IFF_MULTICAST as u32) != 0;
        // SAFETY: a non-null sockaddr, and an AF_INET one is a sockaddr_in.
        if up
            && !ifa.ifa_addr.is_null()
            && unsafe { (*ifa.ifa_addr).sa_family } as i32 == libc::AF_INET
        {
            let sin = unsafe { &*ifa.ifa_addr.cast::<libc::sockaddr_in>() };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if !ip.is_loopback() && !out.contains(&ip) {
                out.push(ip);
            }
        }
        at = ifa.ifa_next;
    }
    // SAFETY: the list getifaddrs returned.
    unsafe { libc::freeifaddrs(list) };
    out
}

fn set_multicast_if(socket: &UdpSocket, ip: Ipv4Addr) {
    use std::os::fd::AsRawFd;
    let addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(ip.octets()),
    };
    // SAFETY: an in_addr option on a live socket.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            std::ptr::addr_of!(addr).cast(),
            std::mem::size_of::<libc::in_addr>() as libc::socklen_t,
        );
    }
}

/// The answer records of a reply to question `id`, names written out in
/// full, in the record and in its data, since a responder compresses them
/// against its own message.
fn answers(msg: &[u8], id: u16) -> Option<Vec<Record>> {
    if msg.len() < 12 || msg[0..2] != id.to_be_bytes() || msg[2] & 0x80 == 0 {
        return None;
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    let an = u16::from_be_bytes([msg[6], msg[7]]);
    let mut at = 12;
    for _ in 0..qd {
        at = read_name(msg, at)?.1 + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        let (name, next) = read_name(msg, at)?;
        let rtype = u16::from_be_bytes([*msg.get(next)?, *msg.get(next + 1)?]);
        let ttl = u32::from_be_bytes(msg.get(next + 4..next + 8)?.try_into().ok()?);
        let len = usize::from(u16::from_be_bytes([
            *msg.get(next + 8)?,
            *msg.get(next + 9)?,
        ]));
        let start = next + 10;
        let rdata = msg.get(start..start + len)?;
        // The types whose data holds a name, and where in it the name starts.
        let named = match rtype {
            2 | 5 | 12 => Some(0),
            15 => Some(2),
            33 => Some(6),
            _ => None,
        };
        let rdata = match named {
            Some(skip) if rdata.len() > skip => {
                let mut full = rdata[..skip].to_vec();
                let (target, _) = read_name(msg, start + skip)?;
                full.extend_from_slice(&wire_name(&target)?);
                full
            }
            _ => rdata.to_vec(),
        };
        out.push(Record {
            name,
            rtype,
            ttl,
            rdata,
        });
        at = start + len;
    }
    Some(out)
}

/// A name at `at`, following compression, in presentation form (`.` and
/// `\` escaped, anything unprintable as `\DDD`), and where the field after
/// it starts.
fn read_name(msg: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    for _ in 0..128 {
        let len = *msg.get(at)?;
        match len {
            0 => return Some((labels.join("."), end.unwrap_or(at + 1))),
            l if l & 0xc0 == 0xc0 => {
                end.get_or_insert(at + 2);
                at = usize::from(u16::from_be_bytes([l, *msg.get(at + 1)?]) & 0x3fff);
            }
            l if l < 64 => {
                let raw = msg.get(at + 1..at + 1 + usize::from(l))?;
                let mut label = String::new();
                for &b in raw {
                    match b {
                        b'.' | b'\\' => {
                            label.push('\\');
                            label.push(b as char);
                        }
                        0x21..=0x7e => label.push(b as char),
                        _ => label.push_str(&format!("\\{b:03}")),
                    }
                }
                labels.push(label);
                at += 1 + usize::from(l);
            }
            _ => return None,
        }
    }
    None
}

/// Where the Mac sends a question for a name: to multicast DNS, or to a
/// nameserver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Mdns,
    Server(std::net::SocketAddr),
}

/// A resolver from the Mac's DNS configuration (`scutil --dns`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolver {
    domain: Option<String>,
    nameservers: Vec<std::net::SocketAddr>,
    order: u32,
    mdns: bool,
}

/// The resolvers of the main configuration, before its scoped copies.
fn parse_resolvers(text: &str) -> Vec<Resolver> {
    let main = text
        .split("DNS configuration (for scoped queries)")
        .next()
        .unwrap_or("");
    let mut out = Vec::new();
    for block in main.split("resolver #").skip(1) {
        let mut r = Resolver {
            domain: None,
            nameservers: Vec::new(),
            order: u32::MAX,
            mdns: false,
        };
        for line in block.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "domain" => r.domain = Some(value.trim_end_matches('.').to_ascii_lowercase()),
                k if k.starts_with("nameserver[") => {
                    // A link-local address carries its interface, which
                    // a socket needs to reach it: `fe80::1%en0`.
                    let (ip, scope) = value.split_once('%').unwrap_or((value, ""));
                    match ip.parse::<std::net::IpAddr>() {
                        Ok(std::net::IpAddr::V6(v6)) => {
                            let index = std::ffi::CString::new(scope)
                                // SAFETY: a NUL-terminated name.
                                .map(|n| unsafe { libc::if_nametoindex(n.as_ptr()) })
                                .unwrap_or(0);
                            r.nameservers
                                .push(std::net::SocketAddrV6::new(v6, 53, 0, index).into());
                        }
                        Ok(ip) => r.nameservers.push((ip, 53).into()),
                        Err(_) => {}
                    }
                }
                "order" => r.order = value.parse().unwrap_or(u32::MAX),
                "options" => r.mdns = value.split_whitespace().any(|o| o == "mdns"),
                _ => {}
            }
        }
        out.push(r);
    }
    out
}

/// Where the Mac's configuration sends a question for `name`: the resolver
/// whose domain is the longest that `name` is in, else the first by order
/// of those for every domain. `None` when it has none with a nameserver.
fn route_in(resolvers: &[Resolver], name: &str) -> Option<Route> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let within = |d: &str| name == d || name.ends_with(&format!(".{d}"));
    let chosen = resolvers
        .iter()
        .filter(|r| r.domain.as_deref().is_some_and(within))
        .max_by_key(|r| r.domain.as_ref().map_or(0, String::len))
        .or_else(|| {
            resolvers
                .iter()
                .filter(|r| r.domain.is_none() && !r.nameservers.is_empty())
                .min_by_key(|r| r.order)
        })?;
    if chosen.mdns {
        return Some(Route::Mdns);
    }
    Some(Route::Server(*chosen.nameservers.first()?))
}

/// Where the Mac would send a question for `name`, from its configuration
/// as of the last ten seconds, read when asked.
pub fn route(name: &str) -> Option<Route> {
    static CACHE: std::sync::Mutex<Option<(Instant, Vec<Resolver>)>> = std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = cache
        .as_ref()
        .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(10));
    if !fresh {
        let text = std::process::Command::new("/usr/sbin/scutil")
            .arg("--dns")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        *cache = Some((Instant::now(), parse_resolvers(&text)));
    }
    route_in(&cache.as_ref().expect("filled above").1, name)
}

/// A name in presentation form (`My\032Printer._ipp._tcp.local`) as wire
/// labels; `None` for one that cannot be.
pub fn wire_name(name: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(name.len() + 2);
    let mut label = Vec::new();
    let bytes = name.trim_end_matches('.').as_bytes();
    let mut i = 0;
    let push = |out: &mut Vec<u8>, label: &mut Vec<u8>| -> Option<()> {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        out.push(label.len() as u8);
        out.append(label);
        Some(())
    };
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let rest = bytes.get(i + 1..)?;
                if rest.len() >= 3 && rest[..3].iter().all(u8::is_ascii_digit) {
                    let v: u32 = std::str::from_utf8(&rest[..3]).ok()?.parse().ok()?;
                    label.push(u8::try_from(v).ok()?);
                    i += 4;
                } else {
                    label.push(*rest.first()?);
                    i += 2;
                }
            }
            b'.' => {
                push(&mut out, &mut label)?;
                i += 1;
            }
            b => {
                label.push(b);
                i += 1;
            }
        }
    }
    if !label.is_empty() {
        push(&mut out, &mut label)?;
    }
    out.push(0);
    (out.len() <= 255).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_names_become_wire_labels() {
        assert_eq!(wire_name("example.com").unwrap(), b"\x07example\x03com\x00");
        assert_eq!(
            wire_name("example.com.").unwrap(),
            b"\x07example\x03com\x00"
        );
        assert_eq!(
            wire_name(r"My\032Printer._ipp._tcp.local").unwrap(),
            b"\x0aMy Printer\x04_ipp\x04_tcp\x05local\x00"
        );
        assert_eq!(wire_name(r"a\.b.c").unwrap(), b"\x03a.b\x01c\x00");
        assert_eq!(wire_name("a..b"), None);
        assert_eq!(wire_name(&"x".repeat(64)), None);
    }

    /// A responder's reply, its names compressed against its own message
    /// as they always are, read with every name written out in full.
    #[test]
    fn a_responders_answers_are_read_whole() {
        let mut m = vec![0x12, 0x34, 0x84, 0x00, 0, 1, 0, 2, 0, 0, 0, 0];
        let qname = wire_name("_ipp._tcp.local").unwrap();
        m.extend_from_slice(&qname);
        m.extend_from_slice(&[0, 12, 0, 1]);
        // PTR _ipp._tcp.local -> "Printer._ipp._tcp.local", compressed.
        m.extend_from_slice(&[0xc0, 12, 0, 12, 0x80, 1, 0, 0, 0, 10, 0, 10]);
        m.extend_from_slice(&[7]);
        m.extend_from_slice(b"Printer");
        m.extend_from_slice(&[0xc0, 12]);
        // SRV Printer._ipp._tcp.local -> 0 0 631 "host.local".
        let ptr_target = 12 + qname.len() + 4 + 12;
        m.extend_from_slice(&(0xc000u16 | ptr_target as u16).to_be_bytes());
        m.extend_from_slice(&[0, 33, 0x80, 1, 0, 0, 0, 120, 0, 13, 0, 0, 0, 0, 0x02, 0x77]);
        m.extend_from_slice(&[4]);
        m.extend_from_slice(b"host");
        m.extend_from_slice(&[0xc0, 12 + 10]);
        let got = answers(&m, 0x1234).unwrap();
        assert_eq!(got[0].name, "_ipp._tcp.local");
        assert_eq!(got[0].rdata, wire_name("Printer._ipp._tcp.local").unwrap());
        assert_eq!(got[1].name, "Printer._ipp._tcp.local");
        assert_eq!(&got[1].rdata[..6], &[0, 0, 0, 0, 0x02, 0x77]);
        assert_eq!(&got[1].rdata[6..], &wire_name("host.local").unwrap()[..]);
        assert!(answers(&m, 0x4321).is_none(), "another question's");
    }

    #[test]
    fn unusual_bytes_round_trip_through_presentation() {
        let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        m.extend_from_slice(&[6]);
        m.extend_from_slice(b"My P.r");
        m.extend_from_slice(&[5]);
        m.extend_from_slice(b"local");
        m.extend_from_slice(&[0, 0, 16, 0, 1, 0, 0, 0, 10, 0, 1, 0]);
        let got = answers(&m, 0).unwrap();
        assert_eq!(got[0].name, r"My\032P\.r.local");
        assert_eq!(wire_name(&got[0].name).unwrap(), &m[12..12 + 14]);
    }

    const SCUTIL: &str = "DNS configuration

resolver #1
  search domain[0] : taile41d51.ts.net
  nameserver[0] : 100.100.100.100
  if_index : 41 (utun7)
  flags    : Supplemental, Request A records, Request AAAA records
  order    : 101600

resolver #2
  nameserver[0] : 2a02:6b67:ea05:9200::1
  nameserver[1] : 192.168.50.1
  order    : 200000

resolver #3
  domain   : taile41d51.ts.net.
  nameserver[0] : 100.100.100.100
  order    : 101601

resolver #4
  domain   : corp.example
  nameserver[0] : fe80::1%en0
  order    : 101700

resolver #5
  domain   : local
  options  : mdns
  timeout  : 5
  order    : 300000

DNS configuration (for scoped queries)

resolver #1
  nameserver[0] : 192.168.50.1
  order    : 1
";

    #[test]
    fn a_name_goes_where_the_macs_configuration_sends_it() {
        let resolvers = parse_resolvers(SCUTIL);
        assert_eq!(resolvers.len(), 5, "the scoped copies left out");
        let at = |ip: &str| {
            Some(Route::Server(
                (ip.parse::<std::net::IpAddr>().unwrap(), 53).into(),
            ))
        };
        let scoped = |ip: &str| {
            let v6: std::net::Ipv6Addr = ip.parse().unwrap();
            let index = unsafe { libc::if_nametoindex(c"en0".as_ptr()) };
            Some(Route::Server(
                std::net::SocketAddrV6::new(v6, 53, 0, index).into(),
            ))
        };
        assert_eq!(
            route_in(&resolvers, "example.com"),
            at("100.100.100.100"),
            "the default, by order"
        );
        assert_eq!(
            route_in(&resolvers, "host.taile41d51.ts.net"),
            at("100.100.100.100")
        );
        assert_eq!(
            route_in(&resolvers, "_ldap._tcp.dc.corp.example."),
            scoped("fe80::1"),
            "with its interface"
        );
        assert_eq!(route_in(&resolvers, "printer.local"), Some(Route::Mdns));
        assert_eq!(
            route_in(&resolvers, "notcorp.example"),
            at("100.100.100.100"),
            "a suffix is whole labels"
        );
    }
}

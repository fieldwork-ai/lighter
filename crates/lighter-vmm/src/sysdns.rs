//! Any record type, from the Mac's own resolver.
//!
//! Address questions go through `getaddrinfo`, which is the Mac's resolver
//! with everything configured into it: per-domain resolvers in
//! `/etc/resolver`, the scoped resolvers a VPN installs, `.local` by
//! multicast DNS. The other types (TXT, SRV, MX, PTR, CAA…) were sent raw
//! to the first nameserver in `resolv.conf`, which knows none of that: a
//! VPN's internal SRV record, or a `.local` service a container browses
//! for, was not found. `DNSServiceQueryRecord` is that same resolver for
//! any type; this asks it once and returns what it says.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::time::{Duration, Instant};

type DNSServiceRef = *mut c_void;
type QueryCallback = extern "C" fn(
    DNSServiceRef,
    u32,
    u32,
    i32,
    *const c_char,
    u16,
    u16,
    u16,
    *const c_void,
    u32,
    *mut c_void,
);

const MORE_COMING: u32 = 0x1;
const ADD: u32 = 0x2;
const RETURN_INTERMEDIATES: u32 = 0x1000;
const TIMEOUT: u32 = 0x10000;
const NO_SUCH_NAME: i32 = -65538;
const NO_SUCH_RECORD: i32 = -65554;

unsafe extern "C" {
    fn DNSServiceQueryRecord(
        sd: *mut DNSServiceRef,
        flags: u32,
        interface: u32,
        fullname: *const c_char,
        rrtype: u16,
        rrclass: u16,
        callback: QueryCallback,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(sd: DNSServiceRef) -> c_int;
    fn DNSServiceProcessResult(sd: DNSServiceRef) -> i32;
    fn DNSServiceRefDeallocate(sd: DNSServiceRef);
}

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

#[derive(Default)]
struct Collected {
    records: Vec<Record>,
    negative: Option<i32>,
    error: Option<i32>,
    batch_done: bool,
}

extern "C" fn on_record(
    _: DNSServiceRef,
    flags: u32,
    _: u32,
    error: i32,
    fullname: *const c_char,
    rrtype: u16,
    _: u16,
    rdlen: u16,
    rdata: *const c_void,
    ttl: u32,
    context: *mut c_void,
) {
    // SAFETY: the context is the `Collected` `query` passed, alive for the
    // whole query; the strings and data are valid for this call.
    let got = unsafe { &mut *context.cast::<Collected>() };
    match error {
        0 if flags & ADD != 0 => {
            let name = unsafe { CStr::from_ptr(fullname) }.to_string_lossy();
            let data = unsafe { std::slice::from_raw_parts(rdata.cast::<u8>(), rdlen as usize) };
            got.records.push(Record {
                name: name.trim_end_matches('.').to_string(),
                rtype: rrtype,
                ttl,
                rdata: data.to_vec(),
            });
        }
        0 => {}
        NO_SUCH_RECORD | NO_SUCH_NAME => got.negative = Some(error),
        e => got.error = Some(e),
    }
    if flags & MORE_COMING == 0 {
        got.batch_done = true;
    }
}

/// Asks the Mac's resolver for `name`'s records of `rtype` in `class`,
/// waiting at most `wait`. `None` when it could not say: not running,
/// timed out, refused the question.
pub fn query(name: &str, rtype: u16, class: u16, wait: Duration) -> Option<Answer> {
    let fullname = CString::new(name).ok()?;
    let mut got = Collected::default();
    let mut sd: DNSServiceRef = std::ptr::null_mut();
    // SAFETY: an out-pointer, a NUL-terminated name, a callback that lives
    // forever and a context that outlives the reference (deallocated below
    // before `got` goes).
    let error = unsafe {
        DNSServiceQueryRecord(
            &mut sd,
            RETURN_INTERMEDIATES | TIMEOUT,
            0,
            fullname.as_ptr(),
            rtype,
            class,
            on_record,
            std::ptr::addr_of_mut!(got).cast(),
        )
    };
    if error != 0 {
        return None;
    }
    // A `.local` answer comes from whoever answers, a moment apart: past
    // the first batch, a little longer for the rest. And with nothing
    // cached the resolver says "no such record" at once, before it has
    // asked anyone, so for these a negative is only what is left when no
    // one has answered in a second and a half.
    let linger = name.ends_with(".local") || name.ends_with(".local.");
    let deadline = Instant::now()
        + if linger {
            wait.min(Duration::from_millis(1500))
        } else {
            wait
        };
    let mut settled_at: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let until = match settled_at {
            Some(at) => (at + Duration::from_millis(150)).min(deadline),
            None => deadline,
        };
        if now >= until {
            break;
        }
        // SAFETY: a live reference's descriptor.
        let fd = unsafe { DNSServiceRefSockFD(sd) };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (until - now).as_millis().min(i32::MAX as u128) as c_int;
        // SAFETY: one pollfd.
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            continue;
        }
        // SAFETY: a live reference with something to read; the callback
        // writes into `got`.
        if unsafe { DNSServiceProcessResult(sd) } != 0 {
            break;
        }
        if got.error.is_some() {
            break;
        }
        if got.batch_done && !linger && (!got.records.is_empty() || got.negative.is_some()) {
            break;
        }
        if got.batch_done && linger && !got.records.is_empty() {
            settled_at.get_or_insert_with(Instant::now);
            got.batch_done = false;
        }
    }
    // SAFETY: the reference is live and not used again; no callback runs
    // after this, so `got` is ours alone.
    unsafe { DNSServiceRefDeallocate(sd) };
    if !got.records.is_empty() {
        return Some(Answer::Records(got.records));
    }
    match got.negative {
        Some(NO_SUCH_NAME) => Some(Answer::NoName),
        Some(_) => Some(Answer::NoData),
        None => None,
    }
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

    /// A question the Mac's resolver answers offline.
    #[test]
    fn the_resolver_answers_or_says_it_cannot() {
        let answer = query("localhost", 1, 1, Duration::from_secs(3));
        match answer {
            Some(Answer::Records(records)) => {
                assert!(
                    records
                        .iter()
                        .any(|r| r.rtype == 1 && r.rdata == [127, 0, 0, 1]),
                    "{records:?}"
                )
            }
            other => panic!("localhost has an A record: {other:?}"),
        }
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

//! Containers' names, answered on the link.
//!
//! The Mac resolves a `.local` name by asking every network it is on by
//! multicast DNS, the link among them. The link card answers the questions
//! for the names it holds (`web.lighter.local`) before they reach the guest,
//! as the first card answers ARP and DHCP: with the addresses, and with an
//! NSEC record saying there is nothing else, so that the Mac's resolver,
//! which asks for an IPv6 address too, has its answer at once instead of
//! waiting out mDNS's five seconds for one that never comes. Only the Mac is
//! on the link, so no other device hears a name; and a name nobody holds
//! gets no answer, since another lighter's link may hold it.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;

const MDNS_PORT: u16 = 5353;
const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_MAC: [u8; 6] = [0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb];
const TYPE_A: u16 = 1;
const ICMP6_NEIGHBOUR_SOLICIT: u8 = 135;
const ICMP6_NEIGHBOUR_ADVERT: u8 = 136;
const TYPE_AAAA: u16 = 28;
const TYPE_NSEC: u16 = 47;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
/// The cache-flush bit: these are the only records for the name.
const CACHE_FLUSH: u16 = 0x8000;
/// Short, so that a container that stops or moves is forgotten soon.
const TTL: u32 = 10;

/// The names the link answers for, and the machine's place on it.
pub struct Names {
    records: Mutex<BTreeMap<String, BTreeSet<IpAddr>>>,
    mac: [u8; 6],
    ip: Ipv4Addr,
    // The guest's IPv6 address on the link and the Mac's: the link's /64
    // is the guest's, apart from the Mac's address (`neighbour`).
    ip6: Option<(Ipv6Addr, Ipv6Addr)>,
    // Where an announcement goes: the link's socket, once the card has one.
    link: Mutex<Option<std::sync::Arc<std::os::fd::OwnedFd>>>,
}

impl Names {
    /// Answering as `ip` from `mac`: the guest's card on the link.
    pub fn new(mac: [u8; 6], ip: Ipv4Addr) -> Names {
        Names {
            records: Mutex::new(BTreeMap::new()),
            mac,
            ip,
            ip6: None,
            link: Mutex::new(None),
        }
    }

    /// The link's IPv6: the guest at `guest`, the Mac at `host`, on a /64.
    pub fn with_ipv6(mut self, guest: Ipv6Addr, host: Ipv6Addr) -> Names {
        self.ip6 = Some((guest, host));
        self
    }

    /// Where announcements go from now on.
    pub(crate) fn attach(&self, link: std::sync::Arc<std::os::fd::OwnedFd>) {
        *self.link.lock().expect("names poisoned") = Some(link);
    }

    /// Replaces every name; each lowercased, without a trailing dot. What
    /// changed is announced, so the Mac's cache follows at once rather
    /// than when its copy expires: a goodbye (TTL 0) for an address a name
    /// no longer has, and a name's new addresses.
    pub fn set(&self, records: BTreeMap<String, BTreeSet<IpAddr>>) {
        let mut held = self.records.lock().expect("names poisoned");
        // A `*.` name is answered for the names under it and never
        // announced as itself: multicast DNS has no wildcards.
        let pairs = |r: &BTreeMap<String, BTreeSet<IpAddr>>| -> BTreeSet<(String, IpAddr)> {
            r.iter()
                .filter(|(n, _)| !n.starts_with("*."))
                .flat_map(|(n, ips)| ips.iter().map(move |ip| (n.clone(), *ip)))
                .collect()
        };
        let (before, after) = (pairs(&held), pairs(&records));
        let gone: Vec<Record> = before
            .difference(&after)
            .map(|(n, ip)| Record::address(n, *ip))
            .collect();
        // A name whose addresses changed is announced whole, since each
        // record carries the cache-flush bit.
        let changed: BTreeSet<&String> = after.difference(&before).map(|(n, _)| n).collect();
        let fresh: Vec<Record> = after
            .iter()
            .filter(|(n, _)| changed.contains(n))
            .map(|(n, ip)| Record::address(n, *ip))
            .collect();
        *held = records;
        drop(held);
        let Some(link) = self.link.lock().expect("names poisoned").clone() else {
            return;
        };
        for (records, ttl) in [(gone, 0), (fresh, TTL)] {
            // A few records a message keeps each frame well under the MTU.
            for chunk in records.chunks(16) {
                let message = response(chunk, &[], ttl);
                let frame = udp_frame(
                    self.mac, MDNS_MAC, self.ip, MDNS_GROUP, MDNS_PORT, MDNS_PORT, &message,
                );
                // SAFETY: a frame buffer of its own length, on a live socket.
                unsafe {
                    libc::send(
                        std::os::fd::AsRawFd::as_raw_fd(&*link),
                        frame.as_ptr().cast(),
                        frame.len(),
                        libc::MSG_DONTWAIT,
                    );
                }
            }
        }
    }

    /// The answer to a frame from the Mac, when it is a question about a
    /// name held here.
    pub fn answer(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if let Some(advert) = self.neighbour(frame) {
            return Some(advert);
        }
        let (src, src_port, query) = mdns_query(frame)?;
        // A query's own answers, or a response: not a question.
        if query.len() < 12 || query[2] & 0x80 != 0 {
            return None;
        }
        let questions = u16::from_be_bytes([query[4], query[5]]);
        let mut answers = Vec::new();
        let mut additional = Vec::new();
        let mut unicast = true;
        let records = self.records.lock().expect("names poisoned");
        let mut at = 12;
        for _ in 0..questions {
            let (name, next) = read_name(query, at)?;
            let qtype = u16::from_be_bytes([*query.get(next)?, *query.get(next + 1)?]);
            let qclass = u16::from_be_bytes([*query.get(next + 2)?, *query.get(next + 3)?]);
            at = next + 4;
            unicast &= qclass & 0x8000 != 0;
            let Some(ips) = lookup(&records, &name.to_ascii_lowercase()) else {
                continue;
            };
            let of = |v6: bool| -> Vec<Record> {
                ips.iter()
                    .filter(|ip| ip.is_ipv6() == v6)
                    .map(|ip| Record::address(&name, *ip))
                    .collect()
            };
            // As macOS answers for its own name: the addresses asked for as
            // answers, the other family's beside them, and the NSEC that
            // says what the name has always as additional, never as an
            // answer, which is how mDNSResponder takes it as a negative
            // for a family the name lacks.
            let (asked, other) = match qtype {
                TYPE_A => (of(false), of(true)),
                TYPE_AAAA => (of(true), of(false)),
                TYPE_ANY => ([of(false), of(true)].concat(), Vec::new()),
                TYPE_NSEC => (Vec::new(), [of(false), of(true)].concat()),
                _ => continue,
            };
            answers.extend(asked);
            additional.extend(other);
            additional.push(Record::Nsec(
                name.clone(),
                ips.iter().any(IpAddr::is_ipv4),
                ips.iter().any(IpAddr::is_ipv6),
            ));
        }
        drop(records);
        if answers.is_empty() && additional.is_empty() {
            return None;
        }
        unique(&mut answers);
        additional.retain(|r| !answers.contains(r));
        unique(&mut additional);
        // A one-shot query from an ordinary port (RFC 6762's legacy
        // unicast, as lighter's own resolver asks, `sysdns::multicast`) is
        // answered straight back to that port, as an ordinary DNS reply: its
        // id, its question, no cache-flush bits.
        if src_port != MDNS_PORT {
            let id = u16::from_be_bytes([query[0], query[1]]);
            let message = response_to(id, &query[12..at], questions, &answers, &additional, TTL);
            return Some(udp_frame(
                self.mac,
                frame_src_mac(frame)?,
                self.ip,
                src,
                MDNS_PORT,
                src_port,
                &message,
            ));
        }
        let message = response(&answers, &additional, TTL);
        // A question asking for a unicast answer (QU) gets one, as RFC 6762
        // allows; otherwise the answer goes to the group, as the Mac sends.
        let (dst, dst_mac, dst_port) = if unicast {
            (src, frame_src_mac(frame)?, src_port)
        } else {
            (MDNS_GROUP, MDNS_MAC, MDNS_PORT)
        };
        Some(udp_frame(
            self.mac, dst_mac, self.ip, dst, MDNS_PORT, dst_port, &message,
        ))
    }
}

impl Names {
    /// The Mac asking who has an address on the link's /64: the guest's
    /// card has every one but the Mac's own, as `proxy_arp` has every
    /// container's IPv4 address. Answered here because Linux proxies
    /// IPv6 neighbours only one listed address at a time, and Docker's
    /// come and go. A solicitation from no address is duplicate address
    /// detection, which only the Mac does, for its own: never answered.
    fn neighbour(&self, frame: &[u8]) -> Option<Vec<u8>> {
        let (guest, host) = self.ip6?;
        if frame.get(12..14)? != [0x86, 0xdd] {
            return None;
        }
        let ip = frame.get(14..)?;
        if ip.len() < 40 + 24 || ip[6] != 58 || ip[7] != 255 {
            return None;
        }
        let src = Ipv6Addr::from(<[u8; 16]>::try_from(&ip[8..24]).ok()?);
        let icmp = &ip[40..];
        if icmp[0] != ICMP6_NEIGHBOUR_SOLICIT || src.is_unspecified() {
            return None;
        }
        let target = Ipv6Addr::from(<[u8; 16]>::try_from(&icmp[8..24]).ok()?);
        if target == host || target.segments()[..4] != guest.segments()[..4] {
            return None;
        }
        // Solicited and override, the target, our link-layer address.
        let mut advert = vec![ICMP6_NEIGHBOUR_ADVERT, 0, 0, 0, 0x60, 0, 0, 0];
        advert.extend_from_slice(&target.octets());
        advert.extend_from_slice(&[2, 1]);
        advert.extend_from_slice(&self.mac);
        let from = link_local(self.mac);
        let sum = icmp6_checksum(from, src, &advert);
        advert[2..4].copy_from_slice(&sum.to_be_bytes());
        let mut out = Vec::with_capacity(14 + 40 + advert.len());
        out.extend_from_slice(frame.get(6..12)?);
        out.extend_from_slice(&self.mac);
        out.extend_from_slice(&[0x86, 0xdd, 0x60, 0, 0, 0]);
        out.extend_from_slice(&(advert.len() as u16).to_be_bytes());
        out.extend_from_slice(&[58, 255]);
        out.extend_from_slice(&from.octets());
        out.extend_from_slice(&src.octets());
        out.extend_from_slice(&advert);
        Some(out)
    }
}

/// The link-local address a card with `mac` has (EUI-64).
fn link_local(mac: [u8; 6]) -> Ipv6Addr {
    Ipv6Addr::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([mac[0] ^ 0x02, mac[1]]),
        u16::from_be_bytes([mac[2], 0xff]),
        u16::from_be_bytes([0xfe, mac[3]]),
        u16::from_be_bytes([mac[4], mac[5]]),
    )
}

fn icmp6_checksum(src: Ipv6Addr, dst: Ipv6Addr, icmp: &[u8]) -> u16 {
    let mut sum = checksum(0, &src.octets());
    sum = checksum(sum, &dst.octets());
    sum += icmp.len() as u32 + 58;
    fold(checksum(sum, icmp))
}

fn unique(records: &mut Vec<Record>) {
    let mut seen = Vec::new();
    records.retain(|r| {
        let new = !seen.contains(r);
        if new {
            seen.push(r.clone());
        }
        new
    });
}

/// The addresses for `name`: its own, else a `*.` name above it holds it
/// (`*.myapp.local`), else it is under a container's own name
/// (`api.web.lighter.local` is `web`'s), the nearest first.
fn lookup<'a>(
    records: &'a BTreeMap<String, BTreeSet<IpAddr>>,
    name: &str,
) -> Option<&'a BTreeSet<IpAddr>> {
    if let Some(ips) = records.get(name) {
        return Some(ips);
    }
    let mut rest = name;
    while let Some((_, parent)) = rest.split_once('.') {
        if parent == "local" || parent == "lighter.local" {
            return None;
        }
        if let Some(ips) = records.get(&format!("*.{parent}")) {
            return Some(ips);
        }
        if parent.ends_with(".lighter.local")
            && let Some(ips) = records.get(parent)
        {
            return Some(ips);
        }
        rest = parent;
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Record {
    A(String, Ipv4Addr),
    Aaaa(String, Ipv6Addr),
    /// What the name has: A records, AAAA records; nothing else.
    Nsec(String, bool, bool),
}

impl Record {
    fn address(name: &str, ip: IpAddr) -> Record {
        match ip {
            IpAddr::V4(v4) => Record::A(name.to_string(), v4),
            IpAddr::V6(v6) => Record::Aaaa(name.to_string(), v6),
        }
    }
}

/// An IPv4 UDP datagram to port 5353 inside an Ethernet frame: its source,
/// source port and payload.
fn mdns_query(frame: &[u8]) -> Option<(Ipv4Addr, u16, &[u8])> {
    if frame.len() < 14 + 20 + 8 || frame[12..14] != [0x08, 0x00] {
        return None;
    }
    let ip = &frame[14..];
    let header = usize::from(ip[0] & 0x0f) * 4;
    if ip[0] >> 4 != 4 || header < 20 || ip[9] != 17 || ip.len() < header + 8 {
        return None;
    }
    // A fragment carries no whole question.
    if u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0 {
        return None;
    }
    let total = usize::from(u16::from_be_bytes([ip[2], ip[3]])).min(ip.len());
    let src = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let udp = &ip[header..total];
    if udp.len() < 8 || u16::from_be_bytes([udp[2], udp[3]]) != MDNS_PORT {
        return None;
    }
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let length = usize::from(u16::from_be_bytes([udp[4], udp[5]])).min(udp.len());
    Some((src, src_port, udp.get(8..length)?))
}

fn frame_src_mac(frame: &[u8]) -> Option<[u8; 6]> {
    frame.get(6..12)?.try_into().ok()
}

/// A name at `at`, following compression pointers, and where the field
/// after it starts.
fn read_name(message: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    for _ in 0..64 {
        let len = *message.get(at)?;
        match len {
            0 => {
                let name = labels.join(".");
                return Some((name, end.unwrap_or(at + 1)));
            }
            l if l & 0xc0 == 0xc0 => {
                let pointer = usize::from(u16::from_be_bytes([l, *message.get(at + 1)?]) & 0x3fff);
                end.get_or_insert(at + 2);
                at = pointer;
            }
            l if l < 64 => {
                let label = message.get(at + 1..at + 1 + usize::from(l))?;
                labels.push(String::from_utf8_lossy(label).into_owned());
                at += 1 + usize::from(l);
            }
            _ => return None,
        }
    }
    None
}

fn write_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.').filter(|l| !l.is_empty()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

fn response(answers: &[Record], additional: &[Record], ttl: u32) -> Vec<u8> {
    response_to(0, &[], 0, answers, additional, ttl)
}

/// A response with `id`, echoing `questions` (their bytes and count): a
/// legacy unicast reply when there are some, which carries no cache-flush
/// bits; multicast DNS's own when not.
fn response_to(
    id: u16,
    question: &[u8],
    questions: u16,
    answers: &[Record],
    additional: &[Record],
    ttl: u32,
) -> Vec<u8> {
    let flush = if questions == 0 { CACHE_FLUSH } else { 0 };
    let mut out = id.to_be_bytes().to_vec();
    out.extend_from_slice(&[0x84, 0x00]);
    out.extend_from_slice(&questions.to_be_bytes());
    out.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(additional.len() as u16).to_be_bytes());
    out.extend_from_slice(question);
    for record in answers.iter().chain(additional) {
        let owner = out.len();
        let name = match record {
            Record::A(name, _) | Record::Aaaa(name, _) | Record::Nsec(name, ..) => name,
        };
        write_name(&mut out, name);
        let (rtype, rdata) = match record {
            Record::A(_, ip) => (TYPE_A, ip.octets().to_vec()),
            Record::Aaaa(_, ip) => (TYPE_AAAA, ip.octets().to_vec()),
            // The next name is the name itself, by a pointer to it, and the
            // types it has: window 0, A in the first byte's bit 1, AAAA in
            // the fourth's bit 4.
            Record::Nsec(_, a, aaaa) => {
                let pointer = 0xc000 | owner as u16;
                let mut rdata = pointer.to_be_bytes().to_vec();
                let first = if *a { 0x40 } else { 0 };
                if *aaaa {
                    rdata.extend_from_slice(&[0, 4, first, 0, 0, 0x08]);
                } else {
                    rdata.extend_from_slice(&[0, 1, first]);
                }
                (TYPE_NSEC, rdata)
            }
        };
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&(CLASS_IN | flush).to_be_bytes());
        out.extend_from_slice(&ttl.to_be_bytes());
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(&rdata);
    }
    out
}

fn checksum(sum: u32, bytes: &[u8]) -> u32 {
    let mut sum = sum;
    for pair in bytes.chunks(2) {
        sum += u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]));
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn udp_frame(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src: Ipv4Addr,
    dst: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let mut udp = Vec::with_capacity(udp_len);
    udp.extend_from_slice(&src_port.to_be_bytes());
    udp.extend_from_slice(&dst_port.to_be_bytes());
    udp.extend_from_slice(&(udp_len as u16).to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(payload);
    let mut pseudo = checksum(0, &src.octets());
    pseudo = checksum(pseudo, &dst.octets());
    pseudo += 17 + udp_len as u32;
    let sum = match fold(checksum(pseudo, &udp)) {
        0 => 0xffff,
        s => s,
    };
    udp[6..8].copy_from_slice(&sum.to_be_bytes());

    let mut ip = vec![0x45, 0];
    ip.extend_from_slice(&((20 + udp_len) as u16).to_be_bytes());
    // No fragments; TTL 255, which mDNS receivers check for.
    ip.extend_from_slice(&[0, 0, 0x40, 0, 255, 17, 0, 0]);
    ip.extend_from_slice(&src.octets());
    ip.extend_from_slice(&dst.octets());
    let sum = fold(checksum(0, &ip));
    ip[10..12].copy_from_slice(&sum.to_be_bytes());

    let mut frame = Vec::with_capacity(14 + ip.len() + udp.len());
    frame.extend_from_slice(&dst_mac);
    frame.extend_from_slice(&src_mac);
    frame.extend_from_slice(&[0x08, 0x00]);
    frame.extend_from_slice(&ip);
    frame.extend_from_slice(&udp);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_MAC: [u8; 6] = [0x7e, 0xd6, 0x2c, 0, 0x96, 0x64];
    const GUEST_MAC: [u8; 6] = [0x76, 0x71, 0xbb, 0xf3, 0x37, 0x9d];
    const MAC: Ipv4Addr = Ipv4Addr::new(10, 211, 0, 1);
    const GUEST: Ipv4Addr = Ipv4Addr::new(10, 211, 0, 2);

    fn names() -> Names {
        let names = Names::new(GUEST_MAC, GUEST);
        names.set(BTreeMap::from([(
            "web.lighter.local".to_string(),
            BTreeSet::from([IpAddr::V4(Ipv4Addr::new(10, 211, 1, 2))]),
        )]));
        names
    }

    /// The Mac's question, as it sends it: one or more (name, type),
    /// unicast-response bit as asked.
    fn query(questions: &[(&str, u16)], qu: bool) -> Vec<u8> {
        let mut q = vec![0, 0, 0, 0];
        q.extend_from_slice(&(questions.len() as u16).to_be_bytes());
        q.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        for (name, qtype) in questions {
            write_name(&mut q, name);
            q.extend_from_slice(&qtype.to_be_bytes());
            q.extend_from_slice(&(CLASS_IN | if qu { 0x8000 } else { 0 }).to_be_bytes());
        }
        udp_frame(MAC_MAC, MDNS_MAC, MAC, MDNS_GROUP, MDNS_PORT, MDNS_PORT, &q)
    }

    /// A record a response carries: (section, name, type, rdata), the
    /// section 0 for an answer and 1 for additional.
    type Parsed = (usize, String, u16, Vec<u8>);

    /// A response frame's destination and records.
    fn parse(frame: &[u8]) -> (Ipv4Addr, Vec<Parsed>) {
        let dst = Ipv4Addr::new(frame[30], frame[31], frame[32], frame[33]);
        assert_eq!(frame[22], 255, "TTL 255");
        assert_eq!(fold(checksum(0, &frame[14..34])), 0, "IP checksum");
        let msg = &frame[42..];
        assert_eq!(msg[2] & 0x84, 0x84, "an authoritative response");
        let an = usize::from(u16::from_be_bytes([msg[6], msg[7]]));
        let ar = usize::from(u16::from_be_bytes([msg[10], msg[11]]));
        let mut at = 12;
        let mut out = Vec::new();
        for i in 0..an + ar {
            let (name, next) = read_name(msg, at).unwrap();
            let rtype = u16::from_be_bytes([msg[next], msg[next + 1]]);
            assert_eq!(
                u16::from_be_bytes([msg[next + 2], msg[next + 3]]),
                CLASS_IN | CACHE_FLUSH
            );
            let len = usize::from(u16::from_be_bytes([msg[next + 8], msg[next + 9]]));
            out.push((
                usize::from(i >= an),
                name,
                rtype,
                msg[next + 10..next + 10 + len].to_vec(),
            ));
            at = next + 10 + len;
        }
        (dst, out)
    }

    #[test]
    fn an_address_question_is_answered_with_no_ipv6_asserted() {
        let reply = names()
            .answer(&query(&[("web.lighter.local", TYPE_A)], true))
            .unwrap();
        let (dst, records) = parse(&reply);
        assert_eq!(dst, MAC, "QU: unicast to the asker");
        assert_eq!(&reply[0..6], &MAC_MAC);
        assert_eq!(
            records[0],
            (0, "web.lighter.local".into(), TYPE_A, vec![10, 211, 1, 2])
        );
        assert_eq!(records[1].2, TYPE_NSEC);
        assert_eq!(records[1].0, 1, "the NSEC as additional");
        assert_eq!(&records[1].3[2..], &[0, 1, 0x40], "A and nothing else");
    }

    #[test]
    fn an_ipv6_question_gets_the_nsec_and_case_does_not_matter() {
        let reply = names()
            .answer(&query(&[("Web.Lighter.Local", TYPE_AAAA)], false))
            .unwrap();
        let (dst, records) = parse(&reply);
        assert_eq!(dst, MDNS_GROUP, "QM: to the group");
        assert!(
            records.iter().all(|r| r.0 == 1),
            "no answers, only additional: {records:?}"
        );
        let types: Vec<u16> = records.iter().map(|r| r.2).collect();
        assert_eq!(types, [TYPE_A, TYPE_NSEC]);
        let nsec = &records[1].3;
        let pointer = usize::from(u16::from_be_bytes([nsec[0], nsec[1]]) & 0x3fff);
        let msg = &reply[42..];
        assert_eq!(
            read_name(msg, pointer).unwrap().0,
            "Web.Lighter.Local",
            "next name: its own"
        );
    }

    #[test]
    fn names_not_held_and_other_traffic_get_nothing() {
        let n = names();
        assert!(
            n.answer(&query(&[("db.lighter.local", TYPE_A)], true))
                .is_none()
        );
        assert!(
            n.answer(&query(&[("printer.local", TYPE_A)], true))
                .is_none()
        );
        let not_mdns = udp_frame(MAC_MAC, GUEST_MAC, MAC, GUEST, 40000, 53, &[0; 20]);
        assert!(n.answer(&not_mdns).is_none());
        assert!(n.answer(&[0u8; 10]).is_none());
    }

    #[test]
    fn a_question_among_others_is_answered_alone() {
        let reply = names()
            .answer(&query(
                &[
                    ("printer.local", TYPE_A),
                    ("web.lighter.local", TYPE_A),
                    ("web.lighter.local", TYPE_AAAA),
                ],
                false,
            ))
            .unwrap();
        let (_, records) = parse(&reply);
        let sections: Vec<_> = records.iter().map(|r| (r.0, r.2)).collect();
        assert_eq!(sections, [(0, TYPE_A), (1, TYPE_NSEC)], "nothing twice");
    }

    #[test]
    fn a_change_is_announced_and_a_removal_says_goodbye() {
        let n = names();
        let (near, far) = lighter_vmnet::socket_pair().unwrap();
        n.attach(std::sync::Arc::new(near));
        n.set(BTreeMap::from([(
            "web.lighter.local".to_string(),
            BTreeSet::from([IpAddr::V4(Ipv4Addr::new(10, 211, 1, 9))]),
        )]));
        let mut frames = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            // SAFETY: a buffer of its own length, on a live socket, not waiting.
            let got = unsafe {
                libc::recv(
                    std::os::fd::AsRawFd::as_raw_fd(&far),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if got <= 0 {
                break;
            }
            frames.push(buf[..got as usize].to_vec());
        }
        assert_eq!(frames.len(), 2, "a goodbye and an announcement");
        let ttl = |frame: &[u8]| {
            let msg = &frame[42..];
            let (_, next) = read_name(msg, 12).unwrap();
            (
                u32::from_be_bytes(msg[next + 4..next + 8].try_into().unwrap()),
                msg[next + 10..next + 14].to_vec(),
            )
        };
        assert_eq!(
            ttl(&frames[0]),
            (0, vec![10, 211, 1, 2]),
            "the old address, goodbye"
        );
        assert_eq!(ttl(&frames[1]), (TTL, vec![10, 211, 1, 9]), "the new one");
        assert_eq!(parse(&frames[1]).0, MDNS_GROUP);
    }

    #[test]
    fn the_udp_checksum_verifies() {
        let reply = names()
            .answer(&query(&[("web.lighter.local", TYPE_A)], true))
            .unwrap();
        let udp = &reply[34..];
        let mut sum = checksum(0, &reply[26..34]);
        sum += 17 + udp.len() as u32;
        assert_eq!(fold(checksum(sum, udp)), 0);
    }

    #[test]
    fn subdomains_and_wildcards_answer() {
        let n = Names::new(GUEST_MAC, GUEST);
        n.set(BTreeMap::from([
            (
                "web.lighter.local".to_string(),
                BTreeSet::from([IpAddr::V4(Ipv4Addr::new(10, 211, 1, 2))]),
            ),
            (
                "*.myapp.local".to_string(),
                BTreeSet::from([IpAddr::V4(Ipv4Addr::new(10, 211, 1, 3))]),
            ),
        ]));
        let a = |name: &str| {
            n.answer(&query(&[(name, TYPE_A)], true))
                .map(|r| parse(&r).1[0].3.clone())
        };
        assert_eq!(
            a("api.web.lighter.local"),
            Some(vec![10, 211, 1, 2]),
            "under a container's name"
        );
        assert_eq!(
            a("v2.api.myapp.local"),
            Some(vec![10, 211, 1, 3]),
            "under a wildcard"
        );
        assert_eq!(a("myapp.local"), None, "a wildcard is not its own parent");
        assert_eq!(a("other.lighter.local"), None);
    }

    #[test]
    fn a_name_with_both_families_answers_each_and_says_so() {
        let n = names();
        let v6: Ipv6Addr = "fd12:3456:789a::1:0:0:2".parse().unwrap();
        n.set(BTreeMap::from([(
            "web.lighter.local".to_string(),
            BTreeSet::from([IpAddr::V4(Ipv4Addr::new(10, 211, 1, 2)), IpAddr::V6(v6)]),
        )]));
        let (_, records) = parse(
            &n.answer(&query(&[("web.lighter.local", TYPE_AAAA)], true))
                .unwrap(),
        );
        assert_eq!(
            (records[0].0, records[0].2, records[0].3.clone()),
            (0, TYPE_AAAA, v6.octets().to_vec())
        );
        let nsec = records.iter().find(|r| r.2 == TYPE_NSEC).unwrap();
        assert_eq!(&nsec.3[2..], &[0, 4, 0x40, 0, 0, 0x08], "A and AAAA");
    }

    fn solicit(src: Ipv6Addr, target: Ipv6Addr) -> Vec<u8> {
        let mut icmp = vec![ICMP6_NEIGHBOUR_SOLICIT, 0, 0, 0, 0, 0, 0, 0];
        icmp.extend_from_slice(&target.octets());
        let mut f = vec![0x33, 0x33, 0xff, 0, 0, 2];
        f.extend_from_slice(&MAC_MAC);
        f.extend_from_slice(&[0x86, 0xdd, 0x60, 0, 0, 0]);
        f.extend_from_slice(&(icmp.len() as u16).to_be_bytes());
        f.extend_from_slice(&[58, 255]);
        f.extend_from_slice(&src.octets());
        f.extend_from_slice(&target.octets());
        f.extend_from_slice(&icmp);
        f
    }

    #[test]
    fn the_card_has_every_address_on_the_link_but_the_macs() {
        let host: Ipv6Addr = "fd12:3456:789a::1".parse().unwrap();
        let guest: Ipv6Addr = "fd12:3456:789a::2".parse().unwrap();
        let n = Names::new(GUEST_MAC, GUEST).with_ipv6(guest, host);
        let container: Ipv6Addr = "fd12:3456:789a:0:1::5".parse().unwrap();
        let advert = n
            .answer(&solicit(host, container))
            .expect("a container's address");
        assert_eq!(&advert[0..6], &MAC_MAC);
        let icmp = &advert[54..];
        assert_eq!((icmp[0], icmp[4]), (ICMP6_NEIGHBOUR_ADVERT, 0x60));
        assert_eq!(&icmp[8..24], &container.octets());
        assert_eq!(&icmp[26..32], &GUEST_MAC);
        let from = Ipv6Addr::from(<[u8; 16]>::try_from(&advert[22..38]).unwrap());
        assert_eq!(icmp6_checksum(from, host, icmp), 0, "the checksum verifies");
        assert!(n.answer(&solicit(host, host)).is_none(), "the Mac's own");
        assert!(
            n.answer(&solicit(Ipv6Addr::UNSPECIFIED, container))
                .is_none(),
            "duplicate address detection"
        );
        assert!(
            n.answer(&solicit(host, "fd99::5".parse().unwrap()))
                .is_none(),
            "off the link"
        );
    }

    #[test]
    fn a_one_shot_query_is_answered_straight_back() {
        let mut q = vec![0xbe, 0xef, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        write_name(&mut q, "web.lighter.local");
        q.extend_from_slice(&TYPE_A.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        let frame = udp_frame(MAC_MAC, MDNS_MAC, MAC, MDNS_GROUP, 40000, MDNS_PORT, &q);
        let reply = names().answer(&frame).unwrap();
        let ip = &reply[14..];
        assert_eq!(
            Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]),
            MAC,
            "to the asker"
        );
        assert_eq!(
            u16::from_be_bytes([reply[36], reply[37]]),
            40000,
            "at its port"
        );
        let msg = &reply[42..];
        assert_eq!(&msg[0..2], &[0xbe, 0xef], "its id");
        assert_eq!(
            u16::from_be_bytes([msg[4], msg[5]]),
            1,
            "its question echoed"
        );
        let (_, after_q) = read_name(msg, 12).unwrap();
        let (_, next) = read_name(msg, after_q + 4).unwrap();
        assert_eq!(u16::from_be_bytes([msg[next], msg[next + 1]]), TYPE_A);
        assert_eq!(
            u16::from_be_bytes([msg[next + 2], msg[next + 3]]),
            CLASS_IN,
            "no cache-flush bit"
        );
    }
}

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
use std::net::Ipv4Addr;
use std::sync::Mutex;

const MDNS_PORT: u16 = 5353;
const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_MAC: [u8; 6] = [0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb];
const TYPE_A: u16 = 1;
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
    records: Mutex<BTreeMap<String, BTreeSet<Ipv4Addr>>>,
    mac: [u8; 6],
    ip: Ipv4Addr,
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
            link: Mutex::new(None),
        }
    }

    /// Where announcements go from now on.
    pub(crate) fn attach(&self, link: std::sync::Arc<std::os::fd::OwnedFd>) {
        *self.link.lock().expect("names poisoned") = Some(link);
    }

    /// Replaces every name; each lowercased, without a trailing dot. What
    /// changed is announced, so the Mac's cache follows at once rather
    /// than when its copy expires: a goodbye (TTL 0) for an address a name
    /// no longer has, and a name's new addresses.
    pub fn set(&self, records: BTreeMap<String, BTreeSet<Ipv4Addr>>) {
        let mut held = self.records.lock().expect("names poisoned");
        let pairs = |r: &BTreeMap<String, BTreeSet<Ipv4Addr>>| -> BTreeSet<(String, Ipv4Addr)> {
            r.iter()
                .flat_map(|(n, ips)| ips.iter().map(move |ip| (n.clone(), *ip)))
                .collect()
        };
        let (before, after) = (pairs(&held), pairs(&records));
        let gone: Vec<Record> = before
            .difference(&after)
            .map(|(n, ip)| Record::A(n.clone(), *ip))
            .collect();
        // A name whose addresses changed is announced whole, since each
        // record carries the cache-flush bit.
        let changed: BTreeSet<&String> = after.difference(&before).map(|(n, _)| n).collect();
        let fresh: Vec<Record> = after
            .iter()
            .filter(|(n, _)| changed.contains(n))
            .map(|(n, ip)| Record::A(n.clone(), *ip))
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
            let Some(ips) = records.get(&name.to_ascii_lowercase()) else {
                continue;
            };
            let a = |out: &mut Vec<Record>| {
                out.extend(ips.iter().map(|ip| Record::A(name.clone(), *ip)))
            };
            // As macOS answers for its own name: the addresses asked for as
            // answers, and the NSEC that says there is nothing else always
            // as additional, never as an answer, which is how mDNSResponder
            // takes it as a negative for the other types.
            match qtype {
                TYPE_A | TYPE_ANY => a(&mut answers),
                TYPE_AAAA | TYPE_NSEC => a(&mut additional),
                _ => {}
            }
            if matches!(qtype, TYPE_A | TYPE_ANY | TYPE_AAAA | TYPE_NSEC) {
                additional.push(Record::Nsec(name.clone()));
            }
        }
        drop(records);
        if answers.is_empty() && additional.is_empty() {
            return None;
        }
        unique(&mut answers);
        additional.retain(|r| !answers.contains(r));
        unique(&mut additional);
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Record {
    A(String, Ipv4Addr),
    /// The name has an A record and nothing else.
    Nsec(String),
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
    let mut out = vec![0, 0, 0x84, 0x00, 0, 0];
    out.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(additional.len() as u16).to_be_bytes());
    for record in answers.iter().chain(additional) {
        let owner = out.len();
        let name = match record {
            Record::A(name, _) | Record::Nsec(name) => name,
        };
        write_name(&mut out, name);
        let (rtype, rdata) = match record {
            Record::A(_, ip) => (TYPE_A, ip.octets().to_vec()),
            // The next name is the name itself, by a pointer to it, and
            // the only type it has is A: window 0, one byte of bitmap.
            Record::Nsec(_) => {
                let pointer = 0xc000 | owner as u16;
                let mut rdata = pointer.to_be_bytes().to_vec();
                rdata.extend_from_slice(&[0, 1, 0x40]);
                (TYPE_NSEC, rdata)
            }
        };
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&(CLASS_IN | CACHE_FLUSH).to_be_bytes());
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
            BTreeSet::from([Ipv4Addr::new(10, 211, 1, 2)]),
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
            BTreeSet::from([Ipv4Addr::new(10, 211, 1, 9)]),
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
}

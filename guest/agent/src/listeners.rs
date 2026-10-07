//! What host-network containers listen on, for the Mac to forward.
//!
//! A container on the host network listens in the guest's own network
//! namespace, where nothing publishes it: Docker has no port bindings to
//! report, so `network_mode: host` was reachable from nothing on the Mac
//! (#60). The kernel knows every socket's cgroup, and a container's cgroup
//! is its own (`/sys/fs/cgroup/docker/<id>`), while Docker's proxies and the
//! engine are `/engine` and the agent is in the root. So "a listener in this
//! namespace whose cgroup is a container's" is exactly a host-network
//! container's service, with nothing of lighter's or Docker's in it and no
//! list of ports to keep.
//!
//! One dump per question over `NETLINK_SOCK_DIAG`: TCP in `LISTEN` and UDP
//! bound but not connected, each with its cgroup id (`INET_DIAG_CGROUP_ID`)
//! and, for v6, whether it is v6-only. One thread keeps the answer, looking
//! again whenever the doorbell (`doorbell.rs`) says a listener may have come
//! or gone, and the Mac watches it (`watch-listeners`): a server that starts
//! listening is forwarded in the time a scan and a line take, and nothing on
//! either side wakes while nothing changes.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

const NETLINK_SOCK_DIAG: libc::c_int = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const TCP_ESTABLISHED: u32 = 1;
const TCP_CLOSE: u32 = 7;
const TCP_LISTEN: u32 = 10;
const INET_DIAG_SKV6ONLY: u16 = 11;
const INET_DIAG_CGROUP_ID: u16 = 21;
/// `struct inet_diag_msg`: family, state, timer, retrans, a 48-byte id,
/// then expires, rqueue, wqueue, uid, inode.
const DIAG_MSG_LEN: usize = 72;
const CGROUP_ROOT: &str = "/sys/fs/cgroup/docker";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Proto {
    Tcp,
    Udp,
}

/// A socket as the kernel reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Socket {
    pub proto: Proto,
    pub addr: IpAddr,
    pub port: u16,
    pub v6only: bool,
    pub cgroup: u64,
}

/// A host-network container's service: where it listens, and whose.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Listener {
    pub proto: Proto,
    pub addr: IpAddr,
    pub port: u16,
    pub container: String,
}

/// The `listeners` reply: `listeners tcp 0.0.0.0 8123 <id>;udp :: 5353 <id>`,
/// or `listeners none`, or `listeners error <why>`. In LAN mode the same
/// answer is what the LAN card's firewall lets through (`lighter_lan`).
pub fn report() -> String {
    let found = current();
    if let Ok(found) = &found {
        lan_firewall(found);
    }
    match found {
        Ok(found) if found.is_empty() => "listeners none\n".into(),
        Ok(found) => {
            let entries: Vec<String> = found
                .iter()
                .map(|l| format!("{} {} {} {}", match l.proto { Proto::Tcp => "tcp", Proto::Udp => "udp" }, l.addr, l.port, l.container))
                .collect();
            format!("listeners {}\n", entries.join(";"))
        }
        Err(e) => format!("listeners error {e}\n"),
    }
}

/// The answer the keeper last published, numbered, and how many watch it.
struct Kept {
    generation: u64,
    report: String,
    watchers: usize,
}

static KEPT: Mutex<Kept> = Mutex::new(Kept { generation: 0, report: String::new(), watchers: 0 });
static CHANGED: Condvar = Condvar::new();

/// How long after a bell to look again. A listener rings as it is
/// released, before it has gone, so one look straight away and another
/// this long after the last bell.
const SETTLE: Duration = Duration::from_millis(25);
/// The least time between looks while bells keep coming, so a storm of
/// them (a host-network container's musl lookups each close an unconnected
/// socket) costs a look per this long rather than one per bell.
const GAP: Duration = Duration::from_millis(10);
/// Without the doorbell, how often to look while someone watches.
const POLL: Duration = Duration::from_secs(1);
/// How often a watch with nothing to say checks its peer is still there.
const PEER_CHECK: Duration = Duration::from_secs(10);

/// Starts the thread that keeps the answer, and the LAN card's firewall
/// with it.
pub fn keep() {
    let spawned = std::thread::Builder::new().name("listeners".into()).spawn(|| {
        let bell = match crate::doorbell::Doorbell::attach(ephemeral_low()) {
            Ok(bell) => Some(bell),
            Err(e) => {
                eprintln!("lighter-agent: no listener doorbell, looking once a second while watched: {e}");
                None
            }
        };
        publish();
        loop {
            match &bell {
                Some(bell) => {
                    if !bell.wait(if lan_pending() { 1000 } else { -1 }) {
                        publish();
                        continue;
                    }
                    publish();
                    loop {
                        let rang = bell.wait(SETTLE.as_millis() as i32);
                        publish();
                        if !rang {
                            break;
                        }
                        std::thread::sleep(GAP);
                    }
                }
                None => {
                    let kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
                    drop(CHANGED.wait_while(kept, |k| k.watchers == 0).unwrap_or_else(|p| p.into_inner()));
                    std::thread::sleep(POLL);
                    publish();
                }
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("lighter-agent: no thread to keep listeners: {e}");
    }
}

fn publish() {
    let report = report();
    let mut kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
    if kept.report != report {
        kept.report = report;
        kept.generation += 1;
        CHANGED.notify_all();
    }
}

/// `watch-listeners`: the `listeners` reply now, and again each time it
/// changes, until the peer goes. `peer_gone` says whether it has, checked
/// while there is nothing to say.
pub fn watch(out: &mut impl io::Write, peer_gone: impl Fn() -> bool) {
    let mut kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
    kept.watchers += 1;
    CHANGED.notify_all();
    let mut seen = None;
    loop {
        if seen != Some(kept.generation) {
            seen = Some(kept.generation);
            let line = kept.report.clone();
            drop(kept);
            let sent = out.write_all(line.as_bytes()).is_ok();
            kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
            if !sent {
                break;
            }
            continue;
        }
        let (again, timeout) = CHANGED.wait_timeout(kept, PEER_CHECK).unwrap_or_else(|p| p.into_inner());
        kept = again;
        if timeout.timed_out() && seen == Some(kept.generation) {
            drop(kept);
            let gone = peer_gone();
            kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
            if gone {
                break;
            }
        }
    }
    kept.watchers -= 1;
}

pub fn current() -> io::Result<Vec<Listener>> {
    let containers = containers(Path::new(CGROUP_ROOT));
    if containers.is_empty() {
        return Ok(Vec::new());
    }
    let mut sockets = Vec::new();
    for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
        sockets.extend(dump(family, libc::IPPROTO_TCP as u8, 1 << TCP_LISTEN)?);
        // A kernel without UDP's diag module answers ENOENT: its TCP
        // listeners are still worth forwarding.
        match dump(family, libc::IPPROTO_UDP as u8, 1 << TCP_CLOSE) {
            Ok(udp) => sockets.extend(udp),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) || e.raw_os_error() == Some(libc::EINVAL) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(listeners(&sockets, &containers, ephemeral_low()))
}

/// The rules last applied to the LAN card's firewall.
static LAN_APPLIED: Mutex<Option<String>> = Mutex::new(None);

/// Whether LAN mode is on and its firewall has no ports yet: init loads its
/// table after this agent starts, and until then there is nothing to fill.
fn lan_pending() -> bool {
    LAN_APPLIED.lock().unwrap_or_else(|p| p.into_inner()).is_none()
        && std::fs::read_to_string("/proc/cmdline").is_ok_and(|c| c.split_whitespace().any(|w| w.starts_with("lighter.lan=")))
}

/// The ports host-network containers listen on, as the LAN card's
/// firewall admits them. Written only when they change: one `nft` run,
/// both sets flushed and filled in one transaction.
fn lan_firewall(found: &[Listener]) {
    if !Path::new("/run/lighter-lan.nft").exists() {
        return;
    }
    let (rules, _) = lan_rules(found);
    let mut last = LAN_APPLIED.lock().unwrap_or_else(|p| p.into_inner());
    if last.as_deref() == Some(rules.as_str()) {
        return;
    }
    let applied = std::process::Command::new("nft")
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().expect("piped").write_all(rules.as_bytes())?;
            child.wait()
        });
    match applied {
        Ok(status) if status.success() => *last = Some(rules),
        other => eprintln!("lighter-agent: the LAN firewall's ports were not updated: {other:?}"),
    }
}

/// The nft input for [`lan_firewall`], and the ports in it.
fn lan_rules(found: &[Listener]) -> (String, Vec<(Proto, u16)>) {
    let mut ports: Vec<(Proto, u16)> = found
        .iter()
        .filter(|l| !l.addr.is_loopback())
        .map(|l| (l.proto, l.port))
        .collect();
    ports.sort();
    ports.dedup();
    let mut rules = String::from("flush set inet lighter_lan tcp_ports\nflush set inet lighter_lan udp_ports\n");
    for (proto, set) in [(Proto::Tcp, "tcp_ports"), (Proto::Udp, "udp_ports")] {
        let list: Vec<String> = ports.iter().filter(|(p, _)| *p == proto).map(|(_, port)| port.to_string()).collect();
        if !list.is_empty() {
            rules.push_str(&format!("add element inet lighter_lan {set} {{ {} }}\n", list.join(", ")));
        }
    }
    (rules, ports)
}

/// The `lan` reply: `lan address 192.168.50.241/24`, `lan waiting`,
/// `lan declined <the Mac's address>`, or `lan off`.
pub fn lan_report() -> String {
    match std::fs::read_to_string("/run/lighter/lan-state") {
        Ok(state) => format!("lan {}\n", state.trim()),
        Err(_) => "lan off\n".into(),
    }
}

/// The rules, apart from the kernel: a container's sockets only; UDP
/// outside the ephemeral range, where a container's own lookups bind; a
/// v6 wildcard that is not v6-only answers v4 too, so it is both.
pub fn listeners(sockets: &[Socket], containers: &HashMap<u64, String>, ephemeral_low: u16) -> Vec<Listener> {
    let mut found = Vec::new();
    for s in sockets {
        let Some(container) = containers.get(&s.cgroup) else { continue };
        if s.proto == Proto::Udp && s.port >= ephemeral_low {
            continue;
        }
        if s.port == 0 {
            continue;
        }
        found.push(Listener { proto: s.proto, addr: s.addr, port: s.port, container: container.clone() });
        if let IpAddr::V6(v6) = s.addr
            && v6.is_unspecified()
            && !s.v6only
        {
            found.push(Listener { proto: s.proto, addr: Ipv4Addr::UNSPECIFIED.into(), port: s.port, container: container.clone() });
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Every container's cgroup, and those beneath it, by cgroup id (the
/// directory's inode on cgroup2), to the container's id.
pub fn containers(root: &Path) -> HashMap<u64, String> {
    let mut map = HashMap::new();
    let Ok(entries) = std::fs::read_dir(root) else { return map };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        walk(&entry.path(), &name, 0, &mut map);
    }
    map
}

fn walk(dir: &Path, container: &str, depth: u32, map: &mut HashMap<u64, String>) {
    let Ok(meta) = std::fs::metadata(dir) else { return };
    if !meta.is_dir() {
        return;
    }
    map.insert(meta.ino(), container.to_string());
    if depth >= 4 {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&entry.path(), container, depth + 1, map);
            }
        }
    }
}

fn ephemeral_low() -> u16 {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(32768)
}

fn dump(family: u8, protocol: u8, states: u32) -> io::Result<Vec<Socket>> {
    // SAFETY: a plain socket call.
    let raw = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_SOCK_DIAG) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let timeout = libc::timeval { tv_sec: 1, tv_usec: 0 };
    // SAFETY: a timeval on a live socket.
    unsafe {
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVTIMEO, std::ptr::addr_of!(timeout).cast(),
            size_of::<libc::timeval>() as libc::socklen_t);
    }
    const REQUEST_LEN: usize = 16 + 56;
    let mut request = [0u8; REQUEST_LEN];
    request[0..4].copy_from_slice(&(REQUEST_LEN as u32).to_ne_bytes());
    request[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    request[6..8].copy_from_slice(&((libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16).to_ne_bytes());
    request[8..12].copy_from_slice(&1u32.to_ne_bytes());
    request[16] = family;
    request[17] = protocol;
    request[20..24].copy_from_slice(&states.to_ne_bytes());
    // SAFETY: a buffer of the length given, to a netlink socket.
    if unsafe { libc::send(fd.as_raw_fd(), request.as_ptr().cast(), request.len(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let proto = if protocol == libc::IPPROTO_TCP as u8 { Proto::Tcp } else { Proto::Udp };
    let mut sockets = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: a buffer of the length given.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(sockets);
        }
        match parse(&buf[..n as usize], proto, &mut sockets) {
            Parsed::More => {}
            Parsed::Done => return Ok(sockets),
            Parsed::Error(errno) => return Err(io::Error::from_raw_os_error(errno)),
        }
    }
}

enum Parsed {
    More,
    Done,
    Error(i32),
}

/// One `recv` of netlink messages, each an `inet_diag_msg` and its
/// attributes.
fn parse(mut buf: &[u8], proto: Proto, out: &mut Vec<Socket>) -> Parsed {
    while buf.len() >= 16 {
        let len = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes(buf[4..6].try_into().unwrap());
        if len < 16 || len > buf.len() {
            return Parsed::Done;
        }
        let body = &buf[16..len];
        match kind {
            NLMSG_DONE => return Parsed::Done,
            NLMSG_ERROR => {
                let errno = body.get(0..4).map(|b| i32::from_ne_bytes(b.try_into().unwrap())).unwrap_or(0);
                return if errno == 0 { Parsed::Done } else { Parsed::Error(-errno) };
            }
            SOCK_DIAG_BY_FAMILY => {
                if let Some(s) = socket(body, proto) {
                    out.push(s);
                }
            }
            _ => {}
        }
        let next = (len + 3) & !3;
        if next >= buf.len() {
            break;
        }
        buf = &buf[next..];
    }
    Parsed::More
}

fn socket(body: &[u8], proto: Proto) -> Option<Socket> {
    if body.len() < DIAG_MSG_LEN {
        return None;
    }
    let family = body[0];
    let state = body[1] as u32;
    if proto == Proto::Udp && state == TCP_ESTABLISHED {
        return None;
    }
    let port = u16::from_be_bytes([body[4], body[5]]);
    let src = &body[8..24];
    let addr: IpAddr = if family == libc::AF_INET as u8 {
        Ipv4Addr::new(src[0], src[1], src[2], src[3]).into()
    } else {
        Ipv6Addr::from(<[u8; 16]>::try_from(src).ok()?).into()
    };
    let (mut v6only, mut cgroup) = (false, 0u64);
    let mut attrs = &body[DIAG_MSG_LEN..];
    while attrs.len() >= 4 {
        let alen = u16::from_ne_bytes([attrs[0], attrs[1]]) as usize;
        let atype = u16::from_ne_bytes([attrs[2], attrs[3]]) & 0x3fff;
        if alen < 4 || alen > attrs.len() {
            break;
        }
        let value = &attrs[4..alen];
        match atype {
            INET_DIAG_SKV6ONLY => v6only = value.first().is_some_and(|v| *v != 0),
            INET_DIAG_CGROUP_ID if value.len() >= 8 => cgroup = u64::from_ne_bytes(value[0..8].try_into().unwrap()),
            _ => {}
        }
        let next = (alen + 3) & !3;
        if next >= attrs.len() {
            break;
        }
        attrs = &attrs[next..];
    }
    Some(Socket { proto, addr, port, v6only, cgroup })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sock(proto: Proto, addr: &str, port: u16, v6only: bool, cgroup: u64) -> Socket {
        Socket { proto, addr: addr.parse().unwrap(), port, v6only, cgroup }
    }

    fn containers() -> HashMap<u64, String> {
        HashMap::from([(7, "a".repeat(64)), (9, "b".repeat(64))])
    }

    #[test]
    fn only_a_containers_sockets_are_its_services() {
        let found = listeners(
            &[
                sock(Proto::Tcp, "0.0.0.0", 8123, false, 7),
                sock(Proto::Tcp, "0.0.0.0", 8555, false, 3), // Docker's proxy, /engine
                sock(Proto::Tcp, "0.0.0.0", 15201, false, 1), // the agent, the root
            ],
            &containers(),
            32768,
        );
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].port, found[0].container.as_str()), (8123, "a".repeat(64).as_str()));
    }

    #[test]
    fn a_dual_stack_wildcard_is_both_families_and_a_v6_only_one_is_not() {
        let found = listeners(
            &[sock(Proto::Tcp, "::", 8123, false, 7), sock(Proto::Tcp, "::", 9000, true, 9)],
            &containers(),
            32768,
        );
        let ports: Vec<(String, u16)> = found.iter().map(|l| (l.addr.to_string(), l.port)).collect();
        assert_eq!(ports, [("0.0.0.0".to_string(), 8123), ("::".to_string(), 8123), ("::".to_string(), 9000)]);
    }

    #[test]
    fn a_containers_own_lookups_are_not_services() {
        let found = listeners(
            &[sock(Proto::Udp, "0.0.0.0", 5353, false, 7), sock(Proto::Udp, "0.0.0.0", 41234, false, 7)],
            &containers(),
            32768,
        );
        assert_eq!(found.iter().map(|l| l.port).collect::<Vec<_>>(), [5353]);
    }

    #[test]
    fn the_lan_firewall_admits_what_is_reachable() {
        let l = |proto, addr: &str, port| Listener { proto, addr: addr.parse().unwrap(), port, container: "a".into() };
        let (rules, ports) = lan_rules(&[
            l(Proto::Tcp, "0.0.0.0", 8123),
            l(Proto::Tcp, "::", 8123),
            l(Proto::Udp, "0.0.0.0", 5353),
            l(Proto::Tcp, "127.0.0.1", 9000),
        ]);
        assert_eq!(ports, [(Proto::Tcp, 8123), (Proto::Udp, 5353)], "loopback is not the LAN's");
        assert!(rules.contains("add element inet lighter_lan tcp_ports { 8123 }"));
        assert!(rules.contains("add element inet lighter_lan udp_ports { 5353 }"));
        let (rules, _) = lan_rules(&[]);
        assert!(!rules.contains("add element"), "an empty set is flushed, not filled");
    }

    #[test]
    fn a_reply_parses_into_sockets() {
        // One inet_diag_msg for 0.0.0.0:8123, with v6only and cgroup
        // attributes, then NLMSG_DONE.
        let mut msg = vec![0u8; DIAG_MSG_LEN];
        msg[0] = libc::AF_INET as u8;
        msg[1] = TCP_LISTEN as u8;
        msg[4..6].copy_from_slice(&8123u16.to_be_bytes());
        let mut attr = |kind: u16, value: &[u8]| {
            let len = 4 + value.len();
            msg.extend_from_slice(&(len as u16).to_ne_bytes());
            msg.extend_from_slice(&kind.to_ne_bytes());
            msg.extend_from_slice(value);
            while msg.len() % 4 != 0 {
                msg.push(0);
            }
        };
        attr(INET_DIAG_SKV6ONLY, &[0]);
        attr(INET_DIAG_CGROUP_ID, &42u64.to_ne_bytes());
        let mut buf = Vec::new();
        for (kind, body) in [(SOCK_DIAG_BY_FAMILY, msg), (NLMSG_DONE, vec![0u8; 4])] {
            buf.extend_from_slice(&((16 + body.len()) as u32).to_ne_bytes());
            buf.extend_from_slice(&kind.to_ne_bytes());
            buf.extend_from_slice(&[0u8; 10]);
            buf.extend_from_slice(&body);
        }
        let mut out = Vec::new();
        assert!(matches!(parse(&buf, Proto::Tcp, &mut out), Parsed::Done));
        assert_eq!(out, [sock(Proto::Tcp, "0.0.0.0", 8123, false, 42)]);
    }

    /// A watcher writing lines into a channel, refusing after `left`.
    struct Lines(std::sync::mpsc::Sender<String>, usize);

    impl io::Write for Lines {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.1 == 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            self.1 -= 1;
            self.0.send(String::from_utf8_lossy(buf).into()).unwrap();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn change(report: &str) {
        let mut kept = KEPT.lock().unwrap();
        kept.report = report.into();
        kept.generation += 1;
        CHANGED.notify_all();
    }

    #[test]
    fn a_watch_hears_the_answer_now_and_each_change_until_its_peer_goes() {
        change("listeners none\n");
        let (tx, rx) = std::sync::mpsc::channel();
        let watching = std::thread::spawn(move || watch(&mut Lines(tx, 2), || false));
        assert_eq!(rx.recv().unwrap(), "listeners none\n");
        change("listeners tcp 0.0.0.0 8123 abc\n");
        assert_eq!(rx.recv().unwrap(), "listeners tcp 0.0.0.0 8123 abc\n");
        // The peer has gone: the next line is refused, and the watch ends.
        change("listeners none\n");
        watching.join().unwrap();
        assert_eq!(KEPT.lock().unwrap().watchers, 0);
    }
}
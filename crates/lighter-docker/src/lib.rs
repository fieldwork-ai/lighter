//! Keeping host port forwards in step with what Docker has published.
//!
//! # The secret this crate keeps
//!
//! What Docker's API says about published ports, and nothing else. It does not
//! know what a forward *is* — that is [`PortMapper`], which the VMM implements
//! over its network sidecar — so the machinery below can be tested against a
//! recorded API response with no VM anywhere near it.
//!
//! # Why this exists at all
//!
//! `docker run -p 15434:5432` publishes a port inside the guest. On a Linux
//! host that is the end of the story; on macOS the port is inside a virtual
//! machine, and something has to notice and open the matching door on the host.
//! Docker publishes at container start, so the set of forwards is never known
//! in advance and cannot be passed to anything at boot.
//!
//! # Reconciliation, not bookkeeping
//!
//! Every event triggers the same operation: ask Docker what is published now,
//! compare with what is forwarded now, and fix the difference. Tracking deltas
//! would be less work per event and would drift the first time an event was
//! missed — and events *are* missed, because the stream drops and the daemon
//! restarts. A reconciler recovers from that by construction.

pub mod http;

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Somewhere to put a forward. Implemented by the VMM over its network stack.
///
/// One port, not two. The host port and the guest port of a published Docker
/// port are always the same number, and a two-port version invites exactly
/// the mistake described on [`Published`].
pub trait PortMapper: Send + Sync {
    fn expose(&self, published: Published) -> Result<(), String>;
    fn unexpose(&self, published: Published) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Proto {
    Tcp,
    Udp,
}

/// One binding Docker has published on the guest: the address it bound,
/// the port, the protocol. `-p 8080:80` is two of these, `0.0.0.0` and
/// `::`; `-p 127.0.0.1:8080:80` is one; `-p 1053:53/udp` is UDP.
///
/// One port, deliberately. `docker run -p 15434:5432` makes Docker listen on
/// the *guest's* 15434 and forward into the container's network namespace
/// itself; 5432 exists only inside that namespace. So the forward this crate
/// asks for is host 15434 to guest 15434, and `PrivatePort` is not part of it.
///
/// Forwarding to the private port instead produces exactly what you would
/// expect and is maddening to diagnose: the host side accepts the forward, the host
/// port opens, and every connection to it hangs, because nothing in the guest
/// is listening on the container's internal port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Published {
    pub addr: IpAddr,
    pub port: u16,
    pub proto: Proto,
}

impl std::fmt::Display for Published {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let proto = match self.proto {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        };
        write!(
            f,
            "{proto} {}",
            std::net::SocketAddr::new(self.addr, self.port)
        )
    }
}

/// Extracts the published bindings from a `GET /containers/json` response.
///
/// The shape is `[{ "Ports": [{ "PrivatePort": 5432, "PublicPort": 15434,
/// "Type": "tcp", "IP": "0.0.0.0" }] }]`. Entries without a `PublicPort` are
/// exposed-but-not-published and are deliberately skipped: publishing is the
/// user asking for a door, and opening one they did not ask for would put a
/// container on the host's network by surprise.
pub fn published_ports(containers: &serde_json::Value) -> HashSet<Published> {
    let mut ports = HashSet::new();
    let Some(list) = containers.as_array() else {
        return ports;
    };

    for container in list {
        let Some(entries) = container.get("Ports").and_then(|p| p.as_array()) else {
            continue;
        };
        for entry in entries {
            let proto = match entry.get("Type").and_then(|t| t.as_str()) {
                Some("tcp") => Proto::Tcp,
                Some("udp") => Proto::Udp,
                // SCTP is not carried; nothing that could carry it exists here.
                _ => continue,
            };
            let Some(public) = entry.get("PublicPort").and_then(|p| p.as_u64()) else {
                continue;
            };
            let Ok(port) = u16::try_from(public) else {
                continue;
            };
            // Docker names the address it bound in the guest. Every
            // interface is what a plain `-p` means, and what a missing or
            // unreadable address is taken to mean too.
            let addr = entry
                .get("IP")
                .and_then(|ip| ip.as_str())
                .and_then(|ip| ip.parse().ok())
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
            ports.insert(Published { addr, port, proto });
        }
    }
    ports
}

/// What a host-network container listens on, as the guest agent reports
/// it (`watch-listeners` on its control channel).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostListener {
    pub proto: Proto,
    pub addr: IpAddr,
    pub port: u16,
    pub container: String,
}

/// Where host-network containers' listeners come from: the guest's agent,
/// over the machine's control channel.
pub trait HostListeners: Send + Sync {
    /// Says what they listen on, at once and each time it changes, until the
    /// source goes away (an error) or `stop` says to.
    fn watch(
        &self,
        stop: &dyn Fn() -> bool,
        heard: &mut dyn FnMut(Result<Vec<HostListener>, String>),
    ) -> Result<(), String>;
}

/// Parses the agent's reply: `listeners tcp 0.0.0.0 8123 <id>;udp :: 5353
/// <id>`, `listeners none`, or `listeners error <why>`.
pub fn parse_listeners(reply: &str) -> Result<Vec<HostListener>, String> {
    let rest = reply
        .trim()
        .strip_prefix("listeners")
        .ok_or_else(|| format!("unexpected reply: {reply}"))?
        .trim();
    if rest == "none" || rest.is_empty() {
        return Ok(Vec::new());
    }
    if let Some(why) = rest.strip_prefix("error") {
        return Err(why.trim().to_string());
    }
    rest.split(';')
        .map(|entry| {
            let mut words = entry.split_whitespace();
            let proto = match words.next() {
                Some("tcp") => Proto::Tcp,
                Some("udp") => Proto::Udp,
                other => return Err(format!("unknown protocol {other:?}")),
            };
            let addr = words
                .next()
                .and_then(|a| a.parse().ok())
                .ok_or_else(|| format!("bad address in {entry:?}"))?;
            let port = words
                .next()
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| format!("bad port in {entry:?}"))?;
            let container = words.next().unwrap_or_default().to_string();
            Ok(HostListener {
                proto,
                addr,
                port,
                container,
            })
        })
        .collect()
}

/// The containers in a `GET /containers/json` response that use the host
/// network, by id.
pub fn host_network_containers(containers: &serde_json::Value) -> HashSet<String> {
    containers
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| {
            c.get("HostConfig")
                .and_then(|h| h.get("NetworkMode"))
                .and_then(|m| m.as_str())
                == Some("host")
        })
        .filter_map(|c| c.get("Id").and_then(|id| id.as_str()).map(String::from))
        .collect()
}

/// Where a host-network listener bound to `addr` is forwarded from: every
/// interface for one on every interface, or on the guest's own address
/// (`own`), which is what reaching the guest means; loopback for one on
/// loopback. A bind anywhere else (a Docker bridge, the LAN card) is not
/// the Mac's to forward.
pub fn forwardable(addr: IpAddr, own: &[IpAddr]) -> Option<IpAddr> {
    match addr {
        a if a.is_unspecified() || a.is_loopback() => Some(a),
        a if own.contains(&a) => Some(match a {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        }),
        _ => None,
    }
}

/// UDP ports a host-network container's listener is never forwarded from.
/// 5353 is mDNS: macOS's mDNSResponder holds it on every Mac, so the forward
/// can never open, and Home Assistant (whose zeroconf binds it) was reported
/// as a port lighter could not forward for as long as it ran. mDNS is
/// multicast on the network the container is on; LAN mode is how it reaches
/// the Mac's.
const NEVER_FORWARDED_UDP: [u16; 1] = [5353];

/// What host-network containers listening as `listeners` add to what is
/// forwarded: the listeners of `containers` (the ones on the host network),
/// at the addresses [`forwardable`] allows.
pub fn host_published(
    listeners: &[HostListener],
    containers: &HashSet<String>,
    own: &[IpAddr],
) -> HashSet<Published> {
    listeners
        .iter()
        .filter(|l| containers.contains(&l.container))
        .filter(|l| !(l.proto == Proto::Udp && NEVER_FORWARDED_UDP.contains(&l.port)))
        .filter_map(|l| {
            forwardable(l.addr, own).map(|addr| Published {
                addr,
                port: l.port,
                proto: l.proto,
            })
        })
        .collect()
}

/// A published port that could not be forwarded, and why. Every address it
/// was published on is left closed (see [`apply`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unforwarded {
    pub proto: Proto,
    pub port: u16,
    pub addrs: Vec<IpAddr>,
    pub reason: String,
}

/// What is forwarded, and what could not be.
#[derive(Default)]
struct Forwards {
    /// What we have opened. The host port is the identity of a forward.
    forwarded: HashSet<Published>,
    failed: BTreeMap<(Proto, u16), Unforwarded>,
}

/// The ports the watcher could not forward, for whoever reports on the
/// machine (`lighter status`, `lighter doctor`): a failure that only reached
/// `machine.log` left a container reported as published and healthy with
/// nothing listening on the Mac.
#[derive(Clone, Default)]
pub struct PortHealth(Arc<Mutex<Forwards>>);

impl PortHealth {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn unforwarded(&self) -> Vec<Unforwarded> {
        let forwards = self.0.lock().expect("port health poisoned");
        forwards.failed.values().cloned().collect()
    }
}

/// How long the watcher waits before trying a failed forward again, at first
/// and at most. A port is most often refused because something else on the
/// Mac holds it, and that clears on its own schedule, not on a container's.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MOST: Duration = Duration::from_secs(30);

/// Watches Docker and keeps the host's forwards matching it.
pub struct PortWatcher;

/// What the watcher's threads share: where Docker is, where forwards go,
/// what is recorded about them, and what host-network containers were last
/// heard to listen on, with the guest's own addresses they may bind.
struct Watched {
    socket: std::path::PathBuf,
    mapper: Arc<dyn PortMapper>,
    health: PortHealth,
    /// Kept while the agent cannot be reached, so that a moment without it
    /// does not withdraw their forwards.
    heard: Mutex<Vec<HostListener>>,
    own: Vec<IpAddr>,
    /// Held for a whole reconcile, from asking Docker to applying: three
    /// threads reconcile, and one that asked before another and applied
    /// after it would put back what the other had just changed.
    reconciling: Mutex<()>,
}

impl PortWatcher {
    /// Starts a thread that reconciles now and on every container event, one
    /// that tries a failed forward again until it opens, and, given where
    /// host-network containers' listeners come from, one that reconciles
    /// each time they change.
    ///
    /// The handle ends the watching: the event stream is dropped and not
    /// reopened, which is what lets dockerd exit at once when the machine
    /// is being stopped.
    pub fn start(
        socket: &Path,
        mapper: Arc<dyn PortMapper>,
        health: PortHealth,
    ) -> std::io::Result<Arc<http::Stop>> {
        Self::start_with(socket, mapper, health, None, Vec::new())
    }

    /// [`PortWatcher::start`], forwarding host-network containers' listeners
    /// too: `listeners` reports them, and `own` is the guest's own
    /// addresses, a bind to which counts as every interface.
    pub fn start_with(
        socket: &Path,
        mapper: Arc<dyn PortMapper>,
        health: PortHealth,
        listeners: Option<Arc<dyn HostListeners>>,
        own: Vec<IpAddr>,
    ) -> std::io::Result<Arc<http::Stop>> {
        let stop = http::Stop::new();
        let (events, retries, heard) = (Arc::clone(&stop), Arc::clone(&stop), Arc::clone(&stop));
        let watched = Arc::new(Watched {
            socket: socket.to_path_buf(),
            mapper,
            health,
            heard: Mutex::new(Vec::new()),
            own,
            reconciling: Mutex::new(()),
        });
        let (again, listening) = (Arc::clone(&watched), Arc::clone(&watched));
        if let Some(source) = listeners {
            std::thread::Builder::new()
                .name("docker-ports-listeners".into())
                .spawn(move || Self::listen(&listening, source.as_ref(), &heard))?;
        }
        std::thread::Builder::new()
            .name("docker-ports".into())
            .spawn(move || Self::watch(&watched, &events))?;
        std::thread::Builder::new()
            .name("docker-ports-retry".into())
            .spawn(move || Self::retry(&again, &retries))?;
        Ok(stop)
    }

    fn watch(watched: &Watched, stop: &http::Stop) {
        // Filters to container events only. URL-encoded because it is a JSON
        // document in a query parameter.
        const EVENTS: &str = "/events?filters=%7B%22type%22%3A%5B%22container%22%5D%7D";

        loop {
            // Reconcile before watching, not after: containers may already be
            // running from a previous session, and a watcher that only reacted
            // to events would never open their doors.
            reconcile(watched);

            // Reconciling INSIDE the callback is the whole point. Setting a
            // flag and acting on it after the call returns looks equivalent and
            // is not: a healthy event stream never returns, so the forwards
            // would only ever be fixed up when Docker went away.
            let result = http::stream_json(&watched.socket, EVENTS, Some(stop), |event| {
                let status = event.get("status").and_then(|s| s.as_str()).unwrap_or("");
                // Only lifecycle transitions can change what is published.
                // Reconciling on every exec_start would be correct and noisy.
                if matches!(status, "start" | "die" | "destroy" | "pause" | "unpause") {
                    reconcile(watched);
                }
            });

            if let Err(e) = result {
                tracing::debug!(%e, "docker event stream ended");
            }
            if stop.asked() {
                return;
            }

            // The stream ended: the daemon restarted, the socket went away, or
            // the guest is not up yet. Sleeping before reconnecting keeps a
            // daemon that is down from turning into a spin.
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    /// Reconciles each time host-network containers' listeners change: a
    /// server starts listening when it is ready, which no container event
    /// marks. Watched again after a pause if the watch ends, which it does
    /// while the guest's agent restarts.
    fn listen(watched: &Watched, source: &dyn HostListeners, stop: &http::Stop) {
        loop {
            let ended = source.watch(&|| stop.asked(), &mut |answer| match answer {
                Ok(listeners) => {
                    *watched.heard.lock().expect("listeners poisoned") = listeners;
                    reconcile(watched);
                }
                Err(e) => tracing::debug!(%e, "the agent could not say what host-network containers listen on"),
            });
            if stop.asked() {
                return;
            }
            if let Err(e) = ended {
                tracing::debug!(%e, "the host-network listener watch ended");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Tries failed forwards again on a backoff. Without it a port refused
    /// once, because Tailscale Serve or another user's process held it on one
    /// of the Mac's addresses, stayed closed after the conflict cleared until
    /// some unrelated container happened to start or stop.
    fn retry(watched: &Watched, stop: &http::Stop) {
        let mut pause = RETRY_FIRST;
        loop {
            let until = std::time::Instant::now() + pause;
            while std::time::Instant::now() < until {
                if stop.asked() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let health = &watched.health;
            if health.unforwarded().is_empty() {
                pause = RETRY_FIRST;
                continue;
            }
            reconcile(watched);
            pause = if health.unforwarded().is_empty() {
                RETRY_FIRST
            } else {
                (pause * 2).min(RETRY_MOST)
            };
        }
    }
}

/// Makes the host's forwards match what Docker currently publishes, and
/// what host-network containers listen on.
fn reconcile(watched: &Watched) {
    let _one = watched.reconciling.lock().expect("reconcile poisoned");
    let containers = match http::get_json(&watched.socket, "/containers/json") {
        Ok(value) => value,
        Err(e) => {
            tracing::debug!(%e, "could not list containers");
            return;
        }
    };
    let mut desired = published_ports(&containers);
    let host = host_network_containers(&containers);
    let heard = watched.heard.lock().expect("listeners poisoned").clone();
    desired.extend(host_published(&heard, &host, &watched.own));
    let mut forwards = watched.health.0.lock().expect("port health poisoned");
    apply(&desired, watched.mapper.as_ref(), &mut forwards);
}

/// Brings `forwards` to `desired`, a port at a time: every address a port is
/// published on is forwarded, or none is. `0.0.0.0` and `[::]` bind
/// separately on the Mac, and one can be refused while the other opens; a
/// port answering on `::1` but not on `127.0.0.1`, nor to the containers'
/// `host.docker.internal`, is much harder to diagnose than one that is down.
fn apply(desired: &HashSet<Published>, mapper: &dyn PortMapper, forwards: &mut Forwards) {
    for gone in forwards
        .forwarded
        .difference(desired)
        .copied()
        .collect::<Vec<_>>()
    {
        match mapper.unexpose(gone) {
            Ok(()) => {
                tracing::info!(published = %gone, "port forward withdrawn");
                forwards.forwarded.remove(&gone);
            }
            Err(e) => tracing::warn!(published = %gone, %e, "could not withdraw a forward"),
        }
    }

    let mut ports: BTreeMap<(Proto, u16), Vec<Published>> = BTreeMap::new();
    for published in desired {
        ports
            .entry((published.proto, published.port))
            .or_default()
            .push(*published);
    }
    forwards.failed.retain(|key, _| ports.contains_key(key));

    for (key, mut members) in ports {
        members.sort();
        let missing: Vec<Published> = members
            .iter()
            .filter(|m| !forwards.forwarded.contains(m))
            .copied()
            .collect();
        if missing.is_empty() {
            continue;
        }
        let mut opened = Vec::new();
        let mut refused = None;
        for published in missing {
            match mapper.expose(published) {
                Ok(()) => opened.push(published),
                Err(e) => {
                    refused = Some(format!("{published}: {e}"));
                    break;
                }
            }
        }
        match refused {
            None => {
                for published in opened {
                    tracing::info!(published = %published, "port forwarded");
                    forwards.forwarded.insert(published);
                }
                if let Some(was) = forwards.failed.remove(&key) {
                    tracing::info!(port = was.port, "a port that could not be forwarded now is");
                }
            }
            Some(reason) => {
                let open: Vec<Published> = members
                    .iter()
                    .filter(|m| forwards.forwarded.contains(m))
                    .copied()
                    .chain(opened)
                    .collect();
                for published in open {
                    let _ = mapper.unexpose(published);
                    forwards.forwarded.remove(&published);
                }
                let failure = Unforwarded {
                    proto: key.0,
                    port: key.1,
                    addrs: members.iter().map(|m| m.addr).collect(),
                    reason,
                };
                // Once per failure, not once per retry: the same refusal
                // every thirty seconds says nothing new.
                if forwards.failed.get(&key) != Some(&failure) {
                    tracing::warn!(port = key.1, reason = %failure.reason, "could not forward a port; retrying");
                }
                forwards.failed.insert(key, failure);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mapper that refuses the bindings it is told to, and remembers what
    /// is open.
    #[derive(Default)]
    struct Mapper {
        refuse: Mutex<HashSet<Published>>,
        open: Mutex<HashSet<Published>>,
    }

    impl PortMapper for Mapper {
        fn expose(&self, published: Published) -> Result<(), String> {
            if self.refuse.lock().unwrap().contains(&published) {
                return Err("Address already in use (os error 48)".into());
            }
            self.open.lock().unwrap().insert(published);
            Ok(())
        }
        fn unexpose(&self, published: Published) -> Result<(), String> {
            self.open.lock().unwrap().remove(&published);
            Ok(())
        }
    }

    /// A port refused on one family is open on neither, recorded with its
    /// reason, and opens on both once the conflict clears; other ports are
    /// not held back by it. Docker Desktop fails such a container's start;
    /// lighter left `[::]:9000` answering and `0.0.0.0:9000` closed.
    #[test]
    fn a_port_is_forwarded_on_every_address_or_on_none() {
        let mapper = Mapper::default();
        let mut forwards = Forwards::default();
        let desired = HashSet::from([
            tcp("0.0.0.0", 9000),
            tcp("::", 9000),
            tcp("0.0.0.0", 9001),
            tcp("::", 9001),
        ]);
        mapper.refuse.lock().unwrap().insert(tcp("0.0.0.0", 9000));

        apply(&desired, &mapper, &mut forwards);
        assert_eq!(
            *mapper.open.lock().unwrap(),
            HashSet::from([tcp("0.0.0.0", 9001), tcp("::", 9001)]),
            "9000 must not be left half open"
        );
        let failed: Vec<_> = forwards.failed.values().cloned().collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].port, 9000);
        assert_eq!(failed[0].addrs.len(), 2);
        assert!(failed[0].reason.contains("Address already in use"));

        mapper.refuse.lock().unwrap().clear();
        apply(&desired, &mapper, &mut forwards);
        assert_eq!(*mapper.open.lock().unwrap(), desired);
        assert!(
            forwards.failed.is_empty(),
            "a port that opened is no longer failing"
        );
    }

    /// The v6 binding opens first in no order the code relies on: a refusal
    /// of `[::]` after `0.0.0.0` opened closes `0.0.0.0` again.
    #[test]
    fn a_refused_second_family_closes_the_first() {
        let mapper = Mapper::default();
        let mut forwards = Forwards::default();
        let desired = HashSet::from([tcp("0.0.0.0", 9000), tcp("::", 9000)]);
        mapper.refuse.lock().unwrap().insert(tcp("::", 9000));
        apply(&desired, &mapper, &mut forwards);
        assert!(mapper.open.lock().unwrap().is_empty());
        assert!(forwards.forwarded.is_empty());
    }

    /// A failure is about a port Docker publishes; once its container is
    /// gone there is nothing to report or to retry.
    #[test]
    fn a_failure_ends_with_its_container() {
        let mapper = Mapper::default();
        let mut forwards = Forwards::default();
        mapper.refuse.lock().unwrap().insert(tcp("0.0.0.0", 9000));
        apply(
            &HashSet::from([tcp("0.0.0.0", 9000)]),
            &mapper,
            &mut forwards,
        );
        assert_eq!(forwards.failed.len(), 1);
        apply(&HashSet::new(), &mapper, &mut forwards);
        assert!(forwards.failed.is_empty());
    }

    fn containers(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    fn tcp(addr: &str, port: u16) -> Published {
        Published {
            addr: addr.parse().unwrap(),
            port,
            proto: Proto::Tcp,
        }
    }

    #[test]
    fn the_agents_listeners_reply_parses() {
        let id = "a".repeat(64);
        let reply = format!("listeners tcp 0.0.0.0 8123 {id};udp :: 5353 {id}\n");
        let parsed = parse_listeners(&reply).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[1],
            HostListener {
                proto: Proto::Udp,
                addr: "::".parse().unwrap(),
                port: 5353,
                container: id
            }
        );
        assert_eq!(parse_listeners("listeners none\n").unwrap(), Vec::new());
        assert_eq!(
            parse_listeners("listeners error ENOENT\n").unwrap_err(),
            "ENOENT"
        );
        assert!(parse_listeners("pong").is_err());
    }

    #[test]
    fn host_network_containers_are_found_by_their_network_mode() {
        let found = host_network_containers(&containers(
            r#"[{"Id": "aaa", "HostConfig": {"NetworkMode": "host"}},
                {"Id": "bbb", "HostConfig": {"NetworkMode": "bridge"}},
                {"Id": "ccc"}]"#,
        ));
        assert_eq!(found, HashSet::from(["aaa".to_string()]));
    }

    #[test]
    fn only_a_wildcard_loopback_or_the_guests_own_address_is_forwarded() {
        let own: Vec<IpAddr> = vec![
            "192.168.127.2".parse().unwrap(),
            "fd6c:6967:6874::2".parse().unwrap(),
        ];
        let f = |a: &str| forwardable(a.parse().unwrap(), &own).map(|a| a.to_string());
        assert_eq!(f("0.0.0.0").as_deref(), Some("0.0.0.0"));
        assert_eq!(f("::").as_deref(), Some("::"));
        assert_eq!(f("127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(f("192.168.127.2").as_deref(), Some("0.0.0.0"));
        assert_eq!(f("fd6c:6967:6874::2").as_deref(), Some("::"));
        assert_eq!(f("172.17.0.1"), None, "a Docker bridge");
        assert_eq!(f("192.168.50.241"), None, "the LAN card");
    }

    #[test]
    fn only_host_network_containers_listeners_are_published() {
        let l = |c: &str, port| HostListener {
            proto: Proto::Tcp,
            addr: "0.0.0.0".parse().unwrap(),
            port,
            container: c.into(),
        };
        let published = host_published(
            &[l("host", 8123), l("bridged", 8000)],
            &HashSet::from(["host".to_string()]),
            &[],
        );
        assert_eq!(published, HashSet::from([tcp("0.0.0.0", 8123)]));
    }

    /// Home Assistant's zeroconf: macOS holds 5353 itself, so it is left
    /// alone, and a UDP service on any other port is forwarded.
    #[test]
    fn a_host_network_mdns_socket_is_not_forwarded() {
        let udp = |port| HostListener {
            proto: Proto::Udp,
            addr: "0.0.0.0".parse().unwrap(),
            port,
            container: "ha".into(),
        };
        let published = host_published(
            &[udp(5353), udp(1900)],
            &HashSet::from(["ha".to_string()]),
            &[],
        );
        assert_eq!(
            published,
            HashSet::from([Published {
                addr: "0.0.0.0".parse().unwrap(),
                port: 1900,
                proto: Proto::Udp
            }])
        );
    }

    #[test]
    fn reads_a_published_port() {
        let value = containers(
            r#"[{"Ports":[{"IP":"0.0.0.0","PrivatePort":5432,"PublicPort":15434,"Type":"tcp"}]}]"#,
        );
        let ports = published_ports(&value);
        assert_eq!(ports.len(), 1);
        assert!(
            ports.contains(&tcp("0.0.0.0", 15434)),
            "the forward is the PUBLISHED port on both sides; 5432 lives only \
             inside the container's namespace and forwarding to it hangs"
        );
    }

    /// `-p 127.0.0.1:8080:80` is the user asking for a door on loopback
    /// only; the address Docker bound is the address the Mac binds.
    #[test]
    fn keeps_the_bind_address() {
        let value = containers(
            r#"[{"Ports":[{"IP":"127.0.0.1","PrivatePort":80,"PublicPort":18097,"Type":"tcp"}]}]"#,
        );
        assert_eq!(
            published_ports(&value),
            HashSet::from([tcp("127.0.0.1", 18097)])
        );
    }

    /// A publish with no address binds every interface, as Docker's does.
    #[test]
    fn a_missing_address_means_every_interface() {
        let value =
            containers(r#"[{"Ports":[{"PrivatePort":80,"PublicPort":18097,"Type":"tcp"}]}]"#);
        assert_eq!(
            published_ports(&value),
            HashSet::from([tcp("0.0.0.0", 18097)])
        );
    }

    #[test]
    fn a_published_binding_prints_as_its_socket_address() {
        assert_eq!(tcp("::", 8080).to_string(), "tcp [::]:8080");
        let udp = Published {
            proto: Proto::Udp,
            ..tcp("0.0.0.0", 1053)
        };
        assert_eq!(udp.to_string(), "udp 0.0.0.0:1053");
    }

    /// An exposed port with no PublicPort is the container declaring what it
    /// listens on, not the user asking for a door on the host. Opening one
    /// anyway would publish every container's ports by surprise.
    #[test]
    fn ignores_exposed_but_unpublished_ports() {
        let value = containers(r#"[{"Ports":[{"PrivatePort":9000,"Type":"tcp"}]}]"#);
        assert!(published_ports(&value).is_empty());
    }

    /// Docker reports one publish twice, once per address family. They are
    /// two listeners on the Mac, one each, so a v6 client is served too.
    #[test]
    fn keeps_both_families_of_one_publish() {
        let value = containers(
            r#"[{"Ports":[
                {"IP":"0.0.0.0","PrivatePort":8025,"PublicPort":18025,"Type":"tcp"},
                {"IP":"::","PrivatePort":8025,"PublicPort":18025,"Type":"tcp"}
            ]}]"#,
        );
        assert_eq!(
            published_ports(&value),
            HashSet::from([tcp("0.0.0.0", 18025), tcp("::", 18025)])
        );
    }

    /// A UDP publish is its own forward, distinct from a TCP one on the
    /// same port; anything else Docker can publish (SCTP) is not carried.
    #[test]
    fn keeps_udp_and_skips_what_nothing_carries() {
        let value = containers(
            r#"[{"Ports":[
                {"IP":"0.0.0.0","PrivatePort":53,"PublicPort":1053,"Type":"udp"},
                {"IP":"0.0.0.0","PrivatePort":53,"PublicPort":1053,"Type":"tcp"},
                {"IP":"0.0.0.0","PrivatePort":53,"PublicPort":1053,"Type":"sctp"}
            ]}]"#,
        );
        let ports = published_ports(&value);
        assert_eq!(ports.len(), 2);
        assert!(ports.contains(&Published {
            proto: Proto::Udp,
            ..tcp("0.0.0.0", 1053)
        }));
        assert!(ports.contains(&tcp("0.0.0.0", 1053)));
    }

    #[test]
    fn handles_containers_with_no_ports_and_a_missing_key() {
        assert!(published_ports(&containers(r#"[{"Ports":[]},{}]"#)).is_empty());
        assert!(published_ports(&containers("null")).is_empty());
    }

    #[test]
    fn reads_several_containers() {
        let value = containers(
            r#"[
                {"Ports":[{"PrivatePort":5432,"PublicPort":15434,"Type":"tcp"}]},
                {"Ports":[{"PrivatePort":9000,"PublicPort":19000,"Type":"tcp"}]},
                {"Ports":[{"PrivatePort":8025,"PublicPort":18025,"Type":"tcp"}]}
            ]"#,
        );
        let mut ports: Vec<_> = published_ports(&value).into_iter().collect();
        ports.sort();
        assert_eq!(
            ports,
            vec![
                tcp("0.0.0.0", 15434),
                tcp("0.0.0.0", 18025),
                tcp("0.0.0.0", 19000),
            ]
        );
    }
}

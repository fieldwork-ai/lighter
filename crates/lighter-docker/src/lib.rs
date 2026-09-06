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

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Watches Docker and keeps the host's forwards matching it.
pub struct PortWatcher {
    socket: PathBuf,
    mapper: Arc<dyn PortMapper>,
    /// What we have opened. The host port is the identity of a forward.
    forwarded: HashSet<Published>,
}

impl PortWatcher {
    /// Starts a thread that reconciles now and on every container event.
    ///
    /// The handle ends the watching: the event stream is dropped and not
    /// reopened, which is what lets dockerd exit at once when the machine
    /// is being stopped.
    pub fn start(socket: &Path, mapper: Arc<dyn PortMapper>) -> std::io::Result<Arc<http::Stop>> {
        let mut watcher = PortWatcher {
            socket: socket.to_path_buf(),
            mapper,
            forwarded: HashSet::new(),
        };
        let stop = http::Stop::new();
        let handle = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("docker-ports".into())
            .spawn(move || watcher.run(&handle))?;
        Ok(stop)
    }

    fn run(&mut self, stop: &http::Stop) {
        // Filters to container events only. URL-encoded because it is a JSON
        // document in a query parameter.
        const EVENTS: &str = "/events?filters=%7B%22type%22%3A%5B%22container%22%5D%7D";

        loop {
            let socket = self.socket.clone();
            let mapper = Arc::clone(&self.mapper);
            let forwarded = &mut self.forwarded;

            // Reconcile before watching, not after: containers may already be
            // running from a previous session, and a watcher that only reacted
            // to events would never open their doors.
            Self::reconcile(&socket, &mapper, forwarded);

            // Reconciling INSIDE the callback is the whole point. Setting a
            // flag and acting on it after the call returns looks equivalent and
            // is not: a healthy event stream never returns, so the forwards
            // would only ever be fixed up when Docker went away.
            let result = http::stream_json(&socket, EVENTS, Some(stop), |event| {
                let status = event.get("status").and_then(|s| s.as_str()).unwrap_or("");
                // Only lifecycle transitions can change what is published.
                // Reconciling on every exec_start would be correct and noisy.
                if matches!(status, "start" | "die" | "destroy" | "pause" | "unpause") {
                    Self::reconcile(&socket, &mapper, forwarded);
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

    /// Makes the host's forwards match what Docker currently publishes.
    fn reconcile(socket: &Path, mapper: &Arc<dyn PortMapper>, forwarded: &mut HashSet<Published>) {
        let containers = match http::get_json(socket, "/containers/json") {
            Ok(value) => value,
            Err(e) => {
                tracing::debug!(%e, "could not list containers");
                return;
            }
        };
        let desired = published_ports(&containers);

        for gone in forwarded.difference(&desired).copied().collect::<Vec<_>>() {
            match mapper.unexpose(gone) {
                Ok(()) => {
                    tracing::info!(published = %gone, "port forward withdrawn");
                    forwarded.remove(&gone);
                }
                Err(e) => tracing::warn!(published = %gone, %e, "could not withdraw a forward"),
            }
        }

        for new in desired.difference(forwarded).copied().collect::<Vec<_>>() {
            match mapper.expose(new) {
                Ok(()) => {
                    tracing::info!(published = %new, "port forwarded");
                    forwarded.insert(new);
                }
                Err(e) => tracing::warn!(published = %new, %e, "could not forward a port"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

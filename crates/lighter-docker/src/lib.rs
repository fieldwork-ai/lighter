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
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Somewhere to put a forward. Implemented by the VMM over its network stack.
///
/// One port, not two. The host port and the guest port of a published Docker
/// port are always the same number, and a two-argument version invites exactly
/// the mistake described on [`Published`].
pub trait PortMapper: Send + Sync {
    fn expose(&self, port: u16) -> Result<(), String>;
    fn unexpose(&self, port: u16) -> Result<(), String>;
}

/// A port Docker has published on the guest.
///
/// One number, deliberately. `docker run -p 15434:5432` makes Docker listen on
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
    pub port: u16,
}

/// Extracts the published ports from a `GET /containers/json` response.
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
            // UDP is not forwarded. The stream carrying a published port is TCP, and silently
            // mapping a UDP publish to a TCP forward would be worse than not
            // doing it: the port would answer, wrongly.
            if entry.get("Type").and_then(|t| t.as_str()) != Some("tcp") {
                continue;
            }
            let Some(public) = entry.get("PublicPort").and_then(|p| p.as_u64()) else {
                continue;
            };
            // Docker lists an IPv4 and an IPv6 binding for the same publish;
            // they are one forward, and the set collapses them.
            if let Ok(port) = u16::try_from(public) {
                ports.insert(Published { port });
            }
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
    pub fn start(socket: &Path, mapper: Arc<dyn PortMapper>) -> std::io::Result<()> {
        let mut watcher = PortWatcher {
            socket: socket.to_path_buf(),
            mapper,
            forwarded: HashSet::new(),
        };

        std::thread::Builder::new()
            .name("docker-ports".into())
            .spawn(move || watcher.run())?;
        Ok(())
    }

    fn run(&mut self) {
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
            let result = http::stream_json(&socket, EVENTS, |event| {
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
            match mapper.unexpose(gone.port) {
                Ok(()) => {
                    tracing::info!(port = gone.port, "port forward withdrawn");
                    forwarded.remove(&gone);
                }
                Err(e) => tracing::warn!(port = gone.port, %e, "could not withdraw a forward"),
            }
        }

        for new in desired.difference(forwarded).copied().collect::<Vec<_>>() {
            match mapper.expose(new.port) {
                Ok(()) => {
                    tracing::info!(port = new.port, "port forwarded");
                    forwarded.insert(new);
                }
                Err(e) => tracing::warn!(port = new.port, %e, "could not forward a port"),
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

    #[test]
    fn reads_a_published_port() {
        let value = containers(
            r#"[{"Ports":[{"IP":"0.0.0.0","PrivatePort":5432,"PublicPort":15434,"Type":"tcp"}]}]"#,
        );
        let ports = published_ports(&value);
        assert_eq!(ports.len(), 1);
        assert!(
            ports.contains(&Published { port: 15434 }),
            "the forward is the PUBLISHED port on both sides; 5432 lives only \
             inside the container's namespace and forwarding to it hangs"
        );
    }

    /// An exposed port with no PublicPort is the container declaring what it
    /// listens on, not the user asking for a door on the host. Opening one
    /// anyway would publish every container's ports by surprise.
    #[test]
    fn ignores_exposed_but_unpublished_ports() {
        let value = containers(r#"[{"Ports":[{"PrivatePort":9000,"Type":"tcp"}]}]"#);
        assert!(published_ports(&value).is_empty());
    }

    /// Docker reports one publish twice, once per address family. They are one
    /// forward, and binding the same host port twice fails the
    /// second time.
    #[test]
    fn collapses_the_ipv4_and_ipv6_entries_of_one_publish() {
        let value = containers(
            r#"[{"Ports":[
                {"IP":"0.0.0.0","PrivatePort":8025,"PublicPort":18025,"Type":"tcp"},
                {"IP":"::","PrivatePort":8025,"PublicPort":18025,"Type":"tcp"}
            ]}]"#,
        );
        assert_eq!(published_ports(&value).len(), 1);
    }

    /// A published port is carried as a TCP stream. Forwarding a UDP publish onto it would leave a
    /// port that answers, incorrectly, which is worse than one that does not.
    #[test]
    fn skips_udp() {
        let value =
            containers(r#"[{"Ports":[{"PrivatePort":53,"PublicPort":1053,"Type":"udp"}]}]"#);
        assert!(published_ports(&value).is_empty());
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
                Published { port: 15434 },
                Published { port: 18025 },
                Published { port: 19000 },
            ]
        );
    }
}

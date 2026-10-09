//! Containers' names, for the Mac to reach them by: `web.lighter.local` for
//! a container named `web`, and `db.shop.lighter.local` for Compose's `db`
//! service in project `shop`.
//!
//! The same shape as the port watcher: every container or network event
//! asks Docker what is running now and hands the whole answer to a
//! [`NameSink`], which keeps the Mac's records in step. What a name means is
//! decided here ([`records`]), against a recorded API response; what holding
//! a record means is the sink's.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::http;

/// The domain every name is under.
pub const DOMAIN: &str = "lighter.local";

/// Every name and the addresses it should answer, in full each time.
pub type Records = BTreeMap<String, BTreeSet<IpAddr>>;

/// Where the names go: the link card, which answers the Mac for them.
pub trait NameSink: Send + Sync {
    fn set(&self, records: Records);
}

/// The names for what `/containers/json` lists. Only addresses `reachable`
/// accepts are named, since a name for an address the Mac cannot reach is
/// worse than none; a host-network container, which has no address of its
/// own, is named at `host`, the machine's.
pub fn records(
    containers: &serde_json::Value,
    reachable: impl Fn(IpAddr) -> bool,
    host: &[IpAddr],
) -> Records {
    let mut out = Records::new();
    for container in containers.as_array().into_iter().flatten() {
        let host_network = container
            .pointer("/HostConfig/NetworkMode")
            .and_then(|m| m.as_str())
            == Some("host");
        let addresses: Vec<IpAddr> = if host_network {
            host.to_vec()
        } else {
            // Of each family, the first of its networks, by name, that the
            // Mac can reach: the same answer every time for a container on
            // several.
            let mut networks: Vec<_> = container
                .pointer("/NetworkSettings/Networks")
                .and_then(|n| n.as_object())
                .into_iter()
                .flatten()
                .collect();
            networks.sort_by_key(|(name, _)| name.as_str());
            ["IPAddress", "GlobalIPv6Address"]
                .iter()
                .filter_map(|field| {
                    networks
                        .iter()
                        .filter_map(|(_, settings)| settings.get(*field)?.as_str()?.parse().ok())
                        .find(|ip| reachable(*ip))
                })
                .collect()
        };
        if addresses.is_empty() {
            continue;
        }
        let mut names = Vec::new();
        if let Some(name) = container
            .get("Names")
            .and_then(|n| n.as_array())
            .and_then(|n| n.first())
            .and_then(|n| n.as_str())
        {
            names.push(name.trim_start_matches('/').to_string());
        }
        let label = |key: &str| {
            container
                .pointer(&format!("/Labels/{}", key.replace('/', "~1")))
                .and_then(|v| v.as_str())
        };
        if let (Some(service), Some(project)) = (
            label("com.docker.compose.service"),
            label("com.docker.compose.project"),
        ) {
            names.push(format!("{service}.{project}"));
        }
        for name in names {
            if let Some(name) = label_safe(&name) {
                out.entry(format!("{name}.{DOMAIN}"))
                    .or_default()
                    .extend(&addresses);
            }
        }
        for name in label(DOMAINS_LABEL).into_iter().flat_map(|v| v.split(',')) {
            match chosen_name(name) {
                Some(name) => {
                    out.entry(name).or_default().extend(&addresses);
                }
                None => tracing::info!(
                    name = name.trim(),
                    "a {DOMAINS_LABEL} name is not used: only .local names reach the Mac this way"
                ),
            }
        }
    }
    out
}

/// The label that gives a container names of its own choosing:
/// `lighter.domains=api.myapp.local,*.myapp.local`.
pub const DOMAINS_LABEL: &str = "lighter.domains";

/// A name from [`DOMAINS_LABEL`], lowercased, if it is one the Mac asks the
/// link about: under `.local`, which the Mac resolves by multicast DNS on
/// every network it is on. `*.` in front holds every name under the rest.
fn chosen_name(name: &str) -> Option<String> {
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    let (wild, rest) = match name.strip_prefix("*.") {
        Some(rest) => (true, rest),
        None => (false, name.as_str()),
    };
    let stem = rest.strip_suffix(".local")?;
    let stem = label_safe(stem)?;
    Some(if wild {
        format!("*.{stem}.local")
    } else {
        format!("{stem}.local")
    })
}

/// A name as DNS can carry it, lowercased: Docker allows `_` and `.` in a
/// container's name, which pass, and nothing else unusual.
fn label_safe(name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    let ok = !name.is_empty()
        && name.len() <= 200
        && name.split('.').all(|l| !l.is_empty() && l.len() <= 63)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    ok.then_some(name)
}

/// Keeps a [`NameSink`] in step with Docker until `stop` is asked.
pub fn watch(
    socket: &Path,
    stop: Arc<http::Stop>,
    sink: Arc<dyn NameSink>,
    reachable: impl Fn(IpAddr) -> bool + Send + 'static,
    host: Vec<IpAddr>,
) -> std::io::Result<()> {
    // Container and network events: a container joining or leaving a
    // network changes its address without starting or stopping.
    const EVENTS: &str = "/events?filters=%7B%22type%22%3A%5B%22container%22%2C%22network%22%5D%7D";
    let socket: PathBuf = socket.to_path_buf();
    std::thread::Builder::new()
        .name("docker-names".into())
        .spawn(move || {
            let reconcile = || match http::get_json(&socket, "/containers/json") {
                Ok(list) => sink.set(records(&list, &reachable, &host)),
                Err(e) => tracing::debug!(%e, "could not list containers for their names"),
            };
            loop {
                reconcile();
                let result = http::stream_json(&socket, EVENTS, Some(&stop), |event| {
                    let action = event
                        .get("Action")
                        .or_else(|| event.get("status"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    if matches!(
                        action,
                        "start" | "die" | "destroy" | "rename" | "connect" | "disconnect"
                    ) {
                        reconcile();
                    }
                });
                if let Err(e) = result {
                    tracing::debug!(%e, "docker event stream for names ended");
                }
                if stop.asked() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => v4.octets()[..2] == [10, 211],
            IpAddr::V6(v6) => v6.segments()[..4] == [0xfd12, 0x3456, 0x789a, 0],
        }
    }

    const HOST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 211, 0, 2));

    #[test]
    fn a_container_and_a_compose_service_are_named() {
        let list = serde_json::json!([
            {
                "Names": ["/web"],
                "HostConfig": {"NetworkMode": "bridge"},
                "NetworkSettings": {"Networks": {"bridge": {"IPAddress": "10.211.1.2"}}}
            },
            {
                "Names": ["/shop-db-1"],
                "Labels": {"com.docker.compose.service": "db", "com.docker.compose.project": "shop"},
                "HostConfig": {"NetworkMode": "shop_default"},
                "NetworkSettings": {"Networks": {"shop_default": {"IPAddress": "10.211.2.3"}}}
            },
            {
                "Names": ["/shop-db-2"],
                "Labels": {"com.docker.compose.service": "db", "com.docker.compose.project": "shop"},
                "HostConfig": {"NetworkMode": "shop_default"},
                "NetworkSettings": {"Networks": {"shop_default": {"IPAddress": "10.211.2.4"}}}
            }
        ]);
        let got = records(&list, link, &[HOST]);
        let ips = |name: &str| {
            got[name]
                .iter()
                .map(|ip| ip.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ips("web.lighter.local"), ["10.211.1.2"]);
        assert_eq!(ips("shop-db-1.lighter.local"), ["10.211.2.3"]);
        assert_eq!(ips("db.shop.lighter.local"), ["10.211.2.3", "10.211.2.4"]);
    }

    #[test]
    fn host_network_is_the_machine_and_unreachable_is_nothing() {
        let list = serde_json::json!([
            {"Names": ["/ha"], "HostConfig": {"NetworkMode": "host"}, "NetworkSettings": {"Networks": {"host": {"IPAddress": ""}}}},
            {"Names": ["/old"], "HostConfig": {"NetworkMode": "legacy"}, "NetworkSettings": {"Networks": {"legacy": {"IPAddress": "172.18.0.2"}}}},
            {"Names": ["/none"], "HostConfig": {"NetworkMode": "none"}, "NetworkSettings": {"Networks": {"none": {"IPAddress": ""}}}}
        ]);
        let got = records(&list, link, &[HOST]);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got["ha.lighter.local"].contains(&HOST));
    }

    #[test]
    fn of_several_networks_the_first_reachable_by_name() {
        let list = serde_json::json!([{
            "Names": ["/App"],
            "HostConfig": {"NetworkMode": "b"},
            "NetworkSettings": {"Networks": {
                "z": {"IPAddress": "10.211.9.2"},
                "b": {"IPAddress": "10.211.4.2"},
                "a": {"IPAddress": "172.18.0.5"}
            }}
        }]);
        let got = records(&list, link, &[HOST]);
        assert_eq!(
            got["app.lighter.local"].iter().collect::<Vec<_>>(),
            [&IpAddr::V4(std::net::Ipv4Addr::new(10, 211, 4, 2))],
            "lowercased, and network b's address"
        );
    }

    #[test]
    fn names_dns_cannot_carry_are_left_out() {
        assert_eq!(label_safe("my_app.v2").as_deref(), Some("my_app.v2"));
        assert_eq!(label_safe("a..b"), None);
        assert_eq!(label_safe("sp ace"), None);
        assert_eq!(label_safe(&"x".repeat(64)), None);
    }

    #[test]
    fn a_label_names_a_container_as_its_owner_chooses() {
        let list = serde_json::json!([{
            "Names": ["/api"],
            "Labels": {"lighter.domains": "API.MyApp.local, *.myapp.local,myapp.test, .local"},
            "HostConfig": {"NetworkMode": "bridge"},
            "NetworkSettings": {"Networks": {"bridge": {"IPAddress": "10.211.1.9"}}}
        }]);
        let got = records(&list, link, &[HOST]);
        let names: Vec<&str> = got.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["*.myapp.local", "api.lighter.local", "api.myapp.local"],
            "{got:?}"
        );
    }

    #[test]
    fn a_container_with_ipv6_on_the_link_is_named_in_both_families() {
        let list = serde_json::json!([{
            "Names": ["/web6"],
            "HostConfig": {"NetworkMode": "v6net"},
            "NetworkSettings": {"Networks": {"v6net": {"IPAddress": "10.211.3.2", "GlobalIPv6Address": "fd12:3456:789a:0:3::2"}}}
        }, {
            "Names": ["/old6"],
            "HostConfig": {"NetworkMode": "bridge"},
            "NetworkSettings": {"Networks": {"bridge": {"IPAddress": "10.211.1.2", "GlobalIPv6Address": "fd6c:6967:6874:d0c::2"}}}
        }]);
        let got = records(&list, link, &[HOST]);
        assert_eq!(got["web6.lighter.local"].len(), 2);
        assert_eq!(
            got["old6.lighter.local"].len(),
            1,
            "a v6 address off the link is not named"
        );
    }
}

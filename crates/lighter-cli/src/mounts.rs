//! Bind mounts from folders on the Mac that the machine does not share.
//!
//! Docker on the Mac binds from the guest's own filesystem, so a source the
//! machine does not share is an empty folder dockerd made in the guest, and
//! the container runs against it without an error anywhere. Docker Desktop
//! refuses such a start ("Mounts denied"); lighter cannot see the request, so
//! it says so afterwards, in `lighter status` and `lighter doctor`.

/// Where a path that is on the Mac, rather than in the guest, starts. A
/// source elsewhere (`/var/run/docker.sock`, `/etc/localtime`) is the
/// guest's own and is meant to be.
const MAC_ROOTS: [&str; 9] = [
    "/Users",
    "/Volumes",
    "/private",
    "/Applications",
    "/Library",
    "/opt",
    "/tmp",
    "/var/folders",
    "/usr/local",
];

/// A running container's bind mount from an unshared folder on the Mac.
#[derive(Debug, PartialEq, Eq)]
pub struct Unshared {
    pub container: String,
    pub source: String,
}

impl Unshared {
    /// What to do about it.
    pub fn remedy(&self) -> String {
        if crate::config::within(&self.source, "/tmp")
            || crate::config::within(&self.source, "/private/tmp")
        {
            "/tmp is the machine's own, not the Mac's: bind from your home folder or $TMPDIR"
                .to_string()
        } else {
            format!(
                "share it with `lighter config --share {}` and `lighter restart`",
                self.source
            )
        }
    }
}

/// The bind mounts in a `GET /containers/json` response whose source is on
/// the Mac and outside every one of `shares`.
pub fn unshared(containers: &serde_json::Value, shares: &[String]) -> Vec<Unshared> {
    let mut found = Vec::new();
    for container in containers.as_array().into_iter().flatten() {
        let name = container
            .get("Names")
            .and_then(|n| n.get(0))
            .and_then(|n| n.as_str())
            .map(|n| n.trim_start_matches('/'))
            .unwrap_or_default();
        for mount in container
            .get("Mounts")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
        {
            if mount.get("Type").and_then(|t| t.as_str()) != Some("bind") {
                continue;
            }
            let Some(source) = mount.get("Source").and_then(|s| s.as_str()) else {
                continue;
            };
            let on_the_mac = MAC_ROOTS
                .iter()
                .any(|root| crate::config::within(source, root));
            let shared = shares
                .iter()
                .any(|share| crate::config::within(source, share));
            if on_the_mac && !shared {
                found.push(Unshared {
                    container: name.to_string(),
                    source: source.to_string(),
                });
            }
        }
    }
    found
}

/// The running containers' unshared bind mounts, or nothing when Docker
/// does not answer: this is a report, never a reason to fail one.
pub fn running(shares: &[String]) -> Vec<Unshared> {
    let Ok(socket) = crate::paths::docker_socket() else {
        return Vec::new();
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    match lighter_docker::http::get_json_until(&socket, "/containers/json", deadline) {
        Ok(containers) => unshared(&containers, shares),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shares() -> Vec<String> {
        crate::config::default_shares()
    }

    fn container(name: &str, mounts: serde_json::Value) -> serde_json::Value {
        serde_json::json!([{ "Names": [format!("/{name}")], "Mounts": mounts }])
    }

    #[test]
    fn a_source_outside_the_shares_is_found() {
        let containers = container(
            "db",
            serde_json::json!([
                { "Type": "bind", "Source": "/opt/data", "Destination": "/data" },
                { "Type": "bind", "Source": "/Users/me/app", "Destination": "/app" },
                { "Type": "bind", "Source": "/Volumes/T9/media", "Destination": "/media" },
            ]),
        );
        assert_eq!(
            unshared(&containers, &shares()),
            [Unshared {
                container: "db".into(),
                source: "/opt/data".into()
            }]
        );
    }

    #[test]
    fn the_guests_own_paths_and_volumes_are_not() {
        let containers = container(
            "agent",
            serde_json::json!([
                { "Type": "bind", "Source": "/var/run/docker.sock", "Destination": "/var/run/docker.sock" },
                { "Type": "bind", "Source": "/etc/localtime", "Destination": "/etc/localtime" },
                { "Type": "volume", "Name": "data", "Source": "/var/lib/docker/volumes/data/_data" },
                { "Type": "bind", "Source": "/optional", "Destination": "/x" },
            ]),
        );
        assert!(unshared(&containers, &shares()).is_empty());
    }

    #[test]
    fn tmp_is_found_with_its_own_remedy() {
        let containers = container(
            "web",
            serde_json::json!([{ "Type": "bind", "Source": "/tmp/build", "Destination": "/out" }]),
        );
        let found = unshared(&containers, &shares());
        assert_eq!(found.len(), 1);
        assert!(found[0].remedy().contains("$TMPDIR"));
    }

    #[test]
    fn a_folder_shared_by_hand_is_not_found() {
        let containers = container(
            "db",
            serde_json::json!([{ "Type": "bind", "Source": "/opt/data/pg", "Destination": "/d" }]),
        );
        let mut shares = shares();
        shares.push("/opt/data".into());
        assert!(unshared(&containers, &shares).is_empty());
    }
}

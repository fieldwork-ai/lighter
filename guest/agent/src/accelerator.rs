//! The idle-poll window while a container talks to an accelerator.
//!
//! A stream to one of the host's accelerator servers (the Neural Engine,
//! ggml on Metal, PyTorch on MPS) is a request loop: llama.cpp sends eight
//! messages a token and waits for the logits, and its client hands every
//! message between two threads. Each wait is a vCPU going idle, and a vCPU
//! that has gone through WFI is woken by the host, late on a busy Mac. The
//! guest kernel polls before WFI (patch 0011) for a window that adapts
//! between a floor and a cap, 50 µs to 200 µs at rest, sized for a machine
//! whose idle should cost the Mac nothing. While a model is answering the
//! opposite is wanted: the vCPUs stay awake between messages, as a native
//! client's threads do. On the M1, Qwen2.5-0.5B over `lighter.sh/metal`:
//! 64 tokens a second at the resting window, 76 with a 2 ms cap, 95 to 99
//! with 5 ms, and no more at 20 or 50, against 107 for a native client of
//! the same server. A token's wait for the logits is nine milliseconds, so
//! 5 ms is the cap that keeps the vCPU awake between messages and lets it
//! sleep through the compute.
//!
//! So the window is widened for exactly as long as a stream to an
//! accelerator port is open, and restored when the last one closes. The
//! ports come from the kernel command line, where init also reads them for
//! the CDI specs. The sysfs knobs are the patch's module parameters.
//!
//! The same ports are gated here. The servers behind them parse what they
//! are sent inside the lighter process, and the CDI device only names the
//! port in the container's environment: without a gate every container
//! could reach them through the host gateway. So a stream to an
//! accelerator port is refused unless the container it comes from asked
//! for that device, which dockerd records as a CDI device request on the
//! container; the agent asks dockerd over its socket, by the connection's
//! source address, and refuses on any doubt.

use std::sync::{Mutex, OnceLock};

const POLL_NS: &str = "/sys/module/idle/parameters/poll_ns";
const POLL_GROW_START_NS: &str = "/sys/module/idle/parameters/poll_grow_start_ns";

/// The cap and the floor while a stream is open, nanoseconds.
const WIDE_NS: &str = "5000000";
const WIDE_START_NS: &str = "2000000";

/// The keys init publishes as devices; each names a host loopback port, and
/// the CDI kind is `lighter.sh/<name>`.
const KEYS: [(&str, &str); 3] = [
    ("lighter.ane", "ane"),
    ("lighter.metal", "metal"),
    ("lighter.mps", "mps"),
];

/// Port and CDI kind of every accelerator on the command line.
fn devices() -> &'static [(u16, &'static str)] {
    static DEVICES: OnceLock<Vec<(u16, &'static str)>> = OnceLock::new();
    DEVICES.get_or_init(|| {
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        cmdline
            .split_whitespace()
            .filter_map(|w| {
                let (key, value) = w.split_once('=')?;
                let kind = KEYS.iter().find(|(k, _)| *k == key)?.1;
                Some((value.parse().ok()?, kind))
            })
            .collect()
    })
}

fn ports() -> impl Iterator<Item = u16> {
    devices().iter().map(|&(port, _)| port)
}

/// The CDI kind behind an accelerator port, if it is one.
pub fn kind_of(port: u16) -> Option<&'static str> {
    devices().iter().find(|&&(p, _)| p == port).map(|&(_, kind)| kind)
}

/// Whether the container at `peer` holds the CDI device `lighter.sh/<kind>`:
/// dockerd is asked for the running container with that address, then for
/// its device requests. Anything short of a clear yes is a no.
pub fn permitted(kind: &str, peer: std::net::IpAddr) -> bool {
    let list = match docker_get("/containers/json") {
        Ok(body) => body,
        Err(e) => {
            eprintln!("lighter-agent: cannot list containers for the {kind} device: {e}");
            return false;
        }
    };
    let Some(id) = container_with_address(&list, &peer.to_string()) else {
        eprintln!("lighter-agent: no running container at {peer} asked for lighter.sh/{kind}");
        return false;
    };
    let inspect = match docker_get(&format!("/containers/{id}/json")) {
        Ok(body) => body,
        Err(e) => {
            eprintln!("lighter-agent: cannot inspect the container at {peer}: {e}");
            return false;
        }
    };
    let yes = requests_device(&inspect, kind);
    if !yes {
        eprintln!("lighter-agent: the container at {peer} did not ask for lighter.sh/{kind}; refused");
    }
    yes
}

/// One HTTP GET on dockerd's socket, the body returned whole.
fn docker_get(path: &str) -> std::io::Result<String> {
    use std::io::{Read, Write};
    let mut sock = std::os::unix::net::UnixStream::connect("/run/docker.sock")?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    sock.write_all(format!("GET {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n").as_bytes())?;
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no HTTP header"))?;
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return Err(std::io::Error::other(head.lines().next().unwrap_or("").to_string()));
    }
    // Chunked transfer: dockerd sends the list that way. The chunks are
    // joined; the bodies never contain a bare CRLF, so the split is safe.
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut out = String::new();
        let mut rest = body;
        loop {
            let Some((size, after)) = rest.split_once("\r\n") else { break };
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if n == 0 || after.len() < n {
                break;
            }
            out.push_str(&after[..n]);
            rest = after[n..].strip_prefix("\r\n").unwrap_or(&after[n..]);
        }
        return Ok(out);
    }
    Ok(body.to_string())
}

/// The id of the container in a `/containers/json` listing whose networks
/// give it `address`. Each container's object begins with its `Id`; the
/// networks' own ids are `NetworkID` and `EndpointID`, so the split holds.
fn container_with_address(listing: &str, address: &str) -> Option<String> {
    let needle = format!("\"IPAddress\":\"{address}\"");
    let mut parts = listing.split("\"Id\":\"");
    parts.next();
    for part in parts {
        if part.contains(&needle) {
            return Some(part.split('"').next()?.to_string());
        }
    }
    None
}

/// Whether a container's inspect output records a CDI request for
/// `lighter.sh/<kind>`: dockerd keeps them under `HostConfig.DeviceRequests`
/// as `{"Driver":"cdi","DeviceIDs":["lighter.sh/<kind>=all"]}`. Only that
/// array is searched, so a matching string elsewhere (an environment
/// variable, a label) does not count.
fn requests_device(inspect: &str, kind: &str) -> bool {
    let Some(start) = inspect.find("\"DeviceRequests\":[") else { return false };
    let array = &inspect[start + "\"DeviceRequests\":".len()..];
    let mut depth = 0usize;
    let mut end = array.len();
    for (i, c) in array.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    let array = &array[..end];
    array.contains("\"Driver\":\"cdi\"") && array.contains(&format!("\"lighter.sh/{kind}="))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = r#"[{"Id":"aaa111","Names":["/frigate"],"NetworkSettings":{"Networks":{"ha":{"NetworkID":"n1","EndpointID":"e1","IPAddress":"172.18.0.5"}}}},{"Id":"bbb222","Names":["/other"],"NetworkSettings":{"Networks":{"bridge":{"NetworkID":"n2","EndpointID":"e2","IPAddress":"172.17.0.3"}}}}]"#;

    #[test]
    fn the_container_is_found_by_its_address() {
        assert_eq!(container_with_address(LISTING, "172.18.0.5").as_deref(), Some("aaa111"));
        assert_eq!(container_with_address(LISTING, "172.17.0.3").as_deref(), Some("bbb222"));
        assert_eq!(container_with_address(LISTING, "172.17.0.30"), None);
    }

    #[test]
    fn only_a_cdi_request_counts() {
        let with = r#"{"Id":"aaa","HostConfig":{"Devices":[],"DeviceRequests":[{"Driver":"cdi","Count":0,"DeviceIDs":["lighter.sh/metal=all"],"Capabilities":null}]},"Config":{"Env":["X=1"]}}"#;
        assert!(requests_device(with, "metal"));
        assert!(!requests_device(with, "ane"));
        let env_only = r#"{"Id":"aaa","HostConfig":{"DeviceRequests":[]},"Config":{"Env":["LIGHTER_METAL=lighter.sh/metal=all"]}}"#;
        assert!(!requests_device(env_only, "metal"));
        let none = r#"{"Id":"aaa","HostConfig":{"DeviceRequests":null}}"#;
        assert!(!requests_device(none, "metal"));
    }
}

/// Streams open to accelerator ports, and the resting values to put back.
static OPEN: Mutex<(u32, Option<(String, String)>)> = Mutex::new((0, None));

/// Held for the life of a stream to an accelerator port; `None` for any
/// other stream.
pub struct Wide(());

impl Wide {
    pub fn open(port: u16) -> Option<Wide> {
        if !ports().any(|p| p == port) {
            return None;
        }
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        if open.0 == 0 {
            let resting = (
                std::fs::read_to_string(POLL_NS).ok()?,
                std::fs::read_to_string(POLL_GROW_START_NS).ok()?,
            );
            // The floor first, so the cap is never below it.
            if std::fs::write(POLL_GROW_START_NS, WIDE_START_NS).is_err()
                || std::fs::write(POLL_NS, WIDE_NS).is_err()
            {
                return None;
            }
            open.1 = Some(resting);
        }
        open.0 += 1;
        Some(Wide(()))
    }
}

impl Drop for Wide {
    fn drop(&mut self) {
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        open.0 -= 1;
        if open.0 == 0
            && let Some((poll_ns, start_ns)) = open.1.take()
        {
            let _ = std::fs::write(POLL_NS, poll_ns.trim());
            let _ = std::fs::write(POLL_GROW_START_NS, start_ns.trim());
        }
    }
}

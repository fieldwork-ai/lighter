//! macOS Local Network privacy, and lighter's machine.
//!
//! A bundled app is denied unicast to other devices on the Mac's own network
//! until the user allows it in System Settings › Privacy & Security › Local
//! Network. The machine is one: since 0.5.4 it runs as an app bundle under a
//! launchd login agent. Three things made that invisible (2026-09-20, a
//! Reolink camera that fed Frigate for twenty minutes and then answered a
//! container nothing): the gateway is exempt, so a container reaching the
//! router proves nothing; command-line tools under Terminal or ssh are not
//! subject, so every check from a shell succeeds; and without
//! `NSLocalNetworkUsageDescription` in the bundle there is no dialog, so the
//! user is never asked and never told. The connection appears to open,
//! because the guest agent accepts before dialing, and no byte ever comes
//! back.
//!
//! So the bundle declares the key (`assets/Info.plist`); the machine asks at
//! start, with one mDNS query, which is what the system watches for; and the
//! doctor asks the running machine to connect to a device on the network
//! that is not the gateway and reports what it got, because only a connect
//! made by the machine's own process tests the machine's own permission.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const SOCKET: &str = "probe.sock";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Asks macOS for local network access the way it watches for: one
/// multicast DNS query for the service directory, sent and forgotten. On a
/// Mac that has not decided yet this raises the Local Network dialog for
/// the machine's bundle, once, at start rather than at the first container
/// that reaches for a device. Nothing listens for the answer.
pub fn ask_permission() {
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return;
    };
    let _ = socket.set_multicast_ttl_v4(255);
    // A DNS query: id 0, no flags, one question, `_services._dns-sd._udp.local`
    // PTR IN, the way `dns-sd -B _services._dns-sd._udp` asks.
    let mut query = vec![0u8, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["_services", "_dns-sd", "_udp", "local"] {
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.extend_from_slice(&[0, 0, 12, 0, 1]);
    let mdns = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353);
    match socket.send_to(&query, mdns) {
        Ok(_) => tracing::debug!("asked for local network access with an mDNS query"),
        Err(e) => {
            tracing::debug!(%e, "could not send the mDNS query that asks for local network access")
        }
    }
}

/// What a connect from the machine's process to a device came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Connected, or refused by the device: the network let the packets
    /// through either way.
    Reachable,
    /// `EHOSTUNREACH`, `ENETUNREACH` or `EPERM` from a device the Mac's own
    /// shell reaches: what Local Network privacy's denial looks like.
    Denied(String),
    /// Nothing came back in time: the device may drop what it does not
    /// expect, so this says nothing about the permission.
    Timeout,
    Other(String),
}

/// One connect, from this process, to a device; the outcome is the point.
pub fn connect(target: SocketAddr) -> Outcome {
    match TcpStream::connect_timeout(&target, CONNECT_TIMEOUT) {
        Ok(_) => Outcome::Reachable,
        Err(e) => match (e.kind(), e.raw_os_error()) {
            (io::ErrorKind::ConnectionRefused, _) => Outcome::Reachable,
            (io::ErrorKind::TimedOut, _) => Outcome::Timeout,
            (_, Some(code))
                if code == libc::EHOSTUNREACH
                    || code == libc::ENETUNREACH
                    || code == libc::EPERM =>
            {
                Outcome::Denied(e.to_string())
            }
            _ => Outcome::Other(e.to_string()),
        },
    }
}

/// The machine's side: a socket in the home on which the doctor asks the
/// running machine to connect to an address, one line in, one line out.
/// The doctor runs from a shell, which is not subject to the permission; the
/// machine's process is, so it is the one that must try.
pub struct Server {
    path: PathBuf,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Caller holds the instance lock for this home throughout our lifetime.
    pub fn start(home: &Path) -> io::Result<Self> {
        let path = home.join(SOCKET);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let thread = std::thread::Builder::new()
            .name("localnet-probe".into())
            .spawn(move || {
                for connection in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = connection else { break };
                    if stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .is_err()
                        || stream
                            .set_write_timeout(Some(Duration::from_secs(1)))
                            .is_err()
                    {
                        continue;
                    }
                    let mut line = String::new();
                    if BufReader::new(&stream).read_line(&mut line).is_err() {
                        continue;
                    }
                    let reply = match line.trim().parse::<SocketAddr>() {
                        Ok(target) => match connect(target) {
                            Outcome::Reachable => "reachable".to_string(),
                            Outcome::Denied(e) => format!("denied {e}"),
                            Outcome::Timeout => "timeout".to_string(),
                            Outcome::Other(e) => format!("other {e}"),
                        },
                        Err(_) => "other not an address".to_string(),
                    };
                    let _ = writeln!(stream, "{reply}");
                }
            })?;
        Ok(Self {
            path,
            stopped,
            thread: Some(thread),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Asks the machine at `home` (the process `pid`) to connect to `target`.
pub fn probe(home: &Path, pid: u32, target: SocketAddr) -> io::Result<Outcome> {
    let mut stream = UnixStream::connect(home.join(SOCKET))?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT + Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    if crate::instance::Identity::peer(home, &stream)?.pid() != pid {
        return Err(io::Error::other(
            "the probe socket belongs to a different daemon",
        ));
    }
    writeln!(stream, "{target}")?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    let reply = reply.trim();
    Ok(match reply.split_once(' ') {
        _ if reply == "reachable" => Outcome::Reachable,
        _ if reply == "timeout" => Outcome::Timeout,
        Some(("denied", why)) => Outcome::Denied(why.to_string()),
        Some((_, why)) => Outcome::Other(why.to_string()),
        None => Outcome::Other(reply.to_string()),
    })
}

/// The Mac's default gateway, which Local Network privacy exempts.
fn gateway() -> Option<Ipv4Addr> {
    let out = std::process::Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("gateway:"))
        .and_then(|g| g.trim().parse().ok())
}

/// Devices on the Mac's networks that are not the gateway, from the ARP
/// table: addresses the Mac has spoken to and knows the hardware of.
fn neighbours() -> Vec<Ipv4Addr> {
    let gateway = gateway();
    let Ok(out) = std::process::Command::new("/usr/sbin/arp")
        .arg("-an")
        .output()
    else {
        return Vec::new();
    };
    let mut seen = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // `? (192.168.1.120) at 9c:8e:cd:xx:xx:xx on en0 ifscope [ethernet]`
        let Some(start) = line.find('(') else {
            continue;
        };
        let Some(end) = line[start..].find(')') else {
            continue;
        };
        let Ok(ip) = line[start + 1..start + end].parse::<Ipv4Addr>() else {
            continue;
        };
        if line.contains("(incomplete)") || line.contains("permanent") || ip.is_multicast() {
            continue;
        }
        if ip.octets()[3] == 255 || Some(ip) == gateway || seen.contains(&ip) {
            continue;
        }
        seen.push(ip);
    }
    seen
}

/// The doctor's row: what the running machine got when it connected to a
/// device on the local network that is not the gateway. Port 7 (echo):
/// closed on nearly everything, and a refusal is as good as a connection
/// here, since either means the packets went through. A device that drops
/// unexpected packets times out and says nothing, so up to three are tried.
pub fn doctor_finding(home: &Path, pid: Option<u32>) -> (bool, String, Option<String>) {
    let Some(pid) = pid else {
        return (true, "untested: the machine is not running".into(), None);
    };
    let peers = neighbours();
    if peers.is_empty() {
        return (
            true,
            "untested: no device other than the gateway seen on this Mac's networks".into(),
            None,
        );
    }
    let mut timeouts = Vec::new();
    for ip in peers.iter().take(3) {
        let target = SocketAddr::V4(SocketAddrV4::new(*ip, 7));
        match probe(home, pid, target) {
            Ok(Outcome::Reachable) => {
                return (true, format!("allowed; the machine reached {ip}"), None);
            }
            Ok(Outcome::Denied(why)) => {
                return (
                    false,
                    format!("denied; the machine cannot reach {ip} ({why}) while this shell can"),
                    Some(
                        "allow lighter in System Settings › Privacy & Security › Local Network, then `lighter stop` and `lighter start`"
                            .into(),
                    ),
                );
            }
            Ok(Outcome::Timeout) => timeouts.push(ip.to_string()),
            Ok(Outcome::Other(why)) => {
                return (true, format!("untested: {ip} answered {why}"), None);
            }
            Err(e) => {
                return (
                    true,
                    format!("untested: the machine did not answer the probe ({e})"),
                    None,
                );
            }
        }
    }
    (
        true,
        format!(
            "untested: {} did not answer in time; a device that answers on any port would tell",
            timeouts.join(", ")
        ),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_connect_means_the_network_let_it_through() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert_eq!(
            connect(SocketAddr::from(([127, 0, 0, 1], port))),
            Outcome::Reachable
        );
    }

    #[test]
    fn an_open_port_is_reachable_too() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(connect(addr), Outcome::Reachable);
    }

    #[test]
    fn the_probe_socket_answers_one_line() {
        let home = std::env::temp_dir().join(format!("lighter-localnet-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let server = Server::start(&home).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut stream = UnixStream::connect(home.join(SOCKET)).unwrap();
        writeln!(stream, "{addr}").unwrap();
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), "reachable");
        drop(server);
        let _ = std::fs::remove_dir_all(&home);
    }
}

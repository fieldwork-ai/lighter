//! TCP as streams: the host half.
//!
//! Inside the guest, netfilter redirects every TCP connection that would
//! leave through the network device to the agent, which opens one vsock
//! connection to us per TCP connection and sends where it was going in a
//! fixed header. Here each becomes an ordinary macOS socket to that
//! destination, and bytes are copied both ways until either side is done.
//!
//! The guest's kernel terminates the container's connection and the Mac's
//! kernel originates the real one; the only thing crossing the boundary is
//! bytes over a device we own. That is what gives a VPN, a proxy and the
//! Mac's own routing their say (the connection is the Mac's), and it is what
//! keeps a TCP implementation out of this process.
//!
//! The header is nineteen bytes: a family byte (4 or 6), sixteen bytes of
//! address with an IPv4 address in the first four, and the port big-endian.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::virtio::vsock::{Accepted, ConnKey, VsockShared, pump};

/// The vsock port the agent dials for an outbound stream.
pub const STREAM_PORT: u32 = 2377;
/// The vsock port the agent answers inbound streams on.
pub const INBOUND_PORT: u32 = 2378;

/// gvproxy's addresses for the Mac itself, as seen from the guest. A
/// container that dials the gateway or `host.docker.internal` wants the
/// Mac, and a socket to loopback is what that is here.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 1);
const HOST_ALIAS: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 254);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_LEN: usize = 19;

/// Starts answering the agent's streams.
pub fn start(shared: Arc<VsockShared>) -> io::Result<()> {
    let accepted = shared.listen(STREAM_PORT);
    std::thread::Builder::new()
        .name("streams".into())
        .spawn(move || {
            for Accepted { key } in accepted {
                let shared = shared.clone();
                let _ = std::thread::Builder::new()
                    .name("stream".into())
                    .stack_size(crate::qos::CONNECTION_STACK)
                    .spawn(move || serve(shared, key));
            }
            tracing::debug!("stream listener stopped");
        })?;
    Ok(())
}

/// Where the guest's header says to go.
fn destination(header: &[u8; HEADER_LEN]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes([header[17], header[18]]);
    let ip: IpAddr = match header[0] {
        4 => {
            let v4 = Ipv4Addr::new(header[1], header[2], header[3], header[4]);
            if v4 == GATEWAY || v4 == HOST_ALIAS {
                Ipv4Addr::LOCALHOST.into()
            } else {
                v4.into()
            }
        }
        6 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&header[1..17]);
            Ipv6Addr::from(octets).into()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// One stream: read the header out of the connection's queue, dial, and
/// pump the connection straight onto the socket. No pair, no extra thread:
/// the bytes go from the device's queue to the TCP socket and back.
fn serve(shared: Arc<VsockShared>, key: ConnKey) {
    let Some(header) = shared.read_outbound_exact(key, HEADER_LEN) else {
        shared.shutdown(key);
        return;
    };
    let header: [u8; HEADER_LEN] = header.try_into().expect("exact length");
    let Some(addr) = destination(&header) else {
        shared.shutdown(key);
        return;
    };
    let mac = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            // Closing the stream is what the agent turns into a close of the
            // container's connection: the same thing a refused connect looks
            // like through any userspace stack.
            tracing::debug!(%addr, %e, "stream: connect failed");
            shared.shutdown(key);
            return;
        }
    };
    let _ = mac.set_nodelay(true);
    crate::sockbuf::widen(&mac);
    pump(shared, key, mac);
}

/// Published ports, the other way round: a listener on the Mac per port
/// Docker publishes, each accepted connection carried into the guest as a
/// vsock stream naming the port, where the agent connects to what Docker
/// has there. The forward that used to be gvproxy's.
pub struct PortMapper {
    shared: Arc<VsockShared>,
    listeners: std::sync::Mutex<std::collections::HashMap<u16, Arc<std::sync::atomic::AtomicBool>>>,
}

impl PortMapper {
    pub fn new(shared: Arc<VsockShared>) -> Arc<PortMapper> {
        Arc::new(PortMapper {
            shared,
            listeners: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }
}

impl lighter_docker::PortMapper for PortMapper {
    fn expose(&self, port: u16) -> Result<(), String> {
        let mut listeners = self.listeners.lock().expect("port mapper poisoned");
        if listeners.contains_key(&port) {
            return Ok(());
        }
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        listeners.insert(port, stop.clone());
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name(format!("port-{port}"))
            .spawn(move || {
                for accepted in listener.incoming() {
                    if stop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let Ok(mac) = accepted else { continue };
                    let shared = shared.clone();
                    let _ = std::thread::Builder::new()
                        .name("inbound".into())
                        .stack_size(crate::qos::CONNECTION_STACK)
                        .spawn(move || carry_inbound(shared, port, mac));
                }
            })
            .map_err(|e| e.to_string())?;
        tracing::info!(port, "port published through a stream");
        Ok(())
    }

    fn unexpose(&self, port: u16) -> Result<(), String> {
        let stop = self
            .listeners
            .lock()
            .expect("port mapper poisoned")
            .remove(&port);
        if let Some(stop) = stop {
            stop.store(true, std::sync::atomic::Ordering::Release);
            // The accept loop notices on its next connection, which this is.
            let _ = TcpStream::connect_timeout(
                &SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                Duration::from_millis(200),
            );
        }
        Ok(())
    }
}

/// One accepted connection on a published port, into the guest: the
/// socket is the connection's from the start, the port goes first.
fn carry_inbound(shared: Arc<VsockShared>, port: u16, mac: TcpStream) {
    let _ = mac.set_nodelay(true);
    crate::sockbuf::widen(&mac);
    let Ok(clone) = mac.try_clone() else { return };
    let key = shared.open(INBOUND_PORT, clone);
    if !shared.await_established(key, Duration::from_secs(4)) {
        tracing::debug!(port, "the agent did not accept an inbound stream");
        return;
    }
    if !shared.send(key, &port.to_be_bytes()) {
        return;
    }
    pump(shared, key, mac);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_v4_header_names_its_address_and_port() {
        let mut header = [0u8; HEADER_LEN];
        header[0] = 4;
        header[1..5].copy_from_slice(&[93, 184, 216, 34]);
        header[17..19].copy_from_slice(&443u16.to_be_bytes());
        assert_eq!(
            destination(&header),
            Some("93.184.216.34:443".parse().unwrap())
        );
    }

    #[test]
    fn the_gateway_and_the_host_alias_are_the_mac() {
        for ip in [GATEWAY, HOST_ALIAS] {
            let mut header = [0u8; HEADER_LEN];
            header[0] = 4;
            header[1..5].copy_from_slice(&ip.octets());
            header[17..19].copy_from_slice(&8080u16.to_be_bytes());
            assert_eq!(
                destination(&header),
                Some("127.0.0.1:8080".parse().unwrap())
            );
        }
    }

    #[test]
    fn a_v6_header_carries_sixteen_bytes() {
        let mut header = [0u8; HEADER_LEN];
        header[0] = 6;
        header[1..17].copy_from_slice(&Ipv6Addr::LOCALHOST.octets());
        header[17..19].copy_from_slice(&53u16.to_be_bytes());
        assert_eq!(destination(&header), Some("[::1]:53".parse().unwrap()));
    }

    #[test]
    fn an_unknown_family_is_refused() {
        let header = [9u8; HEADER_LEN];
        assert_eq!(destination(&header), None);
    }
}

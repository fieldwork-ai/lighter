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

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use crate::virtio::vsock::{Accepted, VsockShared, pump};

/// The vsock port the agent dials for an outbound stream.
pub const STREAM_PORT: u32 = 2377;

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
            for conn in accepted {
                let shared = shared.clone();
                let Accepted {
                    key,
                    device_side,
                    stream,
                } = conn;
                // The pump moves bytes between the device and its end of the
                // pair; this thread moves them between the other end and the
                // real socket.
                let pumped = shared.clone();
                let _ = std::thread::Builder::new()
                    .name("stream-pump".into())
                    .spawn(move || pump(pumped, key, device_side));
                let _ = std::thread::Builder::new()
                    .name("stream".into())
                    .spawn(move || serve(stream));
                drop(shared);
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

/// One stream: read the header, dial, copy until both directions close.
fn serve(mut guest: UnixStream) {
    let mut header = [0u8; HEADER_LEN];
    if guest.read_exact(&mut header).is_err() {
        return;
    }
    let Some(addr) = destination(&header) else {
        return;
    };
    let mac = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            // Dropping the pair closes the stream, which the agent turns into
            // a close of the container's connection: the same thing a refused
            // connect looks like through any userspace stack.
            tracing::debug!(%addr, %e, "stream: connect failed");
            return;
        }
    };
    let _ = mac.set_nodelay(true);
    crate::sockbuf::widen(&mac);
    crate::sockbuf::widen(&guest);
    let Ok(mut mac_read) = mac.try_clone() else { return };
    let mut mac_write = mac;
    let Ok(mut guest_read) = guest.try_clone() else { return };

    // guest -> the world
    let outbound = std::thread::spawn(move || {
        copy(&mut guest_read, &mut mac_write);
        let _ = mac_write.shutdown(std::net::Shutdown::Write);
    });
    // the world -> guest
    copy(&mut mac_read, &mut guest);
    let _ = guest.shutdown(std::net::Shutdown::Write);
    let _ = outbound.join();
}

/// A stream copy with a buffer sized for the link rather than for a pipe.
fn copy(from: &mut impl Read, to: &mut impl Write) {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
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
            assert_eq!(destination(&header), Some("127.0.0.1:8080".parse().unwrap()));
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

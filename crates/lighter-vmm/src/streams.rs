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

use lighter_docker::{Proto, Published};

use crate::virtio::vsock::{Accepted, ConnKey, VsockShared, pump};

/// The vsock port the agent dials for an outbound stream.
pub const STREAM_PORT: u32 = 2377;
/// The vsock port the agent answers inbound streams on.
pub const INBOUND_PORT: u32 = 2378;

/// The card's addresses for the Mac itself, as seen from the guest. A
/// container that dials the gateway or `host.docker.internal` wants the
/// Mac, and a socket to loopback is what that is here.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 1);
const HOST_ALIAS: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 254);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_LEN: usize = 19;

/// Whether streams run on the reactor (one thread for all) or on two
/// threads each. `LIGHTER_STREAM_THREADS=1` keeps the threads measurable.
fn on_threads() -> bool {
    std::env::var("LIGHTER_STREAM_THREADS").is_ok_and(|v| v != "0")
}

/// The reactor, if streams run on it.
static REACTOR: std::sync::OnceLock<Arc<crate::reactor::Reactor>> = std::sync::OnceLock::new();

/// Starts answering the agent's streams.
/// The vsock port the agent dials for UDP.
pub const UDP_PORT: u32 = 2380;
/// The vsock port the agent dials for published UDP ports.
pub const UDP_INBOUND_PORT: u32 = 2383;

pub fn start(shared: Arc<VsockShared>) -> io::Result<()> {
    let accepted = shared.listen(STREAM_PORT);
    // The reactor always runs: DNS and the guest's UDP live on it whichever
    // way the TCP streams are served. With it tied to the streams' mode, the
    // threads mode had no resolver at all and could not fetch a package —
    // an A/B that cannot boot the benchmark measures nothing.
    let reactor = crate::reactor::Reactor::start(shared.clone())?;
    let _ = REACTOR.set(reactor.clone());
    crate::dns::start(shared.clone(), reactor.clone())?;
    // The time, for the agent to ask (clock.rs): here because this is where
    // the host's answering services start, not because it is a stream.
    crate::clock::start(shared.clone())?;
    // The guest's UDP: one stream, every flow on it (the agent's udp.rs).
    let udp = shared.listen(UDP_PORT);
    let udp_reactor = reactor.clone();
    std::thread::Builder::new()
        .name("udp-accept".into())
        .spawn(move || {
            for Accepted { key } in udp {
                udp_reactor.accept_udp(key);
            }
        })?;
    // Published UDP ports: one stream the other way (the agent's
    // udp_inbound.rs), flows opened from here.
    let udp_inbound = shared.listen(UDP_INBOUND_PORT);
    let inbound_reactor = reactor.clone();
    std::thread::Builder::new()
        .name("udp-inbound-accept".into())
        .spawn(move || {
            for Accepted { key } in udp_inbound {
                inbound_reactor.accept_udp_inbound(key);
            }
        })?;
    if !on_threads() {
        std::thread::Builder::new()
            .name("streams-accept".into())
            .spawn(move || {
                for Accepted { key } in accepted {
                    reactor.accept_outbound(key);
                }
            })?;
        return Ok(());
    }
    std::thread::Builder::new()
        .name("streams".into())
        .spawn(move || {
            for Accepted { key } in accepted {
                let shared = shared.clone();
                crate::workers::run("stream", crate::qos::CONNECTION_STACK, move || {
                    serve(shared, key)
                });
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
    pump(shared, key, mac, None);
}

/// Where a publish Docker bound on every interface is bound on the Mac.
///
/// `Lan` is what Docker means by `-p 8080:80`: every interface, so another
/// machine on the network can reach the container. `Localhost` keeps every
/// publish on loopback, for a machine that must not offer its containers
/// to the network it is on. A publish with an address of its own
/// (`-p 127.0.0.1:8080:80`, `-p 192.168.1.5:8080:80`) is bound as asked
/// under either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Lan,
    Localhost,
}

/// The address the Mac binds for a published binding of `addr`.
pub fn bind_address(addr: IpAddr, scope: Scope) -> IpAddr {
    match (scope, addr) {
        (Scope::Localhost, IpAddr::V4(a)) if a.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        (Scope::Localhost, IpAddr::V6(a)) if a.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        _ => addr,
    }
}

/// Where the agent dials, inside the guest, for a publish Docker bound to
/// `addr` there: eth0's address for one on every interface (Docker's DNAT
/// rule answers there, and so does its proxy), and the address itself for
/// one Docker bound somewhere in particular (`127.0.0.1`, where only its
/// proxy answers, which is what the proxy is for).
pub fn guest_address(addr: IpAddr) -> IpAddr {
    if addr.is_unspecified() {
        crate::net::GUEST.into()
    } else {
        addr
    }
}

/// Where a connection reaches a listener bound to `addr` from this Mac:
/// the address itself, or loopback for one bound to every interface.
fn reach(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V4(a) if a.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(a) if a.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        other => other,
    }
}

/// A bound socket of `kind` (`SOCK_STREAM` or `SOCK_DGRAM`) on `addr`.
///
/// Not the standard library's bind, for one option it cannot set: a v6
/// socket here is v6 only, so that the `0.0.0.0` and `::` bindings Docker
/// reports for one publish can both be bound on the same port. Listeners
/// also get `SO_REUSEADDR`, as the standard library's do, so a port can be
/// republished while its last connections are still in TIME_WAIT.
pub(crate) fn bind_socket(addr: SocketAddr, kind: libc::c_int) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let (family, sockaddr, len) = crate::reactor::sockaddr_bytes(addr);
    // SAFETY: a plain socket(2) call.
    let fd = unsafe { libc::socket(family, kind, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl on a live descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    let one: libc::c_int = 1;
    // SAFETY: setsockopt with an int of the size it is told.
    unsafe {
        if family == libc::AF_INET6 {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                (&one as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        if kind == libc::SOCK_STREAM {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&one as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    // SAFETY: the sockaddr bytes are a correctly built sockaddr of `len`.
    if unsafe { libc::bind(fd, sockaddr.as_ptr().cast(), len) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(owned)
}

fn listen(addr: SocketAddr) -> io::Result<std::net::TcpListener> {
    use std::os::fd::AsRawFd;
    let fd = bind_socket(addr, libc::SOCK_STREAM)?;
    // SAFETY: listen on a bound socket we own.
    if unsafe { libc::listen(fd.as_raw_fd(), 128) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(std::net::TcpListener::from(fd))
}

/// Published ports, the other way round: a listener on the Mac per binding
/// Docker publishes, each accepted connection carried into the guest as a
/// vsock stream naming the port, where the agent connects to what Docker
/// has there.
pub struct PortMapper {
    shared: Arc<VsockShared>,
    scope: Scope,
    /// What is bound, by binding: a TCP listener's stop flag, or nothing
    /// for a UDP socket, which the reactor holds.
    listeners: std::sync::Mutex<
        std::collections::HashMap<Published, Option<Arc<std::sync::atomic::AtomicBool>>>,
    >,
}

impl PortMapper {
    pub fn new(shared: Arc<VsockShared>, scope: Scope) -> Arc<PortMapper> {
        Arc::new(PortMapper {
            shared,
            scope,
            listeners: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    fn expose_tcp(&self, published: Published, addr: SocketAddr) -> Result<(), String> {
        let mut listeners = self.listeners.lock().expect("port mapper poisoned");
        if listeners.contains_key(&published) {
            return Ok(());
        }
        let listener = listen(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        listeners.insert(published, Some(stop.clone()));
        let shared = self.shared.clone();
        let dst = SocketAddr::new(guest_address(published.addr), published.port);
        std::thread::Builder::new()
            .name(format!("port-{}", published.port))
            .spawn(move || {
                for accepted in listener.incoming() {
                    if stop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let Ok(mac) = accepted else { continue };
                    if !on_threads()
                        && let Some(reactor) = REACTOR.get()
                    {
                        reactor.carry_inbound(dst, mac);
                        continue;
                    }
                    let shared = shared.clone();
                    crate::workers::run("inbound", crate::qos::CONNECTION_STACK, move || {
                        carry_inbound(shared, dst, mac)
                    });
                }
            })
            .map_err(|e| e.to_string())?;
        tracing::info!(%published, %addr, %dst, "port published through a stream");
        Ok(())
    }
}

impl PortMapper {
    /// A published UDP port: a socket bound here, owned by the reactor,
    /// which carries each client's datagrams to the agent as a flow.
    fn expose_udp(&self, published: Published, addr: SocketAddr) -> Result<(), String> {
        let mut listeners = self.listeners.lock().expect("port mapper poisoned");
        if listeners.contains_key(&published) {
            return Ok(());
        }
        let reactor = REACTOR
            .get()
            .ok_or_else(|| "the reactor is not running".to_string())?;
        let fd = bind_socket(addr, libc::SOCK_DGRAM)
            .map_err(|e| format!("cannot bind udp {addr}: {e}"))?;
        let socket = std::net::UdpSocket::from(fd);
        socket.set_nonblocking(true).map_err(|e| e.to_string())?;
        crate::sockbuf::widen(&socket);
        let dst = SocketAddr::new(guest_address(published.addr), published.port);
        reactor.publish_udp(published, socket, dst);
        listeners.insert(published, None);
        tracing::info!(%published, %addr, %dst, "udp port published through a stream");
        Ok(())
    }
}

impl lighter_docker::PortMapper for PortMapper {
    fn expose(&self, published: Published) -> Result<(), String> {
        let addr = SocketAddr::new(bind_address(published.addr, self.scope), published.port);
        match published.proto {
            Proto::Tcp => self.expose_tcp(published, addr),
            Proto::Udp => self.expose_udp(published, addr),
        }
    }

    fn unexpose(&self, published: Published) -> Result<(), String> {
        let entry = self
            .listeners
            .lock()
            .expect("port mapper poisoned")
            .remove(&published);
        match entry {
            Some(Some(stop)) => {
                stop.store(true, std::sync::atomic::Ordering::Release);
                // The accept loop notices on its next connection, which this is.
                let bound = bind_address(published.addr, self.scope);
                let _ = TcpStream::connect_timeout(
                    &SocketAddr::new(reach(bound), published.port),
                    Duration::from_millis(200),
                );
            }
            Some(None) => {
                if let Some(reactor) = REACTOR.get() {
                    reactor.withdraw_udp(published);
                }
            }
            None => {}
        }
        Ok(())
    }
}

/// One accepted connection on a published port, into the guest: the
/// socket is the connection's from the start, the guest address to dial
/// goes first, in the same nineteen bytes an outbound stream opens with.
fn carry_inbound(shared: Arc<VsockShared>, dst: SocketAddr, mac: TcpStream) {
    let _ = mac.set_nodelay(true);
    crate::sockbuf::widen(&mac);
    let Ok(clone) = mac.try_clone() else { return };
    let key = shared.open(INBOUND_PORT, clone);
    if !shared.await_established(key, Duration::from_secs(4)) {
        tracing::debug!(%dst, "the agent did not accept an inbound stream");
        return;
    }
    if !shared.send(key, &crate::reactor::header_bytes(dst)) {
        return;
    }
    pump(shared, key, mac, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under `Lan` the Mac binds what Docker bound; under `Localhost` a
    /// publish on every interface is kept to loopback of its family, and a
    /// publish with an address of its own is left alone either way.
    #[test]
    fn the_localhost_scope_keeps_every_interface_publishes_on_loopback() {
        let any4: IpAddr = "0.0.0.0".parse().unwrap();
        let any6: IpAddr = "::".parse().unwrap();
        let own: IpAddr = "192.168.1.5".parse().unwrap();
        assert_eq!(bind_address(any4, Scope::Lan), any4);
        assert_eq!(bind_address(any6, Scope::Lan), any6);
        assert_eq!(
            bind_address(any4, Scope::Localhost),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            bind_address(any6, Scope::Localhost),
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        );
        assert_eq!(bind_address(own, Scope::Localhost), own);
    }

    /// Inside the guest the agent dials eth0's address for a publish on
    /// every interface and the bound address itself for any other: Docker
    /// bound `127.0.0.1` there, and only its proxy on `127.0.0.1` answers.
    #[test]
    fn the_agent_dials_where_docker_bound() {
        let any4: IpAddr = "0.0.0.0".parse().unwrap();
        let any6: IpAddr = "::".parse().unwrap();
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(guest_address(any4), IpAddr::V4(crate::net::GUEST));
        assert_eq!(guest_address(any6), IpAddr::V4(crate::net::GUEST));
        assert_eq!(guest_address(lo), lo);
    }

    /// One publish is a v4 and a v6 binding on the same port; both must
    /// bind, which is what the v6-only option is for.
    #[test]
    fn both_families_of_one_port_bind_side_by_side() {
        let v4 = listen("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = v4.local_addr().unwrap().port();
        let v6 = listen(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port)).unwrap();
        assert_eq!(v6.local_addr().unwrap().port(), port);
        assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
        assert!(TcpStream::connect(("::1", port)).is_ok());
    }

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

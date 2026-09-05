//! The link to the host: TCP over the card, to the gateway.
//!
//! Under Virtualization.framework the host terminates TCP on the gateway
//! address itself (`link.rs` in the VMM), and that is the only channel there
//! is: streams, DNS, UDP, memory offers and the file server all connect to
//! 192.168.127.1, and the host connects back to this machine's address for
//! the Docker socket, the control channel and published ports. The init's
//! netfilter rules leave connections to the gateway alone and admit the
//! host's only from the card, so no container reaches these ports.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Duration;

/// The host, as this machine reaches it.
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 1);
/// This machine's own address on the link.
pub const SELF: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 2);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Both socket buffers to `bytes`.
pub fn set_buffer(fd: &OwnedFd, bytes: u64) -> io::Result<()> {
    let size: libc::c_int = bytes.min(i32::MAX as u64) as libc::c_int;
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                std::ptr::addr_of!(size).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A connection to the host's `port`.
pub fn connect(port: u32) -> io::Result<OwnedFd> {
    let addr = SocketAddrV4::new(GATEWAY, port as u16);
    let stream = TcpStream::connect_timeout(&addr.into(), CONNECT_TIMEOUT)?;
    stream.set_nodelay(true)?;
    Ok(OwnedFd::from(stream))
}

/// A port on this machine the host connects to.
pub struct Listener {
    inner: TcpListener,
}

impl Listener {
    pub fn bind(port: u32) -> io::Result<Listener> {
        let inner = match TcpListener::bind(SocketAddrV4::new(SELF, port as u16)) {
            Ok(l) => l,
            // Before the lease has arrived (a development boot without the
            // card, say) the address is not ours yet; any address will do,
            // the netfilter rules being what keeps containers out.
            Err(_) => TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port as u16))?,
        };
        Ok(Listener { inner })
    }

    pub fn accept(&self) -> io::Result<OwnedFd> {
        let (stream, _) = self.inner.accept()?;
        let _ = stream.set_nodelay(true);
        Ok(OwnedFd::from(stream))
    }
}

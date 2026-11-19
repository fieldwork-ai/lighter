//! lighter's root helper, both sides of it.
//!
//! macOS lets a process that is not root bind a port below 1024 only on
//! the wildcard address. A publish of such a port on one address
//! (`-p 127.0.0.1:80:80`, or any port 80 under `--publish localhost`) needs
//! root, which lighter is not, and the alternative, binding every address
//! and refusing the connections that arrive elsewhere, still answers the
//! network's handshakes (#113). So the helper binds it: a launchd daemon
//! that, for a client that is lighter run by a user allowed to, binds the
//! socket asked for and passes it back (`SCM_RIGHTS`). lighter listens on it
//! and serves it like any other.
//!
//! The conversation is one line each way on a fresh connection. The client
//! sends `lighter-helper <version> bind <tcp|udp> <address> <port>`; the
//! helper answers `ok` with the socket attached, or `error <errno> <why>`
//! (the errno 0 for a refusal of its own), and closes. Nothing outlives the
//! answer: the socket is the client's.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bumped when either side's half of the conversation changes.
pub const VERSION: u32 = 1;
/// The launchd job.
pub const LABEL: &str = "dev.lighter.helper";
/// Where the installed helper's binary is copied.
pub const INSTALLED: &str = "/Library/PrivilegedHelperTools/dev.lighter.helper";
/// Its launchd job.
pub const PLIST: &str = "/Library/LaunchDaemons/dev.lighter.helper.plist";
/// Its log.
pub const LOG: &str = "/var/log/dev.lighter.helper.log";
/// Where the installed helper listens (launchd owns the socket).
pub const SOCKET: &str = "/var/run/dev.lighter.helper.sock";

/// The helper's socket: the installed one's, or `LIGHTER_HELPER_SOCKET` for
/// one run by hand (`lighter-helper --socket … --any-client`).
pub fn socket() -> PathBuf {
    std::env::var_os("LIGHTER_HELPER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SOCKET))
}

/// The protocols a published port is bound for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tcp,
    Udp,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Tcp => "tcp",
            Kind::Udp => "udp",
        }
    }

    fn sock_type(self) -> libc::c_int {
        match self {
            Kind::Tcp => libc::SOCK_STREAM,
            Kind::Udp => libc::SOCK_DGRAM,
        }
    }
}

/// A socket of `kind` bound on `addr`, as lighter binds one itself.
///
/// Not the standard library's bind, for one option it cannot set: a v6
/// socket here is v6 only, so that the `0.0.0.0` and `::` bindings Docker
/// reports for one publish can both be bound on the same port. TCP sockets
/// also get `SO_REUSEADDR`, as the standard library's do, so a port can be
/// republished while its last connections are still in TIME_WAIT.
pub fn bind_socket(addr: SocketAddr, kind: Kind) -> io::Result<OwnedFd> {
    bind_with(addr, kind, true)
}

/// A socket of `kind` bound on `addr` by the helper, as root.
///
/// Not simply [`bind_socket`]: with `SO_REUSEADDR` a bind on one address is
/// not refused by a socket on the wildcard, and the check that keeps one
/// user from binding under another's wildcard listener does not apply to
/// root, so the helper would put a container in front of sshd's `*:22` on
/// `127.0.0.1:22`, or on the Mac's LAN address. So it binds without it
/// first, and the kernel refuses (`EADDRINUSE`) whatever holds the port.
///
/// But that also refuses a port whose last connections, ended by the Mac's
/// side, are in TIME_WAIT: up to thirty seconds after every republish, with
/// nothing listening. So a TCP port refused that way is bound again with
/// `SO_REUSEADDR` if nothing accepts a connection on it: a refused connect
/// means no listener, wildcard or not, stands to lose anything.
pub fn bind_socket_as_root(addr: SocketAddr, kind: Kind) -> io::Result<OwnedFd> {
    match bind_with(addr, kind, false) {
        Err(e)
            if kind == Kind::Tcp
                && e.raw_os_error() == Some(libc::EADDRINUSE)
                && !accepting(addr) =>
        {
            bind_with(addr, kind, true)
        }
        bound => bound,
    }
}

/// Whether anything on the Mac accepts a TCP connection on `addr`. Only a
/// refusal counts as no: anything else, a timeout included, is taken as a
/// listener, so the helper errs on the side of reporting the port held.
fn accepting(addr: SocketAddr) -> bool {
    match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(_) => true,
        Err(e) => e.kind() != io::ErrorKind::ConnectionRefused,
    }
}

fn bind_with(addr: SocketAddr, kind: Kind, reuse: bool) -> io::Result<OwnedFd> {
    let (family, sockaddr, len) = sockaddr_bytes(addr);
    // SAFETY: a plain socket(2) call.
    let fd = unsafe { libc::socket(family, kind.sock_type(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
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
        if reuse && kind == Kind::Tcp {
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

/// `addr` as the bytes of a `sockaddr_in` or `sockaddr_in6`, with its
/// family and length.
pub fn sockaddr_bytes(addr: SocketAddr) -> (libc::c_int, Vec<u8>, libc::socklen_t) {
    match addr {
        SocketAddr::V4(v4) => {
            // SAFETY: a zeroed sockaddr_in is a valid one to fill in.
            let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sin.sin_len = size_of::<libc::sockaddr_in>() as u8;
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
            // SAFETY: viewing a plain struct as its bytes.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&sin as *const libc::sockaddr_in).cast::<u8>(),
                    size_of::<libc::sockaddr_in>(),
                )
            };
            (
                libc::AF_INET,
                bytes.to_vec(),
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            // SAFETY: a zeroed sockaddr_in6 is a valid one to fill in.
            let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sin6.sin6_len = size_of::<libc::sockaddr_in6>() as u8;
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            sin6.sin6_scope_id = v6.scope_id();
            // SAFETY: viewing a plain struct as its bytes.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&sin6 as *const libc::sockaddr_in6).cast::<u8>(),
                    size_of::<libc::sockaddr_in6>(),
                )
            };
            (
                libc::AF_INET6,
                bytes.to_vec(),
                size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

/// Whether the helper binds `addr` for a client: only what macOS keeps
/// from a process that is not root, a port below 1024 on one address. The
/// wildcard needs no root, and anything else a client binds itself; the
/// kernel refuses an address that is not the Mac's.
pub fn permitted(addr: SocketAddr) -> Result<(), String> {
    if addr.port() == 0 || addr.port() >= 1024 {
        return Err(format!("port {} needs no helper", addr.port()));
    }
    let ip = addr.ip();
    if ip.is_unspecified() {
        return Err("every address needs no helper".into());
    }
    if ip.is_multicast() {
        return Err(format!(
            "{ip} is a multicast group, not an address of the Mac"
        ));
    }
    Ok(())
}

/// What a client asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Bind(Kind, SocketAddr),
}

/// The line a client sends.
pub fn hello(request: Request) -> String {
    match request {
        Request::Bind(kind, addr) => format!(
            "lighter-helper {VERSION} bind {} {} {}\n",
            kind.word(),
            addr.ip(),
            addr.port()
        ),
    }
}

/// What a client's line asks for, with the version it spoke.
pub fn parse_hello(line: &str) -> Result<(u32, Request), String> {
    let mut words = line.split_whitespace();
    if words.next() != Some("lighter-helper") {
        return Err("not a lighter-helper hello".into());
    }
    let version = words
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or("no version")?;
    match words.next() {
        Some("bind") => {
            let kind = match words.next() {
                Some("tcp") => Kind::Tcp,
                Some("udp") => Kind::Udp,
                other => return Err(format!("bad protocol {other:?}")),
            };
            let ip: IpAddr = words
                .next()
                .and_then(|a| a.parse().ok())
                .ok_or("bad address")?;
            let port: u16 = words
                .next()
                .and_then(|p| p.parse().ok())
                .ok_or("bad port")?;
            if words.next().is_some() {
                return Err("trailing words".into());
            }
            Ok((version, Request::Bind(kind, SocketAddr::new(ip, port))))
        }
        other => Err(format!("unknown request {other:?}")),
    }
}

/// Asks the helper at `socket` to bind `addr` for `kind`.
pub fn bind(socket: &Path, kind: Kind, addr: SocketAddr) -> io::Result<OwnedFd> {
    let mut control = UnixStream::connect(socket)?;
    // A bind is microseconds and launchd starts the helper in a fraction
    // of a second; the caller holds the port forwards meanwhile.
    control.set_read_timeout(Some(Duration::from_secs(3)))?;
    control.set_write_timeout(Some(Duration::from_secs(3)))?;
    control.write_all(hello(Request::Bind(kind, addr)).as_bytes())?;
    let (line, fd) = recv_line_with_fd(&control)?;
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("error ") {
        return Err(refusal(rest));
    }
    if line != "ok" {
        return Err(io::Error::other(format!("unexpected answer {line:?}")));
    }
    fd.ok_or_else(|| io::Error::other("the helper sent no socket"))
}

/// The helper's answer for a request it could not do: the errno, so a
/// client tells a port held elsewhere from any other refusal, and why.
pub fn error_line(e: &io::Error) -> String {
    format!("error {} {e}\n", e.raw_os_error().unwrap_or(0))
}

/// The error an `error` answer stands for: the OS's own when the helper
/// passed one on.
fn refusal(rest: &str) -> io::Error {
    let (errno, why) = rest.split_once(' ').unwrap_or((rest, ""));
    match errno.parse::<i32>() {
        Ok(0) => io::Error::other(why.to_string()),
        Ok(n) => io::Error::from_raw_os_error(n),
        Err(_) => io::Error::other(rest.to_string()),
    }
}

/// Sends one line, with `fd` attached when given.
pub fn send_line_with_fd(stream: &UnixStream, line: &str, fd: Option<RawFd>) -> io::Result<()> {
    let bytes = line.as_bytes();
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    // Room for one descriptor's control message.
    let mut cmsg = [0u8; 64];
    // SAFETY: a zeroed msghdr filled with pointers to locals that outlive
    // the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        // SAFETY: CMSG_SPACE and the header writes stay within `cmsg`.
        unsafe {
            let space = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as usize;
            msg.msg_control = cmsg.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let header = libc::CMSG_FIRSTHDR(&msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        }
    }
    // SAFETY: a msghdr describing live buffers.
    if unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads one line, and a descriptor if one came with it.
pub fn recv_line_with_fd(stream: &UnixStream) -> io::Result<(String, Option<OwnedFd>)> {
    let mut buf = [0u8; 512];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cmsg = [0u8; 64];
    // SAFETY: as in send_line_with_fd.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = cmsg.len() as _;
    // SAFETY: a msghdr describing live buffers.
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the helper closed the connection",
        ));
    }
    let mut fd = None;
    // SAFETY: walking the control messages the kernel wrote into `cmsg`.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let raw = std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>());
                // Close-on-exec does not travel with a descriptor, and macOS
                // has no MSG_CMSG_CLOEXEC: set here, or every child the
                // machine runs (scutil, sample) holds the published port.
                libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC);
                fd = Some(OwnedFd::from_raw_fd(raw));
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other(
            "a descriptor did not fit the control message",
        ));
    }
    Ok((String::from_utf8_lossy(&buf[..n as usize]).into_owned(), fd))
}

/// Reads the client's hello, a line at most 256 bytes long.
pub fn read_hello(stream: &UnixStream) -> io::Result<String> {
    let mut line = String::new();
    BufReader::new(stream.take(256)).read_line(&mut line)?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hello_round_trips() {
        for (kind, addr) in [
            (Kind::Tcp, "127.0.0.1:80"),
            (Kind::Udp, "[::1]:443"),
            (Kind::Tcp, "192.168.50.25:443"),
        ] {
            let request = Request::Bind(kind, addr.parse().unwrap());
            assert_eq!(parse_hello(&hello(request)).unwrap(), (VERSION, request));
        }
    }

    /// A port the helper found held comes back as the OS's own error, so
    /// the client reports it as held; the helper's own refusals do not.
    #[test]
    fn an_error_keeps_its_errno() {
        let held = io::Error::from_raw_os_error(libc::EADDRINUSE);
        let line = error_line(&held);
        let back = refusal(line.trim().strip_prefix("error ").unwrap());
        assert_eq!(back.raw_os_error(), Some(libc::EADDRINUSE));
        let own = error_line(&io::Error::other("port 8080 needs no helper"));
        let back = refusal(own.trim().strip_prefix("error ").unwrap());
        assert_eq!(
            (back.raw_os_error(), back.to_string().as_str()),
            (None, "port 8080 needs no helper")
        );
    }

    #[test]
    fn a_malformed_hello_is_refused() {
        for line in [
            "GET / HTTP/1.1",
            "lighter-bridge 1 en1 02:00:00:00:00:01",
            "lighter-helper x bind tcp 127.0.0.1 80",
            "lighter-helper 1 bridge en1",
            "lighter-helper 1 bind sctp 127.0.0.1 80",
            "lighter-helper 1 bind tcp localhost 80",
            "lighter-helper 1 bind tcp 127.0.0.1 65536",
            "lighter-helper 1 bind tcp 127.0.0.1 80 extra",
        ] {
            assert!(parse_hello(line).is_err(), "{line}");
        }
    }

    /// Only what root alone may bind: a port below 1024, on one address.
    #[test]
    fn the_helper_binds_only_what_needs_root() {
        for ok in [
            "127.0.0.1:80",
            "[::1]:443",
            "192.168.50.25:22",
            "127.0.0.1:1023",
        ] {
            assert!(permitted(ok.parse().unwrap()).is_ok(), "{ok}");
        }
        for refused in [
            "127.0.0.1:1024",
            "127.0.0.1:8080",
            "127.0.0.1:0",
            "0.0.0.0:80",
            "[::]:443",
            "224.0.0.251:80",
        ] {
            assert!(permitted(refused.parse().unwrap()).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_descriptor_travels_with_its_line() {
        let (a, b) = UnixStream::pair().unwrap();
        let socket = bind_socket("127.0.0.1:0".parse().unwrap(), Kind::Udp).unwrap();
        send_line_with_fd(&a, "ok\n", Some(socket.as_raw_fd())).unwrap();
        let (line, fd) = recv_line_with_fd(&b).unwrap();
        assert_eq!(line, "ok\n");
        let fd = fd.expect("a descriptor");
        // SAFETY: fcntl on a live descriptor.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(
            flags & libc::FD_CLOEXEC,
            0,
            "a received socket is close-on-exec"
        );
        let received = std::net::UdpSocket::from(fd);
        let original = std::net::UdpSocket::from(socket);
        assert_eq!(
            received.local_addr().unwrap(),
            original.local_addr().unwrap()
        );
    }

    /// Under a wildcard listener a bind on one address is refused, as the
    /// helper binds. Shown here for one user on a high port; as root the
    /// uid check that would also refuse it with SO_REUSEADDR does not
    /// apply, which is why the helper binds without it first.
    #[test]
    fn a_root_bind_does_not_slip_under_a_wildcard_listener() {
        let wildcard = std::net::TcpListener::from(
            bind_socket("0.0.0.0:0".parse().unwrap(), Kind::Tcp).unwrap(),
        );
        // SAFETY: listen on a bound socket we own.
        assert_eq!(unsafe { libc::listen(wildcard.as_raw_fd(), 1) }, 0);
        let port = wildcard.local_addr().unwrap().port();
        let one = SocketAddr::new("127.0.0.1".parse().unwrap(), port);
        let refused = bind_socket_as_root(one, Kind::Tcp).expect_err("refused under the wildcard");
        assert_eq!(refused.raw_os_error(), Some(libc::EADDRINUSE));
    }

    /// A port whose only trace is a connection in TIME_WAIT, ended by our
    /// side, binds again at once: the strict bind is refused, nothing
    /// accepts, and the bind with SO_REUSEADDR goes through.
    #[test]
    fn a_port_left_in_time_wait_binds_again() {
        let listener = std::net::TcpListener::from(
            bind_socket("127.0.0.1:0".parse().unwrap(), Kind::Tcp).unwrap(),
        );
        // SAFETY: listen on a bound socket we own.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (served, _) = listener.accept().unwrap();
        // The server closes first, so the TIME_WAIT is on the listening port.
        drop(served);
        let mut byte = [0u8; 1];
        let _ = std::io::Read::read(&mut &client, &mut byte);
        drop(client);
        drop(listener);
        assert_eq!(
            bind_with(addr, Kind::Tcp, false)
                .err()
                .and_then(|e| e.raw_os_error()),
            Some(libc::EADDRINUSE),
            "the strict bind is refused while the connection is in TIME_WAIT"
        );
        assert!(bind_socket_as_root(addr, Kind::Tcp).is_ok());
    }

    /// As a client gets it: v6 sockets v6 only, so `0.0.0.0` and `::` share
    /// a port.
    #[test]
    fn both_families_bind_one_port() {
        let v4 = std::net::TcpListener::from(
            bind_socket("0.0.0.0:0".parse().unwrap(), Kind::Tcp).unwrap(),
        );
        let port = v4.local_addr().unwrap().port();
        assert!(bind_socket(SocketAddr::new("::".parse().unwrap(), port), Kind::Tcp).is_ok());
    }
}

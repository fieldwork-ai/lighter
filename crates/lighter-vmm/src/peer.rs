//! Whether a host client that has sent its end-of-stream is still there.
//!
//! A client that half-closes is waiting for the rest of the reply: `docker
//! run` with no stdin shuts its write side and reads the container's
//! output for as long as the container runs. One that closes fully has
//! gone. Both look the same once their FIN has been read, and a stream
//! whose guest end then stays silent was held for good on either, with its
//! threads or its reactor entry (#87). What tells them apart:
//!
//! - **A Unix socket** fails a zero-length send with `EPIPE` once the peer
//!   has closed both ways, and only then.
//! - **A TCP socket** cannot tell, because the peer's kernel answers for a
//!   closed socket until its FIN_WAIT_2 runs out (`net.inet.tcp.fin_timeout`,
//!   60 s on macOS, and the same by default on Linux), and then answers
//!   nothing at all. So keepalive goes on once the client's FIN arrives, and
//!   the first probe after that answers with a reset, which the socket
//!   reports as its error. A half-closed client answers every probe. One
//!   that has vanished, a laptop asleep on the LAN, answers none and ends in
//!   `ETIMEDOUT`. Measured on macOS loopback: a fully closed client found
//!   after 60.2 s, a half-closed one never in 120.
//!
//! Diagnosed by @petrfilip, whose zero-length send this is.

use std::os::fd::RawFd;

/// How often a half-closed stream's client is looked at.
#[cfg(not(test))]
pub const PROBE_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
pub const PROBE_EVERY: std::time::Duration = std::time::Duration::from_millis(200);

/// Keepalive on a half-closed TCP client: the first probe after this much
/// quiet, then one every `KEEPALIVE_INTERVAL`, `KEEPALIVE_COUNT` unanswered
/// before the connection is dropped. A vanished client is found in about
/// thirty seconds; one that closed, a probe interval after its kernel let go.
const KEEPALIVE_IDLE: libc::c_int = 10;
const KEEPALIVE_INTERVAL: libc::c_int = 5;
const KEEPALIVE_COUNT: libc::c_int = 4;

/// Starts keepalive on `fd` if it is a TCP socket. Called once its client's
/// FIN has been read; a socket that refuses is only looked at less closely.
pub fn watch(fd: RawFd) {
    if !crate::sockbuf::is_tcp(fd) {
        return;
    }
    for (level, name, value) in [
        (libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1),
        (libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, KEEPALIVE_IDLE),
        (libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, KEEPALIVE_INTERVAL),
        (libc::IPPROTO_TCP, libc::TCP_KEEPCNT, KEEPALIVE_COUNT),
    ] {
        // SAFETY: an int lives at the pointer for the call's duration and
        // the length is its size.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                std::ptr::addr_of!(value).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

/// Whether the client of `fd`, which has already sent its end-of-stream, has
/// gone for good: closed both ways, reset, or not answering keepalive.
pub fn gone(fd: RawFd) -> bool {
    let mut error: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: an int lives at the pointer for the call's duration and the
    // length is its size.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            std::ptr::addr_of_mut!(error).cast(),
            &mut len,
        )
    };
    if rc != 0 || error != 0 {
        return true;
    }
    // SAFETY: a zero-length send reads no memory.
    let n = unsafe { libc::send(fd, std::ptr::null(), 0, libc::MSG_NOSIGNAL) };
    n < 0
        && matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPIPE | libc::ECONNRESET | libc::ENOTCONN)
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_unix_client_that_half_closed_is_still_there() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs.shutdown(Shutdown::Write).unwrap();
        assert_eq!((&ours).read(&mut [0]).unwrap(), 0);
        assert!(!gone(ours.as_raw_fd()));
        drop(theirs);
        assert!(gone(ours.as_raw_fd()));
    }

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    #[test]
    fn a_tcp_client_that_half_closed_is_still_there_and_watched() {
        let (ours, theirs) = tcp_pair();
        theirs.shutdown(Shutdown::Write).unwrap();
        assert_eq!((&ours).read(&mut [0]).unwrap(), 0);
        watch(ours.as_raw_fd());
        let mut on: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: as in `gone`.
        unsafe {
            libc::getsockopt(
                ours.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_KEEPALIVE,
                std::ptr::addr_of_mut!(on).cast(),
                &mut len,
            );
        }
        assert_ne!(on, 0, "keepalive is on");
        assert!(!gone(ours.as_raw_fd()));
    }

    /// What the peer's kernel sends once it has let go of a closed socket:
    /// a reset, here at once by closing with a zero linger.
    #[test]
    fn a_tcp_client_that_reset_after_its_fin_is_gone() {
        let (ours, theirs) = tcp_pair();
        theirs.shutdown(Shutdown::Write).unwrap();
        assert_eq!((&ours).read(&mut [0]).unwrap(), 0);
        watch(ours.as_raw_fd());
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: a linger lives at the pointer for the call's duration and
        // the length is its size.
        let rc = unsafe {
            libc::setsockopt(
                theirs.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                std::ptr::addr_of!(linger).cast(),
                size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0);
        drop(theirs);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !gone(ours.as_raw_fd()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the reset was not seen"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The real thing, which takes the Mac's FIN_WAIT_2 timeout:
    /// `cargo test -p lighter-vmm --lib peer -- --ignored`.
    #[test]
    #[ignore = "waits out net.inet.tcp.fin_timeout, a minute"]
    fn a_tcp_client_that_closed_fully_is_found_by_keepalive() {
        let (ours, theirs) = tcp_pair();
        drop(theirs);
        assert_eq!((&ours).read(&mut [0]).unwrap(), 0);
        watch(ours.as_raw_fd());
        let started = std::time::Instant::now();
        while !gone(ours.as_raw_fd()) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(120),
                "not found in two minutes"
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        eprintln!("found after {:?}", started.elapsed());
    }
}

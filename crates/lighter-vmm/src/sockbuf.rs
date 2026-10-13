//! Socket buffers sized for a link, not for a pipe.
//!
//! macOS gives a unix stream socket an 8 KiB buffer in each direction
//! (`net.local.stream.sendspace`), which is the size of a shell pipe and was
//! the whole of a 13 Gbit/s ceiling: every byte between the device and its
//! reader crossed the kernel eight kilobytes at a time, with a wakeup for
//! each. The same pair with a megabyte of buffer moves two hundred. So every
//! unix socket this process owns an end of is widened here, to the largest
//! the kernel allows below `kern.ipc.maxsockbuf`.
//!
//! TCP sockets are not: a fixed buffer turns off macOS's autotuning, which
//! grows a connection's buffers to the same 4 MiB (`autosndbufmax`) when it
//! moves data and keeps them small when it does not. Widened, two thousand
//! clients of a published port that had stopped reading held 4 MiB each of
//! the Mac's network buffers (`kern.ipc.nmbclusters`, 512 MiB), every write
//! on the Mac then failed with ENOBUFS, and a request to an unrelated port
//! came back empty (#84). Left to autotuning, the four stream paths read
//! level on the M5 Ultra, three rounds each: published port 82.7/80.5/82.9
//! Gbit/s against 80.4/80.7/81.9, its reverse 93.6/95.9/95.2 against
//! 91.3/96.1/96.5. `LIGHTER_TCP_WIDEN=1` widens them as before.

use std::os::fd::AsRawFd;

/// What we ask for, and what we settle for, in order.
const SIZES: [libc::c_int; 3] = [4 << 20, 1 << 20, 256 << 10];

/// Widens both directions of `socket`. Best effort: a socket that refuses is
/// left as it was, and nothing depends on the outcome but speed.
pub fn widen(socket: &impl AsRawFd) {
    let fd = socket.as_raw_fd();
    static WIDEN_TCP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*WIDEN_TCP.get_or_init(|| std::env::var("LIGHTER_TCP_WIDEN").is_ok_and(|v| v == "1"))
        && is_tcp(fd)
    {
        return;
    }
    for name in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        for size in SIZES {
            // SAFETY: an int lives at the pointer for the call's duration
            // and the length is its size.
            let rc = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    name,
                    std::ptr::addr_of!(size).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if rc == 0 {
                break;
            }
        }
    }
}

/// A TCP socket: a stream socket of either internet family. Darwin has no
/// `SO_PROTOCOL` to ask directly.
fn is_tcp(fd: libc::c_int) -> bool {
    let mut kind: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: an int lives at the pointer for the call's duration and the
    // length is its size.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            std::ptr::addr_of_mut!(kind).cast(),
            &mut len,
        )
    };
    if rc != 0 || kind != libc::SOCK_STREAM {
        return false;
    }
    // SAFETY: a zeroed sockaddr_storage is a valid out-parameter, its size given.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let rc = unsafe { libc::getsockname(fd, std::ptr::addr_of_mut!(addr).cast(), &mut len) };
    rc == 0 && matches!(i32::from(addr.ss_family), libc::AF_INET | libc::AF_INET6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tcp_socket_is_left_to_autotuning() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        assert!(is_tcp(tcp.as_raw_fd()));
        let (unix, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!is_tcp(unix.as_raw_fd()));
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(!is_tcp(udp.as_raw_fd()));
    }

    #[test]
    fn a_unix_socket_is_widened_past_the_default() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        widen(&a);
        let mut size: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                a.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                std::ptr::addr_of_mut!(size).cast(),
                &mut len,
            )
        };
        assert_eq!(rc, 0);
        assert!(size >= 256 << 10, "got {size}");
    }
}

//! Socket buffers sized for a link, not for a pipe.
//!
//! macOS gives a unix stream socket an 8 KiB buffer in each direction
//! (`net.local.stream.sendspace`), which is the size of a shell pipe and was
//! the whole of a 13 Gbit/s ceiling: every byte between the device and its
//! reader crossed the kernel eight kilobytes at a time, with a wakeup for
//! each. The same pair with a megabyte of buffer moves two hundred. So every
//! socket this process owns an end of is widened here, to the largest the
//! kernel allows below `kern.ipc.maxsockbuf`.

use std::os::fd::AsRawFd;

/// What we ask for, and what we settle for, in order.
const SIZES: [libc::c_int; 3] = [4 << 20, 1 << 20, 256 << 10];

/// Widens both directions of `socket`. Best effort: a socket that refuses is
/// left as it was, and nothing depends on the outcome but speed.
pub fn widen(socket: &impl AsRawFd) {
    let fd = socket.as_raw_fd();
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

/// Sets both buffers of `socket` to `bytes`.
pub fn widen_to(socket: &impl AsRawFd, bytes: i32) {
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        // SAFETY: setsockopt with an int we own on a descriptor the caller holds.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                std::ptr::addr_of!(bytes).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

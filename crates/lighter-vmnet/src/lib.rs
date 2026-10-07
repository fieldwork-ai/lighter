//! A vmnet bridged interface, relayed to a datagram socket.
//!
//! [`Bridge::start`] puts an interface on one of the Mac's network cards and
//! gives back the near end of a `SOCK_DGRAM` socket pair: each datagram
//! read from it is a frame from the network, and each written is a frame
//! for it. The relay (`relay.c`) holds the far end. Starting a bridged
//! interface needs root, or `com.apple.vm.networking`; the root helper runs
//! this and passes the near end on, and a lighter with the entitlement runs
//! it itself. Either way the VMM sees the same socket.

pub mod helper;

use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_void};

unsafe extern "C" {
    fn lighter_bridge_start(
        ifname: *const c_char,
        mac: *const u8,
        fd: c_int,
        mtu: *mut u32,
        max_packet: *mut u32,
        err: *mut c_char,
        errlen: usize,
    ) -> *mut c_void;
    fn lighter_bridge_stop(bridge: *mut c_void);
    fn lighter_bridge_counters(bridge: *mut c_void, out: *mut u64);
    fn lighter_bridge_interfaces(buf: *mut c_char, len: usize) -> c_int;
}

/// How much each end of the pair may hold. macOS's defaults for a datagram
/// socket hold about two frames, which drops a burst of discovery replies;
/// four megabytes is a few thousand.
const SOCKET_BUFFER: c_int = 4 << 20;

/// A bridged interface and the relay feeding it. Dropping it stops both.
pub struct Bridge {
    raw: *mut c_void,
    // The relay's end; it reads and writes this from its queue.
    _relayed: OwnedFd,
    pub mtu: u32,
    pub max_packet: u32,
}

// SAFETY: the handle is only passed back to the relay, which serializes
// everything on its own queue.
unsafe impl Send for Bridge {}
unsafe impl Sync for Bridge {}

/// Frames moved, and dropped for want of room on either side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub to_guest: u64,
    pub to_network: u64,
    pub dropped: u64,
}

impl Bridge {
    /// Bridges `interface` (`en0`, `en1`). Returns the bridge and the near
    /// end of its socket.
    pub fn start(interface: &str, mac: [u8; 6]) -> Result<(Bridge, OwnedFd), String> {
        let (near, far) = socket_pair().map_err(|e| format!("socketpair: {e}"))?;
        let name = CString::new(interface).map_err(|_| "bad interface name".to_string())?;
        let (mut mtu, mut max_packet) = (0u32, 0u32);
        let mut err = [0 as c_char; 256];
        // SAFETY: a NUL-terminated name, a six-byte MAC, a live descriptor
        // the relay keeps using (held in the Bridge), and out-pointers to
        // locals.
        let raw = unsafe {
            lighter_bridge_start(
                name.as_ptr(),
                mac.as_ptr(),
                far.as_raw_fd(),
                &mut mtu,
                &mut max_packet,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if raw.is_null() {
            // SAFETY: the relay NUL-terminates what it writes.
            let why = unsafe { CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(why);
        }
        Ok((
            Bridge {
                raw,
                _relayed: far,
                mtu,
                max_packet,
            },
            near,
        ))
    }

    pub fn counters(&self) -> Counters {
        let mut out = [0u64; 3];
        // SAFETY: a live bridge and a three-element array.
        unsafe { lighter_bridge_counters(self.raw, out.as_mut_ptr()) };
        Counters {
            to_guest: out[0],
            to_network: out[1],
            dropped: out[2],
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // SAFETY: the bridge started and has not been stopped.
        unsafe { lighter_bridge_stop(self.raw) };
    }
}

/// The interfaces vmnet can bridge. Needs no privilege.
pub fn interfaces() -> Vec<String> {
    let mut buf = vec![0 as c_char; 4096];
    // SAFETY: a buffer of the length given.
    let n = unsafe { lighter_bridge_interfaces(buf.as_mut_ptr(), buf.len()) };
    if n < 0 {
        return Vec::new();
    }
    // SAFETY: NUL-terminated by the relay.
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .lines()
        .map(String::from)
        .collect()
}

/// A datagram socket pair with room for bursts.
pub fn socket_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: a two-element array for the pair.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors we own.
    let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let size = SOCKET_BUFFER;
    for fd in [&a, &b] {
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            // SAFETY: an int option on a live socket.
            unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    std::ptr::addr_of!(size).cast(),
                    size_of::<c_int>() as libc::socklen_t,
                );
            }
        }
        // SAFETY: plain fcntl.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pair_carries_a_frame_a_datagram() {
        let (a, b) = socket_pair().unwrap();
        let frame = vec![0xabu8; 1514];
        // SAFETY: buffers of the lengths given, on live sockets.
        let sent = unsafe { libc::send(a.as_raw_fd(), frame.as_ptr().cast(), frame.len(), 0) };
        assert_eq!(sent, 1514);
        let mut got = vec![0u8; 2048];
        let n = unsafe { libc::recv(b.as_raw_fd(), got.as_mut_ptr().cast(), got.len(), 0) };
        assert_eq!(n, 1514);
    }

    #[test]
    fn a_burst_fits() {
        let (a, _b) = socket_pair().unwrap();
        let frame = vec![0u8; 1514];
        for i in 0..1000 {
            // SAFETY: as above.
            let sent = unsafe {
                libc::send(
                    a.as_raw_fd(),
                    frame.as_ptr().cast(),
                    frame.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            assert_eq!(sent, 1514, "frame {i} of a burst of 1000");
        }
    }

    #[test]
    fn bridging_needs_privilege_and_says_so() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let err = Bridge::start("en0", [2, 0, 0, 0, 0, 1])
            .err()
            .expect("unprivileged bridging must fail");
        assert!(err.contains("root") || err.contains("vmnet"), "{err}");
    }

    #[test]
    fn the_bridgeable_interfaces_are_listed_by_name() {
        // A Mac's cards, or none on a virtual one (a CI runner): names only.
        for name in interfaces() {
            assert!(
                !name.is_empty()
                    && name.len() <= 15
                    && name.bytes().all(|b| b.is_ascii_alphanumeric()),
                "{name:?}"
            );
        }
    }
}

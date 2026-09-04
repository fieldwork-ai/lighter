//! AF_VSOCK, via libc directly.
//!
//! There are crates for this. It is forty lines of `socket`, `bind`, `listen`
//! and `accept` against a struct with four fields, and writing it out means the
//! guest agent's entire dependency list is `libc` — which matters more than it
//! looks, because this binary is baked into an initramfs that has to stay small
//! and has to build reproducibly in a container with no network.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

/// `AF_VSOCK`. Not in libc's constants for every target, and it is stable ABI.
const AF_VSOCK: libc::sa_family_t = 40;

/// Accept from any CID.
const VMADDR_CID_ANY: u32 = u32::MAX;

/// `struct sockaddr_vm`.
///
/// The layout is fixed by the kernel's uapi header; the zero padding is part of
/// it and must be present and zeroed, not omitted.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrVm {
    svm_family: libc::sa_family_t,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
    svm_zero: [u8; 4],
}

impl SockaddrVm {
    fn new(cid: u32, port: u32) -> SockaddrVm {
        SockaddrVm {
            svm_family: AF_VSOCK,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: cid,
            svm_zero: [0; 4],
        }
    }
}

/// The well-known CID of the host.
pub const VMADDR_CID_HOST: u32 = 2;

/// `SOL_SOCKET`-level option names are the kernel's for AF_VSOCK: the
/// socket's receive buffer, which is the credit it advertises to its peer,
/// and the ceiling it may be raised to. Sixteen-bit ports and a 256 KiB
/// window is a Docker socket; a stream carrying a container's traffic
/// wants a window the size of a millisecond of the link.
const SO_VM_SOCKETS_BUFFER_SIZE: libc::c_int = 0;
const SO_VM_SOCKETS_BUFFER_MAX_SIZE: libc::c_int = 2;

/// Sets the credit window a socket advertises to its peer.
pub fn set_buffer(fd: &OwnedFd, bytes: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    for name in [SO_VM_SOCKETS_BUFFER_MAX_SIZE, SO_VM_SOCKETS_BUFFER_SIZE] {
        // SAFETY: a u64 lives at the pointer for the call's duration, and
        // the length is its size.
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                AF_VSOCK as libc::c_int,
                name,
                std::ptr::addr_of!(bytes).cast(),
                size_of::<u64>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Connects to a port on the host.
pub fn connect(port: u32) -> io::Result<OwnedFd> {
    // SAFETY: a plain socket(2) call with constant arguments.
    let raw = unsafe { libc::socket(AF_VSOCK as libc::c_int, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let addr = SockaddrVm::new(VMADDR_CID_HOST, port);
    // SAFETY: `addr` is a correctly-shaped sockaddr_vm living until the call
    // returns, and its length is its own size.
    let rc = unsafe {
        libc::connect(
            raw,
            std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
            size_of::<SockaddrVm>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// A listening vsock socket.
pub struct VsockListener {
    fd: OwnedFd,
}

impl VsockListener {
    pub fn bind(port: u32) -> io::Result<VsockListener> {
        // SAFETY: a plain socket(2) call with constant arguments.
        let raw = unsafe { libc::socket(AF_VSOCK as libc::c_int, libc::SOCK_STREAM, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh fd we own and have not registered anywhere.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let addr = SockaddrVm::new(VMADDR_CID_ANY, port);
        // SAFETY: `addr` is a correctly-shaped sockaddr_vm living until the
        // call returns, and its length is its own size.
        let rc = unsafe {
            libc::bind(
                raw,
                std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
                size_of::<SockaddrVm>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `raw` is bound and owned.
        if unsafe { libc::listen(raw, 128) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(VsockListener { fd })
    }

    /// Blocks for the next connection.
    pub fn accept(&self) -> io::Result<OwnedFd> {
        use std::os::fd::AsRawFd;
        // SAFETY: accepting with no interest in the peer address, which is what
        // null/null means to accept(2).
        let raw = unsafe {
            libc::accept(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh accepted fd that we now own.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }
}

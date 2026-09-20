//! Linux syscalls, aarch64. The library links no libc, so this is all of the
//! operating system it uses: a TCP socket to the host, anonymous memory for
//! the allocator, and the process environment for the host's address.
use core::arch::asm;

pub const AF_INET: u64 = 2;
pub const SOCK_STREAM: u64 = 1;
const SYS_SOCKET: u64 = 198;
const SYS_CONNECT: u64 = 203;
const SYS_READ: u64 = 63;
const SYS_WRITE: u64 = 64;
const SYS_CLOSE: u64 = 57;
const SYS_MMAP: u64 = 222;
const SYS_MUNMAP: u64 = 215;
const SYS_OPENAT: u64 = 56;
const SYS_EXIT_GROUP: u64 = 94;
const SYS_SETSOCKOPT: u64 = 208;

#[inline(always)]
unsafe fn syscall(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") a0 as i64 => ret, in("x1") a1, in("x2") a2, in("x3") a3, in("x4") a4, in("x5") a5, options(nostack));
    }
    ret
}

pub fn socket(domain: u64, ty: u64, proto: u64) -> i64 {
    unsafe { syscall(SYS_SOCKET, domain, ty, proto, 0, 0, 0) }
}

#[repr(C)]
pub struct SockAddrIn {
    pub family: u16,
    pub port: u16,
    pub addr: [u8; 4],
    pub zero: [u8; 8],
}

pub fn connect(fd: i64, addr: &SockAddrIn) -> i64 {
    unsafe { syscall(SYS_CONNECT, fd as u64, addr as *const SockAddrIn as u64, core::mem::size_of::<SockAddrIn>() as u64, 0, 0, 0) }
}

pub fn setsockopt_int(fd: i64, level: u64, name: u64, value: i32) -> i64 {
    unsafe { syscall(SYS_SETSOCKOPT, fd as u64, level, name, &value as *const i32 as u64, 4, 0) }
}

pub fn read(fd: i64, buf: &mut [u8]) -> i64 {
    unsafe { syscall(SYS_READ, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0) }
}

pub fn write(fd: i64, buf: &[u8]) -> i64 {
    unsafe { syscall(SYS_WRITE, fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0) }
}

pub fn close(fd: i64) -> i64 {
    unsafe { syscall(SYS_CLOSE, fd as u64, 0, 0, 0, 0, 0) }
}

pub fn open_read(path: &[u8]) -> i64 {
    // AT_FDCWD = -100, O_RDONLY | O_CLOEXEC
    unsafe { syscall(SYS_OPENAT, (-100i64) as u64, path.as_ptr() as u64, 0o2000000, 0, 0, 0) }
}

pub fn mmap_anonymous(len: usize) -> *mut u8 {
    // PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE
    let r = unsafe { syscall(SYS_MMAP, 0, len as u64, 3, 0x02 | 0x20 | 0x4000, u64::MAX, 0) };
    if r < 0 { core::ptr::null_mut() } else { r as *mut u8 }
}

pub fn munmap(ptr: *mut u8, len: usize) {
    unsafe { syscall(SYS_MUNMAP, ptr as u64, len as u64, 0, 0, 0, 0) };
}

pub fn exit_group(code: i32) -> ! {
    unsafe { syscall(SYS_EXIT_GROUP, code as u64, 0, 0, 0, 0, 0) };
    loop {}
}

/// Reads all of `fd` into `buf`, up to its capacity.
pub fn read_exact(fd: i64, buf: &mut [u8]) -> bool {
    let mut at = 0;
    while at < buf.len() {
        let n = read(fd, &mut buf[at..]);
        if n == -4 { continue; } // EINTR
        if n <= 0 { return false; }
        at += n as usize;
    }
    true
}

pub fn write_all(fd: i64, buf: &[u8]) -> bool {
    let mut at = 0;
    while at < buf.len() {
        let n = write(fd, &buf[at..]);
        if n == -4 { continue; }
        if n <= 0 { return false; }
        at += n as usize;
    }
    true
}

/// The value of an environment variable, from /proc/self/environ.
pub fn env(name: &[u8], out: &mut [u8]) -> Option<usize> {
    let fd = open_read(b"/proc/self/environ\0");
    if fd < 0 { return None; }
    let mut buf = [0u8; 65536];
    let mut len = 0;
    loop {
        let n = read(fd, &mut buf[len..]);
        if n <= 0 || len + n as usize >= buf.len() { break; }
        len += n as usize;
    }
    close(fd);
    for entry in buf[..len].split(|b| *b == 0) {
        if entry.len() > name.len() && &entry[..name.len()] == name && entry[name.len()] == b'=' {
            let v = &entry[name.len() + 1..];
            let n = v.len().min(out.len());
            out[..n].copy_from_slice(&v[..n]);
            return Some(n);
        }
    }
    None
}

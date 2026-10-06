//! A doorbell for listeners: the kernel says when one may have come or gone.
//!
//! Nothing announces a server starting to listen, and polling for one put
//! up to a second between a host-network container's `listen()` and its
//! port on the Mac, waking both sides for as long as such a container ran.
//! Four programs on the root cgroup, which every socket in the guest is
//! beneath, ring a BPF ring buffer instead:
//!
//!   - `sock_ops`, at `TCP_LISTEN_CB`: a TCP socket is listening;
//!   - `post_bind4`/`post_bind6`: a UDP socket bound a port below the
//!     ephemeral range, where services are and lookups are not;
//!   - `sock_release`: a TCP listener closes, or a UDP socket that was never
//!     connected (a service's, or a lookup that used `sendto`).
//!
//! The bell carries nothing and decides nothing: whoever hears it asks
//! `sock_diag` again, which stays the one account of what listens, so a
//! bell rung for something that turns out not to matter costs one scan.
//! Every program allows what it sees; a doorbell must never be what fails a
//! `bind()`. They are attached by links, which go with this process, so an
//! agent that restarts cannot stack them.
//!
//! Only a socket in the guest's own network namespace rings, which is where
//! the host network is: a bridge-network container's sockets are in its
//! own, and musl's resolver closes an unconnected UDP socket per lookup.
//!
//!     r6 = ctx
//!     ...                            ; the program's own tests
//!     r1 = r6; call get_netns_cookie
//!     if r0 != <the guest's> goto allow
//!     *(u64 *)(r10 - 8) = 0
//!     r1 = &ring; r2 = r10 - 8; r3 = 8; r4 = 0
//!     call ringbuf_output
//!   allow:
//!     r0 = 1; exit

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use crate::bpf::*;

const BPF_MAP_TYPE_RINGBUF: u32 = 27;
const BPF_PROG_TYPE_CGROUP_SOCK: u32 = 9;
const BPF_PROG_TYPE_SOCK_OPS: u32 = 13;
const BPF_CGROUP_SOCK_OPS: u32 = 3;
const BPF_CGROUP_INET4_POST_BIND: u32 = 12;
const BPF_CGROUP_INET6_POST_BIND: u32 = 13;
const BPF_CGROUP_INET_SOCK_RELEASE: u32 = 34;
const BPF_FUNC_GET_NETNS_COOKIE: i32 = 122;
const BPF_FUNC_RINGBUF_OUTPUT: i32 = 130;
const SO_NETNS_COOKIE: libc::c_int = 71;
const BPF_SOCK_OPS_TCP_LISTEN_CB: i32 = 11;
const TCP_CLOSE: i32 = 7;
const TCP_LISTEN: i32 = 10;
/// `struct bpf_sock`: `type`, `src_port` (host order; post_bind only) and
/// `state`. `struct bpf_sock_ops` begins with `op`.
const SK_TYPE: i16 = 8;
const SK_SRC_PORT: i16 = 44;
const SK_STATE: i16 = 72;
/// A power of two and a multiple of the page; it holds empty records only
/// until the next read.
const RING: u32 = 64 * 1024;

/// What every program ends with: whether the socket is in the guest's own
/// network namespace (`netns`, the cookie of the root's), where the host
/// network is, and if so the ring; then allow. Sixteen instructions.
fn tail(netns: u64, ring: RawFd) -> [Insn; 16] {
    [
        insn(MOV64_REG, 1, 6, 0, 0),                     // r1 = ctx
        insn(CALL, 0, 0, 0, BPF_FUNC_GET_NETNS_COOKIE),  // r0 = its namespace
        insn(LD_IMM_DW, 2, 0, 0, netns as u32 as i32),   // r2 = netns
        insn(0, 0, 0, 0, (netns >> 32) as u32 as i32),   //   (high half)
        insn(JNE_REG, 0, 2, 9, 0),                       // another: allow
        insn(MOV64_IMM, 1, 0, 0, 0),                     // r1 = 0
        insn(STX_MEM_DW, 10, 1, -8, 0),                  // *(u64 *)(r10 - 8) = r1
        insn(LD_IMM_DW, 1, BPF_PSEUDO_MAP_FD, 0, ring),  // r1 = &ring
        insn(0, 0, 0, 0, 0),                             //   (second half)
        insn(MOV64_REG, 2, 10, 0, 0),                    // r2 = r10
        insn(ADD64_IMM, 2, 0, 0, -8),                    // r2 -= 8
        insn(MOV64_IMM, 3, 0, 0, 8),                     // r3 = 8
        insn(MOV64_IMM, 4, 0, 0, 0),                     // r4 = 0
        insn(CALL, 0, 0, 0, BPF_FUNC_RINGBUF_OUTPUT),    // ringbuf_output
        insn(MOV64_IMM, 0, 0, 0, 1),                     // allow: r0 = 1
        insn(EXIT, 0, 0, 0, 0),                          // exit
    ]
}

/// Offsets for a jump from the test at `at`, in a program of `n` tests: to
/// `allow`, and to the tail's namespace check.
const fn to_allow(n: i16, at: i16) -> i16 {
    n - at - 1 + 14
}
const fn to_tail(n: i16, at: i16) -> i16 {
    n - at - 1
}

/// `r6 = ctx`, the tests, the tail.
fn program(tests: &[Insn], netns: u64, ring: RawFd) -> Vec<Insn> {
    let mut p = vec![insn(MOV64_REG, 6, 1, 0, 0)];
    p.extend_from_slice(tests);
    p.extend(tail(netns, ring));
    p
}

/// `sock_ops`: rings at `TCP_LISTEN_CB`.
fn listening(netns: u64, ring: RawFd) -> Vec<Insn> {
    const N: i16 = 2;
    program(
        &[
            insn(LDX_MEM_W, 2, 6, 0, 0),                                       // r2 = ops->op
            insn(JNE_IMM, 2, 0, to_allow(N, 1), BPF_SOCK_OPS_TCP_LISTEN_CB),   // not a listen
        ],
        netns,
        ring,
    )
}

/// `post_bind`: rings for a UDP socket bound below `ephemeral`.
fn bound(netns: u64, ring: RawFd, ephemeral: u16) -> Vec<Insn> {
    const N: i16 = 5;
    program(
        &[
            insn(LDX_MEM_W, 2, 6, SK_TYPE, 0),                                 // r2 = sk->type
            insn(JNE_IMM, 2, 0, to_allow(N, 1), libc::SOCK_DGRAM),             // not UDP
            insn(LDX_MEM_W, 2, 6, SK_SRC_PORT, 0),                             // r2 = sk->src_port
            insn(JEQ_IMM, 2, 0, to_allow(N, 3), 0),                            // no port yet
            insn(JGE_IMM, 2, 0, to_allow(N, 4), ephemeral as i32),             // a lookup's
        ],
        netns,
        ring,
    )
}

/// `sock_release`: rings for a TCP listener, or a UDP socket never
/// connected.
fn released(netns: u64, ring: RawFd) -> Vec<Insn> {
    const N: i16 = 7;
    program(
        &[
            insn(LDX_MEM_W, 2, 6, SK_TYPE, 0),                                 // r2 = sk->type
            insn(LDX_MEM_W, 3, 6, SK_STATE, 0),                                // r3 = sk->state
            insn(JNE_IMM, 2, 0, 2, libc::SOCK_STREAM),                         // not TCP: udp
            insn(JNE_IMM, 3, 0, to_allow(N, 3), TCP_LISTEN),                   // not listening
            insn(JA, 0, 0, to_tail(N, 4), 0),                                  // ring
            insn(JNE_IMM, 2, 0, to_allow(N, 5), libc::SOCK_DGRAM),             // udp: not UDP
            insn(JNE_IMM, 3, 0, to_allow(N, 6), TCP_CLOSE),                    // connected
        ],
        netns,
        ring,
    )
}

/// The cookie of this process's network namespace, the guest's own.
fn own_netns() -> io::Result<u64> {
    // SAFETY: a socket we close below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut cookie: u64 = 0;
    let mut len = size_of::<u64>() as libc::socklen_t;
    // SAFETY: a u64 for SO_NETNS_COOKIE to fill.
    let rc = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, SO_NETNS_COOKIE, std::ptr::addr_of_mut!(cookie).cast(), &mut len) };
    let err = io::Error::last_os_error();
    // SAFETY: ours.
    unsafe { libc::close(fd) };
    if rc < 0 { Err(err) } else { Ok(cookie) }
}

/// The ring and the links attaching its programs; dropping it detaches
/// them.
pub struct Doorbell {
    ring: OwnedFd,
    /// The ring's control pages: ours to write (where we have read to), and
    /// the kernel's (where it has written to).
    consumer: *mut u64,
    producer: *const u64,
    page: usize,
    _links: Vec<OwnedFd>,
}

// SAFETY: the pages are touched only by `wait`, which the one thread that
// owns the bell calls.
unsafe impl Send for Doorbell {}

impl Doorbell {
    pub fn attach(ephemeral: u16) -> io::Result<Doorbell> {
        let ring = map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, RING)?;
        // SAFETY: plain sysconf.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let map = |offset: usize, prot: libc::c_int| -> io::Result<*mut libc::c_void> {
            // SAFETY: one page of the ring map, at an offset the kernel lays
            // out: the consumer page writable at 0, the producer read-only
            // after it.
            let p = unsafe { libc::mmap(std::ptr::null_mut(), page, prot, libc::MAP_SHARED, ring.as_raw_fd(), offset as libc::off_t) };
            if p == libc::MAP_FAILED { Err(io::Error::last_os_error()) } else { Ok(p) }
        };
        let consumer = map(0, libc::PROT_READ | libc::PROT_WRITE)?;
        let producer = match map(page, libc::PROT_READ) {
            Ok(p) => p,
            Err(e) => {
                // SAFETY: the page just mapped.
                unsafe { libc::munmap(consumer, page) };
                return Err(e);
            }
        };
        let mut bell = Doorbell { ring, consumer: consumer.cast(), producer: producer.cast(), page, _links: Vec::new() };
        let cgroup = OwnedFd::from(std::fs::File::open("/sys/fs/cgroup")?);
        let (fd, netns) = (bell.ring.as_raw_fd(), own_netns()?);
        for (prog_type, expected, attach, insns) in [
            (BPF_PROG_TYPE_SOCK_OPS, 0, BPF_CGROUP_SOCK_OPS, listening(netns, fd)),
            (BPF_PROG_TYPE_CGROUP_SOCK, BPF_CGROUP_INET4_POST_BIND, BPF_CGROUP_INET4_POST_BIND, bound(netns, fd, ephemeral)),
            (BPF_PROG_TYPE_CGROUP_SOCK, BPF_CGROUP_INET6_POST_BIND, BPF_CGROUP_INET6_POST_BIND, bound(netns, fd, ephemeral)),
            (BPF_PROG_TYPE_CGROUP_SOCK, BPF_CGROUP_INET_SOCK_RELEASE, BPF_CGROUP_INET_SOCK_RELEASE, released(netns, fd)),
        ] {
            let prog = prog_load(prog_type, expected, &insns, 1)
                .map_err(|(e, log)| io::Error::new(e.kind(), format!("{e}: {log}")))?;
            let mut attr = Attr { zero: [0; 128] };
            attr.link = LinkCreate {
                prog_fd: prog.as_raw_fd() as u32,
                target_fd: cgroup.as_raw_fd() as u32,
                attach_type: attach,
                flags: 0,
            };
            bell._links.push(bpf_fd(BPF_LINK_CREATE, &mut attr)?);
        }
        Ok(bell)
    }

    /// Waits up to `timeout_ms` (-1: for as long as it takes) for the bell,
    /// and says whether it rang. Every record waiting is consumed: that it
    /// rang is all there is to know.
    pub fn wait(&self, timeout_ms: i32) -> bool {
        let mut p = libc::pollfd { fd: self.ring.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd.
        if unsafe { libc::poll(&mut p, 1, timeout_ms) } <= 0 {
            return false;
        }
        // SAFETY: both pages are mapped for the bell's lifetime; the kernel
        // reads our position with acquire semantics and we publish it with a
        // release store.
        unsafe {
            let written = std::sync::atomic::AtomicU64::from_ptr(self.producer as *mut u64).load(std::sync::atomic::Ordering::Acquire);
            std::sync::atomic::AtomicU64::from_ptr(self.consumer).store(written, std::sync::atomic::Ordering::Release);
        }
        true
    }
}

impl Drop for Doorbell {
    fn drop(&mut self) {
        // SAFETY: the two pages `attach` mapped.
        unsafe {
            libc::munmap(self.consumer.cast(), self.page);
            libc::munmap(self.producer as *mut libc::c_void, self.page);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test's jump lands on the tail or on `allow`, or forward among
    /// the tests, never past the program or into a map load's second half.
    #[test]
    fn every_jump_lands_on_the_tail_or_allow() {
        for p in [listening(7, 3), bound(7, 3, 32768), released(7, 3)] {
            let tail = p.len() - 16;
            let allow = p.len() - 2;
            for (at, i) in p.iter().enumerate() {
                if matches!(i.code, JA | JEQ_IMM | JNE_IMM | JGE_IMM | JNE_REG) {
                    let to = (at as i64 + 1 + i.off as i64) as usize;
                    let ok = to == tail || to == allow || (at < tail && to > at && to < tail && i.code != JA);
                    assert!(ok, "jump at {at} lands on {to}");
                }
            }
        }
    }

    #[test]
    fn the_namespace_cookie_is_split_across_the_load() {
        let p = listening(0x1122_3344_5566_7788, 3);
        let tail = p.len() - 16;
        assert_eq!(p[tail + 2].imm as u32, 0x5566_7788);
        assert_eq!(p[tail + 3].imm as u32, 0x1122_3344);
    }
}

/// Against the running kernel, as root in the guest's namespaces:
/// `cargo test -- --ignored doorbell` in a privileged container with
/// `--network host --cgroupns host`.
#[cfg(test)]
mod kernel {
    use super::*;
    use std::net::{TcpListener, UdpSocket};

    fn quiet(bell: &Doorbell) {
        while bell.wait(50) {}
    }

    #[test]
    #[ignore]
    fn doorbell_rings_for_listeners_and_not_for_lookups() {
        let bell = Doorbell::attach(32768).expect("attach");
        quiet(&bell);
        let tcp = TcpListener::bind("0.0.0.0:18999").unwrap();
        assert!(bell.wait(1000), "a TCP listen rang");
        quiet(&bell);
        drop(tcp);
        assert!(bell.wait(1000), "a TCP listener closing rang");
        quiet(&bell);
        let udp = UdpSocket::bind("0.0.0.0:18998").unwrap();
        assert!(bell.wait(1000), "a UDP service binding rang");
        quiet(&bell);
        drop(udp);
        assert!(bell.wait(1000), "a UDP service closing rang");

        // Elsewhere, quiet: another namespace's bind, and a connected socket.
        let mut rang = 0;
        for _ in 0..20 {
            quiet(&bell);
            std::thread::spawn(|| {
                // SAFETY: unshare affects this thread only.
                assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNET) }, 0);
                let udp = UdpSocket::bind("0.0.0.0:18997").expect("a bind in another namespace");
                drop(udp);
                let tcp = TcpListener::bind("0.0.0.0:18996").expect("a listen in another namespace");
                drop(tcp);
            })
            .join()
            .unwrap();
            let c = UdpSocket::bind("0.0.0.0:0").unwrap();
            c.connect("127.0.0.1:53").unwrap();
            drop(c);
            rang += bell.wait(100) as u32;
        }
        // The guest's own engine may ring now and then; one round in four
        // would be a bell for these.
        assert!(rang < 5, "{rang} of 20 quiet rounds rang");
    }
}

//! Streams joined inside the kernel.
//!
//! A stream is a TCP socket (the container's connection, redirected here) and
//! a vsock socket (to the host). Copying between them in this process cost a
//! wakeup of this process per direction per request and a copy of every byte
//! through it. BPF sockmap joins the two sockets in the kernel: a verdict
//! program on each socket's ingress redirects every received skb out through
//! the other socket, and this process is left holding the descriptors,
//! watching only for either end to close. vsock can be either end since the
//! kernel's sockmap support for it (6.4): it reads skbs for the verdict and
//! takes redirected ones to send.
//!
//! The program is assembled here by hand — nineteen instructions, no
//! toolchain — and does one thing: look up the socket's cookie in a hash to
//! find its peer's slot, and redirect to it.
//!
//! Two sockmaps, because a socket starts running verdicts the moment it
//! enters a map with a program, and a verdict whose peer is not yet in the
//! target map drops the bytes. Both sockets enter `targets`, which has no
//! program, before either enters `attach`, which has the verdict — so every
//! verdict finds its peer. And joining hooks a socket's data-ready callback
//! without running it, so bytes queued before the join (a request that
//! arrived in full while this process was still connecting the other end)
//! would sit until the next packet: the join ends by poking each socket's
//! receive low-water mark, which is TCP's own "signal readiness now" path
//! and runs the verdict over the queue in order.
//!
//!     r6 = r1                      ; the skb
//!     r1 = r6; call get_socket_cookie ; r0 = cookie
//!     *(u64 *)(r10 - 8) = r0
//!     r1 = &peers; r2 = r10 - 8; call map_lookup_elem
//!     if r0 == 0 goto pass
//!     r3 = *(u32 *)(r0 + 0)        ; the peer's slot
//!     r1 = r6; r2 = &sockets; r4 = 0; call sk_redirect_map
//!     exit                         ; SK_PASS or SK_DROP as the helper said
//!   pass:
//!     r0 = 1                       ; SK_PASS
//!     exit

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;

// bpf(2) commands and constants, from the UAPI; stable ABI.
const BPF_MAP_CREATE: libc::c_int = 0;
const BPF_MAP_LOOKUP_ELEM: libc::c_int = 1;
const BPF_MAP_UPDATE_ELEM: libc::c_int = 2;
const BPF_MAP_DELETE_ELEM: libc::c_int = 3;
const BPF_PROG_LOAD: libc::c_int = 5;
const BPF_PROG_ATTACH: libc::c_int = 8;
const BPF_MAP_TYPE_HASH: u32 = 1;
const BPF_MAP_TYPE_SOCKMAP: u32 = 15;
const BPF_PROG_TYPE_SK_SKB: u32 = 14;
/// `BPF_SK_SKB_VERDICT`: a verdict on each received skb, no stream parser.
const BPF_SK_SKB_VERDICT: u32 = 38;
const BPF_ANY: u64 = 0;
const BPF_PSEUDO_MAP_FD: u8 = 1;
const BPF_FUNC_MAP_LOOKUP_ELEM: i32 = 1;
const BPF_FUNC_GET_SOCKET_COOKIE: i32 = 46;
const BPF_FUNC_SK_REDIRECT_MAP: i32 = 52;
const SO_COOKIE: libc::c_int = 57;

/// How many streams may be joined at once. Each takes two slots.
const SLOTS: u32 = 65536;

#[repr(C)]
#[derive(Clone, Copy)]
struct Insn {
    code: u8,
    regs: u8,
    off: i16,
    imm: i32,
}

const fn insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Insn {
    Insn {
        code,
        regs: (src << 4) | dst,
        off,
        imm,
    }
}

// Opcode bytes, from the eBPF instruction set.
const MOV64_REG: u8 = 0xbf;
const MOV64_IMM: u8 = 0xb7;
const ADD64_IMM: u8 = 0x07;
const STX_MEM_DW: u8 = 0x7b;
const LDX_MEM_W: u8 = 0x61;
const LD_IMM_DW: u8 = 0x18;
const JEQ_IMM: u8 = 0x15;
const CALL: u8 = 0x85;
const EXIT: u8 = 0x95;

/// The verdict program, with the two map descriptors patched in.
fn program(peers: RawFd, sockets: RawFd) -> Vec<Insn> {
    vec![
        insn(MOV64_REG, 6, 1, 0, 0),                    // r6 = r1
        insn(MOV64_REG, 1, 6, 0, 0),                    // r1 = r6
        insn(CALL, 0, 0, 0, BPF_FUNC_GET_SOCKET_COOKIE), // r0 = cookie
        insn(STX_MEM_DW, 10, 0, -8, 0),                 // *(r10-8) = r0
        insn(LD_IMM_DW, 1, BPF_PSEUDO_MAP_FD, 0, peers), // r1 = &peers
        insn(0, 0, 0, 0, 0),                            //   (second half)
        insn(MOV64_REG, 2, 10, 0, 0),                   // r2 = r10
        insn(ADD64_IMM, 2, 0, 0, -8),                   // r2 -= 8
        insn(CALL, 0, 0, 0, BPF_FUNC_MAP_LOOKUP_ELEM),  // r0 = lookup
        insn(JEQ_IMM, 0, 0, 7, 0),                      // if r0 == 0 goto pass
        insn(LDX_MEM_W, 3, 0, 0, 0),                    // r3 = *(u32 *)r0
        insn(MOV64_REG, 1, 6, 0, 0),                    // r1 = r6
        insn(LD_IMM_DW, 2, BPF_PSEUDO_MAP_FD, 0, sockets), // r2 = &sockets
        insn(0, 0, 0, 0, 0),                            //   (second half)
        insn(MOV64_IMM, 4, 0, 0, 0),                    // r4 = 0
        insn(CALL, 0, 0, 0, BPF_FUNC_SK_REDIRECT_MAP),  // r0 = redirect
        insn(EXIT, 0, 0, 0, 0),                         // exit
        insn(MOV64_IMM, 0, 0, 0, 1),                    // pass: r0 = SK_PASS
        insn(EXIT, 0, 0, 0, 0),                         // exit
    ]
}

/// `union bpf_attr`, the pieces used here, zero-padded to the union's size.
#[repr(C)]
union Attr {
    map: MapCreate,
    elem: MapElem,
    prog: ProgLoad,
    attach: ProgAttach,
    zero: [u8; 128],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MapCreate {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MapElem {
    map_fd: u32,
    _pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ProgLoad {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ProgAttach {
    target_fd: u32,
    attach_bpf_fd: u32,
    attach_type: u32,
    attach_flags: u32,
}

fn bpf(cmd: libc::c_int, attr: &mut Attr) -> io::Result<libc::c_int> {
    // SAFETY: the attr union is fully initialized (zeroed then written) and
    // its size is what the kernel expects for these commands.
    let rc = unsafe { libc::syscall(libc::SYS_bpf, cmd, attr as *mut Attr, size_of::<Attr>() as u32) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(rc as libc::c_int)
}

fn map_create(map_type: u32, key_size: u32, value_size: u32, max_entries: u32) -> io::Result<OwnedFd> {
    let mut attr = Attr { zero: [0; 128] };
    attr.map = MapCreate {
        map_type,
        key_size,
        value_size,
        max_entries,
        map_flags: 0,
    };
    let fd = bpf(BPF_MAP_CREATE, &mut attr)?;
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn map_update(map: RawFd, key: &[u8], value: &[u8]) -> io::Result<()> {
    let mut attr = Attr { zero: [0; 128] };
    attr.elem = MapElem {
        map_fd: map as u32,
        _pad: 0,
        key: key.as_ptr() as u64,
        value: value.as_ptr() as u64,
        flags: BPF_ANY,
    };
    bpf(BPF_MAP_UPDATE_ELEM, &mut attr).map(|_| ())
}

fn map_delete(map: RawFd, key: &[u8]) -> io::Result<()> {
    let mut attr = Attr { zero: [0; 128] };
    attr.elem = MapElem {
        map_fd: map as u32,
        _pad: 0,
        key: key.as_ptr() as u64,
        value: 0,
        flags: 0,
    };
    bpf(BPF_MAP_DELETE_ELEM, &mut attr).map(|_| ())
}

fn map_lookup_present(map: RawFd, key: &[u8], value_len: usize) -> bool {
    let value = vec![0u8; value_len];
    let mut attr = Attr { zero: [0; 128] };
    attr.elem = MapElem {
        map_fd: map as u32,
        _pad: 0,
        key: key.as_ptr() as u64,
        value: value.as_ptr() as u64,
        flags: 0,
    };
    bpf(BPF_MAP_LOOKUP_ELEM, &mut attr).is_ok()
}

/// Runs the socket's data-ready path now, so bytes queued before it joined
/// are put through the verdict. Setting the receive low-water mark is how
/// TCP offers that from user space (`tcp_set_rcvlowat` calls
/// `tcp_data_ready`); the value, one byte, is the default anyway.
fn kick(fd: RawFd) {
    let one: libc::c_int = 1;
    // SAFETY: a live socket descriptor and an int-sized option value.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVLOWAT,
            std::ptr::addr_of!(one).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        eprintln!("lighter-agent: kick fd={fd}: {}", io::Error::last_os_error());
    }
}

fn socket_cookie(fd: RawFd) -> io::Result<u64> {
    let mut cookie: u64 = 0;
    let mut len = size_of::<u64>() as libc::socklen_t;
    // SAFETY: a u64 for SO_COOKIE to fill.
    let rc = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, SO_COOKIE, std::ptr::addr_of_mut!(cookie).cast(), &mut len)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cookie)
}

/// Loads a program and reports errno and the verifier's words, for the
/// probe below.
fn try_load(prog_type: u32, insns: &[Insn], log_level: u32, expected_attach: u32) -> String {
    let mut log = vec![0u8; 64 * 1024];
    let mut attr = Attr { zero: [0; 128] };
    attr.prog = ProgLoad {
        prog_type,
        insn_cnt: insns.len() as u32,
        insns: insns.as_ptr() as u64,
        license: c"GPL".as_ptr() as u64,
        log_level,
        log_size: if log_level > 0 { log.len() as u32 } else { 0 },
        log_buf: if log_level > 0 { log.as_mut_ptr() as u64 } else { 0 },
        kern_version: 0,
        prog_flags: 0,
    };
    // expected_attach_type sits after prog_name[16] and prog_ifindex.
    if expected_attach != 0 {
        // SAFETY: writing a u32 at the union's byte offset 76 (24 + 16 + 4 + 16 + 4 + ... as laid out by the UAPI).
        unsafe {
            let base = std::ptr::addr_of_mut!(attr).cast::<u8>();
            std::ptr::write_unaligned(base.add(76).cast::<u32>(), expected_attach);
        }
    }
    match bpf(BPF_PROG_LOAD, &mut attr) {
        Ok(fd) => {
            // SAFETY: a fresh descriptor, closed here.
            unsafe { libc::close(fd) };
            "ok".to_string()
        }
        Err(e) => {
            let text = String::from_utf8_lossy(&log);
            format!("{e}: {}", text.trim_end_matches('\0').trim())
        }
    }
}

/// Tries the program and its attributes in variants, printing each: the
/// kernel says only "invalid argument" for a load it refuses before
/// verifying, and which attribute it disliked is what this finds out.
pub fn probe() {
    let trivial = [insn(MOV64_IMM, 0, 0, 0, 1), insn(EXIT, 0, 0, 0, 0)];
    println!("trivial sk_skb, log 1: {}", try_load(BPF_PROG_TYPE_SK_SKB, &trivial, 1, 0));
    println!("trivial sk_skb, log 0: {}", try_load(BPF_PROG_TYPE_SK_SKB, &trivial, 0, 0));
    println!("trivial sk_skb, expected verdict: {}", try_load(BPF_PROG_TYPE_SK_SKB, &trivial, 1, BPF_SK_SKB_VERDICT));
    println!("trivial sk_msg: {}", try_load(16, &trivial, 1, 0));
    println!("trivial socket_filter: {}", try_load(1, &trivial, 1, 0));
    match (map_create(BPF_MAP_TYPE_SOCKMAP, 4, 4, 16), map_create(BPF_MAP_TYPE_HASH, 8, 4, 16)) {
        (Ok(sockets), Ok(peers)) => {
            let full = program(peers.as_raw_fd(), sockets.as_raw_fd());
            println!("full sk_skb: {}", try_load(BPF_PROG_TYPE_SK_SKB, &full, 1, 0));
            println!("full sk_skb, expected verdict: {}", try_load(BPF_PROG_TYPE_SK_SKB, &full, 1, BPF_SK_SKB_VERDICT));
        }
        (a, b) => println!("maps: {:?} {:?}", a.err(), b.err()),
    }
}

/// The maps and the program, created once.
pub struct Joiner {
    /// Where redirects land; no program attached.
    targets: OwnedFd,
    /// Where the verdict program is attached; entered last.
    attach: OwnedFd,
    peers: OwnedFd,
    _program: OwnedFd,
    /// Free slots in `sockets`, reused after a stream ends.
    free: Mutex<Vec<u32>>,
}

impl Joiner {
    pub fn new() -> io::Result<Joiner> {
        let targets = map_create(BPF_MAP_TYPE_SOCKMAP, 4, 4, SLOTS)?;
        let attach = map_create(BPF_MAP_TYPE_SOCKMAP, 4, 4, SLOTS)?;
        let peers = map_create(BPF_MAP_TYPE_HASH, 8, 4, SLOTS)?;
        let insns = program(peers.as_raw_fd(), targets.as_raw_fd());
        let mut log = vec![0u8; 64 * 1024];
        let mut attr = Attr { zero: [0; 128] };
        attr.prog = ProgLoad {
            prog_type: BPF_PROG_TYPE_SK_SKB,
            insn_cnt: insns.len() as u32,
            insns: insns.as_ptr() as u64,
            license: c"GPL".as_ptr() as u64,
            log_level: 1,
            log_size: log.len() as u32,
            log_buf: log.as_mut_ptr() as u64,
            kern_version: 0,
            prog_flags: 0,
        };
        let prog = match bpf(BPF_PROG_LOAD, &mut attr) {
            Ok(fd) => fd,
            Err(e) => {
                let text = String::from_utf8_lossy(&log);
                let text = text.trim_end_matches('\0');
                eprintln!("lighter-agent: sockmap program refused: {e}\n{text}");
                return Err(e);
            }
        };
        // SAFETY: a fresh descriptor we own.
        let program = unsafe { OwnedFd::from_raw_fd(prog) };
        let mut attr = Attr { zero: [0; 128] };
        attr.attach = ProgAttach {
            target_fd: attach.as_raw_fd() as u32,
            attach_bpf_fd: program.as_raw_fd() as u32,
            attach_type: BPF_SK_SKB_VERDICT,
            attach_flags: 0,
        };
        bpf(BPF_PROG_ATTACH, &mut attr)?;
        Ok(Joiner {
            targets,
            attach,
            peers,
            _program: program,
            free: Mutex::new((0..SLOTS).rev().collect()),
        })
    }

    /// Joins two sockets: bytes on either go to the other, in the kernel.
    /// Returns the pair's slots, for [`Joiner::part`].
    pub fn join(&self, a: RawFd, b: RawFd) -> io::Result<(u32, u32)> {
        let (slot_a, slot_b) = {
            let mut free = self.free.lock().expect("sockmap slots poisoned");
            let (Some(x), Some(y)) = (free.pop(), free.pop()) else {
                return Err(io::Error::other("no free sockmap slots"));
            };
            (x, y)
        };
        let cookie_a = socket_cookie(a)?;
        let cookie_b = socket_cookie(b)?;
        // Peers first, so a message arriving as a socket lands in the map
        // finds where to go.
        map_update(self.peers.as_raw_fd(), &cookie_a.to_ne_bytes(), &slot_b.to_ne_bytes())?;
        map_update(self.peers.as_raw_fd(), &cookie_b.to_ne_bytes(), &slot_a.to_ne_bytes())?;
        let fd_a = (a as u32).to_ne_bytes();
        let fd_b = (b as u32).to_ne_bytes();
        map_update(self.targets.as_raw_fd(), &slot_a.to_ne_bytes(), &fd_a)?;
        map_update(self.targets.as_raw_fd(), &slot_b.to_ne_bytes(), &fd_b)?;
        map_update(self.attach.as_raw_fd(), &slot_a.to_ne_bytes(), &fd_a)?;
        map_update(self.attach.as_raw_fd(), &slot_b.to_ne_bytes(), &fd_b)?;
        kick(a);
        kick(b);
        Ok((slot_a, slot_b))
    }

    /// Undoes [`Joiner::join`]. The sockets themselves leave the map when
    /// they close; this frees the peer entries and the slots.
    pub fn part(&self, a: RawFd, b: RawFd, slots: (u32, u32)) {
        if let Ok(c) = socket_cookie(a) {
            let _ = map_delete(self.peers.as_raw_fd(), &c.to_ne_bytes());
        }
        if let Ok(c) = socket_cookie(b) {
            let _ = map_delete(self.peers.as_raw_fd(), &c.to_ne_bytes());
        }
        for map in [self.attach.as_raw_fd(), self.targets.as_raw_fd()] {
            let _ = map_delete(map, &slots.0.to_ne_bytes());
            let _ = map_delete(map, &slots.1.to_ne_bytes());
        }
        let mut free = self.free.lock().expect("sockmap slots poisoned");
        free.push(slots.0);
        free.push(slots.1);
    }

    /// Whether a socket is still in the map (it leaves when it closes).
    pub fn holds(&self, slot: u32) -> bool {
        map_lookup_present(self.targets.as_raw_fd(), &slot.to_ne_bytes(), 8)
    }
}

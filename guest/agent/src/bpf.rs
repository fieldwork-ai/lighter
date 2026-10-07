//! bpf(2), the pieces the agent's hand-assembled programs use: the
//! instruction encoding and the `bpf_attr` members for maps, loads and
//! attachments. UAPI, so a stable ABI.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

pub const BPF_MAP_CREATE: libc::c_int = 0;
pub const BPF_MAP_LOOKUP_ELEM: libc::c_int = 1;
pub const BPF_MAP_UPDATE_ELEM: libc::c_int = 2;
pub const BPF_MAP_DELETE_ELEM: libc::c_int = 3;
pub const BPF_PROG_LOAD: libc::c_int = 5;
pub const BPF_PROG_ATTACH: libc::c_int = 8;
pub const BPF_LINK_CREATE: libc::c_int = 28;
pub const BPF_PSEUDO_MAP_FD: u8 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Insn {
    pub code: u8,
    pub regs: u8,
    pub off: i16,
    pub imm: i32,
}

pub const fn insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Insn {
    Insn {
        code,
        regs: (src << 4) | dst,
        off,
        imm,
    }
}

// Opcode bytes, from the eBPF instruction set.
pub const MOV64_REG: u8 = 0xbf;
pub const MOV64_IMM: u8 = 0xb7;
pub const ADD64_IMM: u8 = 0x07;
pub const STX_MEM_DW: u8 = 0x7b;
pub const LDX_MEM_W: u8 = 0x61;
pub const LD_IMM_DW: u8 = 0x18;
pub const JA: u8 = 0x05;
pub const JEQ_IMM: u8 = 0x15;
pub const JNE_IMM: u8 = 0x55;
pub const JGE_IMM: u8 = 0x35;
pub const JNE_REG: u8 = 0x5d;
pub const CALL: u8 = 0x85;
pub const EXIT: u8 = 0x95;

/// `union bpf_attr`, the pieces used here, zero-padded to the union's size.
#[repr(C)]
pub union Attr {
    pub map: MapCreate,
    pub elem: MapElem,
    pub prog: ProgLoad,
    pub attach: ProgAttach,
    pub link: LinkCreate,
    pub zero: [u8; 128],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MapCreate {
    pub map_type: u32,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
    pub map_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MapElem {
    pub map_fd: u32,
    pub _pad: u32,
    pub key: u64,
    pub value: u64,
    pub flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProgLoad {
    pub prog_type: u32,
    pub insn_cnt: u32,
    pub insns: u64,
    pub license: u64,
    pub log_level: u32,
    pub log_size: u32,
    pub log_buf: u64,
    pub kern_version: u32,
    pub prog_flags: u32,
    pub prog_name: [u8; 16],
    pub prog_ifindex: u32,
    pub expected_attach_type: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProgAttach {
    pub target_fd: u32,
    pub attach_bpf_fd: u32,
    pub attach_type: u32,
    pub attach_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LinkCreate {
    pub prog_fd: u32,
    pub target_fd: u32,
    pub attach_type: u32,
    pub flags: u32,
}

pub fn bpf(cmd: libc::c_int, attr: &mut Attr) -> io::Result<libc::c_int> {
    // SAFETY: the attr union is fully initialized (zeroed then written) and
    // its size is what the kernel expects for these commands.
    let rc = unsafe { libc::syscall(libc::SYS_bpf, cmd, attr as *mut Attr, size_of::<Attr>() as u32) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(rc as libc::c_int)
}

/// Runs a command that returns a new descriptor.
pub fn bpf_fd(cmd: libc::c_int, attr: &mut Attr) -> io::Result<OwnedFd> {
    let fd = bpf(cmd, attr)?;
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn map_create(map_type: u32, key_size: u32, value_size: u32, max_entries: u32) -> io::Result<OwnedFd> {
    let mut attr = Attr { zero: [0; 128] };
    attr.map = MapCreate {
        map_type,
        key_size,
        value_size,
        max_entries,
        map_flags: 0,
    };
    bpf_fd(BPF_MAP_CREATE, &mut attr)
}

/// Loads a program; a refusal carries the verifier's words when
/// `log_level` asked for them.
pub fn prog_load(prog_type: u32, expected_attach_type: u32, insns: &[Insn], log_level: u32) -> Result<OwnedFd, (io::Error, String)> {
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
        prog_name: [0; 16],
        prog_ifindex: 0,
        expected_attach_type,
    };
    bpf_fd(BPF_PROG_LOAD, &mut attr).map_err(|e| {
        let text = String::from_utf8_lossy(&log);
        (e, text.trim_end_matches('\0').trim().to_string())
    })
}

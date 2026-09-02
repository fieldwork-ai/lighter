//! The FUSE wire format.
//!
//! This is a transcription of the kernel's `include/uapi/linux/fuse.h` for the
//! parts we implement, and nothing else lives here: no policy, no syscalls, no
//! opinions. Keeping it that way is what lets the server be read as filesystem
//! logic rather than as byte arithmetic.
//!
//! # Why hand-written encoders rather than `repr(C)` casts
//!
//! Every structure below is little-endian and unaligned as far as we are
//! concerned: it arrives in a guest buffer we copied, at whatever offset the
//! previous field left us at. Casting a `&[u8]` to a `&fuse_write_in` would be
//! undefined behaviour on the alignment alone, and the transmute would silently
//! read past the end of a truncated request. Explicit field-at-a-time decoding
//! costs nothing measurable next to a filesystem syscall and cannot do either.

#![allow(dead_code)]

/// The ABI we implement. 7.31 is the floor for virtio-fs; anything newer
/// negotiates down to us.
pub const KERNEL_VERSION: u32 = 7;
pub const KERNEL_MINOR_VERSION: u32 = 31;

/// `struct fuse_in_header`.
pub const IN_HEADER_LEN: usize = 40;
/// `struct fuse_out_header`.
pub const OUT_HEADER_LEN: usize = 16;
/// `struct fuse_attr`.
pub const ATTR_LEN: usize = 88;
/// `struct fuse_entry_out`.
pub const ENTRY_OUT_LEN: usize = 128;
/// `struct fuse_dirent` without its name.
pub const DIRENT_HEADER_LEN: usize = 24;

/// Opcodes. Named exactly as the kernel names them, minus the `FUSE_` prefix.
pub mod op {
    /// Ours (guest patch 0005): a whole-file clone of one inode over another
    /// name. Numbered far above anything mainline assigns.
    pub const LIGHTER_CLONE: u32 = 62000;
    pub const LOOKUP: u32 = 1;
    pub const FORGET: u32 = 2;
    pub const GETATTR: u32 = 3;
    pub const SETATTR: u32 = 4;
    pub const READLINK: u32 = 5;
    pub const SYMLINK: u32 = 6;
    pub const MKNOD: u32 = 8;
    pub const MKDIR: u32 = 9;
    pub const UNLINK: u32 = 10;
    pub const RMDIR: u32 = 11;
    pub const RENAME: u32 = 12;
    pub const LINK: u32 = 13;
    pub const OPEN: u32 = 14;
    pub const READ: u32 = 15;
    pub const WRITE: u32 = 16;
    pub const STATFS: u32 = 17;
    pub const RELEASE: u32 = 18;
    pub const FSYNC: u32 = 20;
    pub const SETXATTR: u32 = 21;
    pub const GETXATTR: u32 = 22;
    pub const LISTXATTR: u32 = 23;
    pub const REMOVEXATTR: u32 = 24;
    pub const FLUSH: u32 = 25;
    pub const INIT: u32 = 26;
    pub const OPENDIR: u32 = 27;
    pub const READDIR: u32 = 28;
    pub const RELEASEDIR: u32 = 29;
    pub const FSYNCDIR: u32 = 30;
    pub const GETLK: u32 = 31;
    pub const SETLK: u32 = 32;
    pub const SETLKW: u32 = 33;
    pub const ACCESS: u32 = 34;
    pub const CREATE: u32 = 35;
    pub const INTERRUPT: u32 = 36;
    pub const BMAP: u32 = 37;
    pub const DESTROY: u32 = 38;
    pub const IOCTL: u32 = 39;
    pub const POLL: u32 = 40;
    pub const NOTIFY_REPLY: u32 = 41;
    pub const BATCH_FORGET: u32 = 42;
    pub const FALLOCATE: u32 = 43;
    pub const READDIRPLUS: u32 = 44;
    pub const RENAME2: u32 = 45;
    pub const LSEEK: u32 = 46;
    pub const COPY_FILE_RANGE: u32 = 47;
    pub const SETUPMAPPING: u32 = 48;
    pub const REMOVEMAPPING: u32 = 49;
    pub const SYNCFS: u32 = 50;
    pub const TMPFILE: u32 = 51;
    pub const STATX: u32 = 52;
}

/// INIT feature flags.
pub mod init {
    pub const ASYNC_READ: u32 = 1 << 0;
    pub const POSIX_LOCKS: u32 = 1 << 1;
    pub const ATOMIC_O_TRUNC: u32 = 1 << 3;
    pub const EXPORT_SUPPORT: u32 = 1 << 4;
    pub const BIG_WRITES: u32 = 1 << 5;
    pub const DONT_MASK: u32 = 1 << 6;
    pub const AUTO_INVAL_DATA: u32 = 1 << 12;
    pub const DO_READDIRPLUS: u32 = 1 << 13;
    pub const READDIRPLUS_AUTO: u32 = 1 << 14;
    pub const ASYNC_DIO: u32 = 1 << 15;
    pub const WRITEBACK_CACHE: u32 = 1 << 16;
    pub const PARALLEL_DIROPS: u32 = 1 << 18;
    pub const HANDLE_KILLPRIV: u32 = 1 << 19;
    /// The modern form of the same promise, and the one current kernels
    /// actually offer.
    pub const HANDLE_KILLPRIV_V2: u32 = 1 << 28;
    pub const ABORT_ERROR: u32 = 1 << 21;
    pub const MAX_PAGES: u32 = 1 << 22;
    pub const CACHE_SYMLINKS: u32 = 1 << 23;
    pub const EXPLICIT_INVAL_DATA: u32 = 1 << 25;
    pub const SUBMOUNTS: u32 = 1 << 27;
    /// Changes the size of `fuse_setxattr_in` on the wire. We do not advertise
    /// it, and the parser in `server` relies on that.
    pub const SETXATTR_EXT: u32 = 1 << 29;
    pub const INIT_EXT: u32 = 1 << 30;
}

/// INIT feature flags carried in `flags2`, honored by the kernel only when
/// `INIT_EXT` was negotiated. Bit N here is overall bit N + 32.
pub mod init2 {
    /// Ours, matched by guest patch 0004: FUSE_CREATE without a prior LOOKUP
    /// is handled for existing files too, `fopen::LIGHTER_CREATED` is
    /// truthful, and this server pushes invalidations for whatever it changes
    /// underneath the driver. Overall bit 60, far above anything mainline has
    /// assigned.
    pub const LIGHTER_CREATE: u32 = 1 << 28;
    /// Ours, matched by guest patch 0005: `op::LIGHTER_CLONE` is answered
    /// with a clonefile. Overall bit 61.
    pub const LIGHTER_CLONE: u32 = 1 << 29;
    /// Ours, matched by guest patch 0006: no `security.*` or POSIX ACL
    /// attribute exists here, ever, so the driver answers those reads itself
    /// — a quarter of a million requests per package install, the kernel
    /// asking before every chmod, chown and truncate whether there are
    /// capabilities to strip. Overall bit 59.
    pub const LIGHTER_NO_SECURITY_XATTR: u32 = 1 << 27;
    /// Ours, matched by guest patch 0007: the driver answers a setattr that
    /// would change nothing — same mode, same owner, same size — from the
    /// attributes it still trusts, without a request. Overall bit 58.
    pub const LIGHTER_NOOP_SETATTR: u32 = 1 << 26;
    /// Ours, matched by guest patch 0008: a positive dentry within its
    /// lifetime is trusted for an exclusive create, a mkdir and a rename
    /// target, because this server withdraws entries it learns have
    /// changed. Overall bit 57. Offered only with the notification channel
    /// live, since that is the whole basis for it.
    pub const LIGHTER_TRUST_DENTRIES: u32 = 1 << 25;
}

/// `fuse_open_out.open_flags`.
pub mod fopen {
    pub const DIRECT_IO: u32 = 1 << 0;
    pub const KEEP_CACHE: u32 = 1 << 1;
    pub const NONSEEKABLE: u32 = 1 << 2;
    pub const CACHE_DIR: u32 = 1 << 3;
    /// Ours (guest patch 0004): this CREATE really created the file, known
    /// truthfully because the server creates with O_EXCL first. Mainline has
    /// assigned up to bit 7.
    pub const LIGHTER_CREATED: u32 = 1 << 15;
}

/// `fuse_setattr_in.valid`.
pub mod fattr {
    pub const MODE: u32 = 1 << 0;
    pub const UID: u32 = 1 << 1;
    pub const GID: u32 = 1 << 2;
    pub const SIZE: u32 = 1 << 3;
    pub const ATIME: u32 = 1 << 4;
    pub const MTIME: u32 = 1 << 5;
    pub const FH: u32 = 1 << 6;
    pub const ATIME_NOW: u32 = 1 << 7;
    pub const MTIME_NOW: u32 = 1 << 8;
    pub const LOCKOWNER: u32 = 1 << 9;
    pub const CTIME: u32 = 1 << 10;
    pub const KILL_SUIDGID: u32 = 1 << 11;
}

/// `fuse_getattr_in.getattr_flags`.
pub const GETATTR_FH: u32 = 1 << 0;

/// Linux `RENAME_*` flags, as carried by RENAME2.
pub mod rename {
    pub const NOREPLACE: u32 = 1 << 0;
    pub const EXCHANGE: u32 = 1 << 1;
    pub const WHITEOUT: u32 = 1 << 2;
}

/// The header on every request.
#[derive(Debug, Clone, Copy)]
pub struct InHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

impl InHeader {
    pub fn parse(bytes: &[u8]) -> Option<InHeader> {
        if bytes.len() < IN_HEADER_LEN {
            return None;
        }
        Some(InHeader {
            len: get_u32(bytes, 0)?,
            opcode: get_u32(bytes, 4)?,
            unique: get_u64(bytes, 8)?,
            nodeid: get_u64(bytes, 16)?,
            uid: get_u32(bytes, 24)?,
            gid: get_u32(bytes, 28)?,
            pid: get_u32(bytes, 32)?,
        })
    }
}

/// File attributes as the guest kernel wants them.
///
/// Deliberately not `libc::stat`: the two disagree about field order, about
/// timestamp representation, and about what `st_dev` means on the other side
/// of a mount. Translating once, explicitly, is what keeps a macOS `stat` from
/// leaking host-specific numbers into a Linux inode.
#[derive(Debug, Clone, Copy, Default)]
pub struct Attr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
}

impl Attr {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ino.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.blocks.to_le_bytes());
        // Negative times are not representable in the wire format's unsigned
        // seconds field, and a file dated before 1970 is real enough on a Mac
        // that has restored from a backup. Clamping is the only lossless-enough
        // option: the alternative wraps to the year 2500.
        out.extend_from_slice(&(self.atime.max(0) as u64).to_le_bytes());
        out.extend_from_slice(&(self.mtime.max(0) as u64).to_le_bytes());
        out.extend_from_slice(&(self.ctime.max(0) as u64).to_le_bytes());
        out.extend_from_slice(&self.atimensec.to_le_bytes());
        out.extend_from_slice(&self.mtimensec.to_le_bytes());
        out.extend_from_slice(&self.ctimensec.to_le_bytes());
        out.extend_from_slice(&self.mode.to_le_bytes());
        out.extend_from_slice(&self.nlink.to_le_bytes());
        out.extend_from_slice(&self.uid.to_le_bytes());
        out.extend_from_slice(&self.gid.to_le_bytes());
        out.extend_from_slice(&self.rdev.to_le_bytes());
        out.extend_from_slice(&self.blksize.to_le_bytes());
        // `flags`, which only means anything with FUSE_SUBMOUNTS.
        out.extend_from_slice(&0u32.to_le_bytes());
    }
}

/// A directory entry, with the attributes the guest would otherwise LOOKUP.
#[derive(Debug, Clone, Copy)]
pub struct EntryOut {
    pub nodeid: u64,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: Attr,
}

impl EntryOut {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.nodeid.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.entry_valid.to_le_bytes());
        out.extend_from_slice(&self.attr_valid.to_le_bytes());
        out.extend_from_slice(&self.entry_valid_nsec.to_le_bytes());
        out.extend_from_slice(&self.attr_valid_nsec.to_le_bytes());
        self.attr.encode(out);
    }
}

/// Appends a `fuse_dirent`, padded to the 8-byte alignment the guest's parser
/// assumes. Returns false if the entry would not fit in `budget`.
pub fn push_dirent(
    out: &mut Vec<u8>,
    budget: usize,
    ino: u64,
    off: u64,
    kind: u32,
    name: &[u8],
) -> bool {
    let entry_len = dirent_len(name.len());
    if out.len() + entry_len > budget {
        return false;
    }
    out.extend_from_slice(&ino.to_le_bytes());
    out.extend_from_slice(&off.to_le_bytes());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(name);
    out.resize(out.len() + entry_len - DIRENT_HEADER_LEN - name.len(), 0);
    true
}

/// The on-wire size of a `fuse_dirent` holding a name of `namelen` bytes.
pub const fn dirent_len(namelen: usize) -> usize {
    (DIRENT_HEADER_LEN + namelen).div_ceil(8) * 8
}

// --- little-endian accessors ------------------------------------------------
//
// Every one returns an Option rather than panicking: the input is a buffer the
// guest built, and a request truncated mid-structure must become EINVAL, not a
// vCPU-thread panic that takes the machine down.

pub fn get_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_le_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

pub fn get_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    Some(u64::from_le_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

/// Reads a NUL-terminated name from `bytes`, returning it without the NUL.
///
/// FUSE packs one or two names after a fixed-size structure, each terminated;
/// `rest` is what follows, which is how SYMLINK and RENAME get their second.
pub fn get_name(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = bytes.iter().position(|&b| b == 0)?;
    Some((&bytes[..end], &bytes[end + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_encode_to_the_size_the_kernel_expects() {
        let mut out = Vec::new();
        Attr::default().encode(&mut out);
        assert_eq!(out.len(), ATTR_LEN);
    }

    #[test]
    fn an_entry_encodes_to_the_size_the_kernel_expects() {
        let mut out = Vec::new();
        EntryOut {
            nodeid: 1,
            generation: 0,
            entry_valid: 0,
            attr_valid: 0,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: Attr::default(),
        }
        .encode(&mut out);
        assert_eq!(out.len(), ENTRY_OUT_LEN);
    }

    /// The guest walks a readdir buffer by adding `dirent_len` to a cursor. An
    /// entry we write at any other stride is not a wrong entry, it is a
    /// misaligned parser reading the rest of the buffer as garbage.
    #[test]
    fn dirents_are_padded_to_eight_bytes() {
        for namelen in 0..24 {
            assert_eq!(dirent_len(namelen) % 8, 0);
            assert!(dirent_len(namelen) >= DIRENT_HEADER_LEN + namelen);
        }
        assert_eq!(dirent_len(1), 32);
        assert_eq!(dirent_len(8), 32);
        assert_eq!(dirent_len(9), 40);
    }

    #[test]
    fn a_dirent_that_would_overrun_its_budget_is_refused() {
        let mut out = Vec::new();
        assert!(!push_dirent(&mut out, 8, 1, 1, 4, b"name"));
        assert!(out.is_empty(), "a refused entry must write nothing");
        assert!(push_dirent(&mut out, 4096, 1, 1, 4, b"name"));
        assert_eq!(out.len(), dirent_len(4));
    }

    /// Truncation is a guest-controlled input, so every decoder has to survive
    /// it. A panic here is a vCPU thread dying.
    #[test]
    fn truncated_input_decodes_to_none_rather_than_panicking() {
        assert!(InHeader::parse(&[0u8; 8]).is_none());
        assert!(get_u32(&[0u8; 3], 0).is_none());
        assert!(get_u64(&[0u8; 4], 0).is_none());
        assert!(get_u32(&[0u8; 64], usize::MAX).is_none());
        assert!(get_name(b"no terminator").is_none());
    }

    #[test]
    fn two_names_are_split_at_their_terminators() {
        let (first, rest) = get_name(b"old\0new\0").unwrap();
        assert_eq!(first, b"old");
        let (second, rest) = get_name(rest).unwrap();
        assert_eq!(second, b"new");
        assert!(rest.is_empty());
    }
}

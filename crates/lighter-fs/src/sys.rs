//! The macOS syscall layer.
//!
//! Every `unsafe` block in this crate lives here, and each one is a single
//! libc call with its arguments checked on the safe side of the boundary. The
//! rest of the server is then ordinary Rust that cannot corrupt memory, which
//! matters more than usual for a component whose inputs are supplied by a
//! guest kernel.
//!
//! # The two things macOS does not have
//!
//! **`O_PATH`.** Linux servers keep one of those per inode: a descriptor that
//! names a file without opening it, usable with the `*at` family and immune to
//! renames. The closest macOS equivalent is `O_EVTONLY`, which opens for
//! metadata and event purposes without requiring read permission, and which
//! this module uses for exactly that.
//!
//! **`/proc/self/fd/N`.** Linux servers re-open an inode by path through
//! procfs. macOS instead answers `fcntl(F_GETPATH)`, which resolves the *live*
//! path of an open descriptor — so it follows renames, which is precisely the
//! property we need and precisely what a remembered path string would not give
//! us.

use std::ffi::{CStr, CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::errno;

/// Open a descriptor for metadata only. macOS's nearest thing to `O_PATH`.
const O_EVTONLY: libc::c_int = 0x8000;
/// Open the symlink itself rather than its target.
const O_SYMLINK: libc::c_int = 0x20_0000;

const F_GETPATH: libc::c_int = 50;
const F_PUNCHHOLE: libc::c_int = 99;
const F_PREALLOCATE: libc::c_int = 42;
const F_NOCACHE: libc::c_int = 48;
const F_ALLOCATEALL: libc::c_uint = 0x0004;
const F_PEOFPOSMODE: libc::c_int = 3;

/// `renameatx_np` flags.
const RENAME_SWAP: libc::c_uint = 0x0002;
const RENAME_EXCL: libc::c_uint = 0x0004;

/// Mirrors `fpunchhole_t`.
#[repr(C)]
struct FPunchhole {
    fp_flags: libc::c_uint,
    reserved: libc::c_uint,
    fp_offset: libc::off_t,
    fp_length: libc::off_t,
}

/// Mirrors `fstore_t`.
#[repr(C)]
struct FStore {
    fst_flags: libc::c_uint,
    fst_posmode: libc::c_int,
    fst_offset: libc::off_t,
    fst_length: libc::off_t,
    fst_bytesalloc: libc::off_t,
}

unsafe extern "C" {
    fn renameatx_np(
        fromfd: libc::c_int,
        from: *const libc::c_char,
        tofd: libc::c_int,
        to: *const libc::c_char,
        flags: libc::c_uint,
    ) -> libc::c_int;
}

/// A Linux errno, as the guest will read it.
pub type Result<T> = std::result::Result<T, i32>;

fn check(rc: libc::c_int) -> Result<libc::c_int> {
    if rc < 0 { Err(errno::last()) } else { Ok(rc) }
}

fn check_off(rc: libc::off_t) -> Result<libc::off_t> {
    if rc < 0 { Err(errno::last()) } else { Ok(rc) }
}

fn check_size(rc: libc::ssize_t) -> Result<usize> {
    if rc < 0 {
        Err(errno::last())
    } else {
        Ok(rc as usize)
    }
}

/// A guest-supplied name, validated before it can reach a syscall.
///
/// The three rules are not stylistic. An empty name resolves to the directory
/// itself, `..` walks out of the share, and an embedded slash makes one
/// `openat` traverse arbitrarily far — so a guest that could send any of them
/// could read the whole host filesystem through a bind mount of one directory.
pub fn safe_name(name: &[u8]) -> Result<CString> {
    if name.is_empty() || name == b"." || name == b".." {
        return Err(errno::linux::EINVAL);
    }
    if name.contains(&b'/') {
        return Err(errno::linux::EINVAL);
    }
    CString::new(name).map_err(|_| errno::linux::EINVAL)
}

fn cpath(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| errno::linux::EINVAL)
}

/// The live path of an open descriptor.
///
/// This is what stands in for `/proc/self/fd`, and it is better than the Linux
/// original in one respect: it is resolved from the vnode at call time, so a
/// file renamed since it was opened reports its new path.
pub fn path_of(fd: RawFd) -> Result<PathBuf> {
    let mut buf = [0i8; libc::PATH_MAX as usize];
    // SAFETY: `buf` is PATH_MAX bytes, which is the size F_GETPATH documents
    // as required, and `fd` is a live descriptor for the call's duration.
    check(unsafe { libc::fcntl(fd, F_GETPATH, buf.as_mut_ptr()) })?;
    // SAFETY: F_GETPATH NUL-terminates on success.
    let bytes = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_bytes();
    Ok(PathBuf::from(OsStr::from_bytes(bytes).to_os_string()))
}

/// Opens a metadata-only reference to `name` under `parent`.
///
/// Never follows a final symlink, and never blocks: a FIFO opened for reading
/// waits for a writer, and a server thread that did so would be gone for good.
pub fn open_reference(parent: RawFd, name: &CStr, is_symlink: bool) -> Result<OwnedFd> {
    let flags = if is_symlink {
        // O_EVTONLY cannot be combined with O_SYMLINK; the latter already
        // refuses to follow.
        O_SYMLINK | libc::O_CLOEXEC | libc::O_NONBLOCK
    } else {
        O_EVTONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
    };
    // SAFETY: `name` is a valid NUL-terminated string and `parent` is a live
    // directory descriptor.
    let raw = check(unsafe { libc::openat(parent, name.as_ptr(), flags) })?;
    // SAFETY: `openat` returned a fresh descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Opens a metadata-only reference to an absolute path.
///
/// The parking half of [`crate::inode::Inode`]: a descriptor that was closed to
/// stay under the process limit is reopened by the path it was last known by.
/// Whether that path still names the same file is the caller's question to ask,
/// not this function's to answer.
pub fn open_reference_path(path: &Path, is_symlink: bool) -> Result<OwnedFd> {
    let flags = if is_symlink {
        O_SYMLINK | libc::O_CLOEXEC | libc::O_NONBLOCK
    } else {
        O_EVTONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
    };
    let name = cpath(path)?;
    // SAFETY: `name` is a valid NUL-terminated string.
    let raw = check(unsafe { libc::open(name.as_ptr(), flags) })?;
    // SAFETY: `open` returned a fresh descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// The ceiling macOS puts on one process's open descriptors.
///
/// `setrlimit` is not the whole story: the kernel also enforces
/// `kern.maxfilesperproc`, and an `openat` past it fails with `EMFILE` however
/// generous the rlimit was. The share's descriptor budget is derived from this
/// number, so it has to be the real one.
pub fn descriptor_ceiling() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: an output buffer we own, of the right type.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        u64::MAX
    };
    soft.min(sysctl_u64(c"kern.maxfilesperproc").unwrap_or(10_240))
}

/// Raises this process's descriptor limit as far as the system allows.
///
/// Not housekeeping. A share holds one descriptor per inode the guest is
/// remembering, plus one per file it has open, and a guest walking a package
/// tree remembers tens of thousands. macOS ships a soft limit of 256, so
/// without this the filesystem stops working partway through the first real
/// workload — as `EMFILE` from an `openat`, which surfaces in the guest as a
/// file that exists and cannot be opened.
///
/// Returns the limit now in force.
pub fn raise_file_limit() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: an output buffer we own, of the right type.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 0;
    }
    // The hard limit is not the real ceiling either: macOS also caps a process
    // at `kern.maxfilesperproc`, and asking for more than that fails the whole
    // call rather than clamping.
    let ceiling = sysctl_u64(c"kern.maxfilesperproc").unwrap_or(10_240);
    let wanted = limit.rlim_max.min(ceiling);
    if wanted > limit.rlim_cur {
        let raised = libc::rlimit {
            rlim_cur: wanted,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: a correctly-shaped rlimit whose soft limit is within the
        // hard one.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            return wanted;
        }
    }
    limit.rlim_cur
}

fn sysctl_u64(name: &CStr) -> Option<u64> {
    let mut value: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    // SAFETY: a valid name, and an output buffer of exactly the length passed.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut value as *mut i32 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && value > 0).then_some(value as u64)
}

/// A second descriptor for the same open file.
///
/// Used where a file has just been created and we need a metadata reference to
/// it: `dup` costs a fraction of the `openat` that would otherwise re-resolve a
/// path we are already holding open.
pub fn dup(fd: &OwnedFd) -> Result<OwnedFd> {
    // SAFETY: a live descriptor.
    let raw = check(unsafe { libc::dup(fd.as_raw_fd()) })?;
    // SAFETY: `dup` returned a fresh descriptor that nothing else owns.
    let duplicate = unsafe { OwnedFd::from_raw_fd(raw) };
    // `dup` does not copy FD_CLOEXEC, and a descriptor that survives an exec is
    // a descriptor leaked into every container process we ever spawn.
    // SAFETY: a live descriptor and a flag-setting fcntl with no pointer.
    unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok(duplicate)
}

/// Opens the root of a share.
pub fn open_root(path: &Path) -> Result<OwnedFd> {
    let c = cpath(path)?;
    // SAFETY: a valid NUL-terminated path.
    let raw = check(unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Clones `source` (an absolute path) to `name` under `parent`, copy-on-write.
///
/// APFS metadata operation: the clone shares extents with the source and
/// costs less than half a hardlink. The destination must not exist.
/// Clones an open file to a name: the source is the descriptor, so no path
/// is resolved for it — a `/.vol` identity path costs the kernel a synthetic
/// lookup on every clone, and pnpm clones every file it installs.
pub fn fclonefile_at(source: RawFd, parent: RawFd, name: &CStr) -> Result<()> {
    // SAFETY: a live source descriptor, a live directory descriptor and a
    // valid NUL-terminated name.
    check(unsafe { libc::fclonefileat(source, parent, name.as_ptr(), 0) }).map(|_| ())
}

pub fn clonefile_at(source: &CStr, parent: RawFd, name: &CStr) -> Result<()> {
    // SAFETY: valid NUL-terminated paths and a live directory descriptor. An
    // absolute source makes the source dirfd irrelevant.
    check(unsafe { libc::clonefileat(libc::AT_FDCWD, source.as_ptr(), parent, name.as_ptr(), 0) })
        .map(|_| ())
}

/// The volume's own name for whatever `fd` holds open: `/.vol/dev/ino`.
///
/// `F_GETPATH` is best-effort: it answers from the vnode name cache, and for
/// a file created under a temporary name and renamed into place it can
/// answer with the temporary name — which no longer exists. The identity
/// path has no name to go stale; the descriptor keeps the numbers pinned to
/// the right file. Not every filesystem serves `/.vol`, so callers keep
/// `path_of` as the fallback.
pub fn identity_path(fd: RawFd) -> Result<PathBuf> {
    let st = stat_fd(fd)?;
    Ok(PathBuf::from(format!("/.vol/{}/{}", st.st_dev, st.st_ino)))
}

/// Re-opens an inode for real I/O.
///
/// By identity first: reopening "at whatever path it currently occupies" was
/// the design, and F_GETPATH quietly does not promise that (see
/// [`identity_path`]). The path is the fallback for a filesystem with no
/// `/.vol`, and the error reported is the fallback's, which is what this
/// function has always returned.
pub fn reopen(reference: RawFd, linux_flags: u32, mode: u32) -> Result<OwnedFd> {
    if let Ok(fd) = identity_path(reference).and_then(|p| open_path(&p, linux_flags, mode)) {
        return Ok(fd);
    }
    let path = path_of(reference)?;
    open_path(&path, linux_flags, mode)
}

/// Opens a host path with flags expressed in the *guest's* numbering.
pub fn open_path(path: &Path, linux_flags: u32, mode: u32) -> Result<OwnedFd> {
    let c = cpath(path)?;
    let flags = translate_open_flags(linux_flags) | libc::O_CLOEXEC;
    // SAFETY: a valid NUL-terminated path; `mode` is only consulted when the
    // translated flags contain O_CREAT.
    let raw = check(unsafe { libc::open(c.as_ptr(), flags, mode as libc::c_uint) })?;
    // SAFETY: fresh descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if linux_flags & LINUX_O_DIRECT != 0 {
        // macOS has no O_DIRECT; F_NOCACHE is the equivalent request, and a
        // failure to honour it is a performance matter rather than a
        // correctness one, so it is not propagated.
        // SAFETY: a live descriptor and a flag-setting fcntl with no pointer.
        unsafe { libc::fcntl(fd.as_raw_fd(), F_NOCACHE, 1) };
    }
    Ok(fd)
}

/// A fresh descriptor on a directory the caller already holds, for a listing:
/// `openat(fd, ".")`. Each listing needs its own offset (see the server's
/// `list`), and this costs one name lookup where a reopen by path walked the
/// whole path from the root.
pub fn open_directory_self(dir: RawFd) -> Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: a live directory descriptor and a constant path.
    let raw = check(unsafe { libc::openat(dir, c".".as_ptr(), flags) })?;
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Opens a name under a directory for real I/O.
pub fn openat_path(parent: RawFd, name: &CStr, linux_flags: u32, mode: u32) -> Result<OwnedFd> {
    let flags = translate_open_flags(linux_flags) | libc::O_CLOEXEC;
    // SAFETY: valid name, live directory descriptor.
    let raw = check(unsafe { libc::openat(parent, name.as_ptr(), flags, mode as libc::c_uint) })?;
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// arm64 Linux uses the `asm-generic` numbering; macOS uses its own. The two
/// agree only on the access mode in the low two bits.
const LINUX_O_CREAT: u32 = 0o100;
const LINUX_O_EXCL: u32 = 0o200;
const LINUX_O_NOCTTY: u32 = 0o400;
const LINUX_O_TRUNC: u32 = 0o1000;
const LINUX_O_APPEND: u32 = 0o2000;
const LINUX_O_NONBLOCK: u32 = 0o4000;
const LINUX_O_DSYNC: u32 = 0o10000;
const LINUX_O_DIRECT: u32 = 0o40000;
const LINUX_O_DIRECTORY: u32 = 0o200000;
pub const LINUX_O_NOFOLLOW: u32 = 0o400000;
const LINUX_O_SYNC: u32 = 0o4010000;

/// Rewrites guest open flags into host ones.
///
/// Getting this wrong is quiet in the worst way: Linux `O_APPEND` is 0o2000,
/// which is macOS's `O_TRUNC | O_EXCL`. A pass-through would turn every
/// append-mode open into a truncating one — the file opens, the write
/// succeeds, and the previous contents are simply gone.
pub fn translate_open_flags(linux: u32) -> libc::c_int {
    let mut out = (linux & 0o3) as libc::c_int;
    if linux & LINUX_O_CREAT != 0 {
        out |= libc::O_CREAT;
    }
    if linux & LINUX_O_EXCL != 0 {
        out |= libc::O_EXCL;
    }
    if linux & LINUX_O_NOCTTY != 0 {
        out |= libc::O_NOCTTY;
    }
    if linux & LINUX_O_TRUNC != 0 {
        out |= libc::O_TRUNC;
    }
    if linux & LINUX_O_APPEND != 0 {
        out |= libc::O_APPEND;
    }
    if linux & LINUX_O_NONBLOCK != 0 {
        out |= libc::O_NONBLOCK;
    }
    if linux & (LINUX_O_DSYNC | LINUX_O_SYNC) != 0 {
        out |= libc::O_SYNC;
    }
    if linux & LINUX_O_DIRECTORY != 0 {
        out |= libc::O_DIRECTORY;
    }
    if linux & LINUX_O_NOFOLLOW != 0 {
        out |= libc::O_NOFOLLOW;
    }
    out
}

/// `fstatat`, without following a final symlink.
pub fn stat_at(parent: RawFd, name: &CStr) -> Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid name, live descriptor, and `st` is a correctly-sized
    // output buffer we own.
    check(unsafe { libc::fstatat(parent, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

/// `fstat` on an open descriptor.
pub fn stat_fd(fd: RawFd) -> Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: live descriptor, owned output buffer.
    check(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

pub fn mkdir_at(parent: RawFd, name: &CStr, mode: u32) -> Result<()> {
    // SAFETY: valid name, live descriptor.
    check(unsafe { libc::mkdirat(parent, name.as_ptr(), mode as libc::mode_t) }).map(|_| ())
}

pub fn unlink_at(parent: RawFd, name: &CStr, dir: bool) -> Result<()> {
    let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: valid name, live descriptor.
    check(unsafe { libc::unlinkat(parent, name.as_ptr(), flags) }).map(|_| ())
}

pub fn symlink_at(target: &CStr, parent: RawFd, name: &CStr) -> Result<()> {
    // SAFETY: two valid NUL-terminated strings, live descriptor.
    check(unsafe { libc::symlinkat(target.as_ptr(), parent, name.as_ptr()) }).map(|_| ())
}

pub fn link_at(old_parent: RawFd, old: &CStr, parent: RawFd, name: &CStr) -> Result<()> {
    // SAFETY: valid names, live descriptors. AT_SYMLINK_FOLLOW is omitted so a
    // hard link to a symlink links the symlink, which is Linux's default.
    check(unsafe { libc::linkat(old_parent, old.as_ptr(), parent, name.as_ptr(), 0) }).map(|_| ())
}

/// `renameat`, plus the two Linux `RENAME_*` flags macOS can honour.
pub fn rename_at(
    old_parent: RawFd,
    old: &CStr,
    new_parent: RawFd,
    new: &CStr,
    linux_flags: u32,
) -> Result<()> {
    use crate::fuse::rename;
    if linux_flags & rename::WHITEOUT != 0 {
        // Whiteouts exist for overlayfs, which never sits on a bind mount.
        return Err(errno::linux::EOPNOTSUPP);
    }
    let mut host = 0;
    if linux_flags & rename::NOREPLACE != 0 {
        host |= RENAME_EXCL;
    }
    if linux_flags & rename::EXCHANGE != 0 {
        host |= RENAME_SWAP;
    }
    let rc = if host == 0 {
        // SAFETY: valid names, live descriptors.
        unsafe { libc::renameat(old_parent, old.as_ptr(), new_parent, new.as_ptr()) }
    } else {
        // SAFETY: same, and `renameatx_np` takes exactly these arguments.
        unsafe { renameatx_np(old_parent, old.as_ptr(), new_parent, new.as_ptr(), host) }
    };
    check(rc).map(|_| ())
}

pub fn readlink_at(parent: RawFd, name: &CStr) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: valid name, live descriptor, and a buffer we own of the length
    // we pass.
    let len = check_size(unsafe {
        libc::readlinkat(
            parent,
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
        )
    })?;
    buf.truncate(len);
    Ok(buf)
}

/// `mknod`, which macOS has no `*at` form of.
///
/// Resolved through the parent's live path rather than remembered: the window
/// between reading the path and using it is the same window every other
/// path-based syscall has, and the alternative is not supporting device nodes
/// and FIFOs at all.
pub fn mknod_at(parent: RawFd, name: &CStr, mode: u32, rdev: u32) -> Result<()> {
    let mut path = path_of(parent)?;
    path.push(OsStr::from_bytes(name.to_bytes()));
    let c = cpath(&path)?;
    // SAFETY: a valid NUL-terminated path.
    check(unsafe { libc::mknod(c.as_ptr(), mode as libc::mode_t, rdev as libc::dev_t) }).map(|_| ())
}

pub fn chmod_at(parent: RawFd, name: &CStr, mode: u32) -> Result<()> {
    // SAFETY: valid name, live descriptor.
    check(unsafe {
        libc::fchmodat(
            parent,
            name.as_ptr(),
            mode as libc::mode_t,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })
    .map(|_| ())
}

pub fn chown_at(parent: RawFd, name: &CStr, uid: u32, gid: u32) -> Result<()> {
    // SAFETY: valid name, live descriptor. -1 leaves a field unchanged.
    check(unsafe {
        libc::fchownat(
            parent,
            name.as_ptr(),
            uid as libc::uid_t,
            gid as libc::gid_t,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })
    .map(|_| ())
}

/// One timestamp for [`utimes_at`]: leave, set to now, or set to a value.
#[derive(Debug, Clone, Copy)]
pub enum TimeSpec {
    Omit,
    Now,
    At(i64, u32),
}

impl TimeSpec {
    fn to_timespec(self) -> libc::timespec {
        match self {
            TimeSpec::Omit => libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            TimeSpec::Now => libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            },
            TimeSpec::At(sec, nsec) => libc::timespec {
                tv_sec: sec as libc::time_t,
                tv_nsec: nsec as libc::c_long,
            },
        }
    }
}

pub fn utimes_at(parent: RawFd, name: &CStr, atime: TimeSpec, mtime: TimeSpec) -> Result<()> {
    let times = [atime.to_timespec(), mtime.to_timespec()];
    // SAFETY: valid name, live descriptor, and a two-element array which is
    // what `utimensat` reads.
    check(unsafe {
        libc::utimensat(
            parent,
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })
    .map(|_| ())
}

/// `fchmod`: the descriptor form, for the apply queue, which holds the file
/// rather than a path that may have moved by the time the job runs.
pub fn chmod_fd(fd: RawFd, mode: u32) -> Result<()> {
    // SAFETY: live descriptor.
    check(unsafe { libc::fchmod(fd, mode as libc::mode_t) }).map(|_| ())
}

/// `futimens`, for the same reason as [`chmod_fd`].
pub fn utimes_fd(fd: RawFd, atime: TimeSpec, mtime: TimeSpec) -> Result<()> {
    let times = [atime.to_timespec(), mtime.to_timespec()];
    // SAFETY: live descriptor and a two-element array, which is what
    // `futimens` reads.
    check(unsafe { libc::futimens(fd, times.as_ptr()) }).map(|_| ())
}

pub fn truncate_fd(fd: RawFd, size: u64) -> Result<()> {
    // SAFETY: live descriptor.
    check(unsafe { libc::ftruncate(fd, size as libc::off_t) }).map(|_| ())
}

pub fn read_at(fd: RawFd, buf: &mut [u8], offset: u64) -> Result<usize> {
    // SAFETY: a buffer we own of the length we pass, and a live descriptor.
    check_size(unsafe {
        libc::pread(
            fd,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            offset as libc::off_t,
        )
    })
}

/// `preadv`: one read scattered over `iovs`, which may point anywhere the
/// caller may write — into guest memory, for a reply that never touches a
/// host buffer.
pub fn read_vectored_at(fd: RawFd, iovs: &[libc::iovec], offset: u64) -> Result<usize> {
    // SAFETY: the caller vouches for every iovec (a live descriptor, and
    // spans it may write); the count is bounded by IOV_MAX below.
    check_size(unsafe {
        libc::preadv(
            fd,
            iovs.as_ptr(),
            iovs.len().min(libc::IOV_MAX as usize) as libc::c_int,
            offset as libc::off_t,
        )
    })
}

pub fn write_at(fd: RawFd, buf: &[u8], offset: u64) -> Result<usize> {
    // SAFETY: a buffer we own of the length we pass, and a live descriptor.
    check_size(unsafe {
        libc::pwrite(
            fd,
            buf.as_ptr() as *const libc::c_void,
            buf.len(),
            offset as libc::off_t,
        )
    })
}

/// Appends to a descriptor opened `O_APPEND`.
///
/// `pwrite` on macOS honours the offset even for an append-mode descriptor,
/// which would overwrite rather than append — so append has to use `write`.
pub fn write_append(fd: RawFd, buf: &[u8]) -> Result<usize> {
    // SAFETY: a buffer we own of the length we pass, and a live descriptor.
    check_size(unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) })
}

/// Flushes a file's data to stable storage.
///
/// `fsync` on macOS returns once the write has reached the drive's cache, not
/// the platter; `F_FULLFSYNC` is the real barrier. We use plain `fsync`
/// deliberately: it is what the guest's own `fsync` would give it on a Linux
/// host, matching semantics matters more here than exceeding them, and
/// `F_FULLFSYNC` costs an order of magnitude more on every `npm install`.
pub fn fsync(fd: RawFd, _datasync: bool) -> Result<()> {
    // SAFETY: live descriptor.
    check(unsafe { libc::fsync(fd) }).map(|_| ())
}

pub fn statfs(path: &Path) -> Result<libc::statfs> {
    let c = cpath(path)?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid path, owned output buffer.
    check(unsafe { libc::statfs(c.as_ptr(), &mut st) })?;
    Ok(st)
}

/// Linux `SEEK_DATA` is 3 and `SEEK_HOLE` is 4. macOS has them the other way
/// round. Passing the guest's value through finds the next hole when it asked
/// for the next data, which `cp --sparse` reads as a file of pure zeroes.
pub fn seek(fd: RawFd, offset: u64, linux_whence: u32) -> Result<u64> {
    const LINUX_SEEK_DATA: u32 = 3;
    const LINUX_SEEK_HOLE: u32 = 4;
    let whence = match linux_whence {
        0 => libc::SEEK_SET,
        1 => libc::SEEK_CUR,
        2 => libc::SEEK_END,
        LINUX_SEEK_DATA => libc::SEEK_DATA,
        LINUX_SEEK_HOLE => libc::SEEK_HOLE,
        _ => return Err(errno::linux::EINVAL),
    };
    // SAFETY: live descriptor.
    check_off(unsafe { libc::lseek(fd, offset as libc::off_t, whence) }).map(|v| v as u64)
}

/// Linux `fallocate`, as far as macOS can honour it.
pub fn fallocate(fd: RawFd, mode: u32, offset: u64, length: u64) -> Result<()> {
    const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
    const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;

    if mode & FALLOC_FL_PUNCH_HOLE != 0 {
        let arg = FPunchhole {
            fp_flags: 0,
            reserved: 0,
            fp_offset: offset as libc::off_t,
            fp_length: length as libc::off_t,
        };
        // SAFETY: live descriptor and a correctly-shaped fpunchhole_t.
        return check(unsafe { libc::fcntl(fd, F_PUNCHHOLE, &arg) }).map(|_| ());
    }
    if mode & !FALLOC_FL_KEEP_SIZE != 0 {
        return Err(errno::linux::EOPNOTSUPP);
    }

    let end = offset.checked_add(length).ok_or(errno::linux::EINVAL)?;
    let st = stat_fd(fd)?;
    let current = st.st_size as u64;
    if end > current {
        let mut arg = FStore {
            fst_flags: F_ALLOCATEALL,
            fst_posmode: F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: (end - current) as libc::off_t,
            fst_bytesalloc: 0,
        };
        // Reserving space is advisory: a filesystem that declines still has to
        // produce the file, which `ftruncate` below does.
        // SAFETY: live descriptor and a correctly-shaped fstore_t.
        unsafe { libc::fcntl(fd, F_PREALLOCATE, &mut arg) };
        if mode & FALLOC_FL_KEEP_SIZE == 0 {
            truncate_fd(fd, end)?;
        }
    }
    Ok(())
}

// --- extended attributes ----------------------------------------------------

/// macOS `getxattr` option: do not follow a final symlink.
const XATTR_NOFOLLOW: libc::c_int = 0x0001;
const XATTR_CREATE: libc::c_int = 0x0002;
const XATTR_REPLACE: libc::c_int = 0x0004;

pub fn get_xattr(path: &CStr, name: &CStr, buf: &mut [u8]) -> Result<usize> {
    // SAFETY: two valid NUL-terminated strings and a buffer we own of the
    // length we pass. A null buffer with length zero is how the size is asked
    // for, which is what `getxattr` documents.
    check_size(unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
            XATTR_NOFOLLOW,
        )
    })
}

pub fn set_xattr(path: &CStr, name: &CStr, value: &[u8], linux_flags: u32) -> Result<()> {
    let mut options = XATTR_NOFOLLOW;
    if linux_flags & 1 != 0 {
        options |= XATTR_CREATE;
    }
    if linux_flags & 2 != 0 {
        options |= XATTR_REPLACE;
    }
    // SAFETY: valid strings and a buffer we own of the stated length.
    check(unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
            options,
        )
    })
    .map(|_| ())
}

pub fn list_xattr(path: &CStr, buf: &mut [u8]) -> Result<usize> {
    // SAFETY: valid string and a buffer we own of the stated length.
    check_size(unsafe {
        libc::listxattr(
            path.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            XATTR_NOFOLLOW,
        )
    })
}

pub fn remove_xattr(path: &CStr, name: &CStr) -> Result<()> {
    // SAFETY: two valid NUL-terminated strings.
    check(unsafe { libc::removexattr(path.as_ptr(), name.as_ptr(), XATTR_NOFOLLOW) }).map(|_| ())
}

/// `faccessat` with the caller's effective identity, which is the only one this
/// process has.
pub fn access(path: &CStr, mask: u32) -> Result<()> {
    // SAFETY: a valid NUL-terminated path.
    check(unsafe {
        libc::faccessat(
            libc::AT_FDCWD,
            path.as_ptr(),
            mask as libc::c_int,
            libc::AT_EACCESS,
        )
    })
    .map(|_| ())
}

/// The C string form of a host path, for the syscalls that take one.
pub fn c_path(path: &Path) -> Result<CString> {
    cpath(path)
}

// --- directories ------------------------------------------------------------

/// An open directory stream.
///
/// `DIR*` is not thread-safe and its cursor is shared state, so this is not
/// `Sync`; the server keeps one behind a mutex per open directory, which is
/// also what the guest's own `readdir` semantics require.
///
/// # Why nothing here is resumable
///
/// FUSE readdir offsets are opaque to the kernel but they are *durable*: it
/// stores them in its cached page for a directory and may hand one back long
/// after it got it, from a different process, and — once the server stops being
/// told about opens — after the server has closed and reopened the stream.
///
/// A `telldir` cookie survives none of that. On BSD it is an index into a list
/// held inside the `DIR`, freed by `closedir` and by `rewinddir`, so a cookie
/// presented to a different stream means something arbitrary. The symptom is
/// not an error: `seekdir` quietly lands somewhere else, and a directory
/// listing repeats entries it has already returned. `cp -a` then decides the
/// repeat is a hard link to the copy it already made and fails outright, which
/// is how this was found.
///
/// So a stream here is read once, in full, and the caller keeps the result;
/// see [`crate::inode::OpenDir`]. Offsets are then indices into an immutable
/// list, which means the same thing to everyone forever.
pub struct Dir {
    handle: *mut libc::DIR,
    /// How many entries have been read since the last rewind.
    position: u64,
}

// SAFETY: a `DIR*` may be used from any one thread at a time. The server never
// shares one without a mutex, and nothing in this type is thread-local.
unsafe impl Send for Dir {}

/// One entry, copied out before the next `readdir` invalidates it.
pub struct DirEntry {
    pub ino: u64,
    pub kind: u32,
    pub name: Vec<u8>,
    /// The cursor value that resumes *after* this entry.
    pub next_offset: u64,
}

impl Dir {
    /// Takes ownership of `fd`, which must be an open directory.
    pub fn from_fd(fd: OwnedFd) -> Result<Dir> {
        let raw = fd.as_raw_fd();
        // SAFETY: a live directory descriptor. On success `fdopendir` takes
        // ownership, so the `OwnedFd` is forgotten rather than dropped; on
        // failure it is dropped normally and the descriptor is closed once.
        let handle = unsafe { libc::fdopendir(raw) };
        if handle.is_null() {
            return Err(errno::last());
        }
        std::mem::forget(fd);
        Ok(Dir {
            handle,
            position: 0,
        })
    }

    /// Reads the whole directory.
    ///
    /// One pass, one allocation, and then the stream is done with. Paging
    /// through a `DIR*` across separate requests is what a server does when it
    /// can trust the offsets it hands out, and this one cannot — see the note
    /// on the type.
    pub fn read_all(&mut self) -> Result<Vec<DirEntry>> {
        let mut entries = Vec::new();
        while let Some(entry) = self.read()? {
            entries.push(entry);
        }
        Ok(entries)
    }

    /// The next entry, or `None` at end of directory.
    pub fn read(&mut self) -> Result<Option<DirEntry>> {
        // `readdir` reports end-of-directory and failure identically, so errno
        // has to be cleared first to tell them apart.
        // SAFETY: setting errno through libc's accessor.
        unsafe { *libc::__error() = 0 };
        // SAFETY: a live DIR*. The returned pointer is owned by the stream and
        // is copied out before any further call invalidates it.
        let entry = unsafe { libc::readdir(self.handle) };
        if entry.is_null() {
            // SAFETY: reading errno through libc's accessor.
            let err = unsafe { *libc::__error() };
            return if err == 0 {
                Ok(None)
            } else {
                Err(errno::to_linux(err))
            };
        }
        // SAFETY: non-null, and valid until the next readdir on this stream.
        let entry = unsafe { &*entry };
        let namelen = entry.d_namlen as usize;
        let name = entry.d_name[..namelen]
            .iter()
            .map(|&c| c as u8)
            .collect::<Vec<u8>>();
        self.position += 1;
        Ok(Some(DirEntry {
            ino: entry.d_ino,
            kind: u32::from(entry.d_type),
            name,
            next_offset: self.position,
        }))
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: a live DIR* this type owns; closedir also closes the fd it
        // was built from.
        unsafe { libc::closedir(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this test exists for: Linux O_APPEND is bit 0o2000, which on
    /// macOS is O_TRUNC | O_EXCL. Forwarding the number unchanged makes every
    /// `>>` redirection empty the file it was appending to.
    #[test]
    fn append_does_not_become_truncate() {
        let host = translate_open_flags(LINUX_O_APPEND | 1);
        assert_eq!(host & libc::O_APPEND, libc::O_APPEND);
        assert_eq!(host & libc::O_TRUNC, 0, "append must never imply truncate");
        assert_eq!(host & libc::O_EXCL, 0);
        assert_eq!(host & 0o3, libc::O_WRONLY);
    }

    #[test]
    fn the_access_mode_is_the_low_two_bits_on_both_systems() {
        assert_eq!(translate_open_flags(0) & 0o3, libc::O_RDONLY);
        assert_eq!(translate_open_flags(1) & 0o3, libc::O_WRONLY);
        assert_eq!(translate_open_flags(2) & 0o3, libc::O_RDWR);
    }

    #[test]
    fn create_exclusive_translates_whole() {
        let host = translate_open_flags(LINUX_O_CREAT | LINUX_O_EXCL | LINUX_O_TRUNC | 2);
        assert_eq!(host & libc::O_CREAT, libc::O_CREAT);
        assert_eq!(host & libc::O_EXCL, libc::O_EXCL);
        assert_eq!(host & libc::O_TRUNC, libc::O_TRUNC);
    }

    /// Flags macOS has no equivalent for must be dropped, not forwarded: the
    /// same bit means something else here.
    #[test]
    fn flags_macos_lacks_are_dropped() {
        let host = translate_open_flags(LINUX_O_DIRECT | 0o100000 /* O_LARGEFILE */);
        assert_eq!(host, libc::O_RDONLY);
    }

    #[test]
    fn directory_and_nofollow_survive_translation() {
        let host = translate_open_flags(LINUX_O_DIRECTORY | LINUX_O_NOFOLLOW);
        assert_eq!(host & libc::O_DIRECTORY, libc::O_DIRECTORY);
        assert_eq!(host & libc::O_NOFOLLOW, libc::O_NOFOLLOW);
    }

    /// A share is a directory, and every name the guest sends is resolved
    /// under it. These three are how a guest would try to leave.
    #[test]
    fn escaping_names_are_refused() {
        for name in [&b""[..], b".", b"..", b"../etc/passwd", b"a/b"] {
            assert!(
                safe_name(name).is_err(),
                "{:?} must not reach a syscall",
                String::from_utf8_lossy(name)
            );
        }
        assert!(safe_name(b"ordinary.txt").is_ok());
        assert!(safe_name(b"...").is_ok(), "three dots is an ordinary name");
        assert!(safe_name(b"has\0nul").is_err());
    }

    /// SEEK_DATA and SEEK_HOLE are swapped between the two systems.
    #[test]
    fn sparse_seek_constants_are_not_the_same_number() {
        assert_ne!(libc::SEEK_DATA, 3, "macOS SEEK_DATA is 4; Linux's is 3");
        assert_eq!(libc::SEEK_HOLE, 3);
        assert_eq!(libc::SEEK_DATA, 4);
    }

    #[test]
    fn a_descriptor_reports_its_own_path() {
        let dir = std::env::temp_dir();
        let fd = open_root(&dir).unwrap();
        let reported = path_of(fd.as_raw_fd()).unwrap();
        assert!(
            reported.starts_with("/"),
            "F_GETPATH must give an absolute path, got {reported:?}"
        );
    }
}

/// Raises the calling thread to the user-interactive QoS class, when
/// `LIGHTER_SERVER_QOS` asks for it.
///
/// A guest waiting on the share is waiting on these threads: the poller that
/// takes its request off the ring, the worker that answers it, and the apply
/// queue that lands the outcome on the Mac. On a Mac whose every core is a
/// vCPU — eight on an M1, with eight vCPUs — they compete with the idle
/// polling of the guest they serve. macOS schedules by QoS class before
/// anything else.
pub fn raise_server_qos() {
    static WANTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let wanted =
        *WANTED.get_or_init(|| std::env::var("LIGHTER_SERVER_QOS").is_ok_and(|v| v != "0"));
    if !wanted {
        return;
    }
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    // SAFETY: a plain call on the current thread with constant arguments.
    let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    if rc != 0 {
        tracing::debug!(rc, "could not raise the server thread's QoS");
    }
}

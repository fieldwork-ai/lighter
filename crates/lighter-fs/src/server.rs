//! The passthrough filesystem.
//!
//! One request in, one reply out, with no state beyond the inode and handle
//! registry. Every operation is a small translation followed by a macOS
//! syscall, and the translations are where all the interesting bugs live —
//! errno tables, open flags, seek constants and uid mapping each differ between
//! the two systems in ways that are invisible until something behaves
//! plausibly and wrongly.
//!
//! # Coherence, and what we deliberately do not cache
//!
//! Attribute and entry timeouts are zero and `FOPEN_KEEP_CACHE` is never set,
//! so the guest revalidates on every path resolution and drops a file's page
//! cache whenever it is opened. That is the strictest useful setting: a host
//! edit is visible to the guest at its next `open`, and a guest write is
//! visible to the host as soon as the guest's own page cache is flushed.
//!
//! It is also slow, and making it fast without making it wrong is the whole of
//! the next milestone. The caching goes in *above* this file, driven by host
//! change notifications; nothing here should ever be tempted to guess.
//!
//! # Identity
//!
//! A container runs as root and expects to own the files it can see. The host
//! process is one ordinary user. The map is therefore one pair in each
//! direction — host user to guest root, and back — and everything else passes
//! through unchanged so that a genuinely foreign uid still looks foreign.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::cache::{Answer, Invalidator, Policy, Timings};
use crate::errno::linux;
use crate::fsevents::Watcher;
use crate::fuse::{self, Attr, EntryOut, InHeader, get_name, get_u32, get_u64, op};
use crate::inode::{Handle, Inode, Located, OpenDir, OpenFile, Reference, Registry};
use crate::opencache::OpenCache;
use crate::stats::Stats;
use crate::sys::{self, TimeSpec};

/// The largest write we accept in one request, and the readahead we permit.
///
/// 256 KiB is 64 pages, which is what `max_pages` below advertises. Larger
/// values are legal and faster, but every one of them has to fit in a single
/// descriptor chain, and our virtqueues hold 256 descriptors.
pub const MAX_WRITE: u32 = 256 * 1024;
const MAX_PAGES: u16 = (MAX_WRITE / 4096) as u16;

/// A guest name may not exceed this. Matches Linux's `NAME_MAX`.
const NAME_MAX: usize = 255;

/// Where a reply is written.
///
/// The server does not know whether it is filling a virtqueue descriptor chain
/// or a test's `Vec`, which is what lets the whole protocol be tested without a
/// VM.
pub trait Sink {
    /// How many bytes may still be written, header included.
    fn capacity(&self) -> usize;
    /// Appends. Fails rather than truncating if it would exceed
    /// [`Sink::capacity`], because a short reply is read by the guest as a
    /// well-formed reply about something else.
    fn write(&mut self, data: &[u8]) -> Result<(), SinkFull>;
}

/// The reply did not fit the buffers the guest supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkFull;

impl Sink for Vec<u8> {
    fn capacity(&self) -> usize {
        usize::MAX
    }

    fn write(&mut self, data: &[u8]) -> Result<(), SinkFull> {
        self.extend_from_slice(data);
        Ok(())
    }
}

/// Why held-back creates were queued, by trigger: diagnostics, under
/// `LIGHTER_FS_STATS`, for the question "who is making pnpm's files early".
const MATERIALIZE_WHY: [&str; 10] = [
    "settle-file",
    "settle-dir",
    "all",
    "write",
    "setattr",
    "rename",
    "link",
    "release",
    "fsync",
    "other",
];
static MATERIALIZE_N: [std::sync::atomic::AtomicU64; 10] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// A stat of nothing, typed like `inode`: what a bound inode answers with
/// when it cannot be stat'd at all (its create failed), so the kind at
/// least is right and the guest sees an empty file rather than EIO.
fn libc_stat_default(inode: &Inode) -> libc::stat {
    // SAFETY: `libc::stat` is plain data; all-zero is a valid value.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    st.st_mode = if inode.is_dir {
        libc::S_IFDIR | 0o755
    } else if inode.is_symlink {
        libc::S_IFLNK | 0o777
    } else {
        libc::S_IFREG | 0o644
    };
    st.st_nlink = 1;
    st
}

/// A create acknowledged and not yet queued, with everything the guest has
/// promised about it since: the name it will be made under (a rename of a
/// held file just moves this), the bytes written into it, the flags and
/// mode of the open. See [`Server::materialize_why`].
///
/// pnpm writes every store file under a temporary name, closes it and
/// renames it into place — a create, a write and a rename queued for each
/// of fifty thousand files an install. Held, the three become one job that
/// creates the final name and writes the bytes into it.
struct Held {
    parent: std::sync::Arc<Inode>,
    name: CString,
    flags: u32,
    mode: u32,
    writes: Vec<(u64, Vec<u8>)>,
    bytes: usize,
    since: std::time::Instant,
}

/// Bytes one held file may accumulate before its create is queued, and all
/// held files together. Small on purpose: a held file is one the Mac cannot
/// see yet.
const HELD_FILE_CAP: usize = 4 << 20;
const HELD_TOTAL_CAP: usize = 64 << 20;

/// How long a create may stay held with nothing forcing it. pnpm writes,
/// closes and renames a store file within a millisecond; a file a container
/// wrote and forgot about is on the Mac within this.
const HOLD_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

/// Files up to this size are copied rather than cloned; see `clone_over`.
/// `LIGHTER_FS_COPY_MAX` overrides it (0 clones everything).
fn copy_instead_of_clone_max() -> u64 {
    std::env::var("LIGHTER_FS_COPY_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256 << 10)
}

/// How many creates may be held at once. Past this the oldest are queued,
/// and queuing is where the window's backpressure lives: a held create
/// occupies no slot, so without a cap a `cp -a` of sixty thousand files
/// returned in three seconds with six seconds of creates still to come —
/// paid by whatever ran next, and a visibility window of seconds rather
/// than milliseconds. pnpm holds each store file for about a millisecond
/// across four threads; a few hundred is room to spare.
const HELD_MAX: usize = 256;

#[derive(Default)]
struct Holding {
    map: std::sync::Mutex<std::collections::HashMap<u64, Held>>,
    /// Held nodeids, oldest first: what the cap and the settler walk.
    order: std::sync::Mutex<std::collections::VecDeque<u64>>,
    bytes: AtomicUsize,
}

/// Writes all of `data` at `offset`.
fn write_fully(fd: std::os::fd::RawFd, data: &[u8], offset: u64) -> Result<(), i32> {
    let mut at = 0usize;
    while at < data.len() {
        match sys::write_at(fd, &data[at..], offset + at as u64) {
            Ok(0) => return Err(linux::EIO),
            Ok(n) => at += n,
            Err(errno) => return Err(errno),
        }
    }
    Ok(())
}

/// The job a held create becomes: the open, the attributes promised
/// meanwhile, the bytes written meanwhile, and the binding.
fn create_job(
    registry: std::sync::Arc<Registry>,
    open_cache: Arc<OpenCache>,
    park_creates: bool,
    nodeid: u64,
    inode: std::sync::Arc<Inode>,
    held: Held,
) -> crate::apply::Job {
    let Held {
        parent,
        name,
        flags,
        mode,
        writes,
        ..
    } = held;
    let keys = crate::apply::Keys::of(&[parent.id(), nodeid]);
    let job = move || {
        const LINUX_O_CREAT: u32 = 0o100;
        const LINUX_O_EXCL: u32 = 0o200;
        const LINUX_O_NOFOLLOW: u32 = 0o400000;
        if inode.is_cancelled() {
            // Replaced before it existed; nothing to make.
            inode.bind_failed(linux::ENOENT);
            for _ in &writes {
                inode.write_applied(Err(linux::ENOENT));
            }
            parent.remove_pending_child(name.to_bytes(), nodeid);
            return;
        }
        // A write-only open is made read-write on the host: the guest's
        // own access check has passed, the creating open ignores the mode
        // it sets, and the cached descriptor then serves the read a clone
        // or a copy of this file will make of it — pnpm imports every store
        // file it has just written, and a write-only descriptor cannot be
        // read from or cloned from.
        let host_flags = if flags & 0o3 == 1 {
            (flags & !0o3) | 2
        } else {
            flags
        };
        let result = (|| {
            let parent_fd = parent.reference()?;
            let fd = sys::openat_path(
                parent_fd.raw_fd(),
                &name,
                host_flags | LINUX_O_CREAT | LINUX_O_EXCL | LINUX_O_NOFOLLOW,
                mode,
            )?;
            let st = sys::stat_fd(fd.as_raw_fd())?;
            Ok((fd, st))
        })();
        match result {
            Ok((fd, st)) => {
                // The bytes promised while the create was held back are the
                // create's to keep — and then the attributes, since a write
                // moves the time and the last promise made wins.
                for (offset, data) in &writes {
                    inode.write_applied(write_fully(fd.as_raw_fd(), data, *offset));
                }
                if let Some(meta) = inode.pending_meta() {
                    if meta.mode & 0o7777 != mode & 0o7777 {
                        let _ = sys::chmod_fd(fd.as_raw_fd(), meta.mode & 0o7777);
                    }
                    if meta.atime != meta.born || meta.mtime != meta.born {
                        let _ = sys::utimes_fd(
                            fd.as_raw_fd(),
                            TimeSpec::At(meta.atime.0, meta.atime.1 as u32),
                            TimeSpec::At(meta.mtime.0, meta.mtime.1 as u32),
                        );
                    }
                }
                // Two descriptors, as the synchronous path keeps two: a
                // metadata reference for the inode, and the open file itself
                // in the open cache — which is what lets the guest keep
                // reading and writing after an unlink, exactly as a real
                // file handle would. At a full share the reference would be
                // parked the instant it was bound — a dup, a stat, a path
                // query and a close for nothing, fifty thousand times an
                // install — so there it is bound parked, the open file still
                // cached for the writes that follow.
                let meta = if park_creates && registry.at_budget() {
                    None
                } else {
                    sys::dup(&fd).ok()
                };
                // Cache first, bind second: a reader that settles the instant
                // `pending` clears must find the descriptor already there, or
                // it reopens by path — which an unlinked file no longer has.
                open_cache.put_file(
                    nodeid,
                    std::sync::Arc::new(OpenFile {
                        fd,
                        readable: true,
                        append: false,
                        writable: flags & 0o3 != 0,
                    }),
                );
                match meta {
                    Some(meta) => {
                        registry.bind_pending(nodeid, &inode, meta, st.st_dev as i64, st.st_ino)
                    }
                    None => registry.bind_pending_parked(
                        nodeid,
                        &inode,
                        st.st_dev as i64,
                        st.st_ino,
                        (st.st_birthtime, st.st_birthtime_nsec),
                    ),
                }
            }
            Err(errno) => {
                tracing::warn!(
                    errno,
                    name = %name.to_string_lossy(),
                    "an acknowledged create failed to apply"
                );
                inode.bind_failed(errno);
                for _ in &writes {
                    inode.write_applied(Err(errno));
                }
            }
        }
        // Settled either way: the host directory answers for this name now
        // — with the file, or honestly without it.
        parent.remove_pending_child(name.to_bytes(), nodeid);
    };
    crate::apply::Job::of(crate::apply::Kind::Create, keys, 0, job)
}

/// Queues the held create for `nodeid`, if it is still held.
fn materialize_held(
    holding: &Holding,
    apply: &crate::apply::Apply,
    registry: &std::sync::Arc<Registry>,
    open_cache: &Arc<OpenCache>,
    park_creates: bool,
    nodeid: u64,
) -> bool {
    let held = holding
        .map
        .lock()
        .expect("held creates poisoned")
        .remove(&nodeid);
    let Some(held) = held else {
        return false;
    };
    holding.bytes.fetch_sub(held.bytes, Ordering::Relaxed);
    let parent = held.parent.clone();
    let inode = registry.get(nodeid);
    let Some(inode) = inode else {
        // Forgotten before it was made: nothing to make.
        parent.remove_pending_child(held.name.to_bytes(), nodeid);
        return true;
    };
    let seq = apply.push(create_job(
        registry.clone(),
        open_cache.clone(),
        park_creates,
        nodeid,
        inode.clone(),
        held,
    ));
    parent.settled_by(seq);
    inode.settled_by(seq);
    true
}

/// Queues every held create older than the grace period.
fn materialize_stale(
    holding: &Holding,
    apply: &crate::apply::Apply,
    registry: &std::sync::Arc<Registry>,
    open_cache: &Arc<OpenCache>,
    park_creates: bool,
) {
    loop {
        let oldest = {
            let map = holding.map.lock().expect("held creates poisoned");
            let mut order = holding.order.lock().expect("held order poisoned");
            // Entries already queued or withdrawn have left the map; they
            // are dropped from the front as they are met.
            while let Some(&front) = order.front()
                && !map.contains_key(&front)
            {
                order.pop_front();
            }
            match order.front() {
                Some(&front) if map[&front].since.elapsed() > HOLD_GRACE => {
                    order.pop_front();
                    Some(front)
                }
                _ => None,
            }
        };
        let Some(nodeid) = oldest else {
            break;
        };
        materialize_held(holding, apply, registry, open_cache, park_creates, nodeid);
    }
}

/// A shared directory.
pub struct Server {
    root: PathBuf,
    root_dev: i64,
    registry: Arc<Registry>,
    /// The identity every syscall actually runs as.
    host_uid: u32,
    host_gid: u32,
    /// Negotiated at INIT, and read by READ to bound a reply.
    max_write: AtomicU32,
    /// Whether the share's volume serves `/.vol` identity paths. Probed once.
    volfs: std::sync::OnceLock<bool>,
    /// How many more requests to log in order. See [`Server::trace`].
    trace_left: AtomicUsize,
    /// How long the guest may believe what we tell it.
    policy: Arc<Policy>,
    /// Invalidations waiting to be carried to the guest.
    notifications: Arc<crate::notify::Sink>,
    /// Descriptors held on the guest's behalf, since it no longer reports its
    /// opens. See [`crate::opencache`]. Shared with apply-queue jobs, which
    /// register a pending create's descriptor here once it exists.
    open_cache: Arc<OpenCache>,
    /// Opcode counters, off unless asked for.
    stats: Stats,
    /// The ordered queue that applies acknowledged mutations. See
    /// [`crate::apply`] for the promises it keeps.
    apply: std::sync::Arc<crate::apply::Apply>,
    /// Creates acknowledged and not yet queued — the job and the directory
    /// it changes, by the nodeid promised. See [`Server::materialize`].
    deferred: std::sync::Arc<Holding>,
    /// The thread that keeps the grace period: a held create nothing has
    /// forced is queued by it once it is older than [`HOLD_GRACE`].
    settler_stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    settler: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// `LIGHTER_FS_DEFER_CREATE=0` queues creates at once, as before they
    /// were held back; `LIGHTER_FS_PARK_CREATES=0` binds them with a
    /// metadata descriptor even at a full share. Kill switches for
    /// bisecting a regression by environment rather than by rebuild.
    defer_creates: bool,
    park_creates: bool,
    copy_max: u64,
    /// `LIGHTER_FS_DEBUG_LISTING=1`: log every listing an overlay changed.
    debug_listing: bool,
    /// Read once: `env::var` takes the process environment lock, and a
    /// negative lookup — most of what module resolution does — is hot.
    debug_enoent: bool,
    /// Whether extended attributes are served at all (`LIGHTER_FS_XATTR=0`
    /// disclaims them, see `dispatch`).
    xattrs: bool,
    /// The host watcher that keeps the policy honest.
    ///
    /// Held rather than used: dropping it stops the stream, after which every
    /// answer would be cached for its full timeout with nothing to shorten it.
    /// `None` means FSEvents refused to start, and the server falls back to
    /// exact coherence rather than to a timeout it cannot invalidate.
    _watcher: Option<Watcher>,
}

/// A sentinel errno inside `entry_from`: the reference closure declining to
/// open a descriptor because the share is at its budget and the file will be
/// registered parked instead. Never reaches the guest.
const PARK_ON_ENTRY: i32 = -1;

/// A handle for one operation on an inode, verified: the `At` form has been
/// checked to still name this inode, so a `chmod` through it cannot land on
/// a file the Mac has since given the name to.
enum Via {
    /// A descriptor the caller already held, typically from the open cache.
    Raw(std::os::fd::RawFd),
    Fd(Reference),
    At(Reference, CString),
}

fn via(inode: &Inode) -> Result<Via, i32> {
    match inode.locate()? {
        Located::Fd(fd) => Ok(Via::Fd(fd)),
        Located::At(parent, name) => match sys::stat_at(parent.raw_fd(), &name) {
            Ok(st) if st.st_ino == inode.ino() && st.st_dev as i64 == inode.dev() => {
                Ok(Via::At(parent, name))
            }
            _ => Ok(Via::Fd(inode.reference()?)),
        },
    }
}

/// Opens an inode for I/O with flags in the guest's numbering.
///
/// Through its parent and name where it has no descriptor of its own, and
/// through the descriptor's own directory for a listing; the reopen by
/// identity path — a walk of `/.vol/dev/ino` — is what every other case
/// used to cost, resident or not.
fn open_inode(inode: &Inode, linux_flags: u32) -> Result<std::os::fd::OwnedFd, i32> {
    match inode.locate()? {
        Located::Fd(fd) if inode.is_dir => sys::open_directory_self(fd.raw_fd()),
        Located::Fd(fd) => sys::reopen(fd.raw_fd(), linux_flags, 0),
        Located::At(parent, name) => {
            let opened = sys::openat_path(parent.raw_fd(), &name, linux_flags | sys::LINUX_O_NOFOLLOW, 0)
                .and_then(|fd| {
                    let st = sys::stat_fd(fd.as_raw_fd())?;
                    if st.st_ino == inode.ino() && st.st_dev as i64 == inode.dev() {
                        Ok(fd)
                    } else {
                        Err(linux::ESTALE)
                    }
                });
            match opened {
                Ok(fd) => Ok(fd),
                // The name has moved on; the file may not have.
                Err(_) => sys::reopen(inode.reference()?.raw_fd(), linux_flags, 0),
            }
        }
    }
}

impl Server {
    /// Opens `root` and prepares to serve it.
    pub fn new(root: &Path) -> std::io::Result<Server> {
        Server::with_timings(
            root,
            Timings::from_env_over(Timings::POLLED),
            Timings::from_env_over(Timings::PUSHED),
        )
    }

    /// Opens `root` with explicit caching policies for both cases: what may be
    /// promised when the guest cannot be corrected, and what may be promised
    /// when it can.
    pub fn with_timings(root: &Path, polled: Timings, pushed: Timings) -> std::io::Result<Server> {
        // A share holds a descriptor per remembered inode and per open file,
        // and macOS starts every process at 256 of them.
        let descriptors = sys::raise_file_limit();
        let fd = sys::open_root(root).map_err(std::io::Error::from_raw_os_error)?;
        let st = sys::stat_fd(fd.as_raw_fd()).map_err(std::io::Error::from_raw_os_error)?;
        let (dev, ino) = (st.st_dev as i64, st.st_ino);
        // SAFETY: both take no arguments and cannot fail.
        let (host_uid, host_gid) = unsafe { (libc::geteuid(), libc::getegid()) };

        // Caching is only defensible because the watcher can withdraw it. If
        // the stream will not start, the timeouts go to zero rather than
        // becoming promises that nothing is able to take back.
        let registry = Arc::new(Registry::new(fd, dev, ino));
        let sink = Arc::new(crate::notify::Sink::new());
        let policy = Arc::new(Policy::new(polled, pushed));
        let watcher = if polled.caching() || pushed.caching() {
            match Watcher::start(
                root,
                // Short enough that the guest's staleness is dominated by the
                // channel rather than by FSEvents' own coalescing window.
                std::time::Duration::from_millis(10),
                Box::new(Invalidator::new(
                    policy.clone(),
                    registry.clone(),
                    sink.clone(),
                )),
            ) {
                Ok(started) => Some(started),
                Err(why) => {
                    tracing::warn!(%why, "cannot watch the host; serving with no caching at all");
                    None
                }
            }
        } else {
            None
        };
        // Caching is only defensible because the watcher can withdraw it.
        let policy = if watcher.is_none() {
            Arc::new(Policy::fixed(Timings::NONE))
        } else {
            policy
        };

        tracing::info!(
            root = %root.display(),
            attr_ms = policy.timings().attr.as_millis(),
            entry_ms = policy.timings().entry_file.as_millis(),
            dir_entry_ms = policy.timings().entry_dir.as_millis(),
            watched = watcher.is_some(),
            descriptors,
            "share opened"
        );
        let open_cache = Arc::new(OpenCache::new());
        let apply = std::sync::Arc::new(crate::apply::Apply::start(root.to_path_buf()));
        let deferred = std::sync::Arc::new(Holding::default());
        let park_creates = std::env::var("LIGHTER_FS_PARK_CREATES").as_deref() != Ok("0");
        let settler_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let settler = {
            let holding = deferred.clone();
            let apply = apply.clone();
            let registry = registry.clone();
            let open_cache = open_cache.clone();
            let stop = settler_stop.clone();
            std::thread::Builder::new()
                .name("fs-settler".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(HOLD_GRACE / 2);
                        if holding
                            .map
                            .lock()
                            .expect("held creates poisoned")
                            .is_empty()
                        {
                            continue;
                        }
                        materialize_stale(&holding, &apply, &registry, &open_cache, park_creates);
                    }
                })
                .expect("failed to spawn the filesystem settler thread")
        };
        Ok(Server {
            root: root.to_path_buf(),
            root_dev: dev,
            registry,
            host_uid,
            host_gid,
            max_write: AtomicU32::new(MAX_WRITE),
            volfs: std::sync::OnceLock::new(),
            trace_left: AtomicUsize::new(
                std::env::var("LIGHTER_FS_TRACE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
            ),
            policy,
            notifications: sink,
            open_cache,
            stats: Stats::new(),
            apply,
            deferred,
            defer_creates: std::env::var("LIGHTER_FS_DEFER_CREATE").as_deref() != Ok("0"),
            park_creates,
            copy_max: copy_instead_of_clone_max(),
            debug_listing: std::env::var("LIGHTER_FS_DEBUG_LISTING").as_deref() == Ok("1"),
            debug_enoent: std::env::var("LIGHTER_FS_DEBUG_ENOENT").as_deref() == Ok("1"),
            xattrs: std::env::var("LIGHTER_FS_XATTR").as_deref() != Ok("0"),
            settler_stop,
            settler: std::sync::Mutex::new(Some(settler)),
            _watcher: watcher,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Invalidations waiting for the transport to carry them.
    pub fn notifications(&self) -> &Arc<crate::notify::Sink> {
        &self.notifications
    }

    /// Records whether the guest negotiated the notification queue, which is
    /// what decides how long anything may be cached.
    pub fn set_push_invalidation(&self, available: bool) {
        self.policy.set_pushing(available);
    }

    /// Handles one request, writing the reply into `sink`.
    ///
    /// Returns how many bytes were written, which is zero for the operations
    /// that have no reply at all.
    pub fn dispatch(&self, request: &[u8], sink: &mut dyn Sink) -> usize {
        let Some(header) = InHeader::parse(request) else {
            // Once per server, not per request: a guest whose queue layout
            // disagrees with ours delivers these by the million, and a warn
            // apiece turns a protocol bug into a full disk.
            static SHORT: std::sync::Once = std::sync::Once::new();
            SHORT.call_once(|| {
                tracing::warn!(
                    len = request.len(),
                    "fuse request shorter than its header (reported once; \
                     later occurrences are counted silently)"
                );
            });
            return 0;
        };
        // Ops revive parked descriptors without inserting anything, so the
        // budget has to be re-checked here, not only on insert. One atomic
        // load when the share is under budget.
        self.registry.reclaim_if_over_budget();
        // The header's own length field bounds the body; a guest that lied
        // about it must not let us read the tail of the previous request.
        let end = (header.len as usize)
            .min(request.len())
            .max(fuse::IN_HEADER_LEN);
        let body = &request[fuse::IN_HEADER_LEN..end];

        // FORGET is the only shape with no reply: the guest sends no writable
        // descriptor for it, so writing one would corrupt the ring.
        match header.opcode {
            op::FORGET => {
                if let Some(count) = get_u64(body, 0) {
                    self.forget(header.nodeid, count);
                }
                if self.stats.enabled() {
                    self.stats.record(op::FORGET, std::time::Duration::ZERO);
                }
                return 0;
            }
            op::BATCH_FORGET => {
                self.batch_forget(body);
                if self.stats.enabled() {
                    self.stats
                        .record(op::BATCH_FORGET, std::time::Duration::ZERO);
                }
                return 0;
            }
            _ => {}
        }

        self.trace(&header);
        let started = self.stats.enabled().then(|| {
            self.stats.enter();
            std::time::Instant::now()
        });
        let outcome = self.handle(&header, body, sink.capacity());
        if let Err(errno) = &outcome
            && *errno == linux::ESTALE
            && self.stats.enabled()
        {
            tracing::warn!(
                op = crate::stats::name(header.opcode),
                opcode = header.opcode,
                nodeid = header.nodeid,
                valid = format_args!("{:#x}", get_u32(body, 0).unwrap_or(0)),
                "ESTALE returned"
            );
        }
        if header.opcode == op::LIGHTER_CLONE
            && let Err(errno) = &outcome
        {
            // A refused clone turns a whole install from clones into
            // hardlinks — pnpm decides on the first one — so a refusal is
            // never silent.
            tracing::warn!(errno, nodeid = header.nodeid, "LIGHTER_CLONE refused");
        }
        if let Some(started) = started {
            self.stats.record(header.opcode, started.elapsed());
            self.stats.exit();
        }
        match outcome {
            Ok(payload) => {
                let len = fuse::OUT_HEADER_LEN + payload.len();
                if len > sink.capacity() {
                    // The guest sized the reply buffer, so this can only happen
                    // if we misjudged a bound. Reporting it as EIO is honest;
                    // writing past the chain would be memory corruption.
                    tracing::error!(
                        opcode = header.opcode,
                        len,
                        capacity = sink.capacity(),
                        "reply does not fit the buffer the guest supplied"
                    );
                    return write_error(sink, header.unique, linux::EIO);
                }
                let mut out = Vec::with_capacity(fuse::OUT_HEADER_LEN);
                out.extend_from_slice(&(len as u32).to_le_bytes());
                out.extend_from_slice(&0i32.to_le_bytes());
                out.extend_from_slice(&header.unique.to_le_bytes());
                if sink.write(&out).is_err() || sink.write(&payload).is_err() {
                    return 0;
                }
                len
            }
            Err(code) => write_error(sink, header.unique, code),
        }
    }

    /// Everything with a reply.
    fn handle(&self, header: &InHeader, body: &[u8], capacity: usize) -> Result<Vec<u8>, i32> {
        let nodeid = self.resolve(header.nodeid);
        match header.opcode {
            op::INIT => self.init(body),
            op::SYNCFS => {
                // `sync` on the share is the settling point for everything
                // acknowledged: after it returns, the host answers for all
                // of it.
                self.materialize_all();
                self.apply.drain();
                Ok(Vec::new())
            }
            // Likewise: a flush that can only ever succeed is a round trip on
            // every `close`, and the kernel will stop sending them if told
            // once that we do not implement it.
            op::FLUSH => Err(linux::ENOSYS),
            op::DESTROY => {
                self.materialize_all();
                self.apply.drain();
                self.log_stats();
                Ok(Vec::new())
            }
            op::LOOKUP => self.lookup(nodeid, body),
            op::GETATTR => self.getattr(nodeid, body),
            op::SETATTR => self.setattr(nodeid, body),
            op::READLINK => self.readlink(nodeid),
            op::SYMLINK => self.symlink(nodeid, body),
            op::MKNOD => self.mknod(nodeid, body),
            op::MKDIR => self.mkdir(nodeid, body),
            op::UNLINK => self.unlink(nodeid, body, false),
            op::RMDIR => self.unlink(nodeid, body, true),
            op::RENAME => self.rename(nodeid, body, false),
            op::RENAME2 => self.rename(nodeid, body, true),
            op::LINK => self.link(nodeid, body),
            // Answering ENOSYS here is not a refusal, it is a negotiation:
            // Linux records it once and thereafter opens, closes and flushes a
            // file without telling us. See `crate::opencache` for what that
            // buys and what it costs.
            op::OPEN | op::OPENDIR => Err(linux::ENOSYS),

            op::CREATE => self.create(nodeid, body),
            op::LIGHTER_CLONE => self.clone_over(body),
            op::READ => self.read(nodeid, body, capacity),
            op::WRITE => self.write(nodeid, body),
            op::STATFS => self.statfs(),
            op::RELEASE | op::RELEASEDIR => {
                if let Some(fh) = get_u64(body, 0) {
                    self.registry.release_handle(fh);
                }
                Ok(Vec::new())
            }
            op::FSYNC | op::FSYNCDIR => self.fsync(nodeid, body),
            op::READDIR => self.readdir(nodeid, body, capacity, false),
            op::READDIRPLUS => self.readdir(nodeid, body, capacity, true),
            op::ACCESS => self.access(nodeid, body),
            op::LSEEK => self.lseek(nodeid, body),
            op::FALLOCATE => self.fallocate(nodeid, body),
            // ENOSYS to any of these is a negotiation, not an error: the
            // kernel remembers it and answers every later call of that kind
            // itself, EOPNOTSUPP, with no request made. Measured on pnpm:
            // a quarter of a million getxattr requests per install, nearly
            // all of them the kernel asking for security.capability before
            // a chmod, chown or truncate so it can strip privileges.
            op::GETXATTR | op::SETXATTR | op::LISTXATTR | op::REMOVEXATTR if !self.xattrs => {
                Err(linux::ENOSYS)
            }
            op::GETXATTR => self.getxattr(nodeid, body),
            op::SETXATTR => self.setxattr(nodeid, body),
            op::LISTXATTR => self.listxattr(nodeid, body),
            op::REMOVEXATTR => self.removexattr(nodeid, body),
            // Everything below is answered by the guest's own fallback path
            // once it knows we do not implement it. Answering ENOSYS is how it
            // learns, and for most of these the kernel then stops asking.
            op::IOCTL => Err(25), // ENOTTY: no ioctl reaches a passthrough file
            _ => Err(linux::ENOSYS),
        }
    }

    /// Logs the first few requests, in order, when `LIGHTER_FS_TRACE` is set.
    ///
    /// The histogram says a package install sends two GETATTRs per created
    /// file; only the sequence says whether they come before the create, after
    /// the close, or from something else entirely — and that is the difference
    /// between a guest patch that removes them and one that does nothing.
    fn trace(&self, header: &fuse::InHeader) {
        let left = self.trace_left.load(Ordering::Relaxed);
        if left == 0 {
            return;
        }
        if self
            .trace_left
            .compare_exchange(left, left - 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        tracing::info!(
            op = crate::stats::name(header.opcode),
            nodeid = header.nodeid,
            "FSTRACE"
        );
    }

    /// Whether opcode counting is on.
    /// How many metadata descriptors the share holds, and the most it may.
    ///
    /// Diagnostics, and the thing a test has to be able to see: a reclaim that
    /// silently frees nothing looks exactly like one that was never needed.
    pub fn descriptor_usage(&self) -> (usize, usize) {
        self.registry.descriptor_usage()
    }

    pub fn stats_enabled(&self) -> bool {
        self.stats.enabled()
    }

    /// How many inodes the registry holds. Diagnostics and tests.
    pub fn live_inodes(&self) -> usize {
        self.registry.inode_count()
    }

    /// Prints the opcode histogram, if one was being kept.
    pub fn log_stats(&self) {
        if self.stats.enabled() {
            let (open, budget) = self.registry.descriptor_usage();
            tracing::info!(
                open,
                budget,
                inodes = self.registry.inode_count(),
                queued = self.apply.depth(),
                "FSSTATE"
            );
            for line in self.stats.report().lines() {
                tracing::info!("{line}");
            }
            for line in self.apply.report().lines() {
                tracing::info!("{line}");
            }
            for (i, why) in MATERIALIZE_WHY.iter().enumerate() {
                let n = MATERIALIZE_N[i].swap(0, Ordering::Relaxed);
                if n > 0 {
                    tracing::info!("MATERIALIZE {why:12} n={n}");
                }
            }
            self.stats.reset();
        }
    }

    // --- setup ------------------------------------------------------------

    fn init(&self, body: &[u8]) -> Result<Vec<u8>, i32> {
        let major = get_u32(body, 0).ok_or(linux::EINVAL)?;
        let minor = get_u32(body, 4).ok_or(linux::EINVAL)?;
        let max_readahead = get_u32(body, 8).unwrap_or(128 * 1024);
        let offered = get_u32(body, 12).unwrap_or(0);
        if major < 7 {
            tracing::error!(major, minor, "guest speaks a FUSE version we cannot");
            return Err(linux::EPROTO);
        }

        // Everything we are willing to do, intersected with what the guest
        // offered.
        //
        // Notably absent is WRITEBACK_CACHE. It would make writes faster, and
        // it is the wrong trade: it buffers the guest's writes in the guest and
        // hands the guest ownership of size and mtime, so a file a container
        // has written is not on the Mac until some later flush. That is the one
        // direction of coherence that must stay exact — you save in the editor,
        // you run the tests; you do not want the reverse to need a sync.
        //
        // SETXATTR_EXT is absent for a different reason: it changes a
        // structure's size on the wire, and the parser relies on its absence.
        // ATOMIC_O_TRUNC is deliberately absent, and not for the usual
        // reason. This server answers OPEN with ENOSYS — three round trips
        // deleted per file — and ATOMIC_O_TRUNC tells the kernel to entrust
        // truncation to the OPEN request. Together those made open(O_TRUNC)
        // silently not truncate: a dense overwrite masks it, and a sparse
        // copy or a shrinking rewrite quietly keeps the old file's bytes in
        // every range the new write skipped. A kernel build's Image, copied
        // over its predecessor by cp, booted as neither. Without the flag
        // the VFS truncates through SETATTR, which no_open never bypasses.
        let wanted = fuse::init::ASYNC_READ
            | fuse::init::BIG_WRITES
            | fuse::init::DO_READDIRPLUS
            | fuse::init::READDIRPLUS_AUTO
            | fuse::init::ASYNC_DIO
            | fuse::init::PARALLEL_DIROPS
            | fuse::init::AUTO_INVAL_DATA
            | fuse::init::MAX_PAGES
            // Without these the kernel does the "drop setuid and file
            // capabilities on write" dance itself, which costs it a GETXATTR
            // and sometimes a SETATTR per written file — one request in six of
            // a package install, producing nothing. With them, the server takes
            // on that duty, which on macOS the kernel already performs for us:
            // writing to a file clears its setuid and setgid bits, and Linux
            // file capabilities do not exist here at all. Both the old flag and
            // its replacement are offered, because which one a guest
            // understands depends on its vintage and only the newer one is
            // offered by current kernels.
            | fuse::init::HANDLE_KILLPRIV
            | fuse::init::HANDLE_KILLPRIV_V2
            | fuse::init::ABORT_ERROR;
        // Symlink targets are cached for the same duration as attributes, so
        // this is only offered when there is a watcher to withdraw it.
        let wanted = if self.policy.timings().caching() {
            wanted | fuse::init::CACHE_SYMLINKS
        } else {
            wanted
        };
        // Off, and switchable only so the decision can be re-checked — which
        // it now has been, properly, because the first attempt measured it
        // while it was breaking the ring and concluded "worth nothing" for the
        // wrong reason.
        //
        // It does exactly what it advertises. On the shape a package manager
        // writes in — one file opened once and filled by a decompressor eight
        // kilobytes at a time — it collapses eight WRITEs per file into one:
        // 12,000 requests for 1,500 files becomes 1,506.
        //
        // And it is slower anyway. The eight writes it removes cost 3.7us
        // each; the one it leaves costs 7.7, and it adds two SETATTRs per file
        // at 6.3 because the kernel has taken ownership of size and mtime and
        // has to hand them back. Measured end to end, 84us per file becomes
        // 98us, against 73 on the Mac itself.
        //
        // So there is no trade to weigh after all. It would also have moved
        // the moment a container's work becomes visible on the Mac from "as it
        // is written" to "when the file is closed", which is a promise rather
        // than a tuning knob — but that argument was never needed.
        let wanted = if std::env::var("LIGHTER_FS_WRITEBACK").as_deref() == Ok("1") {
            wanted | fuse::init::WRITEBACK_CACHE
        } else {
            wanted
        };
        // The second flags word exists only behind INIT_EXT, and carries our
        // create dialect (guest patch 0004). Offered only when there is a
        // watcher, because the dialect's promise — "the server will tell you
        // when the directory changes underneath you" — is the watcher.
        let offered2 = if offered & fuse::init::INIT_EXT != 0 {
            get_u32(body, 16).unwrap_or(0)
        } else {
            0
        };
        // Off-switch for measurement: the same kernel can then be run with
        // and without the dialect, which is how its worth is established.
        let dialect = self.policy.timings().caching()
            && std::env::var("LIGHTER_FS_CREATE_DIALECT").as_deref() != Ok("0");
        let wanted = if dialect {
            wanted | fuse::init::INIT_EXT
        } else {
            wanted
        };
        let flags = wanted & offered;
        // Clone gets its own off-switch for the same reason the create
        // dialect has one: the same kernel measured with and without is the
        // only honest account of what it is worth.
        let clone = if std::env::var("LIGHTER_FS_CLONE").as_deref() == Ok("0") {
            0
        } else {
            fuse::init2::LIGHTER_CLONE
        };
        // No security or ACL attribute can exist on a Mac, so the driver
        // is told so and stops asking (guest patch 0006). `LIGHTER_FS_XATTR=0`
        // disclaims every attribute instead, the blunt version for measuring.
        let no_security = if self.xattrs {
            fuse::init2::LIGHTER_NO_SECURITY_XATTR
        } else {
            0
        };
        let flags2 = if flags & fuse::init::INIT_EXT != 0 {
            (fuse::init2::LIGHTER_CREATE
                | clone
                | no_security
                | fuse::init2::LIGHTER_NOOP_SETATTR)
                & offered2
        } else {
            0
        };
        let max_write = if flags & fuse::init::MAX_PAGES != 0 {
            MAX_WRITE
        } else {
            // Without MAX_PAGES the kernel caps a request at 32 pages plus a
            // header, and asking for more than it can send produces short
            // writes that look like a full disk.
            128 * 1024
        };
        self.max_write.store(max_write, Ordering::Relaxed);
        tracing::info!(
            guest = format_args!("{major}.{minor}"),
            max_write,
            readdirplus = flags & fuse::init::DO_READDIRPLUS != 0,
            create_dialect = flags2 & fuse::init2::LIGHTER_CREATE != 0,
            "filesystem negotiated"
        );

        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&fuse::KERNEL_VERSION.to_le_bytes());
        out.extend_from_slice(&fuse::KERNEL_MINOR_VERSION.to_le_bytes());
        out.extend_from_slice(&max_readahead.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // max_background: kernel default
        out.extend_from_slice(&0u16.to_le_bytes()); // congestion_threshold
        out.extend_from_slice(&max_write.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // time_gran: nanoseconds
        out.extend_from_slice(&MAX_PAGES.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // map_alignment: no DAX
        out.extend_from_slice(&flags2.to_le_bytes());
        // The reserved tail. `fuse_init_out` is a fixed 64 bytes and a short
        // reply is read as garbage rather than as a short reply.
        out.resize(64, 0);
        Ok(out)
    }

    fn batch_forget(&self, body: &[u8]) {
        let Some(count) = get_u32(body, 0) else {
            return;
        };
        for index in 0..count as usize {
            let offset = 8 + index * 16;
            let (Some(nodeid), Some(nlookup)) = (get_u64(body, offset), get_u64(body, offset + 8))
            else {
                return;
            };
            self.forget(nodeid, nlookup);
        }
    }

    /// Drops a lookup reference, and with it anything held on the inode's
    /// behalf.
    ///
    /// The cached descriptor has to go with the inode: it is keyed by nodeid,
    /// and nodeids are never reused, so a survivor would be a descriptor that
    /// nothing can ever reach again.
    fn forget(&self, nodeid: u64, count: u64) {
        self.registry.forget(nodeid, count);
        if self.registry.get(nodeid).is_none() {
            self.open_cache.evict(nodeid);
        }
    }

    // --- name resolution ---------------------------------------------------

    /// Waits until `still` reports false of the inode, sleeping on the apply
    /// queue's completions between looks.
    ///
    /// The loop, rather than a single wait, closes a real gap: an overlay
    /// flag is raised before its job is pushed, so a barrier can see the
    /// flag while the queue is still empty. The flag only falls when the job
    /// applies, so looping on the flag is both correct and terminating; the
    /// wait inside costs the inode's own job rather than the whole queue's
    /// backlog.
    fn settle_while(&self, inode: &Inode, still: impl Fn(&Inode) -> bool) {
        if !still(inode) {
            return;
        }
        // Whatever is waited for must be on the queue to be waited for: a
        // held-back create, of this file or of the names in this directory.
        if inode.is_dir {
            self.materialize_children(inode);
        } else {
            self.materialize_why(inode.id(), 0);
        }
        let mut idle_since: Option<std::time::Instant> = None;
        while still(inode) {
            self.apply.wait_while(|| still(inode));
            if !still(inode) {
                break;
            }
            // A flag still up with nothing queued, for longer than any
            // acknowledgement takes to push its job, is a flag nothing will
            // ever lower: the host has applied everything, so the host is
            // the truth and waiting is a hang. Say so, loudly, and answer
            // from the host. Wall time, not spins: an acknowledgement raises
            // its flag before it pushes, and under a burst another worker
            // can arrive in that gap — a thousand yields was measured
            // giving up inside it, and answering ESTALE for a file whose
            // create was a microsecond from the queue.
            if self.apply.depth() == 0 {
                let since = *idle_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > std::time::Duration::from_millis(200) {
                    tracing::error!(
                        dev = inode.dev(),
                        ino = inode.ino(),
                        pending = inode.is_pending(),
                        dirty = inode.is_dirty(),
                        meta_shadowed = inode.meta_shadowed(),
                        listing_shadowed = inode.listing_shadowed(),
                        "a settle flag is up with an empty queue; answering from the host"
                    );
                    break;
                }
            } else {
                idle_since = None;
            }
            std::thread::yield_now();
        }
    }

    /// Queues the create a nodeid was promised with, if it has not been.
    ///
    /// A create is acknowledged as a promise and its job held back until
    /// something needs the file — a write, a rename, a link, a barrier — or
    /// the guest releases the handle. pnpm opens every file it imports and
    /// then clones over it: queued at once, the create had run before the
    /// clone arrived, and every import paid a create, an unlink and a clone.
    /// Held back, the clone withdraws it and pays for the clone alone.
    fn materialize_why(&self, nodeid: u64, why: usize) {
        if materialize_held(
            &self.deferred,
            &self.apply,
            &self.registry,
            &self.open_cache,
            self.park_creates,
            nodeid,
        ) {
            MATERIALIZE_N[why].fetch_add(1, Ordering::Relaxed);
        }
    }

    fn materialize_children(&self, dir: &Inode) {
        for (_, nodeid) in dir.pending_children_snapshot() {
            self.materialize_why(nodeid, 1);
        }
    }

    fn materialize_all(&self) {
        let all: Vec<u64> = self
            .deferred
            .map
            .lock()
            .expect("held creates poisoned")
            .keys()
            .copied()
            .collect();
        for nodeid in all {
            self.materialize_why(nodeid, 2);
        }
    }

    /// Withdraws a held-back create: the file it promised is never made.
    /// `true` if it was still held.
    fn withdraw(&self, nodeid: u64) -> bool {
        let held = self
            .deferred
            .map
            .lock()
            .expect("held creates poisoned")
            .remove(&nodeid);
        match held {
            Some(held) => {
                self.deferred.bytes.fetch_sub(held.bytes, Ordering::Relaxed);
                // Writes acknowledged into a file that will never exist:
                // their promise ends with it.
                if let Some(inode) = self.registry.get(nodeid) {
                    for _ in &held.writes {
                        inode.write_applied(Ok(()));
                    }
                }
                true
            }
            None => false,
        }
    }

    fn inode(&self, nodeid: u64) -> Result<std::sync::Arc<Inode>, i32> {
        self.registry.get(nodeid).ok_or(linux::ENOENT)
    }

    /// The nodeid a request should be served under.
    ///
    /// A clone replaced a file under the guest's open descriptor; the
    /// descriptor still names the old nodeid, and everything keyed by nodeid
    /// — the inode, its cached descriptors, its overlays — belongs to the
    /// replacement. Resolved once, at dispatch, so no handler can see the
    /// old one. Bounded, since a replacement can itself be replaced.
    fn resolve(&self, nodeid: u64) -> u64 {
        let mut id = nodeid;
        for _ in 0..8 {
            match self.registry.get(id).and_then(|inode| inode.forwarded()) {
                Some(next) => id = next,
                None => break,
            }
        }
        id
    }

    fn directory(&self, nodeid: u64) -> Result<std::sync::Arc<Inode>, i32> {
        let inode = self.inode(nodeid)?;
        if !inode.is_dir {
            return Err(linux::ENOTDIR);
        }
        Ok(inode)
    }

    /// The live host path of an inode, as a C string.
    fn path(&self, inode: &Inode) -> Result<CString, i32> {
        // Path-based operations (access, xattr, link, setattr) need a file
        // that exists; settle it once, here, for all of them.
        self.settle_while(inode, |inode| inode.is_pending());
        // A parked file's path is its parent's path and its name — no
        // descriptor of its own is opened for it. The parent's identity path
        // takes child components, so this stays immune to the stale-name
        // problem below; what the name resolves to is checked against the
        // inode's own identity, since the Mac may have moved it since.
        if let Ok(Located::At(parent, name)) = inode.locate()
            && let Ok(st) = sys::stat_at(parent.raw_fd(), &name)
            && st.st_ino == inode.ino()
            && st.st_dev as i64 == inode.dev()
        {
            let base = sys::identity_path(parent.raw_fd()).or_else(|_| sys::path_of(parent.raw_fd()))?;
            return sys::c_path(&base.join(std::ffi::OsStr::from_bytes(name.to_bytes())));
        }
        let fd = match inode.reference() {
            Ok(fd) => fd,
            Err(errno) => {
                if self.stats.enabled() {
                    tracing::warn!(
                        errno,
                        pending = inode.is_pending(),
                        cancelled = inode.is_cancelled(),
                        forwarded = inode.forwarded().is_some(),
                        dirty = inode.is_dirty(),
                        dev = inode.dev(),
                        ino = inode.ino(),
                        "path(): no descriptor"
                    );
                }
                return Err(errno);
            }
        };
        // By identity when the volume allows it. F_GETPATH answers from the
        // vnode name cache, which for a file created under a temporary name
        // and renamed into place can still be the temporary name — pnpm does
        // exactly that to every store file, and the resulting ENOENT killed
        // one install in three. Probed once against the share root; a share
        // on something exotic keeps the old behavior.
        if *self.volfs.get_or_init(|| {
            self.registry
                .get(1)
                .and_then(|root| root.reference().ok())
                .and_then(|r| sys::identity_path(r.raw_fd()).ok())
                .is_some_and(|p| std::fs::metadata(&p).is_ok())
        }) {
            return sys::c_path(&sys::identity_path(fd.raw_fd())?);
        }
        sys::c_path(&sys::path_of(fd.raw_fd())?)
    }

    /// Looks a name up under `parent` and registers it, producing the reply
    /// body that LOOKUP, CREATE, MKDIR and friends all end with.
    fn entry(&self, parent: &Arc<Inode>, name: &CString) -> Result<EntryOut, i32> {
        let mark = self.apply.applied();
        let st = sys::stat_at(parent.reference()?.raw_fd(), name)?;
        self.entry_from(parent, name, st, mark)
    }

    /// [`Server::entry`], for a caller that already has the `stat`.
    /// `mark` is the queue's applied count from before `st` was taken; see
    /// the overlay step in [`Server::entry_with_reference`].
    fn entry_from(
        &self,
        parent: &Arc<Inode>,
        name: &CString,
        st: libc::stat,
        mark: u64,
    ) -> Result<EntryOut, i32> {
        let entry = self.entry_with_reference(parent, st, mark, || {
            let is_symlink = st.st_mode as u32 & libc::S_IFMT as u32 == libc::S_IFLNK as u32;
            // A regular file the guest names into a full share is registered
            // parked, with no descriptor at all: it is operated on through
            // this parent's descriptor from here on (`Inode::locate`), and
            // opening one only to have the sweep close it again was an
            // `open` and a `close` per file of a package tree.
            if !is_symlink
                && st.st_mode as u32 & libc::S_IFMT as u32 == libc::S_IFREG as u32
                && self.registry.at_budget()
            {
                return Err(PARK_ON_ENTRY);
            }
            sys::open_reference(parent.reference()?.raw_fd(), name, is_symlink)
        });
        let entry = match entry {
            Err(PARK_ON_ENTRY) => {
                let (dev, ino) = (st.st_dev as i64, st.st_ino);
                let nodeid = match self.registry.relookup(dev, ino) {
                    Some(existing) => existing,
                    None => {
                        let id = self.registry.insert_parked(
                            dev,
                            ino,
                            (st.st_birthtime, st.st_birthtime_nsec),
                        );
                        self.registry.count_lookup(id);
                        id
                    }
                };
                if let Some(inode) = self.registry.get(nodeid) {
                    inode.set_place(parent, name);
                }
                return self.entry_with_reference(parent, st, mark, || Err(linux::EIO));
            }
            other => other?,
        };
        if let Some(inode) = self.registry.get(entry.nodeid) {
            inode.set_place(parent, name);
        }
        Ok(entry)
    }

    /// The common tail of every reply that names an inode.
    ///
    /// `reference` is only called when the inode is new to us, which is the
    /// whole reason it is a closure: on the hot path — a name the guest has
    /// looked up before — no descriptor is opened at all.
    fn entry_with_reference(
        &self,
        parent: &Inode,
        st: libc::stat,
        mark: u64,
        reference: impl FnOnce() -> Result<std::os::fd::OwnedFd, i32>,
    ) -> Result<EntryOut, i32> {
        let mode = st.st_mode as u32;
        let is_dir = mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32;
        let is_symlink = mode & libc::S_IFMT as u32 == libc::S_IFLNK as u32;
        let (dev, ino) = (st.st_dev as i64, st.st_ino);

        // An inode we already hold needs no second descriptor. Worth the extra
        // branch: this is the single hottest path in the whole server, and the
        // descriptor it skips opening is most of the cost of a repeated lookup.
        let nodeid = match self.registry.relookup(dev, ino) {
            Some(existing) => existing,
            None => self
                .registry
                .insert(reference()?, dev, ino, is_dir, is_symlink),
        };

        // Validity is asked of the *parent*, because that is what the host
        // would have had to change for this name to mean something else.
        let answer = if is_dir {
            Answer::Directory
        } else {
            Answer::File
        };
        let entry_valid = self.policy.validity(parent.dev(), parent.ino(), answer);
        let attr_valid = self.policy.attr_validity(dev, ino);
        let mut attr = self.attr(&st);
        // While writes or setattrs for this file sit on the apply queue, the
        // host stat lags what the guest was promised; the overlay is the truth.
        //
        // The inode was not known until the stat named it, so the promise
        // could not be read first (`attr_of`). If it is empty now but a job
        // for this file was outstanding at `mark`, that job may have applied
        // while the stat was in flight and the stat be of the moment before:
        // the file is stat'd again, with nothing left to race.
        if !is_dir && let Some(inode) = self.registry.get(nodeid) {
            let overlay = inode.overlay();
            if !overlay.is_empty() {
                self.overlay_attr(&overlay, &mut attr);
            } else if inode.settle_seq() > mark {
                attr = self.attr(&self.stat_inode(nodeid, &inode)?);
            }
        }
        Ok(EntryOut {
            nodeid,
            generation: 0,
            entry_valid: entry_valid.as_secs(),
            attr_valid: attr_valid.as_secs(),
            entry_valid_nsec: entry_valid.subsec_nanos(),
            attr_valid_nsec: attr_valid.subsec_nanos(),
            attr,
        })
    }

    /// The reply that says "this name does not exist, and you may remember
    /// that for a while".
    ///
    /// Module resolution is mostly failed lookups — `require('x')` stats a
    /// dozen paths that are not there for every one that is — so caching the
    /// absence is worth as much as caching the presence. A `nodeid` of zero is
    /// how the protocol spells it; with no validity it is just ENOENT, which
    /// is what an unwatched share falls back to.
    fn missing(&self, parent: &Inode) -> Result<Vec<u8>, i32> {
        let valid = self
            .policy
            .validity(parent.dev(), parent.ino(), Answer::Missing);
        if valid.is_zero() {
            return Err(linux::ENOENT);
        }
        let mut out = Vec::with_capacity(fuse::ENTRY_OUT_LEN);
        EntryOut {
            nodeid: 0,
            generation: 0,
            entry_valid: valid.as_secs(),
            attr_valid: 0,
            entry_valid_nsec: valid.subsec_nanos(),
            attr_valid_nsec: 0,
            attr: Attr::default(),
        }
        .encode(&mut out);
        Ok(out)
    }

    fn checked_name(&self, raw: &[u8]) -> Result<CString, i32> {
        if raw.len() > NAME_MAX {
            return Err(linux::ENAMETOOLONG);
        }
        sys::safe_name(raw)
    }

    // --- identity ----------------------------------------------------------

    /// Host uid to the one the guest should see.
    fn to_guest_uid(&self, uid: u32) -> u32 {
        if uid == self.host_uid { 0 } else { uid }
    }

    fn to_guest_gid(&self, gid: u32) -> u32 {
        if gid == self.host_gid { 0 } else { gid }
    }

    /// Guest uid to the one a syscall should use.
    fn to_host_uid(&self, uid: u32) -> u32 {
        if uid == 0 { self.host_uid } else { uid }
    }

    fn to_host_gid(&self, gid: u32) -> u32 {
        if gid == 0 { self.host_gid } else { gid }
    }

    fn attr(&self, st: &libc::stat) -> Attr {
        // Inode numbers must be unique across the whole share, and a share can
        // span mount points — a `~/src` with a case-sensitive disk image
        // mounted inside it is ordinary on a Mac. Numbers from the root device
        // pass through so that hard links and `find -samefile` behave; anything
        // else is mixed with its device so two filesystems cannot collide.
        let ino = if st.st_dev as i64 == self.root_dev {
            st.st_ino
        } else {
            mix(st.st_dev as u64, st.st_ino)
        };
        Attr {
            ino,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
            atime: st.st_atime,
            mtime: st.st_mtime,
            ctime: st.st_ctime,
            atimensec: st.st_atime_nsec as u32,
            mtimensec: st.st_mtime_nsec as u32,
            ctimensec: st.st_ctime_nsec as u32,
            mode: st.st_mode as u32,
            nlink: st.st_nlink as u32,
            uid: self.to_guest_uid(st.st_uid),
            gid: self.to_guest_gid(st.st_gid),
            rdev: st.st_rdev as u32,
            blksize: st.st_blksize as u32,
        }
    }

    fn attr_reply(&self, st: &libc::stat) -> Vec<u8> {
        let valid = self.policy.attr_validity(st.st_dev as i64, st.st_ino);
        let mut out = Vec::with_capacity(fuse::ATTR_LEN + 16);
        out.extend_from_slice(&valid.as_secs().to_le_bytes());
        out.extend_from_slice(&valid.subsec_nanos().to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // padding
        self.attr(st).encode(&mut out);
        out
    }

    fn entry_reply(&self, entry: &EntryOut) -> Vec<u8> {
        let mut out = Vec::with_capacity(fuse::ENTRY_OUT_LEN);
        entry.encode(&mut out);
        out
    }

    // --- operations --------------------------------------------------------

    fn lookup(&self, parent: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let parent = self.directory(parent)?;
        let (name, _) = get_name(body).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        // A name promised to a pending create resolves to its pending inode:
        // the host directory does not know it yet, and ENOENT — worse, a
        // cached ENOENT — would be a lie.
        if let Some(nodeid) = parent.pending_child(name.to_bytes())
            && let Some(inode) = self.registry.get(nodeid)
        {
            let entry = if inode.is_pending() {
                self.registry.count_lookup(nodeid);
                self.pending_entry(&parent, &inode, nodeid)
            } else {
                // Promised here by a queued rename: a real file, answering
                // under a name the host does not have yet.
                self.promised_entry(&parent, &inode, nodeid)?
            };
            return Ok(self.entry_reply(&entry));
        }
        if parent.name_pending_gone(name.to_bytes()) {
            // The host still lists it; the guest was promised it is gone.
            return self.missing(&parent);
        }
        if parent.is_pending() {
            // A directory that is still a promise holds only promises.
            return self.missing(&parent);
        }
        let looked = self.entry(&parent, &name);
        if self.stats.enabled() {
            self.stats.count(match &looked {
                Ok(_) => "lookup=found",
                Err(errno) if *errno == linux::ENOENT => "lookup=enoent",
                Err(_) => "lookup=error",
            });
        }
        match looked {
            Ok(entry) => Ok(self.entry_reply(&entry)),
            Err(linux::ENOENT) => {
                if self.debug_enoent {
                    let held = sys::path_of(parent.reference()?.raw_fd())
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|e| format!("<no path: {e}>"));
                    tracing::warn!(
                        parent_dev = parent.dev(),
                        parent_ino = parent.ino(),
                        held,
                        name = %name.to_string_lossy(),
                        "ENOENT-DEBUG lookup miss"
                    );
                }
                self.missing(&parent)
            }
            Err(other) => Err(other),
        }
    }

    fn getattr(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        if let Ok(inode) = self.inode(nodeid)
            && inode.is_pending()
        {
            // No host file to stat yet; the promise is the truth.
            let entry = self.pending_entry(&inode, &inode, nodeid);
            let valid = self.policy.attr_validity(inode.dev(), inode.ino());
            let mut out = Vec::with_capacity(fuse::ATTR_LEN + 16);
            out.extend_from_slice(&valid.as_secs().to_le_bytes());
            out.extend_from_slice(&valid.subsec_nanos().to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // padding
            entry.attr.encode(&mut out);
            return Ok(out);
        }
        let inode = self.inode(nodeid).ok();
        if let Some(inode) = &inode {
            // A queued unlink will change this inode's link count; a stat
            // now would answer with the past.
            self.settle_while(inode, |inode| inode.meta_shadowed());
        }
        // The promise before the stat: `attr_of` says why.
        let overlay = inode
            .as_ref()
            .map(|inode| inode.overlay())
            .unwrap_or_default();
        let flags = get_u32(body, 0).unwrap_or(0);
        let st = if flags & fuse::GETATTR_FH != 0
            && let Some(fh) = get_u64(body, 8)
            && let Some(handle) = self.registry.handle(fh)
            && let Some(fd) = handle.raw_fd()
        {
            sys::stat_fd(fd)?
        } else {
            let inode = self.inode(nodeid)?;
            self.stat_inode(nodeid, &inode)?
        };
        let mut out = self.attr_reply(&st);
        if !overlay.is_empty() {
            let mut attr = self.attr(&st);
            self.overlay_attr(&overlay, &mut attr);
            out.truncate(16);
            attr.encode(&mut out);
        }
        Ok(out)
    }

    /// Requests for nothing, answered before they cost anything.
    ///
    /// Two shapes a package install sends by the hundred thousand. Every
    /// `open(O_TRUNC)` on a file the guest has just created arrives as a
    /// truncate to zero (ATOMIC_O_TRUNC is deliberately off), against a file
    /// that is still a pending, empty promise. And a chown to root, which
    /// after the identity map is the ownership the file already has. Both
    /// used to settle the file's queued create and revive it by path first,
    /// then discover there was nothing to do.
    fn setattr_noop(
        &self,
        inode: &std::sync::Arc<Inode>,
        nodeid: u64,
        valid: u32,
        body: &[u8],
    ) -> Result<Option<Vec<u8>>, i32> {
        const OWNERSHIP: u32 = fuse::fattr::UID | fuse::fattr::GID;
        const HARMLESS: u32 = fuse::fattr::FH
            | fuse::fattr::LOCKOWNER
            | fuse::fattr::KILL_SUIDGID
            | fuse::fattr::CTIME;
        let effective = valid & !HARMLESS;
        let noop = if effective == fuse::fattr::SIZE {
            let size = get_u64(body, 16).ok_or(linux::EINVAL)?;
            // A pending file that has had nothing written is empty already.
            size == 0 && inode.is_pending() && !inode.is_dirty() && inode.overlay_size(0) == 0
        } else if effective & !OWNERSHIP == 0 && effective != 0 {
            let uid = if valid & fuse::fattr::UID != 0 {
                self.to_host_uid(get_u32(body, 76).ok_or(linux::EINVAL)?)
            } else {
                self.host_uid
            };
            let gid = if valid & fuse::fattr::GID != 0 {
                self.to_host_gid(get_u32(body, 80).ok_or(linux::EINVAL)?)
            } else {
                self.host_gid
            };
            uid == self.host_uid && gid == self.host_gid
        } else {
            false
        };
        if !noop {
            return Ok(None);
        }
        let attr = if inode.is_pending() {
            self.pending_entry(inode, inode, nodeid).attr
        } else {
            self.attr_of(nodeid, inode)?
        };
        let valid_for = self.policy.attr_validity(inode.dev(), inode.ino());
        let mut out = Vec::with_capacity(fuse::ATTR_LEN + 16);
        out.extend_from_slice(&valid_for.as_secs().to_le_bytes());
        out.extend_from_slice(&valid_for.subsec_nanos().to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        attr.encode(&mut out);
        Ok(Some(out))
    }

    /// The asynchronous half of SETATTR: mode and times, which is what a
    /// package manager sets on every file it has just written — a quarter of
    /// a million times per pnpm install, each one having to wait for that
    /// file's queued create and writes before a synchronous chmod or utimes
    /// could even start. Queued behind them instead, in order, with the
    /// promised values laid over every read until the job lands. Ownership
    /// and size keep the synchronous path: one needs a privilege decision,
    /// the other is a truncate.
    fn setattr_pending(
        &self,
        inode: &std::sync::Arc<Inode>,
        nodeid: u64,
        valid: u32,
        body: &[u8],
    ) -> Result<Option<Vec<u8>>, i32> {
        const ASYNC_OK: u32 = fuse::fattr::MODE
            | fuse::fattr::ATIME
            | fuse::fattr::MTIME
            | fuse::fattr::ATIME_NOW
            | fuse::fattr::MTIME_NOW
            | fuse::fattr::CTIME
            | fuse::fattr::FH
            | fuse::fattr::LOCKOWNER
            | fuse::fattr::KILL_SUIDGID;
        if !self.apply.accepting()
            || valid & !ASYNC_OK != 0
            || valid
                & (fuse::fattr::MODE
                    | fuse::fattr::ATIME
                    | fuse::fattr::MTIME
                    | fuse::fattr::ATIME_NOW
                    | fuse::fattr::MTIME_NOW)
                == 0
            || inode.is_dir
            || inode.is_symlink
        {
            return Ok(None);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let now = (now.as_secs() as i64, now.subsec_nanos() as i64);
        let time = |set: u32, set_now: u32, sec: usize, nsec: usize| {
            if valid & set_now != 0 {
                Some(now)
            } else if valid & set != 0 {
                match (get_u64(body, sec), get_u32(body, nsec)) {
                    (Some(s), Some(ns)) => Some((s as i64, ns as i64)),
                    _ => None,
                }
            } else {
                None
            }
        };
        let mut change = crate::inode::AttrOverride {
            mode: (valid & fuse::fattr::MODE != 0).then(|| get_u32(body, 68).unwrap_or(0) & 0o7777),
            atime: time(fuse::fattr::ATIME, fuse::fattr::ATIME_NOW, 32, 56),
            mtime: time(fuse::fattr::MTIME, fuse::fattr::MTIME_NOW, 40, 60),
        };
        // A chmod to the mode the file already has is a request for nothing,
        // and a package manager makes a great many of them; the syscall it
        // would queue is fifteen microseconds of APFS the drainer can spend
        // on something that changes a file.
        if change.mode.is_some() {
            let current_mode = if let Some(meta) = inode.pending_meta() {
                Some(meta.mode & 0o7777)
            } else {
                inode
                    .attr_override()
                    .and_then(|over| over.mode)
                    .or_else(|| {
                        self.stat_inode(nodeid, inode)
                            .ok()
                            .map(|st| st.st_mode as u32 & 0o7777)
                    })
            };
            if change.mode == current_mode {
                change.mode = None;
            }
        }
        if change.mode.is_none() && change.atime.is_none() && change.mtime.is_none() {
            // Nothing to do; answer with what the file already is — which,
            // for a file still pending, is what it was promised. (A stat
            // here was the ESTALE that turned every pnpm import into a
            // hardlink: libuv's copyfile chmods the destination it has just
            // created, to the mode it already has.)
            let attr = if inode.is_pending() {
                let mut attr = self.pending_entry(inode, inode, nodeid).attr;
                self.overlay_attr(&inode.overlay(), &mut attr);
                attr
            } else {
                self.attr_of(nodeid, inode)?
            };
            let valid_for = self.policy.attr_validity(inode.dev(), inode.ino());
            let mut out = Vec::with_capacity(fuse::ATTR_LEN + 16);
            out.extend_from_slice(&valid_for.as_secs().to_le_bytes());
            out.extend_from_slice(&valid_for.subsec_nanos().to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            attr.encode(&mut out);
            return Ok(Some(out));
        }
        // A create still held back takes the change into its promise and
        // its job applies it: no second job and — the point — nothing that
        // forces the create early. libuv chmods every file it copies into
        // before it clones over it, and that chmod was making the file it
        // was about to replace. Under the same lock the job is taken with,
        // so a change cannot fall between the job reading its promise and
        // keeping it.
        let kept_by_create = {
            let held = self.deferred.map.lock().expect("held creates poisoned");
            if held.contains_key(&nodeid) {
                inode.attr_acked(change);
                inode.attr_merged();
                true
            } else {
                false
            }
        };
        if !kept_by_create {
            inode.attr_acked(change);
        }
        // Four setattrs per file is what pnpm sends; one job per file is
        // what APFS should hear. A job still waiting takes the merge, unless
        // a write has been queued since it was opened.
        if kept_by_create {
            // Already kept.
        } else if !inode.merge_attr(change) {
            self.materialize_why(nodeid, 4);
            let batch = std::sync::Arc::new(std::sync::Mutex::new(change));
            // Opened before the push: `Inode::open_attr_batch` says why.
            inode.open_attr_batch(batch.clone());
            let job = {
                let inode = inode.clone();
                let open_cache = self.open_cache.clone();
                let batch = batch.clone();
                move || {
                    let change = inode
                        .take_attr_batch(&batch)
                        .unwrap_or_else(|| *batch.lock().expect("attr batch poisoned"));
                    let result = (|| {
                        let cached = open_cache.file(nodeid, false);
                        let via = match &cached {
                            Some(file) => Via::Raw(file.fd.as_raw_fd()),
                            None => via(&inode)?,
                        };
                        if let Some(mode) = change.mode {
                            match &via {
                                Via::Raw(raw) => sys::chmod_fd(*raw, mode)?,
                                Via::Fd(fd) => sys::chmod_fd(fd.raw_fd(), mode)?,
                                Via::At(parent, name) => sys::chmod_at(parent.raw_fd(), name, mode)?,
                            }
                        }
                        if change.atime.is_some() || change.mtime.is_some() {
                            let at = |t: Option<(i64, i64)>| match t {
                                Some((s, ns)) => TimeSpec::At(s, ns as u32),
                                None => TimeSpec::Omit,
                            };
                            let (atime, mtime) = (at(change.atime), at(change.mtime));
                            match &via {
                                Via::Raw(raw) => sys::utimes_fd(*raw, atime, mtime)?,
                                Via::Fd(fd) => sys::utimes_fd(fd.raw_fd(), atime, mtime)?,
                                Via::At(parent, name) => {
                                    sys::utimes_at(parent.raw_fd(), name, atime, mtime)?
                                }
                            }
                        }
                        Ok(())
                    })();
                    if let Err(errno) = result
                        && !inode.is_cancelled()
                    {
                        tracing::warn!(errno, "an acknowledged setattr failed to apply");
                    }
                    inode.attr_applied(result);
                }
            };
            let seq = self.apply.push(crate::apply::Job::of(
                crate::apply::Kind::Setattr,
                crate::apply::Keys::of(&[nodeid]),
                0,
                job,
            ));
            inode.stamp_attr_batch(seq, &batch);
            inode.settled_by(seq);
        } else {
            // Merged: this request's promise lands with the job already
            // queued, and counts as applied when that job runs.
            inode.attr_merged();
        }
        // The reply: what the guest will see from here on.
        let entry = if inode.is_pending() {
            self.pending_entry(inode, inode, nodeid)
        } else {
            let attr = self.attr_of(nodeid, inode)?;
            EntryOut {
                nodeid,
                generation: 0,
                entry_valid: 0,
                attr_valid: 0,
                entry_valid_nsec: 0,
                attr_valid_nsec: 0,
                attr,
            }
        };
        let valid_for = self.policy.attr_validity(inode.dev(), inode.ino());
        let mut out = Vec::with_capacity(fuse::ATTR_LEN + 16);
        out.extend_from_slice(&valid_for.as_secs().to_le_bytes());
        out.extend_from_slice(&valid_for.subsec_nanos().to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        entry.attr.encode(&mut out);
        Ok(Some(out))
    }

    fn setattr(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let valid = get_u32(body, 0).ok_or(linux::EINVAL)?;
        let inode = self.inode(nodeid)?;
        if self.stats.enabled() {
            self.stats.count(&format!("setattr-mask={valid:#x}"));
            // Which shapes of setattr a workload sends, once each: the async
            // path only takes some of them, and which ones arrive decides
            // whether it is taking any.
            static SEEN: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
            let mut seen = SEEN.lock().expect("seen masks poisoned");
            if !seen.contains(&valid) {
                seen.push(valid);
                tracing::info!(
                    valid = format_args!("{valid:#x}"),
                    dir = inode.is_dir,
                    symlink = inode.is_symlink,
                    "SETATTR-MASK"
                );
            }
        }
        if let Some(reply) = self.setattr_noop(&inode, nodeid, valid, body)? {
            return Ok(reply);
        }
        if let Some(reply) = self.setattr_pending(&inode, nodeid, valid, body)? {
            return Ok(reply);
        }
        // A truncate must land after every write already acknowledged, and
        // the times a `touch` sets must not be overwritten by a stale apply.
        self.settle_while(&inode, |inode| {
            inode.is_dirty() || inode.is_pending() || inode.has_pending_attrs()
        });
        let path = self.path(&inode)?;

        if valid & fuse::fattr::MODE != 0 {
            let mode = get_u32(body, 68).ok_or(linux::EINVAL)?;
            sys::chmod_at(libc::AT_FDCWD, &path, mode & 0o7777)?;
        }

        if valid & (fuse::fattr::UID | fuse::fattr::GID) != 0 {
            let uid = if valid & fuse::fattr::UID != 0 {
                self.to_host_uid(get_u32(body, 76).ok_or(linux::EINVAL)?)
            } else {
                u32::MAX
            };
            let gid = if valid & fuse::fattr::GID != 0 {
                self.to_host_gid(get_u32(body, 80).ok_or(linux::EINVAL)?)
            } else {
                u32::MAX
            };
            // A container chowning a file to root is asking for what it already
            // has once the map is applied. Doing nothing is the honest answer;
            // issuing the syscall would fail with EPERM for an unprivileged
            // host process and turn every `npm install --unsafe-perm` into an
            // error about a change that was not needed.
            let uid_change = uid != u32::MAX && uid != self.host_uid;
            let gid_change = gid != u32::MAX && gid != self.host_gid;
            if uid_change || gid_change {
                sys::chown_at(libc::AT_FDCWD, &path, uid, gid)?;
            }
        }

        if valid & (fuse::fattr::ATIME | fuse::fattr::MTIME) != 0 {
            // Offsets into `fuse_setattr_in`: atime 32, mtime 40, atimensec 56,
            // mtimensec 60. They are listed rather than computed because a
            // one-field slip here sets the wrong timestamp and nothing fails.
            let pick = |set: u32, now: u32, sec: usize, nsec: usize| {
                if valid & now != 0 {
                    TimeSpec::Now
                } else if valid & set != 0 {
                    match (get_u64(body, sec), get_u32(body, nsec)) {
                        (Some(s), Some(ns)) => TimeSpec::At(s as i64, ns),
                        _ => TimeSpec::Omit,
                    }
                } else {
                    TimeSpec::Omit
                }
            };
            sys::utimes_at(
                libc::AT_FDCWD,
                &path,
                pick(fuse::fattr::ATIME, fuse::fattr::ATIME_NOW, 32, 56),
                pick(fuse::fattr::MTIME, fuse::fattr::MTIME_NOW, 40, 60),
            )?;
        }

        if valid & fuse::fattr::SIZE != 0 {
            if inode.is_dir {
                return Err(linux::EISDIR);
            }
            let size = get_u64(body, 16).ok_or(linux::EINVAL)?;
            // Truncating needs a writable descriptor, and the handle the guest
            // supplied may be read-only or absent entirely.
            // The guest may have named a handle, but with opens no longer
            // reported most truncations arrive without one — so the descriptor
            // is resolved the same way a write's is.
            let fh = if valid & fuse::fattr::FH != 0 {
                get_u64(body, 8).unwrap_or(0)
            } else {
                0
            };
            let file = self.file_for(nodeid, fh, true)?;
            sys::truncate_fd(file.fd.as_raw_fd(), size)?;
        }

        let st = self.stat_of(&inode)?;
        Ok(self.attr_reply(&st))
    }

    fn readlink(&self, nodeid: u64) -> Result<Vec<u8>, i32> {
        let inode = self.inode(nodeid)?;
        if !inode.is_symlink {
            return Err(linux::EINVAL);
        }
        self.settle_while(&inode, |inode| inode.is_pending());
        let path = self.path(&inode)?;
        // The reply carries no terminator: the kernel takes the length from the
        // header, and a trailing NUL becomes part of the target.
        sys::readlink_at(libc::AT_FDCWD, &path)
    }

    fn symlink(&self, parent: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let parent = self.directory(parent)?;
        let (name, rest) = get_name(body).ok_or(linux::EINVAL)?;
        let (target, _) = get_name(rest).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        let target = CString::new(target).map_err(|_| linux::EINVAL)?;
        if let Some(reply) = self.name_pending(
            &parent,
            &name,
            crate::inode::PendingKind::Symlink,
            libc::S_IFLNK as u32 | 0o777,
            Some(target.clone()),
        )? {
            return Ok(reply);
        }
        self.settle_while(&parent, |parent| parent.is_pending());
        sys::symlink_at(&target, parent.reference()?.raw_fd(), &name)?;
        let entry = self.entry(&parent, &name)?;
        Ok(self.entry_reply(&entry))
    }

    fn mknod(&self, parent: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let parent = self.directory(parent)?;
        self.settle_while(&parent, |parent| parent.is_pending());
        let mode = get_u32(body, 0).ok_or(linux::EINVAL)?;
        let rdev = get_u32(body, 4).ok_or(linux::EINVAL)?;
        let umask = get_u32(body, 8).unwrap_or(0);
        let (name, _) = get_name(body.get(16..).ok_or(linux::EINVAL)?).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        sys::mknod_at(parent.reference()?.raw_fd(), &name, mode & !umask, rdev)?;
        let entry = self.entry(&parent, &name)?;
        Ok(self.entry_reply(&entry))
    }

    fn mkdir(&self, parent: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let parent = self.directory(parent)?;
        let mode = get_u32(body, 0).ok_or(linux::EINVAL)?;
        let umask = get_u32(body, 4).unwrap_or(0);
        let (name, _) = get_name(body.get(8..).ok_or(linux::EINVAL)?).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        let mode = mode & 0o7777 & !umask;
        if let Some(reply) = self.name_pending(
            &parent,
            &name,
            crate::inode::PendingKind::Directory,
            libc::S_IFDIR as u32 | mode,
            None,
        )? {
            return Ok(reply);
        }
        self.settle_while(&parent, |parent| parent.is_pending());
        sys::mkdir_at(parent.reference()?.raw_fd(), &name, mode)?;
        let entry = self.entry(&parent, &name)?;
        Ok(self.entry_reply(&entry))
    }

    fn unlink(&self, parent: u64, body: &[u8], dir: bool) -> Result<Vec<u8>, i32> {
        let parent = self.directory(parent)?;
        self.settle_while(&parent, |parent| parent.is_pending());
        let (name, _) = get_name(body).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        // A promised name has no host file yet, and its create cannot be
        // withdrawn: the guest may hold it open, and an unlinked file keeps
        // its bytes for whoever does — the server never learns about opens,
        // so it cannot tell. Settled, then removed like any other.
        self.settle_while(&parent, |parent| {
            parent.pending_child(name.to_bytes()).is_some()
        });
        if dir && parent.name_pending_gone(name.to_bytes()) {
            return Err(linux::ENOENT);
        }
        if dir
            && self.apply.accepting()
            && let Ok(st) = sys::stat_at(parent.reference()?.raw_fd(), &name)
            && let Some(child) = self
                .registry
                .nodeid_for(st.st_dev as i64, st.st_ino)
                .and_then(|id| self.registry.get(id))
        {
            // RMDIR, acknowledged: the guest removes a directory it has
            // emptied, and its unlinks are queued ahead of this by the
            // child's key. Waiting for them made `rm -rf` synchronous at
            // the cost of every unlink: twice OrbStack. Emptiness is
            // checked here rather than trusted — a name the host holds
            // that no queued unlink accounts for is ENOTEMPTY now, not a
            // removal that quietly comes back — and the check is one
            // listing, against the four hundred microseconds the wait was.
            if child.pending_children_snapshot().is_empty() {
                let gone = child.pending_gone_snapshot();
                if child.is_pending() {
                    // Nothing on the host yet; nothing promised inside.
                } else if let Ok(listing) = self.list(child.id())
                    && let Some(entry) = listing.iter().find(|entry| {
                        entry.name != b"." && entry.name != b".." && !gone.contains(&entry.name)
                    })
                {
                    // Said aloud: the guest believes this directory empty,
                    // and an entry the queue does not account for is either
                    // the Mac's doing or a promise this server lost.
                    let known = sys::stat_at(
                        child.reference()?.raw_fd(),
                        &CString::new(entry.name.clone()).map_err(|_| linux::EINVAL)?,
                    )
                    .ok()
                    .and_then(|st| self.registry.nodeid_for(st.st_dev as i64, st.st_ino))
                    .and_then(|id| self.registry.get(id).map(|i| (id, i)));
                    tracing::warn!(
                        dir = %name.to_string_lossy(),
                        entry = %String::from_utf8_lossy(&entry.name),
                        entry_kind = entry.kind,
                        entry_nodeid = known.as_ref().map(|(id, _)| *id).unwrap_or(0),
                        entry_pending = known.as_ref().map(|(_, i)| i.is_pending()).unwrap_or(false),
                        entry_gone_inside = known.as_ref().map(|(_, i)| i.pending_gone_snapshot().len()).unwrap_or(0),
                        entry_promised_inside = known.as_ref().map(|(_, i)| i.pending_children_snapshot().len()).unwrap_or(0),
                        promised_gone = ?gone.iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect::<Vec<_>>(),
                        listed = listing.len(),
                        queued = self.apply.depth(),
                        held = self.deferred.map.lock().expect("held creates poisoned").len(),
                        "rmdir refused: an entry no queued removal accounts for"
                    );
                    return Err(linux::ENOTEMPTY);
                }
            } else {
                return Err(linux::ENOTEMPTY);
            }
            parent.add_pending_gone(name.to_bytes());
            let job = {
                let parent = parent.clone();
                let name = name.clone();
                move || {
                    if let Err(errno) =
                        (|| sys::unlink_at(parent.reference()?.raw_fd(), &name, true))()
                    {
                        tracing::warn!(
                            errno,
                            name = %name.to_string_lossy(),
                            "an acknowledged rmdir failed to apply"
                        );
                    }
                    parent.remove_pending_gone(name.to_bytes());
                }
            };
            let seq = self.apply.push(crate::apply::Job::of(
                crate::apply::Kind::Rmdir,
                crate::apply::Keys::of(&[parent.id(), child.id()]),
                0,
                job,
            ));
            parent.settled_by(seq);
            child.settled_by(seq);
            return Ok(Vec::new());
        }
        if !dir && parent.name_pending_gone(name.to_bytes()) {
            // Already promised away; the guest's own dentry cache should have
            // stopped this, so answer what the settled state will say.
            return Err(linux::ENOENT);
        }
        if !dir && self.apply.accepting() {
            // The guest only unlinks a name it holds a positive dentry for,
            // and permission is its own check (`default_permissions`), so the
            // outcome is known now. A failure here is external interference
            // inside a millisecond window; it is logged, not silent, and the
            // next listing tells the truth.
            parent.add_pending_gone(name.to_bytes());
            // The dying name's inode — if the guest knows it — will report a
            // stale link count until the unlink applies; shadow it so GETATTR
            // waits. The stat is two microseconds against the twenty-seven
            // this path no longer spends.
            // `nodeid_for`, not `relookup`: this is the server finding its
            // own entry, not the guest looking the name up. Counting it as a
            // lookup left every unlinked file one FORGET short of release —
            // pinned in the table with its descriptor, until the sweep had
            // nothing left it was allowed to park.
            let target_out = sys::stat_at(parent.reference()?.raw_fd(), &name)
                .ok()
                .and_then(|st| self.registry.nodeid_for(st.st_dev as i64, st.st_ino))
                .and_then(|id| self.registry.get(id));
            if let Some(target) = &target_out {
                target.shadow_meta();
            }
            let job = {
                let parent = parent.clone();
                let name = name.clone();
                let target = target_out.clone();
                move || {
                    if let Err(errno) =
                        (|| sys::unlink_at(parent.reference()?.raw_fd(), &name, false))()
                    {
                        tracing::warn!(
                            errno,
                            name = %name.to_string_lossy(),
                            "an acknowledged unlink failed to apply"
                        );
                    }
                    parent.remove_pending_gone(name.to_bytes());
                    if let Some(target) = &target {
                        target.unshadow_meta();
                    }
                }
            };
            let seq = self.apply.push(crate::apply::Job::of(
                crate::apply::Kind::Unlink,
                crate::apply::Keys::of(&[
                    parent.id(),
                    target_out.as_ref().map(|t| t.id()).unwrap_or(0),
                ]),
                0,
                job,
            ));
            parent.settled_by(seq);
            if let Some(target) = &target_out {
                target.settled_by(seq);
            }
            return Ok(Vec::new());
        }
        if dir {
            // The synchronous fallback for a directory the queue could not
            // take — not known to the registry, or the queue refusing work.
            // Its files' removals may still be queued, keyed by an inode
            // this path does not have, and a host rmdir ahead of them is
            // ENOTEMPTY for a directory the guest has emptied. Everything
            // queued lands first; this path is rare enough to afford it.
            self.apply.drain();
        }
        sys::unlink_at(parent.reference()?.raw_fd(), &name, dir)?;
        Ok(Vec::new())
    }

    /// The asynchronous half of RENAME, for a plain file the guest knows.
    ///
    /// pnpm writes every store file under a temporary name and renames it
    /// into place: seventeen thousand renames an install, each one queued
    /// behind the file's own create and writes by the order it arrives in.
    /// The overlay says the old name is gone and the new one resolves to the
    /// same inode — pending or bound — until the job lands. Directories,
    /// flags and files the registry has never seen keep the synchronous path.
    fn rename_pending(
        &self,
        old_parent: &std::sync::Arc<Inode>,
        old: &CString,
        new_parent: &std::sync::Arc<Inode>,
        new: &CString,
    ) -> Result<Option<Vec<u8>>, i32> {
        if !self.apply.accepting() || old_parent.name_pending_gone(old.to_bytes()) {
            return Ok(None);
        }
        let nodeid = match old_parent.pending_child(old.to_bytes()) {
            Some(nodeid) => {
                // A held create is simply made under the new name: no
                // temporary file, no rename. The new name must be free —
                // a rename over something is a replacement, and that is the
                // queue's job to order — and the check holds the lock the
                // create is taken with.
                let retargeted = {
                    let mut held = self.deferred.map.lock().expect("held creates poisoned");
                    let free = new_parent.pending_child(new.to_bytes()).is_none()
                        && !new_parent.name_pending_gone(new.to_bytes())
                        && (new_parent.is_pending()
                            || matches!(
                                sys::stat_at(new_parent.reference()?.raw_fd(), new),
                                Err(errno) if errno == linux::ENOENT
                            ));
                    match held.get_mut(&nodeid) {
                        Some(held) if free => {
                            held.parent = new_parent.clone();
                            held.name = new.clone();
                            true
                        }
                        _ => false,
                    }
                };
                if retargeted {
                    old_parent.remove_pending_child(old.to_bytes(), nodeid);
                    new_parent.add_pending_child(new.to_bytes(), nodeid);
                    if let Some(inode) = self.registry.get(nodeid) {
                        inode.set_place(new_parent, new);
                    }
                    return Ok(Some(Vec::new()));
                }
                self.materialize_why(nodeid, 5);
                nodeid
            }
            None => {
                let st = match sys::stat_at(old_parent.reference()?.raw_fd(), old) {
                    Ok(st) => st,
                    Err(_) => return Ok(None),
                };
                if st.st_mode & 0o170000 != 0o100000 {
                    return Ok(None);
                }
                match self.registry.nodeid_for(st.st_dev as i64, st.st_ino) {
                    Some(nodeid) => nodeid,
                    None => return Ok(None),
                }
            }
        };
        let Some(inode) = self.registry.get(nodeid) else {
            return Ok(None);
        };
        if inode.is_dir {
            return Ok(None);
        }
        // Reached by its new name from here on, whichever path performs the
        // rename; a name that turns out not to be it yet fails its identity
        // check and falls back to a reopen by identity.
        inode.set_place(new_parent, new);
        // Whatever the new name held loses it; if the guest knows that
        // inode, its link count is about to change.
        let displaced = if let Some(id) = new_parent.pending_child(new.to_bytes()) {
            self.registry.get(id)
        } else {
            sys::stat_at(new_parent.reference()?.raw_fd(), new)
                .ok()
                .and_then(|st| self.registry.nodeid_for(st.st_dev as i64, st.st_ino))
                .and_then(|id| self.registry.get(id))
        };
        if let Some(displaced) = &displaced {
            if displaced.is_dir {
                return Ok(None);
            }
            // Not withdrawn: a descriptor the guest holds on the displaced
            // file keeps reading it after the rename, so it must exist.
            displaced.shadow_meta();
        }
        // A promise moves with its name: the old name stops promising the
        // inode the instant the new one starts, or a listing in between
        // shows the file twice — and a rename chain leaves every step of
        // it behind until the queue catches up.
        old_parent.remove_pending_child(old.to_bytes(), nodeid);
        old_parent.add_pending_gone(old.to_bytes());
        new_parent.add_pending_child(new.to_bytes(), nodeid);
        let job = {
            let old_parent = old_parent.clone();
            let new_parent = new_parent.clone();
            let old = old.clone();
            let new = new.clone();
            let displaced = displaced.clone();
            move || {
                if let Err(errno) = (|| {
                    sys::rename_at(
                        old_parent.reference()?.raw_fd(),
                        &old,
                        new_parent.reference()?.raw_fd(),
                        &new,
                        0,
                    )
                })() {
                    tracing::warn!(
                        errno,
                        from = %old.to_string_lossy(),
                        to = %new.to_string_lossy(),
                        "an acknowledged rename failed to apply"
                    );
                }
                old_parent.remove_pending_gone(old.to_bytes());
                new_parent.remove_pending_child(new.to_bytes(), nodeid);
                if let Some(displaced) = &displaced {
                    displaced.unshadow_meta();
                }
            }
        };
        let seq = self.apply.push(crate::apply::Job::of(
            crate::apply::Kind::Rename,
            crate::apply::Keys::of(&[
                old_parent.id(),
                new_parent.id(),
                inode.id(),
                displaced.as_ref().map(|d| d.id()).unwrap_or(0),
            ]),
            0,
            job,
        ));
        old_parent.settled_by(seq);
        new_parent.settled_by(seq);
        inode.settled_by(seq);
        if let Some(displaced) = &displaced {
            displaced.settled_by(seq);
        }
        Ok(Some(Vec::new()))
    }

    fn rename(&self, parent: u64, body: &[u8], two: bool) -> Result<Vec<u8>, i32> {
        let old_parent = self.directory(parent)?;
        let newdir = self.resolve(get_u64(body, 0).ok_or(linux::EINVAL)?);
        let (flags, names) = if two {
            (
                get_u32(body, 8).ok_or(linux::EINVAL)?,
                body.get(16..).ok_or(linux::EINVAL)?,
            )
        } else {
            (0, body.get(8..).ok_or(linux::EINVAL)?)
        };
        let new_parent = self.directory(newdir)?;
        let (old, rest) = get_name(names).ok_or(linux::EINVAL)?;
        let (new, _) = get_name(rest).ok_or(linux::EINVAL)?;
        let old = self.checked_name(old)?;
        let new = self.checked_name(new)?;
        self.settle_while(&old_parent, |parent| parent.is_pending());
        self.settle_while(&new_parent, |parent| parent.is_pending());
        if flags == 0
            && let Some(reply) = self.rename_pending(&old_parent, &old, &new_parent, &new)?
        {
            return Ok(reply);
        }
        self.settle_while(&old_parent, |parent| {
            parent.pending_child(old.to_bytes()).is_some()
                || parent.name_pending_gone(old.to_bytes())
        });
        self.settle_while(&new_parent, |parent| {
            parent.pending_child(new.to_bytes()).is_some()
                || parent.name_pending_gone(new.to_bytes())
        });
        sys::rename_at(
            old_parent.reference()?.raw_fd(),
            &old,
            new_parent.reference()?.raw_fd(),
            &new,
            flags,
        )?;
        Ok(Vec::new())
    }

    fn link(&self, parent: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let parent = self.directory(parent)?;
        self.settle_while(&parent, |parent| parent.is_pending());
        let oldnodeid = self.resolve(get_u64(body, 0).ok_or(linux::EINVAL)?);
        let target = self.inode(oldnodeid)?;
        let (name, _) = get_name(body.get(8..).ok_or(linux::EINVAL)?).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        if let Some(reply) = self.link_pending(&parent, &target, oldnodeid, &name)? {
            return Ok(reply);
        }
        self.settle_while(&target, |target| target.is_pending() || target.is_dirty());
        self.settle_while(&parent, |parent| {
            parent.pending_child(name.to_bytes()).is_some()
                || parent.name_pending_gone(name.to_bytes())
        });
        // `linkat` has no descriptor-only form on macOS, so the source is named
        // by its live path — which is correct even if it has been renamed since
        // the guest looked it up.
        let source = self.path(&target)?;
        sys::link_at(libc::AT_FDCWD, &source, parent.reference()?.raw_fd(), &name)?;
        let entry = self.entry(&parent, &name)?;
        Ok(self.entry_reply(&entry))
    }

    /// The asynchronous half of MKDIR and SYMLINK: a fresh name acknowledged
    /// with a pending inode of the right kind, the syscall queued behind the
    /// parent's own making by key.
    ///
    /// A pnpm install makes twelve thousand directories and five thousand
    /// symlinks, each one synchronously a hundred microseconds of APFS on
    /// the request thread — in the same directories the clone workers were
    /// filling, so every one was also contention. A directory that is still
    /// a promise holds nothing but promises: lookups and listings under it
    /// are answered from its overlay, and anything that needs its
    /// descriptor settles it first.
    fn name_pending(
        &self,
        parent: &std::sync::Arc<Inode>,
        name: &CString,
        kind: crate::inode::PendingKind,
        mode: u32,
        target: Option<CString>,
    ) -> Result<Option<Vec<u8>>, i32> {
        if !self.apply.accepting() {
            return Ok(None);
        }
        if parent.pending_child(name.to_bytes()).is_some() {
            return Err(linux::EEXIST);
        }
        // A pending parent has nothing in it the overlay does not know.
        if !parent.is_pending() && !parent.name_pending_gone(name.to_bytes()) {
            match sys::stat_at(parent.reference()?.raw_fd(), name) {
                Err(errno) if errno == linux::ENOENT => {}
                _ => return Ok(None),
            }
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let born = (now.as_secs() as i64, now.subsec_nanos() as i64);
        let meta = crate::inode::PendingMeta {
            mode,
            born,
            atime: born,
            mtime: born,
        };
        let (nodeid, inode) = self.registry.insert_pending(parent.dev(), meta, kind);
        inode.set_place(parent, name);
        if let Some(target) = &target {
            // A symlink's size is its target's length; the overlay says so
            // until the host does.
            inode.write_acked(target.as_bytes().len() as u64);
        }
        parent.add_pending_child(name.to_bytes(), nodeid);
        // The reply before the push: the promise is complete now, and a
        // job that runs before the reply is built would have bound the
        // inode out from under it. The pending inode was born with the one
        // lookup this reply is.
        let reply = self.entry_reply(&self.pending_entry(parent, &inode, nodeid));
        let job = {
            let registry = self.registry.clone();
            let parent = parent.clone();
            let inode = inode.clone();
            let name = name.clone();
            let is_symlink = matches!(kind, crate::inode::PendingKind::Symlink);
            move || {
                let result = (|| {
                    let parent_ref = parent.reference()?;
                    match &target {
                        Some(target) => sys::symlink_at(target, parent_ref.raw_fd(), &name)?,
                        None => {
                            let mode = inode.pending_meta().map(|m| m.mode).unwrap_or(mode);
                            sys::mkdir_at(parent_ref.raw_fd(), &name, mode & 0o7777)?;
                        }
                    }
                    let fd = sys::open_reference(parent_ref.raw_fd(), &name, is_symlink)?;
                    let st = sys::stat_fd(fd.as_raw_fd())?;
                    Ok((fd, st))
                })();
                match result {
                    Ok((fd, st)) => {
                        registry.bind_pending(nodeid, &inode, fd, st.st_dev as i64, st.st_ino);
                    }
                    Err(errno) => {
                        tracing::warn!(
                            errno,
                            name = %name.to_string_lossy(),
                            "an acknowledged mkdir or symlink failed to apply"
                        );
                        inode.bind_failed(errno);
                    }
                }
                if target.is_some() {
                    inode.write_applied(Ok(()));
                }
                parent.remove_pending_child(name.to_bytes(), nodeid);
            }
        };
        let seq = self.apply.push(crate::apply::Job::of(
            if matches!(kind, crate::inode::PendingKind::Symlink) {
                crate::apply::Kind::Symlink
            } else {
                crate::apply::Kind::Mkdir
            },
            crate::apply::Keys::of(&[parent.id(), nodeid]),
            0,
            job,
        ));
        parent.settled_by(seq);
        inode.settled_by(seq);
        Ok(Some(reply))
    }

    /// The asynchronous half of LINK: acknowledged with the target's own
    /// entry under the new name, the `linkat` queued behind whatever the
    /// target still has queued.
    ///
    /// pnpm's import when clones are not on offer — which is every other
    /// Docker on a Mac, and so what the comparison is against — is a
    /// hardlink from its store for every file it installs, sixty thousand
    /// an install. Served synchronously each one waited for the store
    /// file's own create and write to land at the back of a full window:
    /// three hundred microseconds a link, seventeen seconds an install.
    /// Queued, it costs the acknowledgement; the job orders itself behind
    /// the target's create by sharing its key, and needs nothing settled.
    fn link_pending(
        &self,
        parent: &std::sync::Arc<Inode>,
        target: &std::sync::Arc<Inode>,
        nodeid: u64,
        name: &CString,
    ) -> Result<Option<Vec<u8>>, i32> {
        if !self.apply.accepting() || target.is_dir || target.is_cancelled() {
            return Ok(None);
        }
        if parent.pending_child(name.to_bytes()).is_some() {
            // Promised to someone: settled, the host would say the same.
            return Err(linux::EEXIST);
        }
        if !parent.name_pending_gone(name.to_bytes()) {
            match sys::stat_at(parent.reference()?.raw_fd(), name) {
                Err(errno) if errno == linux::ENOENT => {}
                _ => return Ok(None),
            }
        }
        self.materialize_why(nodeid, 6);
        target.link_acked();
        parent.add_pending_child(name.to_bytes(), nodeid);
        let job = {
            let parent = parent.clone();
            let target = target.clone();
            let name = name.clone();
            move || {
                let result = (|| {
                    let parent_ref = parent.reference()?;
                    // Bound by now, or its create failed — and a provisional
                    // number names nothing on the volume.
                    if target.is_pending() || target.ino() & crate::inode::PROVISIONAL_INO != 0 {
                        return Err(linux::ESTALE);
                    }
                    let source = sys::c_path(&std::path::PathBuf::from(format!(
                        "/.vol/{}/{}",
                        target.dev(),
                        target.ino()
                    )))?;
                    sys::link_at(libc::AT_FDCWD, &source, parent_ref.raw_fd(), &name)
                })();
                if let Err(errno) = result {
                    tracing::warn!(
                        errno,
                        name = %name.to_string_lossy(),
                        "an acknowledged link failed to apply"
                    );
                }
                target.link_applied();
                parent.remove_pending_child(name.to_bytes(), nodeid);
            }
        };
        let seq = self.apply.push(crate::apply::Job::of(
            crate::apply::Kind::Link,
            crate::apply::Keys::of(&[parent.id(), nodeid]),
            0,
            job,
        ));
        parent.settled_by(seq);
        target.settled_by(seq);
        let entry = if target.is_pending() {
            self.registry.count_lookup(nodeid);
            self.pending_entry(parent, target, nodeid)
        } else {
            self.promised_entry(parent, target, nodeid)?
        };
        Ok(Some(self.entry_reply(&entry)))
    }

    /// What the guest may do with the page cache of a file it just created.
    ///
    /// CREATE is the one open the guest still reports, because it is also a
    /// name change; every later open of the same file happens without telling
    /// us at all. `FOPEN_KEEP_CACHE` matches what the kernel assumes for those,
    /// so a file behaves the same whether or not the process that opened it was
    /// the one that made it.
    fn created_file_flags(&self) -> u32 {
        if self.policy.timings().attr.is_zero() {
            0
        } else {
            fuse::fopen::KEEP_CACHE
        }
    }

    /// The descriptor an operation should act on.
    ///
    /// A non-zero `fh` names a handle the guest was given by CREATE, and is
    /// authoritative. A zero one means the guest has stopped reporting opens,
    /// so the inode is the only thing identifying the file and the descriptor
    /// is ours to find or make.
    fn file_for(&self, nodeid: u64, fh: u64, need_write: bool) -> Result<Arc<OpenFile>, i32> {
        if fh != 0
            && let Some(handle) = self.registry.handle(fh)
        {
            return handle.file().ok_or(linux::EISDIR);
        }
        if let Some(cached) = self.open_cache.file(nodeid, need_write) {
            return Ok(cached);
        }
        let inode = self.inode(nodeid)?;
        if inode.is_dir {
            return Err(linux::EISDIR);
        }
        // Read-write when a write is coming, read-only otherwise: most files
        // are only ever read, and asking for write access to a read-only file
        // fails outright rather than degrading.
        let flags = if need_write { 2 } else { 0 };
        let fd = open_inode(&inode, flags)?;
        let file = Arc::new(OpenFile {
            fd,
            readable: true,
            append: false,
            writable: need_write,
        });
        self.open_cache.put_file(nodeid, file.clone());
        Ok(file)
    }

    /// The listing a READDIR should serve from.
    fn dir_for(&self, nodeid: u64) -> Result<Arc<OpenDir>, i32> {
        if let Some(cached) = self.open_cache.directory(nodeid) {
            return Ok(cached);
        }
        let dir = Arc::new(OpenDir {
            nodeid,
            entries: std::sync::Mutex::new(Vec::new()),
            complete: std::sync::atomic::AtomicBool::new(false),
        });
        self.open_cache.put_directory(nodeid, dir.clone());
        Ok(dir)
    }

    /// Reads a directory in full, from wherever it now lives.
    fn list(&self, nodeid: u64) -> Result<Vec<sys::DirEntry>, i32> {
        let inode = self.directory(nodeid)?;
        // A fresh descriptor per listing, and that is not waste: a dup of
        // the reference shares its offset with every other dup, and two
        // threads listing one directory — ripgrep, pnpm's workers — then
        // advance one offset between them and each gets half the entries.
        // Measured as ripgrep at 87% of native instead of 97%, and a pnpm
        // install that failed its second repetition.
        // 0o200000 is Linux's O_DIRECTORY; the translation layer maps it.
        let fd = open_inode(&inode, 0o200000)?;
        sys::Dir::from_fd(fd)?.read_all()
    }

    /// A whole-file clone of one inode over another name (guest patch 0005).
    ///
    /// The reply to pnpm's FICLONE probe, and the reason its imports run in
    /// clone mode on the share the way they do on the Mac itself.
    ///
    /// Queued, like every other mutation, and that was measured rather than
    /// assumed. An APFS clone costs fifty to a hundred microseconds however
    /// its source is named (the syscall is the whole job; its other phases
    /// time at seven), and pnpm makes sixty thousand an install. Served on
    /// the request thread instead, four wide as libuv issues them, the guest
    /// waited a hundred and eighteen microseconds for each one and the
    /// install took ten seconds against eight. Hardlinks, pnpm's fallback on
    /// every other Docker, cost a hundred and sixty-six each on APFS — twice
    /// a clone — and took seventeen.
    fn clone_over(&self, body: &[u8]) -> Result<Vec<u8>, i32> {
        // Both name nodeids in the body, not the header, so the dispatch-time
        // resolution has not seen them: the source may be the guest's open
        // descriptor on a file a clone has since replaced.
        let nodeid_in = self.resolve(get_u64(body, 0).ok_or(linux::EINVAL)?);
        let parent_out = self.resolve(get_u64(body, 8).ok_or(linux::EINVAL)?);
        let (name, _) = get_name(body.get(16..).ok_or(linux::EINVAL)?).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        let source = self.inode(nodeid_in)?;
        let parent = self.directory(parent_out)?;
        // The source's path is stable once the file exists; its queued writes
        // need no waiting for, because the clone joins the same queue behind
        // them and lands after they do.
        self.settle_while(&source, |source| source.is_pending());
        let source_path = self.path(&source)?;
        // The source by descriptor: the one a recent write left in the open
        // cache, else the inode's own. The clone then names no path for it.
        enum Source {
            Cached(std::sync::Arc<OpenFile>),
            Held(crate::inode::Reference),
        }
        impl Source {
            fn raw_fd(&self) -> std::os::fd::RawFd {
                match self {
                    Source::Cached(file) => file.fd.as_raw_fd(),
                    Source::Held(reference) => reference.raw_fd(),
                }
            }
        }
        // A write-only descriptor cannot be cloned from (EBADF); the
        // event-only reference can, and costs no open.
        let source_fd = match self.open_cache.file(nodeid_in, false) {
            Some(file) if file.readable => Source::Cached(file),
            _ => Source::Held(source.reference()?),
        };
        let st = sys::stat_fd(source_fd.raw_fd())?;
        let size = source.overlay_size(st.st_size as u64);
        let mode = st.st_mode as u32;
        if !self.apply.accepting() {
            self.settle_while(&source, |source| source.is_dirty());
            let parent_ref = parent.reference()?;
            let tmp = sys::c_path(&std::path::PathBuf::from(format!(
                ".lighter-clone-{}",
                std::process::id() as u64 ^ nodeid_in
            )))?;
            sys::clonefile_at(&source_path, parent_ref.raw_fd(), &tmp)?;
            if let Err(e) = sys::rename_at(parent_ref.raw_fd(), &tmp, parent_ref.raw_fd(), &name, 0)
            {
                let _ = sys::unlink_at(parent_ref.raw_fd(), &tmp, false);
                return Err(e);
            }
            let mut out = Vec::with_capacity(8);
            out.extend_from_slice(&size.to_le_bytes());
            return Ok(out);
        }
        #[allow(non_snake_case)]
        let COPY_INSTEAD_OF_CLONE_MAX = self.copy_max;
        // Acknowledged: the destination is a pending inode with the source's
        // size and mode, and the guest's re-lookup of the name (patch 0005
        // invalidates the dentry) resolves to it until the clone lands.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let born = (now.as_secs() as i64, now.subsec_nanos() as i64);
        let meta = crate::inode::PendingMeta {
            mode,
            born,
            atime: born,
            mtime: born,
        };
        // A destination whose create is still queued is withdrawn: the
        // clone replaces it before it ever existed, and the queue is spared
        // a create, a clone to a temporary and a rename — the three
        // operations pnpm's open-then-FICLONE import used to cost.
        let displaced = if let Some(old) = parent.pending_child(name.to_bytes()) {
            self.registry.get(old)
        } else if parent.is_pending() {
            None
        } else {
            sys::stat_at(parent.reference()?.raw_fd(), &name)
                .ok()
                .and_then(|st| self.registry.nodeid_for(st.st_dev as i64, st.st_ino))
                .and_then(|id| self.registry.get(id))
        };
        let (nodeid, dest) =
            self.registry
                .insert_pending(parent.dev(), meta, crate::inode::PendingKind::File);
        dest.set_place(&parent, &name);
        // The reply is a size, not an entry: see `Registry::unname`.
        self.registry.unname(nodeid);
        if let Some(old) = &displaced {
            // The guest's descriptor on the old file keeps working, and sees
            // the clone — as FICLONE promises — because the old inode now
            // answers with the new one. A create still held back is
            // withdrawn for nothing (`materialize`); one already queued is
            // cancelled, and skips itself if it has not run.
            if old.is_pending() && self.withdraw(old.id()) {
                old.bind_failed(linux::ENOENT);
                parent.remove_pending_child(name.to_bytes(), old.id());
            }
            old.cancel_pending();
            old.forward_to(nodeid);
        }
        dest.write_acked(size);
        parent.add_pending_child(name.to_bytes(), nodeid);
        let job = {
            let registry = self.registry.clone();
            let parent = parent.clone();
            let dest = dest.clone();
            let name = name.clone();
            move || {
                let result = (|| {
                    let parent_ref = parent.reference()?;
                    // Straight to the name when nothing holds it — the
                    // common case, pnpm importing into a fresh store — and
                    // the rename is saved: forty-seven microseconds of APFS
                    // per import, fifty thousand imports an install. Only
                    // a name that is taken goes through a temporary, so the
                    // replacement stays atomic.
                    // A small file is copied rather than cloned. An APFS
                    // clone costs sixty to a hundred microseconds whatever
                    // is done around it, and — measured, three workers,
                    // seventy-six thousand imports — the volume serves no
                    // more of them per second for being asked in parallel.
                    // A create with the bytes written into it costs about
                    // the same alone and does scale across directories,
                    // which is how pnpm's four import threads arrive. The
                    // space a copy costs is bounded by the size cap; a large
                    // file is cloned, and shares its blocks as FICLONE
                    // promises.
                    let bytes = if size <= COPY_INSTEAD_OF_CLONE_MAX {
                        // The reference is event-only and cannot be read;
                        // reopened read-only by identity when that is what
                        // there is.
                        let readable;
                        let raw = match &source_fd {
                            Source::Cached(file) if file.readable => file.fd.as_raw_fd(),
                            _ => {
                                readable = sys::reopen(source_fd.raw_fd(), 0, 0)?;
                                readable.as_raw_fd()
                            }
                        };
                        let mut bytes = Vec::with_capacity(size as usize);
                        let mut chunk = vec![0u8; 64 << 10];
                        loop {
                            let n = sys::read_at(raw, &mut chunk, bytes.len() as u64)?;
                            if n == 0 {
                                break;
                            }
                            bytes.extend_from_slice(&chunk[..n]);
                        }
                        Some(bytes)
                    } else {
                        None
                    };
                    let clone = |name: &CString| {
                        if let Some(bytes) = &bytes {
                            const LINUX_O_WRONLY: u32 = 1;
                            const LINUX_O_CREAT: u32 = 0o100;
                            const LINUX_O_EXCL: u32 = 0o200;
                            let fd = sys::openat_path(
                                parent_ref.raw_fd(),
                                name,
                                LINUX_O_WRONLY | LINUX_O_CREAT | LINUX_O_EXCL,
                                mode & 0o7777,
                            )?;
                            return write_fully(fd.as_raw_fd(), bytes, 0);
                        }
                        match sys::fclonefile_at(source_fd.raw_fd(), parent_ref.raw_fd(), name) {
                            // A descriptor the kernel will not clone from
                            // (an event-only reference, say): by path, as
                            // before.
                            Err(e) if e == linux::EBADF || e == linux::EACCES => {
                                sys::clonefile_at(&source_path, parent_ref.raw_fd(), name)
                            }
                            other => other,
                        }
                    };
                    match clone(&name) {
                        Ok(()) => {}
                        Err(e)
                            if e == linux::EEXIST
                                && sys::unlink_at(parent_ref.raw_fd(), &name, false).is_ok()
                                && clone(&name).is_ok() => {}
                        Err(e) if e == linux::EEXIST => {
                            // Unlink-then-clone was refused; the atomic route.
                            // Unique per job: one source is cloned to many
                            // names at once, and a temporary keyed on the
                            // source alone collides.
                            let tmp = sys::c_path(&std::path::PathBuf::from(format!(
                                ".lighter-clone-{}-{}",
                                std::process::id(),
                                nodeid
                            )))?;
                            clone(&tmp)?;
                            if let Err(e) = sys::rename_at(
                                parent_ref.raw_fd(),
                                &tmp,
                                parent_ref.raw_fd(),
                                &name,
                                0,
                            ) {
                                let _ = sys::unlink_at(parent_ref.raw_fd(), &tmp, false);
                                return Err(e);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                    // One stat for the identity; no descriptor. The clone
                    // is bound parked (see `Inode::bind_parked`), and the
                    // first operation that needs it open opens it.
                    sys::stat_at(parent_ref.raw_fd(), &name)
                })();
                match result {
                    Ok(st) => {
                        registry.bind_pending_parked(
                            nodeid,
                            &dest,
                            st.st_dev as i64,
                            st.st_ino,
                            (st.st_birthtime, st.st_birthtime_nsec),
                        );
                    }
                    Err(errno) => {
                        tracing::warn!(
                            errno,
                            name = %name.to_string_lossy(),
                            "an acknowledged clone failed to apply"
                        );
                        dest.bind_failed(errno);
                    }
                }
                dest.write_applied(Ok(()));
                parent.remove_pending_child(name.to_bytes(), nodeid);
            }
        };
        let seq = self.apply.push(crate::apply::Job::of(
            crate::apply::Kind::Clone,
            crate::apply::Keys::of(&[
                parent_out,
                nodeid,
                nodeid_in,
                displaced.as_ref().map(|d| d.id()).unwrap_or(0),
            ]),
            0,
            job,
        ));
        parent.settled_by(seq);
        dest.settled_by(seq);
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&size.to_le_bytes());
        Ok(out)
    }

    /// The asynchronous half of CREATE: a fresh name is acknowledged with a
    /// pending inode and the open itself joins the apply queue.
    ///
    /// Only the clean case goes this way — a name the host does not have and
    /// no queued operation has promised. Anything else returns `None` and the
    /// synchronous path decides, exactly as it always has. The probe that
    /// establishes "fresh" is a stat: two microseconds against the forty-seven
    /// the create costs, which is the whole trade.
    fn create_pending(
        &self,
        parent: &std::sync::Arc<Inode>,
        name: &CString,
        flags: u32,
        mode: u32,
    ) -> Result<Option<Vec<u8>>, i32> {
        const LINUX_O_EXCL: u32 = 0o200;
        const LINUX_O_APPEND: u32 = 0o2000;
        if flags & LINUX_O_APPEND != 0 {
            // Append keeps the size overlay honest by never being async.
            return Ok(None);
        }
        if parent.pending_child(name.to_bytes()).is_some() {
            // Promised already: to the guest this file exists.
            if flags & LINUX_O_EXCL != 0 {
                return Err(linux::EEXIST);
            }
            // Opening it needs the real file; settle the promise first.
            self.settle_while(parent, |parent| {
                parent.pending_child(name.to_bytes()).is_some()
            });
            return Ok(None);
        }
        // A name promised away is fresh — the unlink is queued ahead of the
        // create this acknowledges, so the order the guest asked for is the
        // order the host applies. So is any name in a directory that is
        // itself still a promise. Otherwise the host answers freshness.
        if !parent.is_pending() && !parent.name_pending_gone(name.to_bytes()) {
            match sys::stat_at(parent.reference()?.raw_fd(), name) {
                Err(errno) if errno == linux::ENOENT => {}
                _ => return Ok(None),
            }
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let born = (now.as_secs() as i64, now.subsec_nanos() as i64);
        let meta = crate::inode::PendingMeta {
            mode: libc::S_IFREG as u32 | mode,
            born,
            atime: born,
            mtime: born,
        };
        let (nodeid, inode) =
            self.registry
                .insert_pending(parent.dev(), meta, crate::inode::PendingKind::File);
        inode.set_place(parent, name);
        parent.add_pending_child(name.to_bytes(), nodeid);
        let held = Held {
            parent: parent.clone(),
            name: name.clone(),
            flags,
            mode,
            writes: Vec::new(),
            bytes: 0,
            since: std::time::Instant::now(),
        };
        if self.defer_creates {
            self.deferred
                .map
                .lock()
                .expect("held creates poisoned")
                .insert(nodeid, held);
            let over = {
                let mut order = self.deferred.order.lock().expect("held order poisoned");
                order.push_back(nodeid);
                order.len().saturating_sub(HELD_MAX)
            };
            // The oldest past the cap are queued now, on this thread, which
            // is where the window's backpressure reaches the guest.
            for _ in 0..over {
                let oldest = {
                    let map = self.deferred.map.lock().expect("held creates poisoned");
                    let mut order = self.deferred.order.lock().expect("held order poisoned");
                    while let Some(&front) = order.front()
                        && !map.contains_key(&front)
                    {
                        order.pop_front();
                    }
                    order.pop_front()
                };
                match oldest {
                    Some(old) if old != nodeid => self.materialize_why(old, 9),
                    _ => break,
                }
            }
        } else {
            let seq = self.apply.push(create_job(
                self.registry.clone(),
                self.open_cache.clone(),
                self.park_creates,
                nodeid,
                inode.clone(),
                held,
            ));
            parent.settled_by(seq);
            inode.settled_by(seq);
        }
        let entry = self.pending_entry(parent, &inode, nodeid);
        let mut out = self.entry_reply(&entry);
        let open_flags = self.created_file_flags() | fuse::fopen::LIGHTER_CREATED;
        out.extend_from_slice(&open_reply(0, open_flags));
        Ok(Some(out))
    }

    /// A stat of the inode by whatever names it now: a resident descriptor,
    /// else its parent's descriptor and its name, and only as a last resort
    /// a revival by identity. What the name answers is checked against the
    /// inode's own identity, so a name the Mac has since given to another
    /// file cannot be mistaken for ours.
    fn stat_of(&self, inode: &Inode) -> Result<libc::stat, i32> {
        match inode.locate()? {
            Located::Fd(fd) => sys::stat_fd(fd.raw_fd()),
            Located::At(parent, name) => match sys::stat_at(parent.raw_fd(), &name) {
                Ok(st) if st.st_ino == inode.ino() && st.st_dev as i64 == inode.dev() => Ok(st),
                _ => sys::stat_fd(inode.reference()?.raw_fd()),
            },
        }
    }

    /// A stat of the inode by the cheapest descriptor to hand.
    ///
    /// The open cache holds the descriptor a recent create or write left,
    /// and a file the guest is still working on is almost always in it.
    /// Going through `reference()` instead revives a parked inode by path —
    /// at a full share, transiently, every time — which was seventy
    /// microseconds of every setattr a pnpm install made.
    fn stat_inode(&self, nodeid: u64, inode: &Inode) -> Result<libc::stat, i32> {
        if let Some(file) = self.open_cache.file(nodeid, false) {
            return sys::stat_fd(file.fd.as_raw_fd());
        }
        match self.stat_of(inode) {
            Ok(st) => Ok(st),
            Err(errno) => {
                if self.stats.enabled() {
                    tracing::warn!(
                        errno,
                        nodeid,
                        pending = inode.is_pending(),
                        cancelled = inode.is_cancelled(),
                        forwarded = inode.forwarded().is_some(),
                        dirty = inode.is_dirty(),
                        is_dir = inode.is_dir,
                        "stat_inode: no descriptor"
                    );
                }
                Err(errno)
            }
        }
    }

    /// Lays what a bound inode was promised by queued setattrs over the
    /// attributes the host stat produced.
    /// The attributes of a bound inode: the host's, with what the guest has
    /// been promised laid over them.
    ///
    /// The overlay is read BEFORE the stat, and that order is the whole
    /// correctness of it. A queued write is a promise the host does not show
    /// yet. Read the host first and the promise second, and a job applying
    /// in between has withdrawn the promise while the stat is of the moment
    /// before: a file the guest had written nineteen bytes to, reported
    /// empty, once in a dozen boots. Read first, the promise is either still
    /// there and laid over, or already kept and the stat shows it.
    fn attr_of(&self, nodeid: u64, inode: &Inode) -> Result<Attr, i32> {
        let overlay = inode.overlay();
        let mut attr = self.attr(&self.stat_inode(nodeid, inode)?);
        self.overlay_attr(&overlay, &mut attr);
        Ok(attr)
    }

    fn overlay_attr(&self, overlay: &crate::inode::Overlay, attr: &mut Attr) {
        attr.nlink += overlay.links;
        if let Some(size) = overlay.size {
            attr.size = attr.size.max(size);
            attr.blocks = attr.size.div_ceil(512);
        }
        if let Some(over) = overlay.attrs {
            if let Some(mode) = over.mode {
                attr.mode = (attr.mode & !0o7777) | (mode & 0o7777);
            }
            if let Some((s, ns)) = over.atime {
                attr.atime = s;
                attr.atimensec = ns as u32;
            }
            if let Some((s, ns)) = over.mtime {
                attr.mtime = s;
                attr.mtimensec = ns as u32;
            }
        }
    }

    /// The entry for a bound inode reached through the overlay — a file a
    /// queued rename has promised to this name. It counts as a lookup, as
    /// any reply naming a nodeid does.
    fn promised_entry(
        &self,
        parent: &Inode,
        inode: &std::sync::Arc<Inode>,
        nodeid: u64,
    ) -> Result<EntryOut, i32> {
        let attr = self.attr_of(nodeid, inode)?;
        self.registry.count_lookup(nodeid);
        let entry_valid = self
            .policy
            .validity(parent.dev(), parent.ino(), Answer::File);
        let attr_valid = self.policy.attr_validity(inode.dev(), inode.ino());
        Ok(EntryOut {
            nodeid,
            generation: 0,
            entry_valid: entry_valid.as_secs(),
            attr_valid: attr_valid.as_secs(),
            entry_valid_nsec: entry_valid.subsec_nanos(),
            attr_valid_nsec: attr_valid.subsec_nanos(),
            attr,
        })
    }

    /// The entry a pending inode answers with, from what the guest was told.
    fn pending_entry(&self, parent: &Inode, inode: &Inode, nodeid: u64) -> EntryOut {
        let answer = if inode.is_dir {
            Answer::Directory
        } else {
            Answer::File
        };
        let entry_valid = self.policy.validity(parent.dev(), parent.ino(), answer);
        let attr_valid = self.policy.attr_validity(inode.dev(), inode.ino());
        let Some(meta) = inode.pending_meta() else {
            // Bound since the caller looked — its job ran in the gap — and
            // the host answers now. Answering from a default promise here
            // told the guest a directory it had just made was a file, which
            // its kernel reports as EIO on the mkdir: one install in three.
            let attr = self
                .attr_of(nodeid, inode)
                .unwrap_or_else(|_| self.attr(&libc_stat_default(inode)));
            return EntryOut {
                nodeid,
                generation: 0,
                entry_valid: entry_valid.as_secs(),
                attr_valid: attr_valid.as_secs(),
                entry_valid_nsec: entry_valid.subsec_nanos(),
                attr_valid_nsec: attr_valid.subsec_nanos(),
                attr,
            };
        };
        let size = inode.overlay_size(0);
        EntryOut {
            nodeid,
            generation: 0,
            entry_valid: entry_valid.as_secs(),
            attr_valid: attr_valid.as_secs(),
            entry_valid_nsec: entry_valid.subsec_nanos(),
            attr_valid_nsec: attr_valid.subsec_nanos(),
            attr: Attr {
                ino: inode.ino(),
                size,
                blocks: size.div_ceil(512),
                atime: meta.atime.0,
                mtime: meta.mtime.0,
                ctime: meta.born.0,
                atimensec: meta.atime.1 as u32,
                mtimensec: meta.mtime.1 as u32,
                ctimensec: meta.born.1 as u32,
                mode: meta.mode,
                nlink: if inode.is_dir { 2 } else { 1 } + inode.extra_links(),
                uid: 0,
                gid: 0,
                rdev: 0,
                blksize: 4096,
            },
        }
    }

    fn create(&self, parent: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let parent = self.directory(parent)?;
        let flags = get_u32(body, 0).ok_or(linux::EINVAL)?;
        let mode = get_u32(body, 4).ok_or(linux::EINVAL)?;
        let umask = get_u32(body, 8).unwrap_or(0);
        let (name, _) = get_name(body.get(16..).ok_or(linux::EINVAL)?).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        // CREATE creates, whatever the guest put in `flags`. The kernel always
        // sets O_CREAT, but a request that did not would otherwise be answered
        // with ENOENT for a file the operation was supposed to make.
        //
        // O_EXCL first, then the existing file: the guest may have skipped
        // its pre-create LOOKUP (patch 0004), so whether this open CREATED
        // the file has to be a fact we report, not an assumption it makes —
        // and creation is what CREATE is almost always asked for, so the
        // common case stays one syscall. O_NOFOLLOW because a trailing
        // symlink belongs to the guest's VFS: it comes back as ELOOP and the
        // guest walks it itself.
        const LINUX_O_CREAT: u32 = 0o100;
        const LINUX_O_EXCL: u32 = 0o200;
        const LINUX_O_NOFOLLOW: u32 = 0o400000;
        if self.apply.accepting()
            && let Some(reply) =
                self.create_pending(&parent, &name, flags, mode & 0o7777 & !umask)?
        {
            return Ok(reply);
        }
        self.settle_while(&parent, |parent| parent.is_pending());
        // The synchronous path is about to consult the host about a name
        // whose truth may still be in the queue.
        self.settle_while(&parent, |parent| parent.name_pending_gone(name.to_bytes()));
        let parent_fd = parent.reference()?;
        let mut created = true;
        let mut attempt = 0;
        let fd = loop {
            match sys::openat_path(
                parent_fd.raw_fd(),
                &name,
                flags | LINUX_O_CREAT | LINUX_O_EXCL | LINUX_O_NOFOLLOW,
                mode & 0o7777 & !umask,
            ) {
                Ok(fd) => break fd,
                Err(e) if e == linux::EEXIST && flags & LINUX_O_EXCL == 0 => {}
                Err(e) => return Err(e),
            }
            match sys::openat_path(
                parent_fd.raw_fd(),
                &name,
                (flags & !LINUX_O_CREAT) | LINUX_O_NOFOLLOW,
                0,
            ) {
                Ok(fd) => {
                    created = false;
                    break fd;
                }
                // Unlinked between the two opens: go create it after all,
                // once, so a delete storm cannot pin us here.
                Err(e) if e == linux::ENOENT && attempt == 0 => attempt = 1,
                Err(e) => return Err(e),
            }
        };
        // Everything below avoids re-resolving a path we are already holding
        // open. A package install creates tens of thousands of files, and the
        // naive version costs two `openat`s and a path-based `stat` for each:
        // `fstat` on the descriptor we have, and `dup` rather than a second
        // `openat` for the metadata reference.
        let st = sys::stat_fd(fd.as_raw_fd())?;
        // Linux refuses O_CREAT on an existing directory outright; macOS
        // happily opens it read-only, and the guest may not have looked
        // before asking (patch 0004).
        if st.st_mode & 0o170000 == 0o040000 {
            return Err(linux::EISDIR);
        }
        let entry =
            self.entry_with_reference(&parent, st, self.apply.applied(), || sys::dup(&fd))?;
        let fh = self.registry.add_handle(Handle::File(Arc::new(OpenFile {
            fd,
            readable: true,
            append: flags & 0o2000 != 0,
            writable: flags & 0o3 != 0,
        })));
        let mut out = self.entry_reply(&entry);
        let mut open_flags = self.created_file_flags();
        if created {
            open_flags |= fuse::fopen::LIGHTER_CREATED;
        }
        out.extend_from_slice(&open_reply(fh, open_flags));
        Ok(out)
    }

    fn read(&self, nodeid: u64, body: &[u8], capacity: usize) -> Result<Vec<u8>, i32> {
        let fh = get_u64(body, 0).ok_or(linux::EINVAL)?;
        let offset = get_u64(body, 8).ok_or(linux::EINVAL)?;
        let size = get_u32(body, 16).ok_or(linux::EINVAL)? as usize;
        // Reads never lie: bytes this file was promised must be in it first —
        // and a pending file must exist at all.
        let inode = self.inode(nodeid)?;
        self.settle_while(&inode, |inode| inode.is_dirty() || inode.is_pending());
        if let Some(errno) = inode.take_write_error() {
            return Err(errno);
        }
        let file = self.file_for(nodeid, fh, false)?;
        // The guest sized the reply chain; never promise more than it can hold.
        let size = size.min(capacity.saturating_sub(fuse::OUT_HEADER_LEN));
        let mut buf = vec![0u8; size];
        let read = sys::read_at(file.fd.as_raw_fd(), &mut buf, offset)?;
        buf.truncate(read);
        Ok(buf)
    }

    fn write(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let fh = get_u64(body, 0).ok_or(linux::EINVAL)?;
        let offset = get_u64(body, 8).ok_or(linux::EINVAL)?;
        let size = get_u32(body, 16).ok_or(linux::EINVAL)? as usize;
        let data = body.get(40..40 + size).ok_or(linux::EINVAL)?;
        let inode = self.inode(nodeid)?;
        // A failure parked by an earlier acknowledged write surfaces on the
        // next one, so a writer learns within one operation, not at fsync.
        if let Some(errno) = inode.take_write_error() {
            return Err(errno);
        }
        // A write to a pending file cannot open a descriptor yet; the job
        // resolves one at apply time, after the create it is ordered behind.
        if inode.is_pending() && self.apply.accepting() {
            // A write to a file whose create is still held is held with
            // it, within the caps: one job makes the file with its bytes
            // in it, under whatever name it has by then.
            let absorbed = {
                let mut held = self.deferred.map.lock().expect("held creates poisoned");
                match held.get_mut(&nodeid) {
                    Some(held)
                        if held.bytes + size <= HELD_FILE_CAP
                            && self.deferred.bytes.load(Ordering::Relaxed) + size
                                <= HELD_TOTAL_CAP =>
                    {
                        held.writes.push((offset, data.to_vec()));
                        held.bytes += size;
                        self.deferred.bytes.fetch_add(size, Ordering::Relaxed);
                        true
                    }
                    _ => false,
                }
            };
            if absorbed {
                inode.write_acked(offset + size as u64);
                // A write moves the modification time, as it will on the
                // Mac: a time promised before this write is void, one
                // promised after it stands.
                inode.note_write_time();
                let mut out = Vec::with_capacity(8);
                out.extend_from_slice(&(size as u32).to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
                return Ok(out);
            }
            self.materialize_why(nodeid, 3);
            inode.write_acked(offset + size as u64);
            let data = data.to_vec();
            let job = {
                let inode = inode.clone();
                let open_cache = self.open_cache.clone();
                move || {
                    let result = (|| {
                        // The create that precedes this in the queue put the
                        // open descriptor in the cache; it survives unlink,
                        // which a path reopen does not.
                        let fd: std::sync::Arc<OpenFile>;
                        let raw = if let Some(cached) = open_cache.file(nodeid, true) {
                            fd = cached;
                            fd.fd.as_raw_fd()
                        } else {
                            fd = std::sync::Arc::new(OpenFile {
                                fd: open_inode(&inode, 2)?,
                                readable: true,
                                append: false,
                                writable: true,
                            });
                            fd.fd.as_raw_fd()
                        };
                        let _hold = &fd;
                        let mut at = 0usize;
                        while at < data.len() {
                            match sys::write_at(raw, &data[at..], offset + at as u64) {
                                Ok(0) => return Err(linux::EIO),
                                Ok(n) => at += n,
                                Err(errno) => return Err(errno),
                            }
                        }
                        Ok(())
                    })();
                    inode.write_applied(result);
                }
            };
            let seq = self.apply.push(crate::apply::Job::of(
                crate::apply::Kind::Write,
                crate::apply::Keys::of(&[nodeid]),
                size,
                job,
            ));
            inode.settled_by(seq);
            inode.note_write(seq);
            let mut out = Vec::with_capacity(8);
            out.extend_from_slice(&(size as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            return Ok(out);
        }
        // The queue is refusing work (full disk, retired); the file must
        // exist before a synchronous write can reach it.
        self.settle_while(&inode, |inode| inode.is_pending());
        let file = self.file_for(nodeid, fh, true)?;
        // Append is served synchronously: its end position is unknowable
        // before the syscall, and the size overlay must never have to guess.
        let written = if file.append {
            sys::write_append(file.fd.as_raw_fd(), data)?
        } else if self.apply.accepting() {
            // Acknowledged now, applied in order on the queue. The guest's
            // own kernel keeps the pages it just wrote, reads on this inode
            // drain first, and lookup answers with the overlay size — so
            // nothing can observe the gap except as latency it no longer
            // pays.
            inode.write_acked(offset + data.len() as u64);
            let data = data.to_vec();
            let job = {
                let file = file.clone();
                let inode = inode.clone();
                move || {
                    let mut at = 0usize;
                    let mut result = Ok(());
                    while at < data.len() {
                        match sys::write_at(file.fd.as_raw_fd(), &data[at..], offset + at as u64) {
                            Ok(0) => {
                                result = Err(linux::EIO);
                                break;
                            }
                            Ok(n) => at += n,
                            Err(errno) => {
                                result = Err(errno);
                                break;
                            }
                        }
                    }
                    inode.write_applied(result);
                }
            };
            let seq = self.apply.push(crate::apply::Job::of(
                crate::apply::Kind::Write,
                crate::apply::Keys::of(&[nodeid]),
                size,
                job,
            ));
            inode.settled_by(seq);
            inode.note_write(seq);
            size
        } else {
            sys::write_at(file.fd.as_raw_fd(), data, offset)?
        };
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&(written as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        Ok(out)
    }

    fn fsync(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let fh = get_u64(body, 0).ok_or(linux::EINVAL)?;
        let datasync = get_u32(body, 8).unwrap_or(0) & 1 != 0;
        // A directory's own sync is a settling point for the names inside
        // it: entries reach the host when their queued operations apply, so
        // apply them.
        if let Ok(inode) = self.inode(nodeid)
            && inode.is_dir
        {
            self.settle_while(&inode, |inode| inode.listing_shadowed());
            return Ok(Vec::new());
        }
        // Durability is never claimed early: everything acknowledged reaches
        // the file before the sync, and a parked failure surfaces here rather
        // than being flushed into silence.
        let inode = self.inode(nodeid)?;
        self.settle_while(&inode, |inode| inode.is_dirty() || inode.is_pending());
        if let Some(errno) = inode.take_write_error() {
            return Err(errno);
        }
        let file = self.file_for(nodeid, fh, false)?;
        sys::fsync(file.fd.as_raw_fd(), datasync)?;
        Ok(Vec::new())
    }

    fn statfs(&self) -> Result<Vec<u8>, i32> {
        self.apply.drain();
        let st = sys::statfs(&self.root)?;
        let mut out = Vec::with_capacity(80);
        out.extend_from_slice(&st.f_blocks.to_le_bytes());
        out.extend_from_slice(&st.f_bfree.to_le_bytes());
        out.extend_from_slice(&st.f_bavail.to_le_bytes());
        out.extend_from_slice(&st.f_files.to_le_bytes());
        out.extend_from_slice(&st.f_ffree.to_le_bytes());
        out.extend_from_slice(&st.f_bsize.to_le_bytes());
        out.extend_from_slice(&(NAME_MAX as u32).to_le_bytes());
        out.extend_from_slice(&st.f_bsize.to_le_bytes()); // frsize
        out.resize(80, 0);
        Ok(out)
    }

    fn readdir(
        &self,
        nodeid: u64,
        body: &[u8],
        capacity: usize,
        plus: bool,
    ) -> Result<Vec<u8>, i32> {
        let offset = get_u64(body, 8).ok_or(linux::EINVAL)? as usize;
        let size = get_u32(body, 16).ok_or(linux::EINVAL)? as usize;
        let budget = size.min(capacity.saturating_sub(fuse::OUT_HEADER_LEN));

        let open = self.dir_for(nodeid)?;
        let mut entries = open.entries.lock().expect("directory listing poisoned");
        let parent = self.directory(nodeid)?;
        // A caller starting from the beginning is asking for a fresh view;
        // anyone resuming gets the list the first page came from, so the
        // offsets they were given still mean what they meant.
        if offset == 0 || entries.is_empty() {
            // The host listing cannot show a file that is still a promise,
            // nor stop showing one promised away — so the overlay is merged
            // in rather than waited for. Waiting was a hundred and forty
            // microseconds per listing, sixteen thousand times a pnpm
            // install, on a queue that was busy precisely because of it.
            //
            // The overlay is read BEFORE the host is listed. A job applies
            // its host change first and withdraws its promise second, so a
            // promise read first is in one view or the other, and a
            // promise read second can be in neither: two files of two
            // thousand went missing from a listing that way.
            let gone = parent.pending_gone_snapshot();
            let promised = parent.pending_children_snapshot();
            let mut listed = if parent.is_pending() {
                // Nothing on the host yet: the dots, and the promises below.
                [b".".as_slice(), b"..".as_slice()]
                    .into_iter()
                    .map(|name| sys::DirEntry {
                        ino: parent.ino(),
                        kind: 4,
                        name: name.to_vec(),
                        next_offset: 0,
                    })
                    .collect()
            } else {
                self.list(nodeid)?
            };
            if !gone.is_empty() || !promised.is_empty() {
                if self.debug_listing {
                    let dropped: Vec<String> = listed
                        .iter()
                        .filter(|entry| gone.contains(&entry.name))
                        .map(|entry| String::from_utf8_lossy(&entry.name).into_owned())
                        .collect();
                    tracing::warn!(
                        nodeid,
                        host = listed.len(),
                        promised = promised.len(),
                        gone = gone.len(),
                        dropped = ?dropped,
                        queued = self.apply.depth(),
                        "LISTING with an overlay"
                    );
                }
                listed.retain(|entry| !gone.contains(&entry.name));
                for (name, id) in promised {
                    let Some(inode) = self.registry.get(id) else {
                        continue;
                    };
                    let kind = if inode.is_dir {
                        4
                    } else if inode.is_symlink {
                        10
                    } else {
                        8
                    };
                    match listed.iter_mut().find(|entry| entry.name == name) {
                        // A name the host already has, promised to a new
                        // inode (a clone or rename over it): the promise wins.
                        Some(entry) => {
                            entry.ino = inode.ino();
                            entry.kind = kind;
                        }
                        None => listed.push(sys::DirEntry {
                            ino: inode.ino(),
                            kind,
                            name,
                            next_offset: 0,
                        }),
                    }
                }
            }
            *entries = listed;
            open.complete.store(false, Ordering::Relaxed);
        }

        let mut out = Vec::new();
        let mut reached_end = true;
        for (index, entry) in entries.iter().enumerate().skip(offset) {
            let next = index as u64 + 1;
            if plus {
                // A READDIRPLUS record is an entry followed by a dirent, and
                // the entry has to be built before its size is known, so the
                // budget is checked against the pair up front.
                if out.len() + fuse::ENTRY_OUT_LEN + fuse::dirent_len(entry.name.len()) > budget {
                    reached_end = false;
                    break;
                }
                // "." and ".." carry nodeid 0, which the kernel reads as "no
                // entry to instantiate". Looking them up instead would take a
                // lookup reference on every directory a walk passes through,
                // and nothing would ever forget them.
                let looked_up = if entry.name == b"." || entry.name == b".." {
                    None
                } else if let Some(id) = parent.pending_child(&entry.name)
                    && let Some(inode) = self.registry.get(id)
                {
                    // Promised here: answered from the promise, and counted
                    // as the lookup the kernel takes it for.
                    if inode.is_pending() {
                        self.registry.count_lookup(id);
                        Some(self.pending_entry(&parent, &inode, id))
                    } else {
                        self.promised_entry(&parent, &inode, id).ok()
                    }
                } else {
                    self.checked_name(&entry.name)
                        .and_then(|name| self.entry(&parent, &name))
                        .ok()
                };
                match looked_up {
                    Some(found) => found.encode(&mut out),
                    // A file that vanished between the listing and the lookup
                    // is ordinary. A zeroed entry names nothing, and the guest
                    // treats the record as a plain dirent.
                    None => out.resize(out.len() + fuse::ENTRY_OUT_LEN, 0),
                }
            }
            if !fuse::push_dirent(&mut out, budget, entry.ino, next, entry.kind, &entry.name) {
                // Only reachable in the plain READDIR case; the READDIRPLUS
                // path checked the whole record above.
                reached_end = false;
                break;
            }
        }
        if reached_end {
            open.complete.store(true, Ordering::Relaxed);
        }
        Ok(out)
    }

    fn access(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let mask = get_u32(body, 0).ok_or(linux::EINVAL)?;
        let inode = self.inode(nodeid)?;
        let path = self.path(&inode)?;
        sys::access(&path, mask)?;
        Ok(Vec::new())
    }

    fn lseek(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let fh = get_u64(body, 0).ok_or(linux::EINVAL)?;
        let offset = get_u64(body, 8).ok_or(linux::EINVAL)?;
        let whence = get_u32(body, 16).ok_or(linux::EINVAL)?;
        let file = self.file_for(nodeid, fh, false)?;
        let at = sys::seek(file.fd.as_raw_fd(), offset, whence)?;
        Ok(at.to_le_bytes().to_vec())
    }

    fn fallocate(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let fh = get_u64(body, 0).ok_or(linux::EINVAL)?;
        let offset = get_u64(body, 8).ok_or(linux::EINVAL)?;
        let length = get_u64(body, 16).ok_or(linux::EINVAL)?;
        let mode = get_u32(body, 24).ok_or(linux::EINVAL)?;
        let file = self.file_for(nodeid, fh, true)?;
        sys::fallocate(file.fd.as_raw_fd(), mode, offset, length)?;
        Ok(Vec::new())
    }

    // --- extended attributes ------------------------------------------------

    fn getxattr(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let size = get_u32(body, 0).ok_or(linux::EINVAL)? as usize;
        let (name, _) = get_name(body.get(8..).ok_or(linux::EINVAL)?).ok_or(linux::EINVAL)?;
        if is_linux_only_namespace(name) {
            return Err(linux::ENODATA);
        }
        let name = CString::new(name).map_err(|_| linux::EINVAL)?;
        let inode = self.inode(nodeid)?;
        let path = self.path(&inode)?;
        if size == 0 {
            let len = sys::get_xattr(&path, &name, &mut [])?;
            return Ok(size_reply(len));
        }
        let mut buf = vec![0u8; size];
        let len = sys::get_xattr(&path, &name, &mut buf)?;
        buf.truncate(len);
        Ok(buf)
    }

    fn setxattr(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        // Eight bytes, not sixteen: the longer `fuse_setxattr_in` only appears
        // when FUSE_SETXATTR_EXT was negotiated, and INIT never offers it.
        let size = get_u32(body, 0).ok_or(linux::EINVAL)? as usize;
        let flags = get_u32(body, 4).ok_or(linux::EINVAL)?;
        let (name, rest) = get_name(body.get(8..).ok_or(linux::EINVAL)?).ok_or(linux::EINVAL)?;
        let value = rest.get(..size).ok_or(linux::EINVAL)?;
        let name = CString::new(name).map_err(|_| linux::EINVAL)?;
        let inode = self.inode(nodeid)?;
        let path = self.path(&inode)?;
        sys::set_xattr(&path, &name, value, flags)?;
        Ok(Vec::new())
    }

    fn listxattr(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let size = get_u32(body, 0).ok_or(linux::EINVAL)? as usize;
        let inode = self.inode(nodeid)?;
        let path = self.path(&inode)?;
        if size == 0 {
            let len = sys::list_xattr(&path, &mut [])?;
            return Ok(size_reply(len));
        }
        let mut buf = vec![0u8; size];
        let len = sys::list_xattr(&path, &mut buf)?;
        buf.truncate(len);
        Ok(buf)
    }

    fn removexattr(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let (name, _) = get_name(body).ok_or(linux::EINVAL)?;
        let name = CString::new(name).map_err(|_| linux::EINVAL)?;
        let inode = self.inode(nodeid)?;
        let path = self.path(&inode)?;
        sys::remove_xattr(&path, &name)?;
        Ok(Vec::new())
    }
}

/// Whether an extended attribute is one only Linux has.
///
/// `security.capability` is asked for on every `execve` and, without
/// `FUSE_HANDLE_KILLPRIV`, on every write; `system.posix_acl_*` is asked for on
/// every permission check. None of them can exist on a Mac, and answering from
/// here rather than from a syscall removed about one request in nine from a
/// package install. `user.*` and `trusted.*` still go to the filesystem, so an
/// attribute someone actually set still works.
fn is_linux_only_namespace(name: &[u8]) -> bool {
    name.starts_with(b"security.") || name.starts_with(b"system.posix_acl_")
}

/// Mixes a device and inode number into one that cannot collide with either.
fn mix(dev: u64, ino: u64) -> u64 {
    // 64-bit FNV-1a over the two, which spreads well enough that a share
    // spanning a handful of mounts will not produce a duplicate.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in dev.to_le_bytes().iter().chain(ino.to_le_bytes().iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn open_reply(fh: u64, open_flags: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&fh.to_le_bytes());
    out.extend_from_slice(&open_flags.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

fn size_reply(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

fn write_error(sink: &mut dyn Sink, unique: u64, code: i32) -> usize {
    if sink.capacity() < fuse::OUT_HEADER_LEN {
        return 0;
    }
    let mut out = Vec::with_capacity(fuse::OUT_HEADER_LEN);
    out.extend_from_slice(&(fuse::OUT_HEADER_LEN as u32).to_le_bytes());
    out.extend_from_slice(&(-code).to_le_bytes());
    out.extend_from_slice(&unique.to_le_bytes());
    if sink.write(&out).is_err() {
        return 0;
    }
    fuse::OUT_HEADER_LEN
}

impl Drop for Server {
    fn drop(&mut self) {
        self.settler_stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.settler.lock().expect("settler poisoned").take() {
            let _ = handle.join();
        }
        // Every promise is kept before the queue behind it is drained.
        self.materialize_all();
    }
}

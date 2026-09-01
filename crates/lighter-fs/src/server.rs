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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::cache::{Answer, Invalidator, Policy, Timings};
use crate::errno::linux;
use crate::fsevents::Watcher;
use crate::fuse::{self, Attr, EntryOut, InHeader, get_name, get_u32, get_u64, op};
use crate::inode::{Handle, Inode, OpenDir, OpenFile, Registry};
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
    /// opens. See [`crate::opencache`].
    open_cache: OpenCache,
    /// Opcode counters, off unless asked for.
    stats: Stats,
    /// The host watcher that keeps the policy honest.
    ///
    /// Held rather than used: dropping it stops the stream, after which every
    /// answer would be cached for its full timeout with nothing to shorten it.
    /// `None` means FSEvents refused to start, and the server falls back to
    /// exact coherence rather than to a timeout it cannot invalidate.
    _watcher: Option<Watcher>,
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
            open_cache: OpenCache::new(),
            stats: Stats::new(),
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
                return 0;
            }
            op::BATCH_FORGET => {
                self.batch_forget(body);
                return 0;
            }
            _ => {}
        }

        self.trace(&header);
        let started = self.stats.enabled().then(std::time::Instant::now);
        let outcome = self.handle(&header, body, sink.capacity());
        if let Some(started) = started {
            self.stats.record(header.opcode, started.elapsed());
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
        match header.opcode {
            op::INIT => self.init(body),
            op::SYNCFS => Ok(Vec::new()),
            // Likewise: a flush that can only ever succeed is a round trip on
            // every `close`, and the kernel will stop sending them if told
            // once that we do not implement it.
            op::FLUSH => Err(linux::ENOSYS),
            op::DESTROY => {
                self.log_stats();
                Ok(Vec::new())
            }
            op::LOOKUP => self.lookup(header.nodeid, body),
            op::GETATTR => self.getattr(header.nodeid, body),
            op::SETATTR => self.setattr(header.nodeid, body),
            op::READLINK => self.readlink(header.nodeid),
            op::SYMLINK => self.symlink(header.nodeid, body),
            op::MKNOD => self.mknod(header.nodeid, body),
            op::MKDIR => self.mkdir(header.nodeid, body),
            op::UNLINK => self.unlink(header.nodeid, body, false),
            op::RMDIR => self.unlink(header.nodeid, body, true),
            op::RENAME => self.rename(header.nodeid, body, false),
            op::RENAME2 => self.rename(header.nodeid, body, true),
            op::LINK => self.link(header.nodeid, body),
            // Answering ENOSYS here is not a refusal, it is a negotiation:
            // Linux records it once and thereafter opens, closes and flushes a
            // file without telling us. See `crate::opencache` for what that
            // buys and what it costs.
            op::OPEN | op::OPENDIR => Err(linux::ENOSYS),
            op::CREATE => self.create(header.nodeid, body),
            op::READ => self.read(header.nodeid, body, capacity),
            op::WRITE => self.write(header.nodeid, body),
            op::STATFS => self.statfs(),
            op::RELEASE | op::RELEASEDIR => {
                if let Some(fh) = get_u64(body, 0) {
                    self.registry.release_handle(fh);
                }
                Ok(Vec::new())
            }
            op::FSYNC | op::FSYNCDIR => self.fsync(header.nodeid, body),
            op::READDIR => self.readdir(header.nodeid, body, capacity, false),
            op::READDIRPLUS => self.readdir(header.nodeid, body, capacity, true),
            op::ACCESS => self.access(header.nodeid, body),
            op::LSEEK => self.lseek(header.nodeid, body),
            op::FALLOCATE => self.fallocate(header.nodeid, body),
            op::GETXATTR => self.getxattr(header.nodeid, body),
            op::SETXATTR => self.setxattr(header.nodeid, body),
            op::LISTXATTR => self.listxattr(header.nodeid, body),
            op::REMOVEXATTR => self.removexattr(header.nodeid, body),
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

    /// Prints the opcode histogram, if one was being kept.
    pub fn log_stats(&self) {
        if self.stats.enabled() {
            for line in self.stats.report().lines() {
                tracing::info!("{line}");
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
        let wanted = fuse::init::ASYNC_READ
            | fuse::init::BIG_WRITES
            | fuse::init::ATOMIC_O_TRUNC
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
        let flags2 = if flags & fuse::init::INIT_EXT != 0 {
            fuse::init2::LIGHTER_CREATE & offered2
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

    fn inode(&self, nodeid: u64) -> Result<std::sync::Arc<Inode>, i32> {
        self.registry.get(nodeid).ok_or(linux::ENOENT)
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
        let fd = inode.reference()?;
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
    fn entry(&self, parent: &Inode, name: &CString) -> Result<EntryOut, i32> {
        let st = sys::stat_at(parent.reference()?.raw_fd(), name)?;
        self.entry_from(parent, name, st)
    }

    /// [`Server::entry`], for a caller that already has the `stat`.
    fn entry_from(&self, parent: &Inode, name: &CString, st: libc::stat) -> Result<EntryOut, i32> {
        self.entry_with_reference(parent, st, || {
            let is_symlink = st.st_mode as u32 & libc::S_IFMT as u32 == libc::S_IFLNK as u32;
            sys::open_reference(parent.reference()?.raw_fd(), name, is_symlink)
        })
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
        let entry_valid = self.policy.validity(parent.dev, parent.ino, answer);
        let attr_valid = self.policy.attr_validity(dev, ino);
        Ok(EntryOut {
            nodeid,
            generation: 0,
            entry_valid: entry_valid.as_secs(),
            attr_valid: attr_valid.as_secs(),
            entry_valid_nsec: entry_valid.subsec_nanos(),
            attr_valid_nsec: attr_valid.subsec_nanos(),
            attr: self.attr(&st),
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
            .validity(parent.dev, parent.ino, Answer::Missing);
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
        match self.entry(&parent, &name) {
            Ok(entry) => Ok(self.entry_reply(&entry)),
            Err(linux::ENOENT) => {
                if std::env::var("LIGHTER_FS_DEBUG_ENOENT").as_deref() == Ok("1") {
                    let held = sys::path_of(parent.reference()?.raw_fd())
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|e| format!("<no path: {e}>"));
                    tracing::warn!(
                        parent_dev = parent.dev,
                        parent_ino = parent.ino,
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
        let flags = get_u32(body, 0).unwrap_or(0);
        let st = if flags & fuse::GETATTR_FH != 0
            && let Some(fh) = get_u64(body, 8)
            && let Some(handle) = self.registry.handle(fh)
            && let Some(fd) = handle.raw_fd()
        {
            sys::stat_fd(fd)?
        } else {
            let inode = self.inode(nodeid)?;
            sys::stat_fd(inode.reference()?.raw_fd())?
        };
        Ok(self.attr_reply(&st))
    }

    fn setattr(&self, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let valid = get_u32(body, 0).ok_or(linux::EINVAL)?;
        let inode = self.inode(nodeid)?;
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

        let st = sys::stat_fd(inode.reference()?.raw_fd())?;
        Ok(self.attr_reply(&st))
    }

    fn readlink(&self, nodeid: u64) -> Result<Vec<u8>, i32> {
        let inode = self.inode(nodeid)?;
        if !inode.is_symlink {
            return Err(linux::EINVAL);
        }
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
        sys::symlink_at(&target, parent.reference()?.raw_fd(), &name)?;
        let entry = self.entry(&parent, &name)?;
        Ok(self.entry_reply(&entry))
    }

    fn mknod(&self, parent: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        let parent = self.directory(parent)?;
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
        sys::mkdir_at(parent.reference()?.raw_fd(), &name, mode & 0o7777 & !umask)?;
        let entry = self.entry(&parent, &name)?;
        Ok(self.entry_reply(&entry))
    }

    fn unlink(&self, parent: u64, body: &[u8], dir: bool) -> Result<Vec<u8>, i32> {
        let parent = self.directory(parent)?;
        let (name, _) = get_name(body).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        sys::unlink_at(parent.reference()?.raw_fd(), &name, dir)?;
        Ok(Vec::new())
    }

    fn rename(&self, parent: u64, body: &[u8], two: bool) -> Result<Vec<u8>, i32> {
        let old_parent = self.directory(parent)?;
        let newdir = get_u64(body, 0).ok_or(linux::EINVAL)?;
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
        let oldnodeid = get_u64(body, 0).ok_or(linux::EINVAL)?;
        let target = self.inode(oldnodeid)?;
        let (name, _) = get_name(body.get(8..).ok_or(linux::EINVAL)?).ok_or(linux::EINVAL)?;
        let name = self.checked_name(name)?;
        // `linkat` has no descriptor-only form on macOS, so the source is named
        // by its live path — which is correct even if it has been renamed since
        // the guest looked it up.
        let source = self.path(&target)?;
        sys::link_at(libc::AT_FDCWD, &source, parent.reference()?.raw_fd(), &name)?;
        let entry = self.entry(&parent, &name)?;
        Ok(self.entry_reply(&entry))
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
        let fd = sys::reopen(inode.reference()?.raw_fd(), flags, 0)?;
        let file = Arc::new(OpenFile {
            fd,
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
        });
        self.open_cache.put_directory(nodeid, dir.clone());
        Ok(dir)
    }

    /// Reads a directory in full, from wherever it now lives.
    fn list(&self, nodeid: u64) -> Result<Vec<sys::DirEntry>, i32> {
        let inode = self.directory(nodeid)?;
        // 0o200000 is Linux's O_DIRECTORY; the translation layer maps it.
        let fd = sys::reopen(inode.reference()?.raw_fd(), 0o200000, 0)?;
        sys::Dir::from_fd(fd)?.read_all()
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
        let entry = self.entry_with_reference(&parent, st, || sys::dup(&fd))?;
        let fh = self.registry.add_handle(Handle::File(Arc::new(OpenFile {
            fd,
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
        let file = self.file_for(nodeid, fh, true)?;
        let written = if file.append {
            sys::write_append(file.fd.as_raw_fd(), data)?
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
        // A directory has nothing to sync: its metadata reached the host when
        // the operation that changed it returned.
        if self.inode(nodeid).is_ok_and(|inode| inode.is_dir) {
            return Ok(Vec::new());
        }
        let file = self.file_for(nodeid, fh, false)?;
        sys::fsync(file.fd.as_raw_fd(), datasync)?;
        Ok(Vec::new())
    }

    fn statfs(&self) -> Result<Vec<u8>, i32> {
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
        // A caller starting from the beginning is asking for a fresh view;
        // anyone resuming gets the list the first page came from, so the
        // offsets they were given still mean what they meant.
        if offset == 0 || entries.is_empty() {
            *entries = self.list(nodeid)?;
        }
        let parent = self.directory(nodeid)?;

        let mut out = Vec::new();
        for (index, entry) in entries.iter().enumerate().skip(offset) {
            let next = index as u64 + 1;
            if plus {
                // A READDIRPLUS record is an entry followed by a dirent, and
                // the entry has to be built before its size is known, so the
                // budget is checked against the pair up front.
                if out.len() + fuse::ENTRY_OUT_LEN + fuse::dirent_len(entry.name.len()) > budget {
                    break;
                }
                // "." and ".." carry nodeid 0, which the kernel reads as "no
                // entry to instantiate". Looking them up instead would take a
                // lookup reference on every directory a walk passes through,
                // and nothing would ever forget them.
                let looked_up = if entry.name == b"." || entry.name == b".." {
                    None
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
                break;
            }
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

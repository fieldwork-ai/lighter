//! What a `nodeid` and a `fh` mean on this side of the wire.
//!
//! The guest addresses files by two opaque 64-bit numbers it never
//! interprets: a `nodeid` naming an inode, and an `fh` naming an open handle.
//! This module is the only place either is minted or resolved.
//!
//! # Why an inode is a descriptor and not a path
//!
//! A remembered path is wrong the instant anything renames it, and "anything"
//! includes a `git checkout` on the host moving a directory the guest has open.
//! Holding a descriptor per inode makes the identity survive that: the
//! descriptor follows the file, not the name. It is also what makes
//! unlink-while-open behave — an inode with no remaining links still has a live
//! descriptor, so the guest's open file keeps working exactly as it would on a
//! local filesystem.
//!
//! # Lookup counts
//!
//! FUSE is explicit that the kernel owns the lifetime: every reply that carries
//! a `nodeid` increments a count the guest later decrements with FORGET, and
//! the server may only drop an inode when the count reaches zero. Getting this
//! wrong in the generous direction leaks descriptors until the process hits its
//! limit; getting it wrong in the other direction hands the guest a `nodeid`
//! that resolves to nothing, which surfaces as random `ESTALE` under load.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// The `nodeid` of a mount's root. Fixed by the protocol.
pub const ROOT_ID: u64 = 1;

/// One file, as long as the guest remembers it.
/// Where a parked inode can be found again, and how to be sure it is still
/// the same file.
#[derive(Clone)]
struct Parked {
    path: PathBuf,
    /// `(st_birthtime, st_birthtime_nsec)` at the moment of parking.
    birthtime: (i64, i64),
}

/// The bit that marks an inode number as provisional: handed out for a
/// pending create, reported to the guest until the real number exists. High
/// enough that no filesystem's real numbers reach it.
pub const PROVISIONAL_INO: u64 = 1 << 62;

pub struct Inode {
    /// A metadata-only descriptor, or nothing if it has been parked.
    ///
    /// Shared rather than owned so that parking is never a race: a reclaimer
    /// takes the `Arc` out of the slot, and the descriptor is closed by
    /// whichever thread lets go of it last. A worker mid-syscall keeps it
    /// alive without holding a lock for the duration.
    fd: RwLock<Option<Arc<OwnedFd>>>,
    /// Where the descriptor was, so it can be found again after parking.
    ///
    /// Only meaningful when the slot is empty, and only a hint even then:
    /// whether it still names this file is settled by `(dev, ino)` after the
    /// reopen, never assumed.
    parked_at: Mutex<Option<Parked>>,
    /// The reference bit of a clock: set on use, cleared as the reclaimer
    /// passes. An inode found with it clear is one nothing has touched since
    /// the last sweep.
    used: AtomicBool,
    /// Whether the slot currently holds a descriptor.
    ///
    /// A mirror of `fd.is_some()`, kept so the reclaimer can skip an inode
    /// without taking its lock — and, more to the point, so it skips parked
    /// ones at all. A parked inode is by definition not being used, so its
    /// reference bit stays clear forever and it is the *first* thing every
    /// sweep chooses. Without this it fills the batch, `park()` refuses it for
    /// having nothing to park, and the sweep frees nothing while reporting
    /// that it looked at sixteen thousand candidates.
    held: AtomicBool,
    /// The registry's running totals.
    ///
    /// Held here rather than adjusted by the registry because every edge that
    /// moves them is an inode's own: construction, parking, reviving a parked
    /// one, and being dropped. Splitting the accounting across the two is how
    /// the first version came to leak — parking decremented, reviving did not
    /// increment, and the tally drifted down until it stopped triggering a
    /// reclaim while the real descriptor count climbed to the kernel's ceiling.
    census: Arc<Census>,
    /// Host identity, which is what makes two paths to one file share a
    /// `nodeid` — as hard links must. Atomics because an inode born pending
    /// (acknowledged before its create has been applied) binds its real
    /// identity when the apply lands; for every other inode they are set at
    /// construction and never move.
    dev: std::sync::atomic::AtomicI64,
    ino: AtomicU64,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// How many times the guest has been told about this inode.
    lookups: Mutex<u64>,
    /// Writes acknowledged to the guest but not yet applied to the host file.
    ///
    /// Nonzero is what makes this inode "dirty": a read of its bytes must
    /// wait for the apply queue, and its reported size must take the overlay
    /// below into account.
    dirty: AtomicU32,
    /// The size the guest believes, while writes are still queued: the
    /// furthest end of any acknowledged write. Zero whenever `dirty` is zero,
    /// which is what lets `overlay_size` be an unconditional `max`.
    pending_size: AtomicU64,
    /// The errno of a queued write that failed, parked here for the next
    /// operation on this file to report — the same posture as the kernel's
    /// own writeback. Taken (cleared) when reported.
    write_error: Mutex<Option<i32>>,
    /// Where operations on this inode now belong: the inode a clone put in
    /// its place. The guest's open descriptor still names this one — FICLONE
    /// on Linux leaves the descriptor looking at the cloned bytes — so any
    /// request arriving here is answered by the replacement. Zero is none.
    forward: AtomicU64,
    /// A pending create the guest has since replaced — cloned over, renamed
    /// over, or unlinked — before its job ran. The job then does nothing:
    /// the file was never going to be observed, and skipping it is one APFS
    /// operation fewer on a queue that is the bottleneck.
    cancelled: AtomicBool,
    /// True from an acknowledged CREATE until the apply queue performs it.
    ///
    /// A pending inode has no descriptor and no host identity yet: `dev`/
    /// `ino` hold a provisional number, attribute replies come from
    /// `pending_meta`, and anything that genuinely needs the host file
    /// drains the queue first. `bind` ends the state.
    pending: AtomicBool,
    /// What the guest was told at creation, until the host file can answer.
    pending_meta: Mutex<Option<PendingMeta>>,
    /// Children promised inside this directory but not yet applied, by name.
    ///
    /// The overlay that keeps lookups truthful while creates are queued: a
    /// name in here resolves to its pending inode, never to ENOENT. Kept on
    /// the parent because that is who lookup asks.
    pending_children: Mutex<std::collections::HashMap<Vec<u8>, u64>>,
    /// Mirror of `pending_children.len()`, so the empty case — every
    /// directory, almost always — costs one load and no lock.
    pending_count: AtomicUsize,
    /// Names removed from this directory by acknowledged unlinks that have
    /// not applied yet: the host still lists them, the guest must not.
    pending_gone: Mutex<std::collections::HashSet<Vec<u8>>>,
    /// Mirror of `pending_gone.len()`, same reason as `pending_count`.
    gone_count: AtomicUsize,
    /// Queued operations that will change this inode's *metadata* — an
    /// unlink of one of its names changes the link count — counted so a
    /// GETATTR knows a host stat would be stale.
    meta_shadow: AtomicU32,
    /// The apply-queue sequence number of the last job that touches this
    /// inode: its own create or writes, or — for a directory — the naming
    /// operations inside it. A barrier waits to here and no further.
    settle_seq: AtomicU64,
    /// Our own nodeid, once the registry has issued it: what a queued job
    /// names to be ordered against the other jobs on this inode.
    id: AtomicU64,
    /// Links acknowledged to this inode and not yet made: what readers add
    /// to the host's link count.
    extra_links: AtomicU32,
    /// Setattrs acknowledged but not yet applied, and what they promised.
    attr_pending: AtomicU32,
    attr_override: Mutex<AttrOverride>,
    /// The batch an unapplied setattr job will read when it runs, and the
    /// job's sequence number: a later setattr can merge into it rather than
    /// queue a job of its own — as long as no write has been acknowledged
    /// since, because a write moves mtime and the batch must land after it.
    attr_batch: Mutex<Option<AttrBatch>>,
    last_write_seq: AtomicU64,
}

/// What a pending inode will be once its job lands.
#[derive(Clone, Copy)]
pub enum PendingKind {
    File,
    Directory,
    Symlink,
}

/// The attributes a pending file was promised at creation, and any it has
/// been promised since by a queued setattr.
#[derive(Clone, Copy)]
pub struct PendingMeta {
    pub mode: u32,
    /// Seconds and nanoseconds since the epoch, captured at acknowledgement.
    pub born: (i64, i64),
    pub atime: (i64, i64),
    pub mtime: (i64, i64),
}

/// A setattr job's batch, open to merges until the job takes it: the job's
/// sequence number once it has one, and the values it will apply.
type AttrBatch = (Option<u64>, Arc<Mutex<AttrOverride>>);

/// A snapshot of an inode's promises; see [`Inode::overlay`].
#[derive(Clone, Copy, Default)]
pub struct Overlay {
    pub size: Option<u64>,
    pub attrs: Option<AttrOverride>,
    /// Links promised and not yet made.
    pub links: u32,
}

impl Overlay {
    pub fn is_empty(&self) -> bool {
        self.size.is_none() && self.attrs.is_none() && self.links == 0
    }
}

/// Attributes a bound inode has been promised by setattrs still on the
/// queue. Reads must show them; the host stat lags until the jobs land.
#[derive(Clone, Copy, Default)]
pub struct AttrOverride {
    pub mode: Option<u32>,
    pub atime: Option<(i64, i64)>,
    pub mtime: Option<(i64, i64)>,
}

/// What a share is holding, counted where it changes.
#[derive(Debug, Default)]
pub struct Census {
    /// Open metadata descriptors. The number the budget is about.
    descriptors: AtomicUsize,
    /// Of those, directories. See [`Inode::parkable`].
    resident_dirs: AtomicUsize,
    /// Live `Inode` values, as against how many the table lists. The two
    /// disagreeing means something is holding `Arc`s the table has already
    /// forgotten, which from the outside looks exactly like a descriptor leak
    /// — so it is worth being able to tell them apart.
    inodes: AtomicUsize,
    /// The registry's descriptor budget, mirrored here so an inode reviving
    /// itself can ask whether there is room without a pointer back to the
    /// registry. Zero until the registry sets it.
    budget: AtomicUsize,
}

impl Census {
    fn descriptors(&self) -> usize {
        self.descriptors.load(Ordering::Relaxed)
    }

    fn inodes(&self) -> usize {
        self.inodes.load(Ordering::Relaxed)
    }
}

impl Drop for Inode {
    fn drop(&mut self) {
        self.census.inodes.fetch_sub(1, Ordering::Relaxed);
        if self.fd.get_mut().expect("inode slot poisoned").is_some() {
            self.census.descriptors.fetch_sub(1, Ordering::Relaxed);
            if self.is_dir {
                self.census.resident_dirs.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// A descriptor borrowed from an inode for the length of one operation.
///
/// Holding it keeps the descriptor open even if the reclaimer parks the inode
/// underneath — which is the whole point, because otherwise every call site
/// would need to hold a lock across its syscall.
pub struct Reference(Arc<OwnedFd>);

impl Reference {
    pub fn raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl Inode {
    fn new(
        fd: OwnedFd,
        dev: i64,
        ino: u64,
        is_dir: bool,
        is_symlink: bool,
        lookups: u64,
        census: Arc<Census>,
    ) -> Inode {
        census.descriptors.fetch_add(1, Ordering::Relaxed);
        census.inodes.fetch_add(1, Ordering::Relaxed);
        if is_dir {
            census.resident_dirs.fetch_add(1, Ordering::Relaxed);
        }
        Inode {
            fd: RwLock::new(Some(Arc::new(fd))),
            parked_at: Mutex::new(None),
            used: AtomicBool::new(true),
            held: AtomicBool::new(true),
            census,
            dev: std::sync::atomic::AtomicI64::new(dev),
            ino: AtomicU64::new(ino),
            is_dir,
            is_symlink,
            lookups: Mutex::new(lookups),
            dirty: AtomicU32::new(0),
            pending_size: AtomicU64::new(0),
            write_error: Mutex::new(None),
            forward: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            pending_meta: Mutex::new(None),
            pending_children: Mutex::new(std::collections::HashMap::new()),
            pending_count: AtomicUsize::new(0),
            pending_gone: Mutex::new(std::collections::HashSet::new()),
            gone_count: AtomicUsize::new(0),
            meta_shadow: AtomicU32::new(0),
            settle_seq: AtomicU64::new(0),
            id: AtomicU64::new(0),
            extra_links: AtomicU32::new(0),
            attr_pending: AtomicU32::new(0),
            attr_override: Mutex::new(AttrOverride::default()),
            attr_batch: Mutex::new(None),
            last_write_seq: AtomicU64::new(0),
        }
    }

    /// An inode acknowledged before it exists: no descriptor, provisional
    /// identity, attributes served from `meta` until [`Registry::bind_pending`].
    fn new_pending(
        dev: i64,
        ino: u64,
        meta: PendingMeta,
        kind: PendingKind,
        census: Arc<Census>,
    ) -> Inode {
        census.inodes.fetch_add(1, Ordering::Relaxed);
        Inode {
            fd: RwLock::new(None),
            parked_at: Mutex::new(None),
            used: AtomicBool::new(true),
            held: AtomicBool::new(false),
            census,
            dev: std::sync::atomic::AtomicI64::new(dev),
            ino: AtomicU64::new(ino),
            is_dir: matches!(kind, PendingKind::Directory),
            is_symlink: matches!(kind, PendingKind::Symlink),
            lookups: Mutex::new(1),
            dirty: AtomicU32::new(0),
            pending_size: AtomicU64::new(0),
            write_error: Mutex::new(None),
            forward: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            pending: AtomicBool::new(true),
            pending_meta: Mutex::new(Some(meta)),
            pending_children: Mutex::new(std::collections::HashMap::new()),
            pending_count: AtomicUsize::new(0),
            pending_gone: Mutex::new(std::collections::HashSet::new()),
            gone_count: AtomicUsize::new(0),
            meta_shadow: AtomicU32::new(0),
            settle_seq: AtomicU64::new(0),
            id: AtomicU64::new(0),
            extra_links: AtomicU32::new(0),
            attr_pending: AtomicU32::new(0),
            attr_override: Mutex::new(AttrOverride::default()),
            attr_batch: Mutex::new(None),
            last_write_seq: AtomicU64::new(0),
        }
    }

    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }

    /// Withdraws a pending create whose file the guest has already replaced.
    /// Only meaningful while still pending; returns whether it was.
    pub fn cancel_pending(&self) -> bool {
        if !self.pending.load(Ordering::Relaxed) {
            return false;
        }
        self.cancelled.store(true, Ordering::Relaxed);
        true
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Redirects everything that arrives at this inode to `nodeid`.
    pub fn forward_to(&self, nodeid: u64) {
        self.forward.store(nodeid, Ordering::Relaxed);
    }

    pub fn forwarded(&self) -> Option<u64> {
        match self.forward.load(Ordering::Relaxed) {
            0 => None,
            id => Some(id),
        }
    }

    /// A held write moved the promised modification time to now.
    pub fn note_write_time(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        if let Some(meta) = self
            .pending_meta
            .lock()
            .expect("pending meta poisoned")
            .as_mut()
        {
            meta.mtime = (now.as_secs() as i64, now.subsec_nanos() as i64);
        }
    }

    pub fn pending_meta(&self) -> Option<PendingMeta> {
        *self.pending_meta.lock().expect("pending meta poisoned")
    }

    /// The apply queue performed the create: install the descriptor and the
    /// real identity, and let the host file answer from here on.
    fn bind(&self, fd: OwnedFd, dev: i64, ino: u64) {
        self.dev.store(dev, Ordering::Relaxed);
        self.ino.store(ino, Ordering::Relaxed);
        *self.fd.write().expect("inode slot poisoned") = Some(Arc::new(fd));
        self.census.descriptors.fetch_add(1, Ordering::Relaxed);
        if self.is_dir {
            self.census.resident_dirs.fetch_add(1, Ordering::Relaxed);
        }
        self.held.store(true, Ordering::Relaxed);
        *self.pending_meta.lock().expect("pending meta poisoned") = None;
        self.pending.store(false, Ordering::Relaxed);
    }

    /// Binds a real identity with no descriptor: the inode is born parked,
    /// at its identity path, and opens itself on first use.
    ///
    /// For a file the guest will most likely never touch again — pnpm's
    /// sixty thousand imports an install — the descriptor the bound path
    /// opens is one the sweep closes moments later, at a full share
    /// immediately: an open, a stat, a path query and a close, all for
    /// nothing. Parked from birth, the file costs the clone and one stat.
    fn bind_parked(&self, dev: i64, ino: u64, birthtime: (i64, i64)) {
        self.dev.store(dev, Ordering::Relaxed);
        self.ino.store(ino, Ordering::Relaxed);
        *self.parked_at.lock().expect("parked path poisoned") = Some(Parked {
            path: PathBuf::from(format!("/.vol/{dev}/{ino}")),
            birthtime,
        });
        *self.pending_meta.lock().expect("pending meta poisoned") = None;
        self.pending.store(false, Ordering::Relaxed);
    }

    /// The create itself failed; the file will never exist. The errno parks
    /// where the next operation on this inode reports it.
    pub fn bind_failed(&self, errno: i32) {
        self.write_error
            .lock()
            .expect("write error poisoned")
            .get_or_insert(errno);
        // Pending ends here even in failure — a barrier loops while the flag
        // is up, and a corpse that stays "pending" forever would hold it up
        // forever. With no descriptor and no parked path, `reference` answers
        // ESTALE, which is the truth about this file.
        self.pending.store(false, Ordering::Relaxed);
    }

    /// Promises `name` inside this directory to the pending inode `nodeid`.
    pub fn add_pending_child(&self, name: &[u8], nodeid: u64) {
        let mut children = self
            .pending_children
            .lock()
            .expect("pending children poisoned");
        children.insert(name.to_vec(), nodeid);
        self.pending_count.store(children.len(), Ordering::Relaxed);
    }

    /// The promise is settled (kept or failed); the host directory answers
    /// now — unless a later promise has taken the name, in which case that
    /// one is the truth and this removal must not erase it.
    pub fn remove_pending_child(&self, name: &[u8], nodeid: u64) {
        let mut children = self
            .pending_children
            .lock()
            .expect("pending children poisoned");
        if children.get(name) == Some(&nodeid) {
            children.remove(name);
        }
        self.pending_count.store(children.len(), Ordering::Relaxed);
    }

    /// The pending inode `name` resolves to, if it is promised here.
    pub fn pending_child(&self, name: &[u8]) -> Option<u64> {
        if self.pending_count.load(Ordering::Relaxed) == 0 {
            return None;
        }
        self.pending_children
            .lock()
            .expect("pending children poisoned")
            .get(name)
            .copied()
    }

    pub fn has_pending_children(&self) -> bool {
        self.pending_count.load(Ordering::Relaxed) != 0
    }

    /// Promises that `name` is gone from this directory.
    pub fn add_pending_gone(&self, name: &[u8]) {
        let mut gone = self.pending_gone.lock().expect("pending gone poisoned");
        gone.insert(name.to_vec());
        self.gone_count.store(gone.len(), Ordering::Relaxed);
    }

    /// The unlink applied (or failed, loudly); the host answers again.
    pub fn remove_pending_gone(&self, name: &[u8]) {
        let mut gone = self.pending_gone.lock().expect("pending gone poisoned");
        gone.remove(name);
        self.gone_count.store(gone.len(), Ordering::Relaxed);
    }

    /// Whether `name` has been promised away.
    pub fn name_pending_gone(&self, name: &[u8]) -> bool {
        if self.gone_count.load(Ordering::Relaxed) == 0 {
            return false;
        }
        self.pending_gone
            .lock()
            .expect("pending gone poisoned")
            .contains(name)
    }

    /// A write job with this sequence number was queued: any setattr batch
    /// opened before it must not absorb later setattrs.
    pub fn note_write(&self, seq: u64) {
        self.last_write_seq.fetch_max(seq, Ordering::Relaxed);
    }

    /// Merges a setattr into the batch of a job still waiting to run, if
    /// there is one and no write has been queued since it was opened.
    /// Returns whether it merged; if not the caller queues a job and opens a
    /// batch with [`Inode::open_attr_batch`].
    pub fn merge_attr(&self, change: AttrOverride) -> bool {
        let batch = self.attr_batch.lock().expect("attr batch poisoned");
        let Some((seq, values)) = batch.as_ref() else {
            return false;
        };
        // A batch queued before a write must not absorb a change meant to
        // land after it — the write would clobber the time. A batch not yet
        // stamped is newer than any write acknowledged so far.
        if seq.is_some_and(|seq| seq <= self.last_write_seq.load(Ordering::Relaxed)) {
            return false;
        }
        let mut values = values.lock().expect("attr batch poisoned");
        if change.mode.is_some() {
            values.mode = change.mode;
        }
        if change.atime.is_some() {
            values.atime = change.atime;
        }
        if change.mtime.is_some() {
            values.mtime = change.mtime;
        }
        true
    }

    /// Opens the batch a setattr job about to be queued will read.
    ///
    /// Opened BEFORE the job is pushed, and identified by the batch itself
    /// rather than by the job's sequence number, because the number does
    /// not exist until the push returns — and on an empty queue the job can
    /// run before it does. Opened after, the job had already come and gone,
    /// the batch stayed open with nothing behind it, and the next chmod
    /// merged into it and was never applied: mode 600 after a chmod to 755,
    /// once in six boots.
    pub fn open_attr_batch(&self, values: Arc<Mutex<AttrOverride>>) {
        *self.attr_batch.lock().expect("attr batch poisoned") = Some((None, values));
    }

    /// The job for `values` has its sequence number now.
    pub fn stamp_attr_batch(&self, seq: u64, values: &Arc<Mutex<AttrOverride>>) {
        let mut batch = self.attr_batch.lock().expect("attr batch poisoned");
        if let Some((open_seq, open)) = batch.as_mut()
            && Arc::ptr_eq(open, values)
        {
            *open_seq = Some(seq);
        }
    }

    /// The job for `values` is about to run: its batch is closed to merges
    /// from here on, and its contents are what to apply. `None` when a newer
    /// batch has replaced it, in which case the job applies its own copy.
    pub fn take_attr_batch(&self, values: &Arc<Mutex<AttrOverride>>) -> Option<AttrOverride> {
        let mut batch = self.attr_batch.lock().expect("attr batch poisoned");
        match batch.as_ref() {
            Some((_, open)) if Arc::ptr_eq(open, values) => {
                let taken = *values.lock().expect("attr batch poisoned");
                *batch = None;
                Some(taken)
            }
            _ => None,
        }
    }

    /// A setattr has been acknowledged: what it promised is the truth until
    /// the job lands. A pending inode takes it into its meta; a bound one
    /// into an override the readers apply over the host stat.
    pub fn attr_acked(&self, change: AttrOverride) {
        self.attr_pending.fetch_add(1, Ordering::Relaxed);
        let mut meta = self.pending_meta.lock().expect("pending meta poisoned");
        if let Some(meta) = meta.as_mut() {
            if let Some(mode) = change.mode {
                meta.mode = (meta.mode & !0o7777) | (mode & 0o7777);
            }
            if let Some(atime) = change.atime {
                meta.atime = atime;
            }
            if let Some(mtime) = change.mtime {
                meta.mtime = mtime;
            }
        }
        let mut current = self.attr_override.lock().expect("attr override poisoned");
        if change.mode.is_some() {
            current.mode = change.mode;
        }
        if change.atime.is_some() {
            current.atime = change.atime;
        }
        if change.mtime.is_some() {
            current.mtime = change.mtime;
        }
    }

    /// The setattr was folded into a job already queued: one job, one
    /// decrement, so the count it added is taken back here.
    pub fn attr_merged(&self) {
        self.attr_pending.fetch_sub(1, Ordering::Relaxed);
    }

    /// A queued setattr landed; on the last one the host answers for itself.
    pub fn attr_applied(&self, result: Result<(), i32>) {
        if let Err(errno) = result {
            self.write_error
                .lock()
                .expect("write error poisoned")
                .get_or_insert(errno);
        }
        if self.attr_pending.fetch_sub(1, Ordering::Relaxed) == 1 {
            *self.attr_override.lock().expect("attr override poisoned") = AttrOverride::default();
        }
    }

    /// What a reader must lay over the host stat, if anything.
    /// What the guest has been promised about this file that the host may
    /// not show yet — the size its queued writes reach, the mode and times
    /// its queued setattrs set. Taken BEFORE the host is consulted, never
    /// after: `Server::attr_of` says why.
    pub fn overlay(&self) -> Overlay {
        // The dirty count first, the size second: a write applying between
        // the two clears both, and a `Some(0)` laid over the host's size is
        // the host's size — which is then the truth.
        let size = if self.is_dirty() {
            Some(self.pending_size.load(Ordering::Relaxed))
        } else {
            None
        };
        Overlay {
            size,
            attrs: self.attr_override(),
            links: self.extra_links(),
        }
    }

    pub fn attr_override(&self) -> Option<AttrOverride> {
        if self.attr_pending.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some(*self.attr_override.lock().expect("attr override poisoned"))
    }

    pub fn has_pending_attrs(&self) -> bool {
        self.attr_pending.load(Ordering::Relaxed) != 0
    }

    /// Remembers that the job with this sequence number touches this inode.
    pub fn settled_by(&self, seq: u64) {
        self.settle_seq.fetch_max(seq, Ordering::Relaxed);
    }

    /// The sequence a barrier on this inode must wait to.
    /// A link to this inode has been acknowledged and is queued.
    pub fn link_acked(&self) {
        self.extra_links.fetch_add(1, Ordering::Relaxed);
    }

    /// A queued link landed, or failed; either way the host answers now.
    pub fn link_applied(&self) {
        self.extra_links.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn extra_links(&self) -> u32 {
        self.extra_links.load(Ordering::Relaxed)
    }

    /// The nodeid the registry issued for this inode; see [`Registry`].
    pub fn id(&self) -> u64 {
        self.id.load(Ordering::Relaxed)
    }

    pub fn settle_seq(&self) -> u64 {
        self.settle_seq.load(Ordering::Relaxed)
    }

    /// A queued operation will change this inode's metadata when it applies.
    pub fn shadow_meta(&self) {
        self.meta_shadow.fetch_add(1, Ordering::Relaxed);
    }

    pub fn unshadow_meta(&self) {
        self.meta_shadow.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn meta_shadowed(&self) -> bool {
        self.meta_shadow.load(Ordering::Relaxed) != 0
    }

    /// The names promised into this directory, with their inodes.
    pub fn pending_children_snapshot(&self) -> Vec<(Vec<u8>, u64)> {
        if self.pending_count.load(Ordering::Relaxed) == 0 {
            return Vec::new();
        }
        self.pending_children
            .lock()
            .expect("pending children poisoned")
            .iter()
            .map(|(name, id)| (name.clone(), *id))
            .collect()
    }

    /// The names promised away from this directory.
    pub fn pending_gone_snapshot(&self) -> std::collections::HashSet<Vec<u8>> {
        if self.gone_count.load(Ordering::Relaxed) == 0 {
            return std::collections::HashSet::new();
        }
        self.pending_gone
            .lock()
            .expect("pending gone poisoned")
            .clone()
    }

    /// Whether any queued operation still shadows this directory's listing.
    pub fn listing_shadowed(&self) -> bool {
        self.pending_count.load(Ordering::Relaxed) != 0
            || self.gone_count.load(Ordering::Relaxed) != 0
    }

    /// A write to this file has been acknowledged and queued; `end` is where
    /// it will finish. Called before the job is pushed, which is what makes
    /// the "reset on last applied" in `write_applied` race-free.
    pub fn write_acked(&self, end: u64) {
        self.dirty.fetch_add(1, Ordering::Relaxed);
        self.pending_size.fetch_max(end, Ordering::Relaxed);
    }

    /// A queued write finished. On the last one the overlay resets: the host
    /// file now answers for itself.
    pub fn write_applied(&self, result: Result<(), i32>) {
        if let Err(errno) = result {
            let mut slot = self.write_error.lock().expect("write error poisoned");
            // The first failure is the story; later ones are usually its echo.
            slot.get_or_insert(errno);
        }
        if self.dirty.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.pending_size.store(0, Ordering::Relaxed);
        }
    }

    /// Whether acknowledged writes are still queued.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed) != 0
    }

    /// The size the guest should be told, given what the host file says.
    pub fn overlay_size(&self, host: u64) -> u64 {
        host.max(self.pending_size.load(Ordering::Relaxed))
    }

    /// Reports and clears a parked write failure, if one is waiting.
    pub fn take_write_error(&self) -> Option<i32> {
        self.write_error
            .lock()
            .expect("write error poisoned")
            .take()
    }

    pub fn dev(&self) -> i64 {
        self.dev.load(Ordering::Relaxed)
    }

    pub fn ino(&self) -> u64 {
        self.ino.load(Ordering::Relaxed)
    }

    /// Borrows the descriptor, reopening it first if it was parked.
    ///
    /// The reopen is where a parked inode discovers it has been overtaken: the
    /// path is reopened and the result checked against `(dev, ino)`, so a name
    /// that now belongs to a different file produces `ESTALE` rather than
    /// silent work on the wrong one.
    pub fn reference(&self) -> Result<Reference, i32> {
        self.used.store(true, Ordering::Relaxed);
        if let Some(fd) = self.fd.read().expect("inode slot poisoned").as_ref() {
            return Ok(Reference(fd.clone()));
        }
        let mut slot = self.fd.write().expect("inode slot poisoned");
        // Another thread may have revived it between the two locks.
        if let Some(fd) = slot.as_ref() {
            return Ok(Reference(fd.clone()));
        }
        let parked = self
            .parked_at
            .lock()
            .expect("parked path poisoned")
            .clone()
            .ok_or(crate::errno::linux::ESTALE)?;
        let fd = Arc::new(self.reopen(&parked)?);
        // Reviving into a full share is how a working set larger than the
        // budget turns into thrash: every revival forces the sweep to park
        // something else, which the next operation revives in turn — cache
        // churn plus sweep scanning, on every request. With no room, the
        // descriptor serves this one operation and is dropped with it; the
        // inode stays parked and nothing else is evicted to make space.
        let budget = self.census.budget.load(Ordering::Relaxed);
        if budget > 0
            && self.census.descriptors.load(Ordering::Relaxed) >= budget
            && self.parkable()
        {
            return Ok(Reference(fd));
        }
        self.held.store(true, Ordering::Relaxed);
        self.census.descriptors.fetch_add(1, Ordering::Relaxed);
        if self.is_dir {
            self.census.resident_dirs.fetch_add(1, Ordering::Relaxed);
        }
        *slot = Some(fd.clone());
        Ok(Reference(fd))
    }

    /// Reopens a parked inode: by its remembered path, and failing that by
    /// its identity.
    ///
    /// The path is tried first and checked against `(dev, ino)`, so a name
    /// that now belongs to a different file cannot be mistaken for ours. When
    /// the path no longer answers — the file was renamed while parked — the
    /// volume can still open the inode itself, via macOS's `/.vol/dev/ino`
    /// namespace. That door needs a stronger check than the numbers: APFS
    /// recycles inode numbers briskly, and `/.vol` would open the recycled
    /// number's new owner with a matching `(dev, ino)` by construction. Birth
    /// time is the tiebreak — it is immutable for the life of a file, so the
    /// same numbers with the same birth time is the same file, and anything
    /// else is `ESTALE`.
    fn reopen(&self, parked: &Parked) -> Result<std::os::fd::OwnedFd, i32> {
        if let Ok(fd) = crate::sys::open_reference_path(&parked.path, self.is_symlink) {
            match crate::sys::stat_fd(fd.as_raw_fd()) {
                Ok(st) if st.st_ino == self.ino() && st.st_dev as i64 == self.dev() => {
                    return Ok(fd);
                }
                _ => {}
            }
        }
        let vol = PathBuf::from(format!("/.vol/{}/{}", self.dev(), self.ino()));
        let fd = crate::sys::open_reference_path(&vol, self.is_symlink)
            .map_err(|_| crate::errno::linux::ESTALE)?;
        match crate::sys::stat_fd(fd.as_raw_fd()) {
            Ok(st)
                if st.st_ino == self.ino()
                    && st.st_dev as i64 == self.dev()
                    && (st.st_birthtime, st.st_birthtime_nsec) == parked.birthtime =>
            {
                Ok(fd)
            }
            _ => Err(crate::errno::linux::ESTALE),
        }
    }

    /// Reopens a parked inode and installs the descriptor, making it
    /// resident. The sweep's other half: parking makes room, promotion
    /// spends it on something the guest has touched since the hand last
    /// came round.
    fn promote(&self) -> bool {
        let mut slot = self.fd.write().expect("inode slot poisoned");
        if slot.is_some() {
            return false;
        }
        let Some(parked) = self.parked_at.lock().expect("parked path poisoned").clone() else {
            return false;
        };
        let Ok(fd) = self.reopen(&parked) else {
            return false;
        };
        self.held.store(true, Ordering::Relaxed);
        self.census.descriptors.fetch_add(1, Ordering::Relaxed);
        if self.is_dir {
            self.census.resident_dirs.fetch_add(1, Ordering::Relaxed);
        }
        *slot = Some(Arc::new(fd));
        true
    }

    /// Closes the descriptor, remembering where it was.
    ///
    /// Refuses for anything that could not be found again: a file with no
    /// remaining links has no path, and its descriptor is the only way the
    /// guest's open handle on it keeps working.
    fn park(&self) -> bool {
        let Some(fd) = self.fd.read().expect("inode slot poisoned").clone() else {
            return false;
        };
        let birthtime = match crate::sys::stat_fd(fd.as_raw_fd()) {
            Ok(st) if st.st_nlink > 0 => (st.st_birthtime, st.st_birthtime_nsec),
            _ => return false,
        };
        let Ok(path) = crate::sys::path_of(fd.as_raw_fd()) else {
            return false;
        };
        drop(fd);
        let mut slot = self.fd.write().expect("inode slot poisoned");
        if slot.is_none() {
            return false;
        }
        *self.parked_at.lock().expect("parked path poisoned") = Some(Parked { path, birthtime });
        *slot = None;
        self.held.store(false, Ordering::Relaxed);
        self.census.descriptors.fetch_sub(1, Ordering::Relaxed);
        if self.is_dir {
            self.census.resident_dirs.fetch_sub(1, Ordering::Relaxed);
        }
        true
    }

    /// Whether the polite sweep may park this inode.
    ///
    /// A parked file costs nothing until that file is touched again. A parked
    /// directory taxes every name inside it: each lookup revives it by path —
    /// an `open` against the host, at a full share transiently, so every
    /// lookup — and a package install is nothing but lookups into the
    /// directories it just made. Profiled mid-install, that revival was the
    /// single hottest frame on the request thread. So directories stay
    /// resident while they are less than half the budget, which on a stock
    /// Mac is three thousand of them; only past that do they compete with
    /// files, and the forced phase, which exists for the case where nothing
    /// else works, ignores this entirely.
    fn parkable(&self) -> bool {
        if !self.is_dir {
            return true;
        }
        let budget = self.census.budget.load(Ordering::Relaxed);
        // The privilege is for the common case, not a right. Past the slack
        // the sweep is already in the regime where it would otherwise force,
        // and a privilege that holds there starves it: pnpm's tree is
        // directories most of the way down, and with them exempt a pass
        // examined eight thousand inodes and parked none while every request
        // swept. Under pressure a directory is an ordinary candidate.
        let slack = (budget / SWEEP_SLACK_DIVISOR).max(64);
        self.census.descriptors.load(Ordering::Relaxed) > budget + slack
            || self.census.resident_dirs.load(Ordering::Relaxed) > budget / 2
    }

    /// Whether this inode is still the file those numbers name.
    ///
    /// It is not enough to have matched on `(dev, ino)` once. APFS reuses inode
    /// numbers, and briskly: a package install deletes tens of thousands of
    /// temporary files and the numbers come straight back. A map keyed on those
    /// numbers alone will therefore hand out the *old* file's nodeid for a
    /// brand-new file — and since the guest keys its own inode cache on the
    /// nodeid, two unrelated files become one file inside the VM. That is not a
    /// performance bug; `cp -a` of a package tree fails outright because it
    /// concludes half the files are hard links to each other, and anything that
    /// did not check would read the wrong contents.
    ///
    /// The descriptor is what settles it: it still points at whatever it was
    /// opened on, so if that file has been unlinked its link count is zero, and
    /// if the number now belongs to something else the numbers no longer match.
    fn still_is(&self, dev: i64, ino: u64) -> bool {
        let Ok(reference) = self.reference() else {
            return false;
        };
        match crate::sys::stat_fd(reference.raw_fd()) {
            Ok(st) => st.st_nlink > 0 && st.st_ino == ino && st.st_dev as i64 == dev,
            Err(_) => false,
        }
    }
}

/// An open file or directory, named by an `fh`.
pub enum Handle {
    File(Arc<OpenFile>),
}

/// A directory's contents, as one immutable list.
///
/// Not a stream. A FUSE readdir offset has to mean the same thing to whoever
/// presents it, whenever they present it, and the only offset with that
/// property is an index into a list that does not move — see the note on
/// [`crate::sys::Dir`] for what the alternative does to `cp -a`.
///
/// Reading the whole directory up front also happens to be faster: one pass
/// instead of a seek per page, and every page after the first is free.
pub struct OpenDir {
    /// Which directory this lists. READDIRPLUS has to look each entry up under
    /// the right parent, and a list of names cannot be asked where it came
    /// from.
    pub nodeid: u64,
    /// Refilled whenever a caller starts again from the beginning, which is
    /// what makes a listing see changes at all.
    pub entries: Mutex<Vec<crate::sys::DirEntry>>,
}

/// An open regular file.
pub struct OpenFile {
    pub fd: OwnedFd,
    /// Whether the descriptor can be read through. False only for a guest
    /// write-only open kept for the apply queue; a read must not be handed
    /// a descriptor that will EBADF.
    pub readable: bool,
    /// Whether the descriptor can be written through.
    ///
    /// Only meaningful for the descriptors the server opens on its own
    /// initiative, once the guest has stopped telling it about opens: those
    /// start read-only and are upgraded on the first write, because most files
    /// are only ever read and a read-write open of a read-only file fails.
    pub writable: bool,
    /// Opened `O_APPEND` on the host.
    ///
    /// Worth a field of its own because it changes which syscall writes it:
    /// POSIX has `pwrite` ignore its offset on an append-mode descriptor, so
    /// the offset FUSE supplies would be silently discarded and two writers
    /// would still be safe — but a *non*-append handle must use `pwrite`, and
    /// telling them apart needs this.
    pub append: bool,
}

impl Handle {
    pub fn file(&self) -> Option<Arc<OpenFile>> {
        match self {
            Handle::File(file) => Some(file.clone()),
        }
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        match self {
            Handle::File(file) => Some(file.fd.as_raw_fd()),
        }
    }
}

/// How many independent locks the tables are split across.
///
/// Sixteen workers all take these on every request, and one mutex per table
/// makes the whole server as parallel as one thread. Measured on a package
/// install, an uncontended `RELEASE` — a map removal and a `close` — was taking
/// twenty microseconds, which is an order of magnitude more than the work in
/// it. Sharding is the cheapest fix that does not change the interface.
const SHARDS: usize = 16;

/// Which identity shard a host inode number belongs to.
///
/// Keyed on the inode rather than the pair, because the device is the same for
/// every file on a share and mixing it in would only cost an instruction. APFS
/// hands out inode numbers sequentially, so the low bits spread on their own.
fn identity_shard(ino: u64) -> usize {
    (ino as usize) % SHARDS
}

fn shard(key: u64) -> usize {
    // The keys are consecutive counters, so the low bits are already uniform
    // and a hash would be pure cost.
    (key as usize) % SHARDS
}

/// Every inode and open handle the guest currently holds.
pub struct Registry {
    /// Sharded by `nodeid`.
    by_id: [Mutex<HashMap<u64, Arc<Inode>>>; SHARDS],
    /// Host identity to `nodeid`, so a second path to the same file resolves to
    /// the same inode rather than a second one with its own descriptor.
    ///
    /// Sharded, and it has to be. `insert` holds the write side across an
    /// `fstat` — it must, because deciding whether an existing entry still
    /// names this file is what that syscall is for — so one lock here is one
    /// lock every thread creating a file queues behind. Measured on a package
    /// install, a create costs 26 microseconds on one thread and 39 under
    /// sixteen, and the difference is this.
    by_identity: [RwLock<HashMap<(i64, u64), u64>>; SHARDS],
    /// Sharded by `fh`.
    handles: [Mutex<HashMap<u64, Arc<Handle>>>; SHARDS],
    next_id: AtomicU64,
    next_handle: AtomicU64,
    /// How many metadata descriptors the inode table is holding open.
    ///
    /// Shared with every inode, which is the only arrangement that stays
    /// honest: each of the four edges that moves it — construction, parking,
    /// reviving, dropping — belongs to an inode and adjusts it there.
    census: Arc<Census>,
    /// The most it may hold before parking the cold ones.
    budget: usize,
    /// Which shard the hand is on, so successive sweeps cover all of them.
    sweep: AtomicUsize,
    /// When the descriptor pressure was last reported.
    reported: Mutex<std::time::Instant>,
    /// Every live nodeid, in the order the hand will consider it.
    ///
    /// A clock needs somewhere to point, and a hash map is the wrong shape for
    /// it: there is no cursor into one that survives an insert, so every sweep
    /// has to start at the beginning and read its way in. That is fine for a
    /// few thousand inodes and quadratic for a few hundred thousand.
    ///
    /// So the hand is its own queue. An id is pushed when its inode is made,
    /// popped when the hand reaches it, and pushed back behind everything
    /// else. An id whose inode the guest has forgotten is simply not put back,
    /// which is the whole of the cleanup.
    hands: [Mutex<VecDeque<u64>>; SHARDS],
    /// The count at which a full pass last found nothing to park.
    ///
    /// Hysteresis, and the difference between degrading and collapsing. A
    /// share whose whole working set is hot has nothing to give, and asking it
    /// again on the very next insert costs a sweep of every shard for the same
    /// answer. So after a fruitless pass the question is not re-asked until the
    /// count has grown a little further — and only while the drift is still
    /// inside the slack, because past that a wasted sweep is cheaper than
    /// EMFILE.
    quiet_until: AtomicUsize,
    /// Whether a thread is already in the forced phase of a sweep, so the
    /// others go back to work instead of queueing up to park the same inodes.
    forcing: AtomicBool,
    /// When the registry was built, so times can be atomics of elapsed millis.
    born: std::time::Instant,
    /// Elapsed millis at the last over-budget sweep.
    last_sweep: AtomicUsize,
    /// The count above which a sweep may no longer be deferred.
    ///
    /// Between the budget and this line, sweeps are rate-limited and the count
    /// breathes: a working set genuinely hotter and larger than the budget is
    /// a fact, and holding the line by sweeping on every request turns a
    /// ten-second install into ten minutes of parking the same inodes. Past
    /// the line, the ceiling is near and EMFILE is worse than any stall.
    red_line: usize,
}

impl Registry {
    /// Builds a registry whose root is `root_fd`.
    pub fn new(root_fd: OwnedFd, dev: i64, ino: u64) -> Registry {
        // The root is never forgotten: the kernel does not FORGET nodeid 1, and
        // a count that could reach zero would let a buggy guest drop it and
        // take the whole mount with it.
        let census = Arc::new(Census::default());
        census.budget.store(descriptor_budget(), Ordering::Relaxed);
        let root = Arc::new(Inode::new(
            root_fd,
            dev,
            ino,
            true,
            false,
            u64::MAX,
            census.clone(),
        ));
        let by_id: [Mutex<HashMap<u64, Arc<Inode>>>; SHARDS] =
            std::array::from_fn(|_| Mutex::new(HashMap::new()));
        root.id.store(ROOT_ID, Ordering::Relaxed);
        by_id[shard(ROOT_ID)]
            .lock()
            .expect("inode table poisoned")
            .insert(ROOT_ID, root);
        let by_identity: [RwLock<HashMap<(i64, u64), u64>>; SHARDS] =
            std::array::from_fn(|_| RwLock::new(HashMap::new()));
        by_identity[identity_shard(ino)]
            .write()
            .expect("identity table poisoned")
            .insert((dev, ino), ROOT_ID);
        Registry {
            by_id,
            by_identity,
            handles: std::array::from_fn(|_| Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(ROOT_ID + 1),
            next_handle: AtomicU64::new(1),
            census,
            budget: descriptor_budget(),
            sweep: AtomicUsize::new(0),
            reported: Mutex::new(std::time::Instant::now()),
            hands: std::array::from_fn(|_| Mutex::new(VecDeque::new())),
            quiet_until: AtomicUsize::new(0),
            forcing: AtomicBool::new(false),
            born: std::time::Instant::now(),
            last_sweep: AtomicUsize::new(0),
            red_line: descriptor_budget() + descriptor_red_headroom(),
        }
    }

    pub fn get(&self, id: u64) -> Option<Arc<Inode>> {
        self.by_id[shard(id)]
            .lock()
            .expect("inode table poisoned")
            .get(&id)
            .cloned()
    }

    /// Counts a lookup of an inode we already hold, if we hold it.
    ///
    /// The point is the descriptor that is *not* opened: a LOOKUP of a path the
    /// guest has seen before would otherwise cost an `openat` every time, and a
    /// package install looks up the same few thousand directories tens of
    /// thousands of times.
    pub fn relookup(&self, dev: i64, ino: u64) -> Option<u64> {
        let id = *self.by_identity[identity_shard(ino)]
            .read()
            .expect("identity table poisoned")
            .get(&(dev, ino))?;
        let inode = self.by_id[shard(id)]
            .lock()
            .expect("inode table poisoned")
            .get(&id)
            .cloned()?;
        if !inode.still_is(dev, ino) {
            self.retire_identity(id, dev, ino);
            return None;
        }
        let mut lookups = inode.lookups.lock().expect("lookup count poisoned");
        *lookups = lookups.saturating_add(1);
        Some(id)
    }

    /// A reply named this nodeid outside the usual paths: count it, so the
    /// kernel's FORGET total and ours agree.
    pub fn count_lookup(&self, id: u64) {
        if let Some(inode) = self.by_id[shard(id)]
            .lock()
            .expect("inode table poisoned")
            .get(&id)
        {
            let mut lookups = inode.lookups.lock().expect("lookup count poisoned");
            *lookups = lookups.saturating_add(1);
        }
    }

    /// The nodeid for a host identity, without counting it as a lookup.
    ///
    /// For the watcher, which needs to know whether the guest has ever heard of
    /// something before it bothers telling it to forget.
    pub fn nodeid_for(&self, dev: i64, ino: u64) -> Option<u64> {
        self.by_identity[identity_shard(ino)]
            .read()
            .expect("identity table poisoned")
            .get(&(dev, ino))
            .copied()
    }

    /// Stops a nodeid answering to a pair of numbers, without destroying it.
    ///
    /// The inode itself stays: the guest may still be holding that nodeid for a
    /// file it has open and unlinked, and that file must keep working. What has
    /// to go is only the claim that those numbers *mean* this inode, so that
    /// whatever holds them now gets an identity of its own.
    fn retire_identity(&self, id: u64, dev: i64, ino: u64) {
        let _writer = self.by_identity[identity_shard(ino)]
            .write()
            .map(|mut map| {
                if map.get(&(dev, ino)) == Some(&id) {
                    map.remove(&(dev, ino));
                }
            });
    }

    /// Records an inode the guest is about to be told about, and counts the
    /// telling.
    ///
    /// `fd` is only installed if this is a new inode; otherwise it is dropped
    /// and the existing descriptor kept, which is what keeps one file to one
    /// descriptor however many names it has.
    pub fn insert(&self, fd: OwnedFd, dev: i64, ino: u64, is_dir: bool, is_symlink: bool) -> u64 {
        // Two threads can miss `relookup` for the same file at once; the
        // identity map is the arbiter, and the loser's descriptor is dropped.
        let mut identity = self.by_identity[identity_shard(ino)]
            .write()
            .expect("identity table poisoned");
        if let Some(&id) = identity.get(&(dev, ino)) {
            // Someone installed it between our caller's miss and this lock.
            // Their entry wins if it is still the file these numbers name; if
            // it is not, it is a stale claim on a reused inode number and it
            // is replaced below rather than reused.
            let existing = self.by_id[shard(id)]
                .lock()
                .expect("inode table poisoned")
                .get(&id)
                .cloned();
            if let Some(inode) = existing
                && inode.still_is(dev, ino)
            {
                let mut lookups = inode.lookups.lock().expect("lookup count poisoned");
                *lookups = lookups.saturating_add(1);
                return id;
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        identity.insert((dev, ino), id);
        drop(identity);
        let inode = Arc::new(Inode::new(
            fd,
            dev,
            ino,
            is_dir,
            is_symlink,
            1,
            self.census.clone(),
        ));
        inode.id.store(id, Ordering::Relaxed);
        // Admission control, and the difference between degrading and
        // seizing. A full share used to admit the newcomer and lean on the
        // sweep to evict something — but the sweep finds the few thousand
        // held descriptors by walking a hand that lists EVERY live inode,
        // and an insert storm at the ceiling (cp -a of a big tree on a
        // stock Mac) ran that walk per insert: quadratic, hours, and
        // indistinguishable from a hang. A newcomer to a full share is
        // parked on arrival instead — one close(), no walk — and revives
        // transiently on use like anything else parked. Inserts can no
        // longer push the count past the budget at all; the paced sweep is
        // left with only its real job, keeping the RESIDENT set the
        // recently-used one.
        if self.census.descriptors() > self.budget && inode.parkable() {
            let _ = inode.park();
        }
        self.by_id[shard(id)]
            .lock()
            .expect("inode table poisoned")
            .insert(id, inode);
        self.hands[shard(id)]
            .lock()
            .expect("reclaim hand poisoned")
            .push_back(id);
        self.reclaim_if_over_budget();
        id
    }

    /// Applies a FORGET.
    ///
    /// The inode is dropped from the table when its count reaches zero, but any
    /// operation still in flight holds an `Arc` and keeps the descriptor alive
    /// until it finishes — which is why the worker pool needs no ordering
    /// guarantee between FORGET and everything else.
    /// Registers an inode for a file that does not exist yet.
    ///
    /// The identity is provisional — a number from a range no real device
    /// uses — and the entry deliberately stays out of the identity map: there
    /// is no host identity to claim until [`Registry::bind_pending`].
    pub fn insert_pending(
        &self,
        dev: i64,
        meta: PendingMeta,
        kind: PendingKind,
    ) -> (u64, Arc<Inode>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let inode = Arc::new(Inode::new_pending(
            dev,
            PROVISIONAL_INO | id,
            meta,
            kind,
            self.census.clone(),
        ));
        inode.id.store(id, Ordering::Relaxed);
        self.by_id[shard(id)]
            .lock()
            .expect("inode table poisoned")
            .insert(id, inode.clone());
        (id, inode)
    }

    /// A file the host has and the guest has not yet named, registered
    /// parked: no descriptor, found by identity when the guest looks it up.
    /// Zero lookups until then, as any file the guest has not named.
    pub fn insert_parked(&self, dev: i64, ino: u64, birthtime: (i64, i64)) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let meta = PendingMeta {
            mode: libc::S_IFREG as u32 | 0o644,
            born: (0, 0),
            atime: (0, 0),
            mtime: (0, 0),
        };
        let inode = Arc::new(Inode::new_pending(
            dev,
            PROVISIONAL_INO | id,
            meta,
            PendingKind::File,
            self.census.clone(),
        ));
        inode.id.store(id, Ordering::Relaxed);
        inode.bind_parked(dev, ino, birthtime);
        self.by_id[shard(id)]
            .lock()
            .expect("inode table poisoned")
            .insert(id, inode);
        self.claim_identity(id, dev, ino);
        id
    }

    /// The apply queue performed a pending create: bind the real identity.
    ///
    /// The identity map only gains the entry if the guest still knows the
    /// nodeid — a FORGET may have raced the apply — and if no other entry
    /// claimed these numbers first, which the same lock arbitration as
    /// [`Registry::insert`] settles.
    pub fn bind_pending(&self, id: u64, inode: &Arc<Inode>, fd: OwnedFd, dev: i64, ino: u64) {
        inode.bind(fd, dev, ino);
        // The same admission control as `insert`: a bind must not push the
        // descriptor count past the budget, or a create storm larger than the
        // budget outruns the sweep and climbs to the kernel's ceiling. A
        // directory keeps its residency privilege (`parkable`).
        if self.census.descriptors() > self.budget && inode.parkable() {
            let _ = inode.park();
        }
        self.claim_identity(id, dev, ino);
    }

    /// [`Registry::bind_pending`] without a descriptor: see
    /// [`Inode::bind_parked`].
    pub fn bind_pending_parked(
        &self,
        id: u64,
        inode: &Arc<Inode>,
        dev: i64,
        ino: u64,
        birthtime: (i64, i64),
    ) {
        inode.bind_parked(dev, ino, birthtime);
        self.claim_identity(id, dev, ino);
    }

    fn claim_identity(&self, id: u64, dev: i64, ino: u64) {
        let still_known = self.by_id[shard(id)]
            .lock()
            .expect("inode table poisoned")
            .contains_key(&id);
        if !still_known {
            return;
        }
        let mut identity = self.by_identity[identity_shard(ino)]
            .write()
            .expect("identity table poisoned");
        identity.entry((dev, ino)).or_insert(id);
    }

    pub fn forget(&self, id: u64, count: u64) {
        if id == ROOT_ID {
            return;
        }
        let mut table = self.by_id[shard(id)].lock().expect("inode table poisoned");
        let Some(inode) = table.get(&id) else {
            return;
        };
        let remaining = {
            let mut lookups = inode.lookups.lock().expect("lookup count poisoned");
            *lookups = lookups.saturating_sub(count);
            *lookups
        };
        if remaining > 0 {
            return;
        }
        let identity = (inode.dev(), inode.ino());
        table.remove(&id);
        drop(table);

        let mut map = self.by_identity[identity_shard(identity.1)]
            .write()
            .expect("identity table poisoned");
        // Only unmap the identity if it still points at us: a file removed and
        // recreated could have reused the inode number and repointed it.
        if map.get(&identity) == Some(&id) {
            map.remove(&identity);
        }
    }

    /// Parks cold inodes until the descriptor count is back under budget.
    ///
    /// A clock, not an LRU. Keeping a true recency order would mean a global
    /// list every lookup has to move an entry in, which is exactly the kind of
    /// shared write the sharded tables exist to avoid. A reference bit costs a
    /// relaxed store on use and one pass to read, and the two agree about
    /// which inodes are cold — which is all the decision needs.
    ///
    /// Sweeping is bounded: one pass over every shard, parking whatever it
    /// finds cold and clearing the bit on the rest. If that is not enough the
    /// next insert sweeps again, and the second pass sees the bits the first
    /// one cleared. Refusing to loop here is what stops a share whose inodes
    /// are genuinely all hot from spinning instead of running.
    /// Cheap when under budget — one atomic load — so it is called on every
    /// request as well as every insert: a revival storm holds no inserts, and
    /// reclaim that only ran on insert let one climb to the ceiling unchecked.
    pub(crate) fn reclaim_if_over_budget(&self) {
        let open = self.census.descriptors();
        // Strictly under, not at: admission control parks newcomers the
        // moment the budget fills, so a busy share sits exactly AT budget —
        // and that is when the paced sweep has its real work, rotating
        // residency toward what is actually being touched.
        if open < self.budget {
            return;
        }
        // Between the budget and the red line, sweeps are paced, not
        // per-request: a working set larger than the budget revives inodes as
        // fast as they are parked, and a sweep per request is how a correct
        // bound becomes a ten-minute install. Ten sweeps a second keeps the
        // count breathing inside the headroom for a fraction of a percent of
        // the machine; at the red line every request sweeps, because the next
        // stop after the headroom is the kernel's ceiling.
        if open < self.red_line {
            let now = self.born.elapsed().as_millis() as usize;
            let last = self.last_sweep.load(Ordering::Relaxed);
            if now.saturating_sub(last) < SWEEP_EVERY_MS
                || self
                    .last_sweep
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_err()
            {
                return;
            }
        }
        let slack = (self.budget / SWEEP_SLACK_DIVISOR).max(64);
        if open <= self.budget + slack && open < self.quiet_until.load(Ordering::Relaxed) {
            return;
        }
        let target = self.budget - self.budget / 8;
        let mut examined = 0usize;
        let mut parked_total = 0usize;
        let mut promoted_total = 0usize;
        // Why the pass skipped what it saw: the numbers that say whether a
        // sweep that frees nothing is looking at a hot set, a parked set, a
        // privileged set, or files it cannot park (unlinked, nlink zero).
        let (mut skip_unheld, mut skip_used, mut skip_privileged, mut skip_unparkable) =
            (0usize, 0usize, 0usize, 0usize);

        while examined < SWEEP_STEPS && parked_total < SWEEP_TAKE {
            if self.census.descriptors() <= target {
                break;
            }
            let index = self.sweep.fetch_add(1, Ordering::Relaxed) % SHARDS;
            let batch: Vec<u64> = {
                let mut hand = self.hands[index].lock().expect("reclaim hand poisoned");
                let take = SWEEP_BATCH.min(hand.len());
                hand.drain(..take).collect()
            };
            if batch.is_empty() {
                examined += SWEEP_BATCH;
                continue;
            }
            for id in batch {
                examined += 1;
                let inode = self.by_id[index]
                    .lock()
                    .expect("inode table poisoned")
                    .get(&id)
                    .cloned();
                // Gone: the guest forgot it, and the hand cleans itself up by
                // simply not putting it back.
                let Some(inode) = inode else { continue };
                self.hands[index]
                    .lock()
                    .expect("reclaim hand poisoned")
                    .push_back(id);
                if !inode.held.load(Ordering::Relaxed) {
                    skip_unheld += 1;
                    // Parked but touched since the hand last came round: hot,
                    // and resident is where hot belongs. Promotion spends the
                    // room the parks on this pass have made (plus any slack
                    // under the budget), so the census never grows past the
                    // budget by rotation — the sets swap.
                    if inode.used.swap(false, Ordering::Relaxed) {
                        let room =
                            parked_total + self.budget.saturating_sub(self.census.descriptors());
                        if promoted_total < room && inode.promote() {
                            promoted_total += 1;
                        }
                    }
                    continue;
                }
                // `swap` is the ageing: an inode used since the hand last came
                // round survives this pass and is a candidate on the next.
                if inode.used.swap(false, Ordering::Relaxed) {
                    skip_used += 1;
                    continue;
                }
                if !inode.parkable() {
                    skip_privileged += 1;
                    continue;
                }
                // Parked outside the shard lock: parking stats and closes a
                // descriptor, and holding the lock across those would stall
                // every worker whose path runs through that shard.
                if inode.park() {
                    parked_total += 1;
                } else {
                    skip_unparkable += 1;
                }
            }
        }

        // The pass above defers to recency, which is right up to the moment it
        // stops working: a working set hotter than the budget re-sets every
        // reference bit faster than the hand comes round, so the polite pass
        // parks nothing while the count ratchets up — a thousand a second, on
        // a machine where it was watched — until it meets the kernel's ceiling
        // and the guest sees EMFILE. Past the slack, recency is a luxury: one
        // thread keeps the hand turning and parks whatever is held, reference
        // bit or no, until the count is back under target or it has been all
        // the way around. The others skip past and go back to work.
        if self.census.descriptors() > self.budget + slack
            && self
                .forcing
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            let mut remaining = self.census.inodes();
            while remaining > 0 && self.census.descriptors() > target {
                let index = self.sweep.fetch_add(1, Ordering::Relaxed) % SHARDS;
                let batch: Vec<u64> = {
                    let mut hand = self.hands[index].lock().expect("reclaim hand poisoned");
                    let take = SWEEP_BATCH.min(hand.len());
                    hand.drain(..take).collect()
                };
                remaining = remaining.saturating_sub(SWEEP_BATCH.max(batch.len()));
                for id in batch {
                    let inode = self.by_id[index]
                        .lock()
                        .expect("inode table poisoned")
                        .get(&id)
                        .cloned();
                    let Some(inode) = inode else { continue };
                    self.hands[index]
                        .lock()
                        .expect("reclaim hand poisoned")
                        .push_back(id);
                    if !inode.held.load(Ordering::Relaxed) {
                        continue;
                    }
                    // Files first, here too. Parking a hot directory by force
                    // was measured as a hundred milliseconds per directory on
                    // a tree walk: the walk revived it, revival re-admitted
                    // it, the count crossed the slack, and this pass swept
                    // the whole hand again — per readdir, for sixteen minutes.
                    // `parkable` still lets directories go once they alone
                    // exceed half the budget, so this always terminates.
                    if !inode.parkable() {
                        continue;
                    }
                    if inode.park() {
                        parked_total += 1;
                    }
                }
            }
            self.forcing.store(false, Ordering::Release);
        }

        let open = self.census.descriptors();
        if open > self.budget {
            self.quiet_until
                .store(open.saturating_add(slack), Ordering::Relaxed);
        }
        // Sitting at the budget with nothing to park is the reclaim working,
        // not failing: everything it can reach is in use. Drifting past the
        // slack is the other thing, and the next event after it is `EMFILE`
        // inside the guest on a file that is plainly there — which is a
        // miserable thing to diagnose from that end, so it is said here.
        if open > self.budget + slack && self.should_report() {
            tracing::warn!(
                open,
                budget = self.budget,
                inodes = self.inode_count(),
                live = self.census.inodes(),
                examined,
                parked = parked_total,
                unheld = skip_unheld,
                used = skip_used,
                privileged = skip_privileged,
                unparkable = skip_unparkable,
                "the share is holding more descriptors than it may"
            );
        }
    }

    /// Whether enough time has passed to say this again.
    ///
    /// Throttled because the thing being reported is a condition, not an
    /// event: a share that is over budget is over budget on every insert, and
    /// twenty-two thousand identical lines is both useless and, at the rate
    /// inserts arrive, a measurable cost of its own.
    fn should_report(&self) -> bool {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(1);
        let mut last = self.reported.lock().expect("report clock poisoned");
        if last.elapsed() < EVERY {
            return false;
        }
        *last = std::time::Instant::now();
        true
    }

    /// How many metadata descriptors are open, and the ceiling. Diagnostics
    /// only.
    /// Whether a new descriptor would be over the budget — and so parked
    /// the moment it was bound.
    pub fn at_budget(&self) -> bool {
        self.budget > 0 && self.census.descriptors() >= self.budget
    }

    pub fn descriptor_usage(&self) -> (usize, usize) {
        (self.census.descriptors(), self.budget)
    }

    /// How many inodes are live. Diagnostics only.
    pub fn inode_count(&self) -> usize {
        self.by_id
            .iter()
            .map(|shard| shard.lock().expect("inode table poisoned").len())
            .sum()
    }

    pub fn add_handle(&self, handle: Handle) -> u64 {
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles[shard(id)]
            .lock()
            .expect("handle table poisoned")
            .insert(id, Arc::new(handle));
        id
    }

    pub fn handle(&self, id: u64) -> Option<Arc<Handle>> {
        self.handles[shard(id)]
            .lock()
            .expect("handle table poisoned")
            .get(&id)
            .cloned()
    }

    pub fn release_handle(&self, id: u64) {
        self.handles[shard(id)]
            .lock()
            .expect("handle table poisoned")
            .remove(&id);
    }

    pub fn handle_count(&self) -> usize {
        self.handles
            .iter()
            .map(|shard| shard.lock().expect("handle table poisoned").len())
            .sum()
    }
}

/// How many entries one sweep of a shard reads, and how many of them it may
/// park.
///
/// The scan bound keeps the cost of a sweep flat as a share grows; the take
/// bound keeps one sweep from closing thousands of descriptors that are about
/// to be wanted again. Neither has to be exactly right, because a sweep that
/// did not free enough is followed by another once the count has grown.
///
/// The step bound is what makes the reclaim cost independent of how big the
/// share has become. Walking the inode table instead does not: a sweep of a
/// three-hundred-thousand-inode table, repeated on every insert that finds
/// itself over budget, is billions of loads — measured, it turned a
/// ten-second tree copy into eight minutes.
const SWEEP_STEPS: usize = 8192;
const SWEEP_TAKE: usize = 1024;
/// How many ids are taken from one shard's hand before moving to the next.
const SWEEP_BATCH: usize = 256;

/// How far above the budget a share may drift before every insert sweeps
/// again, as a fraction of the budget.
///
/// Both bounds matter. Without hysteresis, a share whose whole working set is
/// hot sweeps every shard on every insert to be told the same thing, which is
/// what turned a twelve-second install into a four-minute one. Without a hard
/// limit on the drift, hysteresis is just the original unbounded growth with
/// extra steps, and the process still meets the kernel's ceiling. So the drift
/// is capped well inside the reserve the budget already leaves.
const SWEEP_SLACK_DIVISOR: usize = 64;

/// How often an over-budget share is swept while it is still under the red
/// line.
const SWEEP_EVERY_MS: usize = 100;

/// How far past the budget the count may breathe before sweeps stop being
/// paced. Bounded by both scales: a slice of the ceiling so the process's
/// other descriptors keep their room at the worst moment between sweeps, and
/// a slice of the budget so a deliberately tiny test budget still meets its
/// red line instead of hiding under a machine-sized one.
fn descriptor_red_headroom() -> usize {
    let ceiling = crate::sys::descriptor_ceiling() as usize;
    (ceiling / 32).min(descriptor_budget() / 4).max(64)
}

/// How many metadata descriptors one share may hold open.
///
/// The guest decides how many inodes it remembers, and it is under no
/// obligation to remember few: nothing pressures a dentry cache in an idle
/// eight-gigabyte VM, so FORGET may never come for a tree the container walked
/// once. Left unbounded, one descriptor per remembered inode meets
/// `kern.maxfilesperproc` — which it does, three copies into a
/// sixty-six-thousand-file package tree, and the guest sees `EMFILE` from
/// `open` on a file that is plainly there.
///
/// So the descriptor is treated as what it is: a cache of where the file lives,
/// not the file's identity. Identity is `(dev, ino)`, and a parked inode
/// reopens by path and re-checks it.
///
/// Three quarters of the ceiling, less a reserve for everything else the
/// process opens — the guest's open files, its directories, the VMM's disks,
/// its sockets, the network sidecar's.
///
/// Generous on purpose. Parking is not free: a parked inode is reopened by
/// path on next use, which is an `open` the workload did not ask for, and if
/// the name has moved in between it is an ESTALE the workload has to recover
/// from. So the budget is a ceiling to stay under, not a size to run at, and
/// it is set high enough that an ordinary package tree never reaches it.
///
/// Halving it, briefly, proved the point: a sixty-six-thousand-file install
/// wants about ninety thousand inodes, and against a ninety-two-thousand
/// budget it thrashed — park, revive, park — turning a twelve-second install
/// into a four-minute one.
fn descriptor_budget() -> usize {
    // Overridable so the reclaim can be exercised against a few thousand
    // inodes instead of a hundred thousand, which is the difference between a
    // test that runs in a second and one nobody runs.
    if let Some(budget) = std::env::var("LIGHTER_FS_FD_BUDGET")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return budget.max(8);
    }
    // The reserve is proportional with a floor, not flat. A flat 8192 was
    // sized on a machine whose ceiling was 184,320 and left a stock Mac —
    // `kern.maxfilesperproc` of 10,240 — a budget of 1,536, which a pnpm
    // install walks straight through. A fifth of the ceiling covers the
    // guest's open files, the VMM's disks and sockets, and the sidecar on
    // either machine.
    let ceiling = crate::sys::descriptor_ceiling();
    let reserve = (ceiling / 8).max(2048);
    ((ceiling.saturating_sub(reserve) / 4) * 3).max(1024) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::MetadataExt;

    /// A temporary directory that cleans itself up.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir().join(format!(
                "lighter-inode-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn registry(&self) -> Registry {
            let fd = crate::sys::open_root(&self.0).unwrap();
            let st = crate::sys::stat_fd(fd.as_raw_fd()).unwrap();
            Registry::new(fd, st.st_dev as i64, st.st_ino)
        }

        /// A real file, and the numbers the filesystem gave it.
        ///
        /// Real rather than fabricated because the registry now verifies that a
        /// descriptor still answers to the numbers it was filed under — which
        /// is the whole point of it, and which invented numbers cannot satisfy.
        fn file(&self, name: &str) -> (OwnedFd, i64, u64) {
            let path = self.0.join(name);
            std::fs::write(&path, name.as_bytes()).unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            let fd = crate::sys::open_path(&path, 0, 0).unwrap();
            (fd, meta.dev() as i64, meta.ino())
        }

        fn link(&self, from: &str, to: &str) -> OwnedFd {
            std::fs::hard_link(self.0.join(from), self.0.join(to)).unwrap();
            crate::sys::open_path(&self.0.join(to), 0, 0).unwrap()
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    /// A descriptor for the handle tests, which do not care what it points at.
    fn spare_fd() -> OwnedFd {
        crate::sys::open_root(&std::env::temp_dir()).unwrap()
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Two names for one file must be one inode, or the guest sees two
    /// different `nodeid`s for something `stat` says is the same file — and
    /// `find -samefile` and every build system's hard-link cache disagree with
    /// the kernel.
    #[test]
    fn one_host_file_is_one_nodeid_however_many_names() {
        let scratch = Scratch::new("names");
        let reg = scratch.registry();
        let (fd, dev, ino) = scratch.file("original");
        let first = reg.insert(fd, dev, ino, false, false);
        let second = reg.insert(scratch.link("original", "alias"), dev, ino, false, false);
        assert_eq!(first, second);
        assert_eq!(reg.inode_count(), 2, "root plus the one file");
    }

    /// Two lookups mean two forgets. Dropping on the first would hand the guest
    /// a dangling nodeid it is still entitled to use.
    #[test]
    fn an_inode_survives_until_every_lookup_is_forgotten() {
        let scratch = Scratch::new("forget");
        let reg = scratch.registry();
        let (fd, dev, ino) = scratch.file("counted");
        let id = reg.insert(fd, dev, ino, false, false);
        assert_eq!(reg.relookup(dev, ino), Some(id), "a second lookup");

        reg.forget(id, 1);
        assert!(reg.get(id).is_some(), "one forget of two must not drop it");
        reg.forget(id, 1);
        assert!(reg.get(id).is_none());
        assert_eq!(reg.inode_count(), 1, "only the root remains");
    }

    /// The kernel may batch forgets, and it may over-count after an abort.
    #[test]
    fn a_batched_or_excessive_forget_does_not_underflow() {
        let scratch = Scratch::new("batch");
        let reg = scratch.registry();
        let (fd, dev, ino) = scratch.file("counted");
        let id = reg.insert(fd, dev, ino, false, false);
        reg.forget(id, u64::MAX);
        assert!(reg.get(id).is_none());
        reg.forget(id, 1);
    }

    /// The root has no FORGET in the protocol, and a guest that sent one must
    /// not be able to unmount itself.
    #[test]
    fn the_root_cannot_be_forgotten() {
        let scratch = Scratch::new("root");
        let reg = scratch.registry();
        reg.forget(ROOT_ID, u64::MAX);
        assert!(reg.get(ROOT_ID).is_some());
    }

    /// A forgotten identity must be re-lookupable, with a fresh nodeid rather
    /// than the old one: the guest may still have the old number in flight.
    #[test]
    fn a_reinserted_file_gets_a_new_nodeid() {
        let scratch = Scratch::new("reinsert");
        let reg = scratch.registry();
        let (fd, dev, ino) = scratch.file("here");
        let first = reg.insert(fd, dev, ino, false, false);
        reg.forget(first, 1);
        let second = reg.insert(
            crate::sys::open_path(&scratch.path().join("here"), 0, 0).unwrap(),
            dev,
            ino,
            false,
            false,
        );
        assert_ne!(first, second);
    }

    /// The inode-reuse bug, reproduced at the layer it lives in.
    ///
    /// A real file is registered, then deleted while its descriptor is still
    /// held — exactly what a package install does to its temporaries. A second,
    /// unrelated file then arrives claiming the same numbers, which APFS
    /// genuinely does hand out again. It must not be given the first file's
    /// nodeid, because the guest would then treat two files as one.
    #[test]
    fn a_reused_inode_number_does_not_alias_onto_the_dead_file() {
        let scratch = Scratch::new("reuse");
        let reg = scratch.registry();

        let (held, dev, ino) = scratch.file("doomed");
        let first = reg.insert(held, dev, ino, false, false);

        // The name goes; our descriptor, and the guest's nodeid, remain.
        std::fs::remove_file(scratch.path().join("doomed")).unwrap();
        assert!(
            reg.get(first).is_some(),
            "an unlinked open file must survive"
        );

        // A different file, which the filesystem has handed the same numbers.
        let (fresh, _, _) = scratch.file("successor");

        assert!(
            reg.relookup(dev, ino).is_none(),
            "the dead file must stop answering to its old numbers"
        );
        let second = reg.insert(fresh, dev, ino, false, false);
        assert_ne!(
            first, second,
            "two different files were given the same nodeid"
        );
    }

    #[test]
    fn handles_are_issued_and_released() {
        let scratch = Scratch::new("handles");
        let reg = scratch.registry();
        let fh = reg.add_handle(Handle::File(Arc::new(OpenFile {
            fd: spare_fd(),
            readable: true,
            append: false,
            writable: true,
        })));
        assert!(reg.handle(fh).is_some());
        assert_eq!(reg.handle_count(), 1);
        reg.release_handle(fh);
        assert!(reg.handle(fh).is_none());
        assert_eq!(reg.handle_count(), 0);
    }

    /// Parking is a descriptor decision, not an identity one: the inode keeps
    /// answering to the same nodeid and the same numbers, and the next use
    /// reopens it.
    #[test]
    fn a_parked_inode_comes_back() {
        let scratch = Scratch::new("park-revive");
        let reg = scratch.registry();
        let (fd, dev, ino) = scratch.file("parked");
        let id = reg.insert(fd, dev, ino, false, false);
        let inode = reg.get(id).unwrap();

        assert!(inode.park(), "an ordinary file must be parkable");
        assert!(
            inode.fd.read().unwrap().is_none(),
            "parking must actually let the descriptor go"
        );

        let reference = inode.reference().expect("a parked inode must reopen");
        let st = crate::sys::stat_fd(reference.raw_fd()).unwrap();
        assert_eq!((st.st_dev as i64, st.st_ino), (dev, ino));
    }

    /// The path a descriptor was parked at is a hint, not a promise. If it now
    /// names a different file the guest must be told its nodeid is stale
    /// rather than handed the wrong file — which is the same failure APFS
    /// inode reuse produces, arriving by a different route.
    #[test]
    fn a_parked_inode_whose_name_was_taken_is_stale() {
        let scratch = Scratch::new("park-stolen");
        let reg = scratch.registry();
        let (fd, dev, ino) = scratch.file("victim");
        let id = reg.insert(fd, dev, ino, false, false);
        let inode = reg.get(id).unwrap();
        assert!(inode.park());

        let path = scratch.0.join("victim");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"someone else").unwrap();

        assert_eq!(
            inode.reference().err(),
            Some(crate::errno::linux::ESTALE),
            "a reopen that lands on another file must not be handed out"
        );
    }

    /// An unlinked file has no path to be reopened by, and its descriptor is
    /// the only thing keeping the guest's open handle working. Parking it
    /// would lose the file.
    #[test]
    fn an_unlinked_inode_is_never_parked() {
        let scratch = Scratch::new("park-unlinked");
        let reg = scratch.registry();
        let (fd, dev, ino) = scratch.file("doomed");
        let id = reg.insert(fd, dev, ino, false, false);
        std::fs::remove_file(scratch.0.join("doomed")).unwrap();

        let inode = reg.get(id).unwrap();
        assert!(
            !inode.park(),
            "a file with no links must keep its descriptor"
        );
        assert!(inode.reference().is_ok());
    }

    /// A parked inode revives across a rename.
    ///
    /// The remembered path is stale the moment anything moves the file, and
    /// answering ESTALE for a file that plainly exists — merely elsewhere —
    /// punishes the guest for the host's tidying. `/.vol` opens the inode by
    /// identity, and the birth time check keeps a recycled inode number from
    /// impersonating it.
    #[test]
    fn a_parked_inode_survives_being_renamed() {
        let scratch = Scratch::new("volfs");
        let reg = scratch.registry();
        let (fd, dev, ino) = scratch.file("original");
        let id = reg.insert(fd, dev, ino, false, false);
        let inode = reg.get(id).unwrap();
        assert!(inode.park());
        std::fs::rename(scratch.0.join("original"), scratch.0.join("moved")).unwrap();
        let reference = inode
            .reference()
            .expect("a renamed file is still the same file");
        let st = crate::sys::stat_fd(reference.raw_fd()).unwrap();
        assert_eq!(
            st.st_ino, ino,
            "the revived descriptor must be the same inode"
        );
    }

    /// A parked inode whose file is gone answers ESTALE, not somebody else.
    #[test]
    fn a_parked_inode_does_not_survive_deletion() {
        let scratch = Scratch::new("volfs-gone");
        let reg = scratch.registry();
        let (fd, dev, ino) = scratch.file("doomed");
        let id = reg.insert(fd, dev, ino, false, false);
        let inode = reg.get(id).unwrap();
        assert!(inode.park());
        std::fs::remove_file(scratch.0.join("doomed")).unwrap();
        assert!(
            inode.reference().is_err(),
            "a deleted file must not revive as anything"
        );
    }

    /// The tally is the only thing standing between a share and the kernel's
    /// descriptor ceiling, so it has to survive the full cycle. It did not:
    /// parking decremented it and reviving did not increment it, so on a
    /// workload that parks and revives — which is every workload big enough to
    /// park at all — it drifted down, stopped triggering a reclaim, and the
    /// process climbed to exactly `kern.maxfilesperproc` while the counter
    /// insisted it was well under budget.
    #[test]
    fn the_descriptor_tally_survives_park_and_revive() {
        let scratch = Scratch::new("tally");
        let reg = scratch.registry();
        let root_held = reg.descriptor_usage().0;

        let (fd, dev, ino) = scratch.file("counted");
        let id = reg.insert(fd, dev, ino, false, false);
        assert_eq!(reg.descriptor_usage().0, root_held + 1);

        let inode = reg.get(id).unwrap();
        assert!(inode.park());
        assert_eq!(
            reg.descriptor_usage().0,
            root_held,
            "parking must be counted"
        );

        inode.reference().expect("a parked inode must reopen");
        assert_eq!(
            reg.descriptor_usage().0,
            root_held + 1,
            "reviving must be counted too, or the tally drifts down forever"
        );

        drop(inode);
        reg.forget(id, 1);
        assert_eq!(
            reg.descriptor_usage().0,
            root_held,
            "a forgotten inode must give its descriptor back"
        );
    }

    /// The reclaim has to actually reclaim. It did not: parked at exactly the
    /// budget, finding nothing to park, and re-sweeping every shard on every
    /// insert — which turned a twelve-second package install into a
    /// four-minute one and logged a hundred thousand warnings doing it.
    ///
    /// This is the shape of that failure, at a thousandth of the size.
    #[test]
    fn the_reclaim_keeps_a_share_under_its_budget() {
        // SAFETY: single-threaded test setup, before any registry is built.
        unsafe { std::env::set_var("LIGHTER_FS_FD_BUDGET", "256") };
        let scratch = Scratch::new("budget");
        let reg = scratch.registry();
        assert_eq!(reg.descriptor_usage().1, 256);

        for n in 0..2000 {
            let (fd, dev, ino) = scratch.file(&format!("f{n}"));
            reg.insert(fd, dev, ino, false, false);
        }

        let (open, budget) = reg.descriptor_usage();
        unsafe { std::env::remove_var("LIGHTER_FS_FD_BUDGET") };
        assert!(
            open <= budget + budget / 4,
            "held {open} descriptors against a budget of {budget}"
        );
    }

    /// A share bigger than one sweep can carry must still be kept under its
    /// budget.
    ///
    /// Sized deliberately: the failure this catches only appears once a shard
    /// holds more inodes than one sweep may take from it. Below that every
    /// candidate fits in the batch and the reclaim looks fine. Above it the
    /// batch filled with inodes that were *already* parked — permanently cold,
    /// so chosen first every time, and refused by `park()` for having nothing
    /// to park — and the sweep freed almost nothing while reporting sixteen
    /// thousand candidates considered. Measured on a package install, 16,384
    /// chosen and 27 parked.
    #[test]
    fn a_share_larger_than_one_sweep_is_still_kept_in_budget() {
        // SAFETY: single-threaded test setup, before any registry is built.
        unsafe { std::env::set_var("LIGHTER_FS_FD_BUDGET", "4096") };
        let scratch = Scratch::new("big");
        let reg = scratch.registry();

        // Big enough that most of the table is parked and the parked ones
        // dominate every shard's iteration order. That is the shape that
        // broke: the sweep spent its whole budget skipping inodes it had
        // already parked, found one that still held a descriptor, and let the
        // share drift sixteen thousand descriptors over the budget.
        const FILES: usize = 24_000;
        for n in 0..FILES {
            let (fd, dev, ino) = scratch.file(&format!("b{n}"));
            reg.insert(fd, dev, ino, false, false);
        }

        let (open, budget) = reg.descriptor_usage();
        unsafe { std::env::remove_var("LIGHTER_FS_FD_BUDGET") };
        assert!(
            open <= budget * 2,
            "held {open} descriptors against a budget of {budget} over {FILES} inodes"
        );
    }

    /// A working set hotter than the budget must still be held near it.
    ///
    /// The shape that produced EMFILE on a stock Mac (`kern.maxfilesperproc`
    /// of 10,240): every inode is touched again before the hand comes back
    /// around, so the recency pass clears reference bits and parks nothing,
    /// every pass, while the count climbs about a thousand a second until it
    /// meets the kernel's ceiling. Past the slack the reclaim has to stop
    /// deferring to recency and park hot inodes anyway.
    #[test]
    fn a_working_set_hotter_than_the_budget_is_still_bounded() {
        // SAFETY: single-threaded test setup, before any registry is built.
        unsafe { std::env::set_var("LIGHTER_FS_FD_BUDGET", "256") };
        let scratch = Scratch::new("hot");
        let reg = scratch.registry();
        let mut ids = Vec::new();
        let mut worst = 0usize;
        for n in 0..1200 {
            let (fd, dev, ino) = scratch.file(&format!("h{n}"));
            ids.push(reg.insert(fd, dev, ino, false, false));
            // Everything stays hot — and parked inodes are revived, the way
            // an install re-walks the tree it is writing.
            if n % 64 == 0 {
                for &id in &ids {
                    if let Some(inode) = reg.get(id) {
                        let _ = inode.reference();
                    }
                }
                // Every real op runs the same check through dispatch; the
                // revival loop above stands in for a burst of them.
                reg.reclaim_if_over_budget();
            }
            worst = worst.max(reg.descriptor_usage().0);
        }
        let budget = reg.descriptor_usage().1;
        unsafe { std::env::remove_var("LIGHTER_FS_FD_BUDGET") };
        // Revival between sweeps means the count breathes above the budget;
        // what it must never do is ratchet toward the ceiling.
        assert!(
            worst <= budget * 3,
            "descriptors reached {worst} against a budget of {budget} with everything hot"
        );
    }

    /// The budget has to be under what the kernel will actually allow, or it
    /// is not a budget at all — which is how a package tree three copies deep
    /// produced EMFILE inside the guest on a file that was plainly there.
    #[test]
    fn the_descriptor_budget_leaves_room() {
        let budget = descriptor_budget() as u64;
        let ceiling = crate::sys::descriptor_ceiling();
        assert!(
            budget < ceiling,
            "budget {budget} must be under the ceiling {ceiling}"
        );
    }

    /// Handle numbers are never reused. A stale `fh` from a released file must
    /// fail rather than land on whatever was opened next.
    #[test]
    fn handle_numbers_are_not_recycled() {
        let scratch = Scratch::new("recycle");
        let reg = scratch.registry();
        let first = reg.add_handle(Handle::File(Arc::new(OpenFile {
            fd: spare_fd(),
            readable: true,
            append: false,
            writable: true,
        })));
        reg.release_handle(first);
        let second = reg.add_handle(Handle::File(Arc::new(OpenFile {
            fd: spare_fd(),
            readable: true,
            append: false,
            writable: true,
        })));
        assert_ne!(first, second);
    }
}

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

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// The `nodeid` of a mount's root. Fixed by the protocol.
pub const ROOT_ID: u64 = 1;

/// One file, as long as the guest remembers it.
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
    parked_at: Mutex<Option<PathBuf>>,
    /// The reference bit of a clock: set on use, cleared as the reclaimer
    /// passes. An inode found with it clear is one nothing has touched since
    /// the last sweep.
    used: AtomicBool,
    /// Host identity, which is what makes two paths to one file share a
    /// `nodeid` — as hard links must.
    pub dev: i64,
    pub ino: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// How many times the guest has been told about this inode.
    lookups: Mutex<u64>,
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
    fn new(fd: OwnedFd, dev: i64, ino: u64, is_dir: bool, is_symlink: bool, lookups: u64) -> Inode {
        Inode {
            fd: RwLock::new(Some(Arc::new(fd))),
            parked_at: Mutex::new(None),
            used: AtomicBool::new(true),
            dev,
            ino,
            is_dir,
            is_symlink,
            lookups: Mutex::new(lookups),
        }
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
        let path = self
            .parked_at
            .lock()
            .expect("parked path poisoned")
            .clone()
            .ok_or(crate::errno::linux::ESTALE)?;
        let fd = crate::sys::open_reference_path(&path, self.is_symlink)
            .map_err(|_| crate::errno::linux::ESTALE)?;
        match crate::sys::stat_fd(fd.as_raw_fd()) {
            Ok(st) if st.st_ino == self.ino && st.st_dev as i64 == self.dev => {}
            _ => return Err(crate::errno::linux::ESTALE),
        }
        let fd = Arc::new(fd);
        *slot = Some(fd.clone());
        Ok(Reference(fd))
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
        match crate::sys::stat_fd(fd.as_raw_fd()) {
            Ok(st) if st.st_nlink > 0 => {}
            _ => return false,
        }
        let Ok(path) = crate::sys::path_of(fd.as_raw_fd()) else {
            return false;
        };
        drop(fd);
        let mut slot = self.fd.write().expect("inode slot poisoned");
        if slot.is_none() {
            return false;
        }
        *self.parked_at.lock().expect("parked path poisoned") = Some(path);
        *slot = None;
        true
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
    /// Not sharded, but read-mostly: it is written once per new inode and read
    /// on every lookup, which is exactly the shape an `RwLock` is for.
    by_identity: RwLock<HashMap<(i64, u64), u64>>,
    /// Sharded by `fh`.
    handles: [Mutex<HashMap<u64, Arc<Handle>>>; SHARDS],
    next_id: AtomicU64,
    next_handle: AtomicU64,
    /// How many metadata descriptors the inode table is holding open.
    ///
    /// An estimate, and deliberately so: it counts descriptors handed to
    /// inodes rather than descriptors the kernel has, which is close enough to
    /// steer a reclaim and cheap enough to keep on a hot path.
    open_fds: AtomicUsize,
    /// The most it may hold before parking the cold ones.
    budget: usize,
    /// Where the clock hand is, so successive sweeps cover every shard.
    sweep: AtomicUsize,
}

impl Registry {
    /// Builds a registry whose root is `root_fd`.
    pub fn new(root_fd: OwnedFd, dev: i64, ino: u64) -> Registry {
        // The root is never forgotten: the kernel does not FORGET nodeid 1, and
        // a count that could reach zero would let a buggy guest drop it and
        // take the whole mount with it.
        let root = Arc::new(Inode::new(root_fd, dev, ino, true, false, u64::MAX));
        let by_id: [Mutex<HashMap<u64, Arc<Inode>>>; SHARDS] =
            std::array::from_fn(|_| Mutex::new(HashMap::new()));
        by_id[shard(ROOT_ID)]
            .lock()
            .expect("inode table poisoned")
            .insert(ROOT_ID, root);
        let mut by_identity = HashMap::new();
        by_identity.insert((dev, ino), ROOT_ID);
        Registry {
            by_id,
            by_identity: RwLock::new(by_identity),
            handles: std::array::from_fn(|_| Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(ROOT_ID + 1),
            next_handle: AtomicU64::new(1),
            open_fds: AtomicUsize::new(1),
            budget: descriptor_budget(),
            sweep: AtomicUsize::new(0),
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
        let id = *self
            .by_identity
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

    /// The nodeid for a host identity, without counting it as a lookup.
    ///
    /// For the watcher, which needs to know whether the guest has ever heard of
    /// something before it bothers telling it to forget.
    pub fn nodeid_for(&self, dev: i64, ino: u64) -> Option<u64> {
        self.by_identity
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
        let _writer = self.by_identity.write().map(|mut map| {
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
        let mut identity = self.by_identity.write().expect("identity table poisoned");
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
        self.by_id[shard(id)]
            .lock()
            .expect("inode table poisoned")
            .insert(
                id,
                Arc::new(Inode::new(fd, dev, ino, is_dir, is_symlink, 1)),
            );
        self.open_fds.fetch_add(1, Ordering::Relaxed);
        self.reclaim_if_over_budget();
        id
    }

    /// Applies a FORGET.
    ///
    /// The inode is dropped from the table when its count reaches zero, but any
    /// operation still in flight holds an `Arc` and keeps the descriptor alive
    /// until it finishes — which is why the worker pool needs no ordering
    /// guarantee between FORGET and everything else.
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
        let identity = (inode.dev, inode.ino);
        let held = inode.fd.read().expect("inode slot poisoned").is_some();
        table.remove(&id);
        drop(table);
        if held {
            self.open_fds.fetch_sub(1, Ordering::Relaxed);
        }

        let mut map = self.by_identity.write().expect("identity table poisoned");
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
    fn reclaim_if_over_budget(&self) {
        if self.open_fds.load(Ordering::Relaxed) <= self.budget {
            return;
        }
        let target = self.budget - self.budget / 8;
        for _ in 0..SHARDS {
            if self.open_fds.load(Ordering::Relaxed) <= target {
                return;
            }
            let index = self.sweep.fetch_add(1, Ordering::Relaxed) % SHARDS;
            // The inodes are cloned out under the lock and parked outside it:
            // parking stats and closes a descriptor, and a shard lock held
            // across those would stall every worker whose path runs through it.
            let candidates: Vec<Arc<Inode>> = self.by_id[index]
                .lock()
                .expect("inode table poisoned")
                .iter()
                .filter(|&(&id, _)| id != ROOT_ID)
                .map(|(_, inode)| inode.clone())
                .collect();
            for inode in candidates {
                if inode.used.swap(false, Ordering::Relaxed) {
                    continue;
                }
                if inode.park() {
                    self.open_fds.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// How many metadata descriptors are open, and the ceiling. Diagnostics
    /// only.
    pub fn descriptor_usage(&self) -> (usize, usize) {
        (self.open_fds.load(Ordering::Relaxed), self.budget)
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
/// sockets and the network sidecar.
fn descriptor_budget() -> usize {
    const RESERVE: u64 = 8192;
    let ceiling = crate::sys::descriptor_ceiling().saturating_sub(RESERVE);
    ((ceiling / 4) * 3).max(1024) as usize
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
            append: false,
            writable: true,
        })));
        reg.release_handle(first);
        let second = reg.add_handle(Handle::File(Arc::new(OpenFile {
            fd: spare_fd(),
            append: false,
            writable: true,
        })));
        assert_ne!(first, second);
    }
}

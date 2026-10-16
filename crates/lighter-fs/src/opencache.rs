//! Descriptors the server holds because the guest stopped telling it about
//! opens.
//!
//! # Why the guest stops telling us
//!
//! Answering `OPEN` with `ENOSYS` makes Linux set `no_open` on the connection
//! and never send `OPEN`, `RELEASE` or `FLUSH` for a regular file again; the
//! same for `OPENDIR` and `RELEASEDIR`. That is three round trips deleted for
//! every file something reads and two for every directory something walks —
//! and on a read-heavy workload those were three of the four round trips a
//! file cost. It is also, unusually, a change the kernel *prefers*: with no
//! open to report flags for, it defaults the file to `FOPEN_KEEP_CACHE` and a
//! directory to `FOPEN_CACHE_DIR`, which is exactly what we were asking for
//! anyway.
//!
//! The price is that `READ` and `WRITE` then arrive with no file handle, only
//! an inode — so somebody has to hold the descriptor. That is this module.
//!
//! # Bounded, and why eviction is safe
//!
//! Entries are handed out as `Arc`s, so evicting one does not close anything:
//! the descriptor lives until the last operation using it finishes. Eviction is
//! therefore free to be crude, and it is — when the map is over its limit, an
//! arbitrary slice of it goes. A perfect LRU would cost a lock ordering and a
//! list write on every read, to make slightly better decisions about which
//! descriptor to re-open.
//!
//! A file on a network volume is an open handle on its server as well, and
//! the server has only so many, so the cache keeps only that volume's
//! allowance of them ([`crate::remote`]), whatever room the cache has.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

use crate::inode::{OpenDir, OpenFile};
use crate::remote::Remote;

/// How many of each kind to keep. Sized to be comfortably under the descriptor
/// limit while still covering the working set of a build: a compiler with a
/// thousand headers open at once is ordinary, ten thousand is not.
const FILE_LIMIT: usize = 2048;
const DIR_LIMIT: usize = 512;

#[derive(Default)]
pub struct OpenCache {
    files: Mutex<HashMap<u64, Cached>>,
    dirs: Mutex<HashMap<u64, Arc<OpenDir>>>,
}

/// A cached file, counted on its network volume while it is in the cache.
/// The count is kept by construction and drop, so every way an entry
/// leaves the map, eviction, trimming and replacement alike, uncounts it.
struct Cached {
    file: Arc<OpenFile>,
    remote: Option<Arc<Remote>>,
}

impl Cached {
    fn new(file: Arc<OpenFile>, remote: Option<Arc<Remote>>) -> Cached {
        if let Some(remote) = &remote {
            remote.cached_file();
        }
        Cached { file, remote }
    }
}

impl Drop for Cached {
    fn drop(&mut self) {
        if let Some(remote) = &self.remote {
            remote.uncached_file();
        }
    }
}

impl OpenCache {
    pub fn new() -> OpenCache {
        OpenCache::default()
    }

    /// The cached descriptor for an inode, if there is one and it is good
    /// enough for what the caller intends.
    pub fn file(&self, nodeid: u64, need_write: bool) -> Option<Arc<OpenFile>> {
        let files = self.files.lock().expect("open cache poisoned");
        let file = &files.get(&nodeid)?.file;
        // A read-only descriptor cannot serve a write, nor a write-only one a
        // read; the caller replaces it. Reporting a miss is how it finds out.
        let fits = if need_write {
            file.writable
        } else {
            file.readable
        };
        fits.then(|| file.clone())
    }

    /// Caches the file `nodeid` has open. `remote` is the network volume
    /// it is on, if it is on one, whose allowance it is kept to.
    pub fn put_file(&self, nodeid: u64, file: Arc<OpenFile>, remote: Option<Arc<Remote>>) {
        let mut files = self.files.lock().expect("open cache poisoned");
        let over = remote.clone();
        files.insert(nodeid, Cached::new(file, remote));
        if let Some(remote) = over
            && remote.files_over() > 0
        {
            // A quarter of the allowance besides, so a volume at its
            // allowance does not pay an eviction on every open.
            let excess = remote.files_over() + remote.file_allowance() / 4;
            let doomed: Vec<u64> = files
                .iter()
                .filter(|(key, cached)| {
                    **key != nodeid
                        && cached
                            .remote
                            .as_ref()
                            .is_some_and(|other| Arc::ptr_eq(other, &remote))
                })
                .map(|(key, _)| *key)
                .take(excess)
                .collect();
            for key in doomed {
                files.remove(&key);
            }
        }
        trim(&mut files, FILE_LIMIT);
    }

    pub fn directory(&self, nodeid: u64) -> Option<Arc<OpenDir>> {
        self.dirs
            .lock()
            .expect("open cache poisoned")
            .get(&nodeid)
            .cloned()
    }

    pub fn put_directory(&self, nodeid: u64, dir: Arc<OpenDir>) {
        let mut dirs = self.dirs.lock().expect("open cache poisoned");
        dirs.insert(nodeid, dir);
        // Only listings the guest has finished reading are evictable; see
        // `OpenDir::complete`. A cache full of open listings simply grows —
        // a walk that deep is bounded by the tree's depth, not its size.
        if dirs.len() > DIR_LIMIT {
            let excess = dirs.len() - DIR_LIMIT + DIR_LIMIT / 4;
            let doomed: Vec<u64> = dirs
                .iter()
                .filter(|(_, dir)| dir.complete.load(std::sync::atomic::Ordering::Relaxed))
                .map(|(key, _)| *key)
                .take(excess)
                .collect();
            for key in doomed {
                dirs.remove(&key);
            }
        }
    }

    /// Drops whatever is held for an inode.
    ///
    /// Called when the guest forgets it, and when something makes the
    /// descriptor wrong — a truncation through a different path, say — so that
    /// the next access opens the file again.
    pub fn evict(&self, nodeid: u64) {
        self.files
            .lock()
            .expect("open cache poisoned")
            .remove(&nodeid);
        self.dirs
            .lock()
            .expect("open cache poisoned")
            .remove(&nodeid);
    }

    /// Drops every descriptor on `dev`, returning how many. Read from the
    /// descriptors themselves rather than through the inodes they belong to:
    /// a volume being ejected must lose every one, including any whose inode
    /// the registry has already forgotten.
    pub fn evict_device(&self, dev: i64) -> usize {
        let mut files = self.files.lock().expect("open cache poisoned");
        let before = files.len();
        files.retain(|_, cached| {
            crate::sys::stat_fd(cached.file.fd.as_raw_fd())
                .map_or(true, |st| st.st_dev as i64 != dev)
        });
        before - files.len()
    }

    /// Drops every file cached on the network volume `dev`, returning how
    /// many. Unlike [`OpenCache::evict_device`] it asks no descriptor
    /// anything: the volume's server is out of handles, and each entry says
    /// which volume it is on.
    pub fn evict_remote(&self, dev: i64) -> usize {
        let mut files = self.files.lock().expect("open cache poisoned");
        let before = files.len();
        files.retain(|_, cached| {
            cached
                .remote
                .as_ref()
                .is_none_or(|remote| remote.dev() != dev)
        });
        before - files.len()
    }

    pub fn len(&self) -> usize {
        self.files.lock().expect("open cache poisoned").len()
            + self.dirs.lock().expect("open cache poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drops entries until the map is back under `limit`.
///
/// A quarter at a time rather than one at a time, so a map sitting exactly on
/// the limit does not pay an eviction on every single insertion.
fn trim<T>(map: &mut HashMap<u64, T>, limit: usize) {
    if map.len() <= limit {
        return;
    }
    let excess = map.len() - limit + limit / 4;
    let doomed: Vec<u64> = map.keys().take(excess).copied().collect();
    for key in doomed {
        map.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;

    fn spare_fd() -> OwnedFd {
        crate::sys::open_root(&std::env::temp_dir()).unwrap()
    }

    fn file(writable: bool) -> Arc<OpenFile> {
        Arc::new(OpenFile {
            fd: spare_fd(),
            readable: true,
            writable,
        })
    }

    #[test]
    fn a_cached_descriptor_comes_back() {
        let cache = OpenCache::new();
        cache.put_file(7, file(true), None);
        assert!(cache.file(7, false).is_some());
        assert!(cache.file(8, false).is_none());
    }

    /// The upgrade path. Most files are only ever read, so descriptors start
    /// read-only; a write has to miss rather than fail on a descriptor that
    /// cannot serve it.
    #[test]
    fn a_read_only_descriptor_is_a_miss_for_a_write() {
        let cache = OpenCache::new();
        cache.put_file(7, file(false), None);
        assert!(cache.file(7, false).is_some());
        assert!(cache.file(7, true).is_none());
    }

    #[test]
    fn eviction_removes_what_it_is_asked_to() {
        let cache = OpenCache::new();
        cache.put_file(7, file(true), None);
        cache.evict(7);
        assert!(cache.file(7, false).is_none());
        assert!(cache.is_empty());
    }

    /// The cache exists to hold descriptors, which are the scarcest thing the
    /// process has. It must have a ceiling.
    #[test]
    fn the_cache_stays_under_its_limit() {
        let cache = OpenCache::new();
        for nodeid in 0..(FILE_LIMIT as u64 * 2) {
            cache.put_file(nodeid, file(true), None);
        }
        assert!(
            cache.len() <= FILE_LIMIT,
            "the cache grew to {} entries",
            cache.len()
        );
    }

    /// A network volume's files are kept to its allowance however much room
    /// the cache has, and counted out however they leave.
    #[test]
    fn a_network_volume_keeps_only_its_allowance_of_files() {
        let remotes = crate::remote::Remotes::default();
        remotes.set(7, true);
        let remote = remotes.get(7).unwrap();
        let allowance = remote.allowance().1;
        let cache = OpenCache::new();
        for nodeid in 0..(allowance as u64 * 4) {
            cache.put_file(nodeid, file(false), Some(remote.clone()));
            assert!(
                remote.holding().1 <= allowance,
                "{} files cached on the volume against an allowance of {allowance}",
                remote.holding().1
            );
        }
        // The newest is always kept: the read that opened it is about to use it.
        assert!(cache.file(allowance as u64 * 4 - 1, false).is_some());
        // Files elsewhere are not the volume's to evict.
        cache.put_file(1_000_000, file(false), None);
        let on_volume = remote.holding().1;
        assert_eq!(cache.evict_remote(7), on_volume);
        assert_eq!(remote.holding().1, 0);
        assert!(cache.file(1_000_000, false).is_some());
    }

    /// Eviction must not close a descriptor another thread is mid-read on.
    #[test]
    fn an_evicted_descriptor_stays_alive_for_its_current_user() {
        let cache = OpenCache::new();
        cache.put_file(7, file(true), None);
        let borrowed = cache.file(7, false).unwrap();
        cache.evict(7);
        // Still a live descriptor: `fstat` on a closed one would fail.
        assert!(crate::sys::stat_fd(std::os::fd::AsRawFd::as_raw_fd(&borrowed.fd)).is_ok());
    }
}

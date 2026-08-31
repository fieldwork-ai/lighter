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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::inode::{OpenDir, OpenFile};

/// How many of each kind to keep. Sized to be comfortably under the descriptor
/// limit while still covering the working set of a build: a compiler with a
/// thousand headers open at once is ordinary, ten thousand is not.
const FILE_LIMIT: usize = 2048;
const DIR_LIMIT: usize = 512;

#[derive(Default)]
pub struct OpenCache {
    files: Mutex<HashMap<u64, Arc<OpenFile>>>,
    dirs: Mutex<HashMap<u64, Arc<OpenDir>>>,
}

impl OpenCache {
    pub fn new() -> OpenCache {
        OpenCache::default()
    }

    /// The cached descriptor for an inode, if there is one and it is good
    /// enough for what the caller intends.
    pub fn file(&self, nodeid: u64, need_write: bool) -> Option<Arc<OpenFile>> {
        let files = self.files.lock().expect("open cache poisoned");
        let file = files.get(&nodeid)?;
        // A read-only descriptor cannot serve a write, and the caller will
        // replace it. Reporting a miss is how it finds out.
        (!need_write || file.writable).then(|| file.clone())
    }

    pub fn put_file(&self, nodeid: u64, file: Arc<OpenFile>) {
        let mut files = self.files.lock().expect("open cache poisoned");
        files.insert(nodeid, file);
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
        trim(&mut dirs, DIR_LIMIT);
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
            append: false,
            writable,
        })
    }

    #[test]
    fn a_cached_descriptor_comes_back() {
        let cache = OpenCache::new();
        cache.put_file(7, file(true));
        assert!(cache.file(7, false).is_some());
        assert!(cache.file(8, false).is_none());
    }

    /// The upgrade path. Most files are only ever read, so descriptors start
    /// read-only; a write has to miss rather than fail on a descriptor that
    /// cannot serve it.
    #[test]
    fn a_read_only_descriptor_is_a_miss_for_a_write() {
        let cache = OpenCache::new();
        cache.put_file(7, file(false));
        assert!(cache.file(7, false).is_some());
        assert!(cache.file(7, true).is_none());
    }

    #[test]
    fn eviction_removes_what_it_is_asked_to() {
        let cache = OpenCache::new();
        cache.put_file(7, file(true));
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
            cache.put_file(nodeid, file(true));
        }
        assert!(
            cache.len() <= FILE_LIMIT,
            "the cache grew to {} entries",
            cache.len()
        );
    }

    /// Eviction must not close a descriptor another thread is mid-read on.
    #[test]
    fn an_evicted_descriptor_stays_alive_for_its_current_user() {
        let cache = OpenCache::new();
        cache.put_file(7, file(true));
        let borrowed = cache.file(7, false).unwrap();
        cache.evict(7);
        // Still a live descriptor: `fstat` on a closed one would fail.
        assert!(crate::sys::stat_fd(std::os::fd::AsRawFd::as_raw_fd(&borrowed.fd)).is_ok());
    }
}

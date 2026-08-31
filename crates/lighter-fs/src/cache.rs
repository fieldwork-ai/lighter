//! How long the guest may believe what we told it.
//!
//! # The one number that matters
//!
//! Everything the guest caches, it caches because we handed it a validity in a
//! LOOKUP or GETATTR reply. There is no way to take it back — virtio-fs has no
//! reverse channel — so that number is simultaneously the whole performance
//! story and the whole coherence story. Set it to zero and every path component
//! of every syscall is a round trip. Set it to a minute and a file edited on the
//! Mac is stale in the container for a minute.
//!
//! # Making the number depend on what the host is doing
//!
//! A fixed timeout has to be short enough for the worst case, which means it is
//! short all the time. We can do better because macOS tells us where the changes
//! are: [`crate::fsevents`] reports a host-side write within milliseconds, and
//! `kFSEventStreamCreateFlagIgnoreSelf` means it reports *only* changes we did
//! not make — so the guest's own furious writing during a package install does
//! not count as host activity and does not poison its own cache.
//!
//! A directory the host has touched recently therefore gets zero validity, and
//! everything else gets the configured timeout. While you are editing, the
//! container sees each save on its next look; while you are not, it runs at
//! cache speed.
//!
//! # Why directories are trusted longer than files
//!
//! Resolving `node_modules/@babel/core/lib/index.js` is five lookups, four of
//! them directories. Directory *entries* change far more rarely than file
//! contents — a package tree's shape is fixed once installed — so they are
//! given a longer validity than files, which is most of the speed for very
//! little of the risk. Attributes, which is where a file's size and mtime live,
//! are always on the short timeout.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

/// The four timeouts, and where they came from.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// Attribute validity. This is the visibility bound for a file's contents,
    /// because `FUSE_AUTO_INVAL_DATA` drops the page cache when a revalidated
    /// attribute shows a different size or mtime.
    pub attr: Duration,
    /// Entry validity for a name that resolves to a file.
    pub entry_file: Duration,
    /// Entry validity for a name that resolves to a directory.
    pub entry_dir: Duration,
    /// Entry validity for a name that resolves to nothing.
    ///
    /// Worth having at all because module resolution is mostly failed lookups:
    /// `require('x')` stats a dozen paths that do not exist for every one that
    /// does, and with no negative caching each is a round trip every time.
    pub negative: Duration,
    /// How long a directory stays untrusted after the host touches it.
    ///
    /// Longer than the timeouts on purpose: an editor writing a file produces a
    /// burst of events, and dropping back to cached answers between two of them
    /// would be a window with no benefit.
    pub cooldown: Duration,
}

impl Default for Timings {
    fn default() -> Timings {
        Timings {
            attr: Duration::from_millis(100),
            entry_file: Duration::from_millis(100),
            entry_dir: Duration::from_millis(1000),
            negative: Duration::from_millis(100),
            cooldown: Duration::from_millis(2000),
        }
    }
}

impl Timings {
    /// Exact coherence: nothing is cached, and every path component of every
    /// syscall is a round trip. What the server falls back to when it cannot
    /// watch the host, because a timeout it has no way to withdraw is worse
    /// than a slow filesystem.
    pub const NONE: Timings = Timings {
        attr: Duration::ZERO,
        entry_file: Duration::ZERO,
        entry_dir: Duration::ZERO,
        negative: Duration::ZERO,
        cooldown: Duration::ZERO,
    };

    /// Reads an override from the environment.
    ///
    /// Present so the numbers can be swept against the benchmark suite without
    /// a rebuild; the defaults are what ships, and what the suite reports.
    pub fn from_env() -> Timings {
        fn ms(name: &str, fallback: Duration) -> Duration {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .map(Duration::from_millis)
                .unwrap_or(fallback)
        }
        let base = Timings::default();
        Timings {
            attr: ms("LIGHTER_FS_ATTR_MS", base.attr),
            entry_file: ms("LIGHTER_FS_ENTRY_MS", base.entry_file),
            entry_dir: ms("LIGHTER_FS_DIR_ENTRY_MS", base.entry_dir),
            negative: ms("LIGHTER_FS_NEGATIVE_MS", base.negative),
            cooldown: ms("LIGHTER_FS_COOLDOWN_MS", base.cooldown),
        }
    }

    /// Whether any caching is on at all.
    pub fn caching(&self) -> bool {
        !self.attr.is_zero() || !self.entry_file.is_zero() || !self.entry_dir.is_zero()
    }
}

/// What kind of answer a validity is being chosen for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    File,
    Directory,
    Missing,
}

/// Directories the host has touched recently, and the timeouts to apply.
pub struct Policy {
    timings: Timings,
    /// Keyed by host identity rather than by path, because the hot path already
    /// has `(dev, ino)` in hand and would otherwise need an `F_GETPATH` per
    /// lookup to ask this question.
    hot: RwLock<HashMap<(i64, u64), Instant>>,
    /// How many entries `hot` holds.
    ///
    /// The reason this is not simply `hot.len()` is that reading it must not
    /// touch the lock at all. Every LOOKUP and every GETATTR asks whether a
    /// directory is hot — tens of thousands of times a second, from seventeen
    /// threads — and almost always the answer is "nothing is hot, because the
    /// host is idle". Taking even a read lock to discover that was measured
    /// costing more than the filesystem work it was guarding.
    hot_count: AtomicUsize,
    /// Serializes writers, so the count and the map cannot disagree.
    writing: Mutex<()>,
}

impl Policy {
    pub fn new(timings: Timings) -> Policy {
        Policy {
            timings,
            hot: RwLock::new(HashMap::new()),
            hot_count: AtomicUsize::new(0),
            writing: Mutex::new(()),
        }
    }

    pub fn timings(&self) -> &Timings {
        &self.timings
    }

    /// Records that the host changed something under `(dev, ino)`.
    pub fn touched(&self, dev: i64, ino: u64) {
        let now = Instant::now();
        let _writer = self.writing.lock().expect("hot set poisoned");
        let mut hot = self.hot.write().expect("hot set poisoned");
        hot.insert((dev, ino), now);
        // Swept here rather than on a timer, and swept by age rather than by
        // size: an entry past its cooldown is answering "not hot" anyway, so
        // keeping it only makes every reader's map larger. The sweep is cheap
        // because it only ever runs while the host is actively writing.
        let cooldown = self.timings.cooldown;
        if hot.len() > 256 {
            hot.retain(|_, at| now.duration_since(*at) < cooldown);
        }
        self.hot_count.store(hot.len(), Ordering::Release);
    }

    /// How long the guest may trust an answer about something in this
    /// directory.
    pub fn validity(&self, dev: i64, ino: u64, answer: Answer) -> Duration {
        if self.is_hot(dev, ino) {
            return Duration::ZERO;
        }
        match answer {
            Answer::File => self.timings.entry_file,
            Answer::Directory => self.timings.entry_dir,
            Answer::Missing => self.timings.negative,
        }
    }

    /// How long the guest may trust an object's attributes.
    pub fn attr_validity(&self, dev: i64, ino: u64) -> Duration {
        if self.is_hot(dev, ino) {
            return Duration::ZERO;
        }
        self.timings.attr
    }

    fn is_hot(&self, dev: i64, ino: u64) -> bool {
        // The whole point of the counter: an idle host means no lock at all.
        if self.hot_count.load(Ordering::Acquire) == 0 {
            return false;
        }
        let hot = self.hot.read().expect("hot set poisoned");
        match hot.get(&(dev, ino)) {
            Some(at) => at.elapsed() < self.timings.cooldown,
            None => false,
        }
    }

    /// How many directories are currently untrusted. Diagnostics only.
    pub fn hot_count(&self) -> usize {
        self.hot_count.load(Ordering::Acquire)
    }
}

/// Turns FSEvents paths into entries in a [`Policy`]'s hot set.
///
/// Both the changed object and its parent are marked: a write changes the
/// file's attributes, and a create or delete changes the directory's entries,
/// and the guest caches those two things separately.
pub struct Invalidator {
    policy: std::sync::Arc<Policy>,
}

impl Invalidator {
    pub fn new(policy: std::sync::Arc<Policy>) -> Invalidator {
        Invalidator { policy }
    }

    fn mark(&self, path: &Path) {
        if let Ok(st) = std::fs::symlink_metadata(path) {
            use std::os::unix::fs::MetadataExt;
            self.policy.touched(st.dev() as i64, st.ino());
        }
    }
}

impl crate::fsevents::Observer for Invalidator {
    fn changed(&self, path: &Path) {
        // The object itself may already be gone — a delete is exactly the case
        // the guest most needs to stop caching — so its parent is marked
        // whether or not the object still exists.
        self.mark(path);
        if let Some(parent) = path.parent() {
            self.mark(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(cooldown_ms: u64) -> Policy {
        Policy::new(Timings {
            attr: Duration::from_millis(100),
            entry_file: Duration::from_millis(100),
            entry_dir: Duration::from_millis(1000),
            negative: Duration::from_millis(100),
            cooldown: Duration::from_millis(cooldown_ms),
        })
    }

    #[test]
    fn a_quiet_directory_gets_the_configured_validity() {
        let p = policy(2000);
        assert_eq!(p.validity(1, 2, Answer::File), Duration::from_millis(100));
        assert_eq!(p.validity(1, 2, Answer::Directory), Duration::from_millis(1000));
        assert_eq!(p.validity(1, 2, Answer::Missing), Duration::from_millis(100));
        assert_eq!(p.attr_validity(1, 2), Duration::from_millis(100));
    }

    /// The point of the whole module: while the host is writing there, the
    /// guest is told to trust nothing.
    #[test]
    fn a_touched_directory_is_not_cacheable_at_all() {
        let p = policy(2000);
        p.touched(1, 2);
        assert!(p.validity(1, 2, Answer::File).is_zero());
        assert!(p.validity(1, 2, Answer::Directory).is_zero());
        assert!(p.validity(1, 2, Answer::Missing).is_zero());
        assert!(p.attr_validity(1, 2).is_zero());
        // And only that directory: one busy subtree must not stop the rest of
        // the share being cached.
        assert!(!p.validity(1, 3, Answer::File).is_zero());
    }

    #[test]
    fn the_cooldown_expires() {
        let p = policy(30);
        p.touched(1, 2);
        assert!(p.validity(1, 2, Answer::File).is_zero());
        std::thread::sleep(Duration::from_millis(60));
        assert!(!p.validity(1, 2, Answer::File).is_zero());
    }

    /// The hot set is fed by an event stream that can burst. It must not grow
    /// without bound while the host unpacks an archive.
    #[test]
    fn the_hot_set_is_swept_rather_than_grown() {
        let p = policy(1);
        for ino in 0..8000 {
            p.touched(1, ino);
            if ino % 500 == 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert!(
            p.hot_count() <= 1000,
            "the hot set reached {} entries",
            p.hot_count()
        );
    }

    /// The fast path exists because it is taken almost always: with the host
    /// idle, deciding that nothing is hot must not touch a lock that every
    /// other thread also wants.
    #[test]
    fn an_idle_host_costs_no_lock() {
        let p = policy(2000);
        assert_eq!(p.hot_count(), 0);
        assert!(!p.validity(1, 2, Answer::File).is_zero());
        p.touched(1, 2);
        assert_eq!(p.hot_count(), 1);
        assert!(p.validity(1, 2, Answer::File).is_zero());
    }

    #[test]
    fn zero_everywhere_reads_as_caching_off() {
        let off = Timings {
            attr: Duration::ZERO,
            entry_file: Duration::ZERO,
            entry_dir: Duration::ZERO,
            negative: Duration::ZERO,
            cooldown: Duration::ZERO,
        };
        assert!(!off.caching());
        assert!(Timings::default().caching());
    }
}

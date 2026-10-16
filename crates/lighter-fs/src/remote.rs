//! What a share may hold open on a network volume.
//!
//! # Why a network volume has an allowance of its own
//!
//! A descriptor on the Mac's own disk costs this process a slot in its table,
//! and the share's budget ([`crate::inode`]) is sized from that table: over a
//! hundred thousand on a Mac whose limit was raised. A descriptor on an SMB
//! or NFS volume costs that too, and an open handle on the server besides,
//! and the server's limit is not ours to read. Samba's is its process's own
//! descriptor limit, shared by every client of the share; a QNAP's is lower
//! than a photo library. Held to the Mac's budget, a container that walked a
//! library left one handle per photo open on the NAS until the guest forgot
//! the file, which a guest with memory to spare never does. Once the server
//! was out, every open on the share failed, the Mac's own included: Immich
//! saw EMFILE, and folders listed empty in containers and in Finder alike
//! (#93).
//!
//! So a network volume holds little and holds it for a reason. Its files hold
//! no descriptor between requests: each is reached through its directory and
//! its name, as a parked file always has been. Its directories, which every
//! name inside them is reached through, stay resident up to an allowance, and
//! the open cache keeps an allowance of its files open for reads. Both are a
//! few dozen handles, which is what a Finder window costs the same server.
//!
//! # A server that refuses anyway
//!
//! The allowance is a guess about a server lighter cannot ask. A server that
//! refuses an open (EMFILE, ENFILE, or EAGAIN from the Mac's SMB client) is
//! out of handles, whatever the reason, so the share lets go of everything
//! idle it holds on that volume, lowers the volume's allowance to half what
//! it held, and serves the request once more.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// How many directories a network volume keeps resident to begin with.
///
/// The top of a tree is looked up first and is on the path to everything
/// below it, so a resident set filled in arrival order holds the directories
/// that save the most: every name further down is reached from one of them
/// in a single `openat`.
const DIRS: usize = 128;
/// How many files the open cache keeps open on a network volume to begin
/// with: more than a container reads at once, which is what it is for.
const FILES: usize = 64;
/// The least either allowance is lowered to.
const FLOOR: usize = 8;
/// How long after letting go a refusal is taken to be the same shortage.
/// Every worker refused at once would otherwise each let go and each halve
/// the allowance, sixteen halvings for one shortage.
const RELIEF_EVERY: Duration = Duration::from_secs(1);

/// One network volume, as every share of a process sees it.
#[derive(Debug)]
pub struct Remote {
    dev: i64,
    /// Metadata descriptors held resident on it, across the pool's shares:
    /// directories, files being reached through them.
    resident: AtomicUsize,
    /// Files the open caches hold open on it.
    files: AtomicUsize,
    resident_allowance: AtomicUsize,
    file_allowance: AtomicUsize,
    /// How many times its server has refused an open.
    refusals: AtomicU64,
    /// When the shares last let go of what they held here. Locked across
    /// the letting go, so a worker refused meanwhile waits for it and is
    /// served after, against a server with room.
    relieved: Mutex<Option<Instant>>,
    /// How many times the shares have let go of what they held here.
    reliefs: AtomicU64,
}

impl Remote {
    fn new(dev: i64) -> Remote {
        Remote {
            dev,
            resident: AtomicUsize::new(0),
            files: AtomicUsize::new(0),
            resident_allowance: AtomicUsize::new(allowance("LIGHTER_FS_REMOTE_DIRS", DIRS)),
            file_allowance: AtomicUsize::new(allowance("LIGHTER_FS_REMOTE_FILES", FILES)),
            refusals: AtomicU64::new(0),
            relieved: Mutex::new(None),
            reliefs: AtomicU64::new(0),
        }
    }

    pub fn dev(&self) -> i64 {
        self.dev
    }

    /// Whether another directory may be held resident.
    pub fn room(&self) -> bool {
        self.resident.load(Ordering::Relaxed) < self.resident_allowance.load(Ordering::Relaxed)
    }

    pub(crate) fn held(&self) {
        self.resident.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn released(&self) {
        self.resident.fetch_sub(1, Ordering::Relaxed);
    }

    /// How many files the open cache holds past the allowance.
    pub(crate) fn files_over(&self) -> usize {
        self.files
            .load(Ordering::Relaxed)
            .saturating_sub(self.file_allowance.load(Ordering::Relaxed))
    }

    pub(crate) fn file_allowance(&self) -> usize {
        self.file_allowance.load(Ordering::Relaxed)
    }

    pub(crate) fn cached_file(&self) {
        self.files.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn uncached_file(&self) {
        self.files.fetch_sub(1, Ordering::Relaxed);
    }

    /// What the volume holds: resident descriptors, cached files.
    pub fn holding(&self) -> (usize, usize) {
        (
            self.resident.load(Ordering::Relaxed),
            self.files.load(Ordering::Relaxed),
        )
    }

    /// The most it may hold of each.
    pub fn allowance(&self) -> (usize, usize) {
        (
            self.resident_allowance.load(Ordering::Relaxed),
            self.file_allowance.load(Ordering::Relaxed),
        )
    }

    /// The server refused an open with `errno`: unless the shares let go
    /// of what they held here within [`RELIEF_EVERY`], lowers each
    /// allowance to half of what the volume held, if that is lower, and has
    /// `release` close what is idle. Returns what `release` closed. Says so
    /// the first time, and every hundredth time after.
    pub(crate) fn relieve(&self, errno: i32, release: impl FnOnce() -> usize) -> usize {
        self.refusals.fetch_add(1, Ordering::Relaxed);
        let mut relieved = self.relieved.lock().expect("relief poisoned");
        if relieved.is_some_and(|at| at.elapsed() < RELIEF_EVERY) {
            return 0;
        }
        let (resident, files) = self.holding();
        let lower = |allowance: &AtomicUsize, held: usize| {
            allowance.fetch_min((held / 2).max(FLOOR), Ordering::Relaxed);
        };
        lower(&self.resident_allowance, resident);
        lower(&self.file_allowance, files);
        let closed = release();
        let first = relieved.is_none();
        *relieved = Some(Instant::now());
        let reliefs = self.reliefs.fetch_add(1, Ordering::Relaxed);
        if first || reliefs.is_multiple_of(100) {
            let (resident_allowance, file_allowance) = self.allowance();
            tracing::warn!(
                dev = self.dev,
                errno,
                refusals = self.refusals(),
                resident,
                files,
                closed,
                resident_allowance,
                file_allowance,
                "a network volume's server refused to open more files; lighter let go of what it held there and holds less from now on"
            );
        }
        closed
    }

    /// How many times its server has refused an open.
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }
}

fn allowance(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

/// The network volumes a process's shares have learned, by device.
#[derive(Debug, Default)]
pub struct Remotes {
    /// Whether there is any: the question asked on every new inode, which on
    /// a Mac with no network volume must cost no lock.
    any: AtomicBool,
    by_dev: RwLock<HashMap<i64, Arc<Remote>>>,
}

impl Remotes {
    /// The volume `dev` is, if it is a network one.
    pub fn get(&self, dev: i64) -> Option<Arc<Remote>> {
        if !self.any.load(Ordering::Relaxed) {
            return None;
        }
        self.by_dev
            .read()
            .expect("remote volumes poisoned")
            .get(&dev)
            .cloned()
    }

    /// Records whether `dev` is a network volume. Learning one twice keeps
    /// what the first learned, its allowance included.
    pub fn set(&self, dev: i64, remote: bool) {
        let mut by_dev = self.by_dev.write().expect("remote volumes poisoned");
        if remote {
            by_dev
                .entry(dev)
                .or_insert_with(|| Arc::new(Remote::new(dev)));
        } else {
            by_dev.remove(&dev);
        }
        self.any.store(!by_dev.is_empty(), Ordering::Relaxed);
    }
}

/// Whether an open failed because the server behind a network volume has no
/// handle left to give, in Linux numbers. The Mac's SMB client reports
/// Samba's `NT_STATUS_TOO_MANY_OPENED_FILES` as EAGAIN, and passes the
/// descriptor errors through as they are. EAGAIN means nothing else here:
/// the guest keeps its own locks, so no request the server answers is
/// refused for a contested one.
pub fn is_refusal(errno: i32) -> bool {
    use crate::errno::linux;
    errno == linux::EMFILE || errno == linux::ENFILE || errno == linux::EAGAIN
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holding(remote: &Remote, resident: usize, files: usize) {
        remote.resident.store(resident, Ordering::Relaxed);
        remote.files.store(files, Ordering::Relaxed);
    }

    fn relieve(remote: &Remote) -> bool {
        let mut released = false;
        remote.relieve(crate::errno::linux::EMFILE, || {
            released = true;
            0
        });
        // As if a second earlier, so the next refusal is a new shortage.
        *remote.relieved.lock().unwrap() = Some(Instant::now() - RELIEF_EVERY);
        released
    }

    #[test]
    fn a_refusal_lowers_each_allowance_to_half_of_what_was_held() {
        let remote = Remote::new(7);
        assert_eq!(remote.allowance(), (DIRS, FILES));
        holding(&remote, 100, 40);
        assert!(relieve(&remote));
        assert_eq!(remote.allowance(), (50, 20));
        // Never raised by a later refusal while holding more.
        holding(&remote, 120, 60);
        assert!(relieve(&remote));
        assert_eq!(remote.allowance(), (50, 20));
        // Never below the floor, however little was held.
        holding(&remote, 3, 0);
        assert!(relieve(&remote));
        assert_eq!(remote.allowance(), (FLOOR, FLOOR));
        assert_eq!(remote.refusals(), 3);
    }

    /// Workers refused together are one shortage: one of them lets go and
    /// lowers the allowance, and the rest are served after it.
    #[test]
    fn refusals_together_let_go_once() {
        let remote = Remote::new(7);
        holding(&remote, 100, 40);
        let mut released = 0;
        for _ in 0..16 {
            remote.relieve(crate::errno::linux::EAGAIN, || {
                released += 1;
                0
            });
        }
        assert_eq!(released, 1);
        assert_eq!(remote.allowance(), (50, 20));
        assert_eq!(remote.refusals(), 16);
    }

    #[test]
    fn a_volume_learned_twice_keeps_its_allowance() {
        let remotes = Remotes::default();
        assert!(remotes.get(7).is_none());
        remotes.set(7, true);
        let remote = remotes.get(7).unwrap();
        holding(&remote, 20, 20);
        relieve(&remote);
        holding(&remote, 0, 0);
        remotes.set(7, true);
        assert_eq!(remotes.get(7).unwrap().allowance(), (10, 10));
        remotes.set(7, false);
        assert!(remotes.get(7).is_none());
    }
}

//! The queue that lets a mutation be acknowledged before it has happened.
//!
//! # Why the guest should not wait for APFS
//!
//! A package install is six hundred thousand filesystem requests issued one
//! at a time, and the guest spends the full APFS latency of each one blocked:
//! forty-seven microseconds for a create, forty for an unlink, ten for a
//! write. Measured on the large fixture, that serial track *is* the wall
//! clock — ten and a half of ten point seven seconds. The guest gains nothing
//! by waiting. It never looks at the result except through later filesystem
//! operations, and those we can answer ourselves.
//!
//! So the server acknowledges the request as soon as it is safe to describe
//! its outcome, and performs the syscall a moment later, in order, on this
//! queue. The guest's serial track shrinks to the acknowledgement cost and
//! the wall clock snaps to the workload's own compute.
//!
//! # What "safe" means
//!
//! Three promises, kept in this order of importance:
//!
//! 1. **Reads never lie.** Any operation whose answer could be changed by a
//!    queued job either consults an overlay that already knows the outcome
//!    (a written size) or drains the queue first (a read of the bytes). The
//!    barrier points are in `server.rs`, at the top of the handlers they
//!    protect.
//! 2. **Durability is never claimed early.** `fsync` and `DESTROY` drain
//!    before they reply. When the guest has been told data is on the Mac, it
//!    is.
//! 3. **Errors are not swallowed.** A job that fails parks its errno on the
//!    inode it was for, where the next write, read or fsync of that file
//!    picks it up — the same posture as the kernel's own writeback. Running
//!    out of disk is refused *before* acknowledgement wherever possible: the
//!    drainer keeps a free-space figure fresh, and the acknowledging path
//!    falls back to synchronous service when the volume is close to full, so
//!    ENOSPC arrives on the write that hits it rather than on a later fsync.
//!
//! # Shape
//!
//! One queue per share, one drainer thread, jobs applied strictly in arrival
//! order. A single drainer is not a bottleneck: APFS serializes metadata
//! internally, so a second thread would only queue inside the kernel instead
//! of here — and ordering across every file and directory comes free, which
//! is what makes the overlay reasoning tractable.
//!
//! `LIGHTER_FS_ASYNC=0` turns the whole thing off, so any suspected
//! misbehavior can be re-tested against synchronous service in one boot.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// A queued syscall, already acknowledged to the guest. It reports failure to
/// the inode it belongs to; the queue itself only sequences.
pub struct Job {
    run: Box<dyn FnOnce() + Send>,
    /// Payload bytes held in memory until applied, for backpressure.
    bytes: usize,
    /// What kind of work, for the drainer's own histogram.
    kind: Kind,
}

/// The kinds of job the queue applies, for accounting: when the drainer is
/// the bottleneck, which syscalls it spends its time in is the question.
#[derive(Clone, Copy)]
pub enum Kind {
    Create,
    Write,
    Unlink,
    Rename,
    Clone,
    Setattr,
}

const KINDS: usize = 6;

impl Job {
    pub fn new(bytes: usize, run: impl FnOnce() + Send + 'static) -> Job {
        Job::of(Kind::Write, bytes, run)
    }

    pub fn of(kind: Kind, bytes: usize, run: impl FnOnce() + Send + 'static) -> Job {
        Job {
            run: Box::new(run),
            bytes,
            kind,
        }
    }
}

/// Payload the queue may hold before acknowledgement starts waiting.
///
/// Backpressure, not a limit on correctness: a guest writing faster than the
/// disk absorbs indefinitely must eventually feel the disk, or the queue
/// becomes an unbounded copy of the workload's output. Sixty-four megabytes
/// rides out any burst a package manager produces while capping what a crash
/// of the VMM could lose to roughly one second of disk work.
const BYTES_CAP: usize = 64 << 20;

/// Jobs the queue may hold before acknowledgement starts waiting.
///
/// The bytes cap bounds memory; this bounds *contention*. An unbounded queue
/// let the guest race the whole install ahead of APFS, and the drainer then
/// ran flat out for tens of seconds — against which every synchronous
/// operation left (a lookup's stat, an acknowledgement's probe) contended
/// inside APFS, measured degrading from three microseconds to thirty as the
/// backlog deepened. A bounded window keeps the overlap that makes
/// acknowledgement worth anything while capping how much concurrent mutation
/// the rest of the filesystem has to live beside.
fn jobs_cap() -> usize {
    std::env::var("LIGHTER_FS_APPLY_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1024)
}

/// Below this much free space, acknowledgement stops and service goes back to
/// synchronous, so ENOSPC lands on the operation that earned it.
const FREE_FLOOR: u64 = 512 << 20;

/// How many jobs between refreshes of the free-space figure.
const FREE_EVERY: u64 = 256;

struct State {
    /// `None` once retired; the drainer exits when it sees that.
    jobs: Option<VecDeque<Job>>,
}

pub struct Apply {
    shared: Arc<Shared>,
    drainer: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Off means every push is refused and callers do the work themselves.
    on: bool,
}

struct Shared {
    state: Mutex<State>,
    arrived: Condvar,
    /// Signaled as jobs finish: what `drain` and the byte cap wait on.
    settled: Condvar,
    /// Jobs queued and not yet applied. Read without the lock as the fast
    /// path of every barrier: a workload that is not writing pays one relaxed
    /// load to find the queue empty.
    depth: AtomicUsize,
    /// Jobs ever pushed; a job's sequence number is this counter after its
    /// push. `jobs_done` chasing it is what a scoped barrier waits on.
    pushed: AtomicU64,
    bytes: AtomicUsize,
    /// Free bytes on the share's volume, as of the drainer's last look.
    free: AtomicU64,
    /// The depth cap; see [`jobs_cap`].
    window: usize,
    /// Per-kind job counts and nanoseconds, the drainer's histogram.
    kind_count: [AtomicU64; KINDS],
    kind_nanos: [AtomicU64; KINDS],
    jobs_done: AtomicU64,
}

impl Apply {
    /// `statfs` is consulted through `root_fd`'s volume.
    pub fn start(root: std::path::PathBuf) -> Apply {
        let on = std::env::var("LIGHTER_FS_ASYNC").as_deref() != Ok("0");
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                jobs: Some(VecDeque::new()),
            }),
            arrived: Condvar::new(),
            settled: Condvar::new(),
            depth: AtomicUsize::new(0),
            pushed: AtomicU64::new(0),
            bytes: AtomicUsize::new(0),
            free: AtomicU64::new(u64::MAX),
            window: jobs_cap(),
            kind_count: std::array::from_fn(|_| AtomicU64::new(0)),
            kind_nanos: std::array::from_fn(|_| AtomicU64::new(0)),
            jobs_done: AtomicU64::new(0),
        });
        shared.refresh_free(&root);
        let drainer = if on {
            let shared = shared.clone();
            Some(
                std::thread::Builder::new()
                    .name("fs-apply".into())
                    .spawn(move || shared.run(&root))
                    .expect("failed to spawn the filesystem apply thread"),
            )
        } else {
            None
        };
        Apply {
            shared,
            drainer: Mutex::new(drainer),
            on,
        }
    }

    /// Whether a push would be accepted right now.
    ///
    /// The free-space check is what keeps promise 3: close to a full disk,
    /// everything goes back to synchronous and errors land where they belong.
    pub fn accepting(&self) -> bool {
        self.on
            && self.shared.free.load(Ordering::Relaxed)
                > FREE_FLOOR + self.shared.bytes.load(Ordering::Relaxed) as u64
    }

    /// Queues a job whose outcome has already been described to the guest,
    /// returning its sequence number — the mark a scoped barrier waits to.
    /// Blocks only when the queue holds more payload than [`BYTES_CAP`].
    pub fn push(&self, job: Job) -> u64 {
        let shared = &self.shared;
        let mut state = shared.state.lock().expect("apply queue poisoned");
        while shared.bytes.load(Ordering::Relaxed) > BYTES_CAP
            || shared.depth.load(Ordering::Relaxed) > shared.window
        {
            state = shared.settled.wait(state).expect("apply queue poisoned");
        }
        let Some(jobs) = state.jobs.as_mut() else {
            // Retired mid-flight; nothing will drain, so do it here.
            drop(state);
            (job.run)();
            return shared.jobs_done.load(Ordering::Relaxed);
        };
        shared.depth.fetch_add(1, Ordering::Relaxed);
        shared.bytes.fetch_add(job.bytes, Ordering::Relaxed);
        let seq = shared.pushed.fetch_add(1, Ordering::Relaxed) + 1;
        jobs.push_back(job);
        drop(state);
        shared.arrived.notify_one();
        seq
    }

    /// Returns once every job queued before the call has been applied.
    ///
    /// A watermark, not "until empty": under a sustained storm the queue may
    /// never be empty, and a barrier that waits for that is a livelock — it
    /// was measured as an RCU stall in the guest, a vCPU waiting on a reply
    /// behind a drain that other vCPUs kept feeding. Work queued *after* the
    /// call is not this barrier's business.
    ///
    /// The relaxed fast path is the whole cost for a clean queue, which is
    /// the common case at every barrier point.
    pub fn drain(&self) {
        self.drain_to(self.shared.pushed.load(Ordering::Relaxed));
    }

    /// Returns once the job with sequence `seq` — and everything queued
    /// before it — has been applied.
    ///
    /// This is the barrier almost every caller wants: an operation that
    /// depends on one file's queued work should not wait behind thirty
    /// thousand writes to files it has never heard of. Under a deep backlog
    /// that scoping is the difference between a barrier and a stall.
    pub fn drain_to(&self, seq: u64) {
        let shared = &self.shared;
        if shared.jobs_done.load(Ordering::Relaxed) >= seq {
            return;
        }
        let mut state = shared.state.lock().expect("apply queue poisoned");
        while shared.jobs_done.load(Ordering::Relaxed) < seq && state.jobs.is_some() {
            state = shared.settled.wait(state).expect("apply queue poisoned");
        }
    }

    /// The drainer's histogram since the last call, one line per kind.
    pub fn report(&self) -> String {
        const NAMES: [&str; KINDS] = ["create", "write", "unlink", "rename", "clone", "setattr"];
        let mut out = String::new();
        for (i, name) in NAMES.iter().enumerate() {
            let n = self.shared.kind_count[i].swap(0, Ordering::Relaxed);
            let ns = self.shared.kind_nanos[i].swap(0, Ordering::Relaxed);
            if n > 0 {
                out.push_str(&format!(
                    "APPLY {name:8} n={n:<8} total_ms={:<7} mean_us={:.1}\n",
                    ns / 1_000_000,
                    ns as f64 / n as f64 / 1000.0
                ));
            }
        }
        out
    }

    /// Jobs queued and not yet applied, for diagnostics.
    pub fn depth(&self) -> usize {
        self.shared.depth.load(Ordering::Relaxed)
    }

    /// Whether anything is queued: the one-load fast path that keeps the
    /// overlay checks off the clean-queue hot path.
    pub fn busy(&self) -> bool {
        self.shared.depth.load(Ordering::Relaxed) != 0
    }

    /// How many jobs have ever been applied; used by tests to prove a path
    /// went through the queue rather than around it.
    pub fn applied(&self) -> u64 {
        self.shared.jobs_done.load(Ordering::Relaxed)
    }
}

impl Drop for Apply {
    fn drop(&mut self) {
        self.drain();
        {
            let mut state = self.shared.state.lock().expect("apply queue poisoned");
            state.jobs = None;
        }
        self.shared.arrived.notify_all();
        self.shared.settled.notify_all();
        if let Some(handle) = self.drainer.lock().expect("apply queue poisoned").take() {
            let _ = handle.join();
        }
    }
}

impl Shared {
    fn run(&self, root: &std::path::Path) {
        loop {
            let job = {
                let mut state = self.state.lock().expect("apply queue poisoned");
                loop {
                    let Some(jobs) = state.jobs.as_mut() else {
                        return;
                    };
                    if let Some(job) = jobs.pop_front() {
                        break job;
                    }
                    state = self.arrived.wait(state).expect("apply queue poisoned");
                }
            };
            // A job that panics must not take the drainer with it: with no
            // drainer every barrier waits forever, which is a hung guest.
            let bytes = job.bytes;
            let kind = job.kind as usize;
            let started = std::time::Instant::now();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || (job.run)())).is_err()
            {
                tracing::error!("an apply job panicked; the queue continues");
            }
            self.kind_count[kind].fetch_add(1, Ordering::Relaxed);
            self.kind_nanos[kind].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            self.bytes.fetch_sub(bytes, Ordering::Relaxed);
            self.depth.fetch_sub(1, Ordering::Relaxed);
            let done = self.jobs_done.fetch_add(1, Ordering::Relaxed) + 1;
            if done.is_multiple_of(FREE_EVERY) {
                self.refresh_free(root);
            }
            // The lock is taken and dropped before notifying, and that is not
            // decoration: the counters above are atomics mutated outside the
            // mutex, so without this a waiter can observe the old count, and
            // the notification can fire in the gap before it sleeps — a lost
            // wake-up, and a barrier that sleeps forever on a queue that is
            // already empty. Holding the mutex for even an instant orders the
            // update before any waiter's re-check.
            drop(self.state.lock().expect("apply queue poisoned"));
            self.settled.notify_all();
        }
    }

    fn refresh_free(&self, root: &std::path::Path) {
        if let Ok(st) = crate::sys::statfs(root) {
            let free = st.f_bavail.saturating_mul(st.f_bsize as u64);
            self.free.store(free, Ordering::Relaxed);
        }
    }
}

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
//! One queue per share, a few worker threads, and an order that is total
//! where it matters and free where it does not.
//!
//! Every job names the inodes it touches — the directory it changes, the
//! file it writes — and two jobs that share one are applied in the order they
//! were queued. Jobs that share nothing run side by side. That is exactly the
//! order the overlays reason about: a directory's promises are made and kept
//! in sequence, a file's writes land in sequence, and a clone lands after the
//! writes to its source; what happens in one directory has no bearing on
//! another, and the guest's own processes were racing each other across
//! directories anyway.
//!
//! One thread was the first design, on the theory that APFS serializes
//! metadata internally and a second thread would only queue in the kernel.
//! Measured, it does not: cloning small files from four threads runs at a
//! third of the per-file latency of one, and a pnpm install is sixty-six
//! thousand clones. With one thread the drainer was the wall clock — a
//! hundred microseconds a clone, end to end, while every acknowledgement
//! waited its turn in the window.
//!
//! `LIGHTER_FS_ASYNC=0` turns the whole thing off, so any suspected
//! misbehavior can be re-tested against synchronous service in one boot;
//! `LIGHTER_FS_APPLY_THREADS=1` keeps the queue and removes the concurrency,
//! which separates the two kinds of bug.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// The inodes a job touches: what it is ordered against.
///
/// Nodeids, up to four — a rename names two directories, the file and what
/// it displaces. Zero is never a nodeid and marks an unused slot.
#[derive(Clone, Copy)]
pub struct Keys([u64; 4]);

impl Keys {
    pub fn of(keys: &[u64]) -> Keys {
        let mut out = [0u64; 4];
        let mut n = 0;
        for &key in keys {
            if key != 0 && !out[..n].contains(&key) {
                assert!(n < 4, "a job names at most four inodes");
                out[n] = key;
                n += 1;
            }
        }
        assert!(n > 0, "a job must name the inode it touches");
        Keys(out)
    }

    fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().copied().filter(|&k| k != 0)
    }
}

/// A queued syscall, already acknowledged to the guest. It reports failure to
/// the inode it belongs to; the queue itself only sequences.
pub struct Job {
    run: Box<dyn FnOnce() + Send>,
    /// Payload bytes held in memory until applied, for backpressure.
    bytes: usize,
    /// What kind of work, for the drainer's own histogram.
    kind: Kind,
    keys: Keys,
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
    Link,
    Mkdir,
    Symlink,
}

const KINDS: usize = 9;

const fn kind_of(index: usize) -> Kind {
    match index {
        0 => Kind::Create,
        1 => Kind::Write,
        2 => Kind::Unlink,
        3 => Kind::Rename,
        4 => Kind::Clone,
        5 => Kind::Setattr,
        6 => Kind::Link,
        7 => Kind::Mkdir,
        _ => Kind::Symlink,
    }
}

impl Job {
    pub fn of(kind: Kind, keys: Keys, bytes: usize, run: impl FnOnce() + Send + 'static) -> Job {
        Job {
            run: Box::new(run),
            bytes,
            kind,
            keys,
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

/// How many workers apply jobs: one applies everything in order; the rest
/// let clones run beside it (see `State::serial_running`). Three, four and
/// five measured the same on a pnpm install — clone throughput on APFS
/// stops improving past about three — so three it is.
fn workers() -> usize {
    std::env::var("LIGHTER_FS_APPLY_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(3)
}

/// Below this much free space, acknowledgement stops and service goes back to
/// synchronous, so ENOSPC lands on the operation that earned it.
const FREE_FLOOR: u64 = 512 << 20;

/// How many jobs between refreshes of the free-space figure.
const FREE_EVERY: u64 = 256;

/// A job the queue holds: waiting on predecessors, ready, or running.
struct Slot {
    /// Taken by the worker that runs it.
    job: Option<Job>,
    /// Predecessors not yet applied. Ready at zero.
    waiting: usize,
    /// Jobs waiting on this one.
    successors: Vec<u64>,
    done: bool,
}

struct State {
    retired: bool,
    /// Whether a job that is not a clone is running. Only clones run beside
    /// one another; everything else keeps to one at a time.
    ///
    /// Four workers applying everything were slower at everything: APFS
    /// hands a directory between threads at a cost each time, and creates,
    /// renames and unlinks in one tree collide constantly. Clones are the
    /// one job whose cost is inside the file rather than the directory —
    /// measured, four threads cloning small files reach a third of the
    /// per-file latency of one — and the one job a pnpm install is made of.
    serial_running: bool,
    slots: HashMap<u64, Slot>,
    /// Ready to run, oldest first.
    ready: VecDeque<u64>,
    /// The most recent job queued on each key, while it is still held.
    tails: HashMap<u64, u64>,
    /// Sequence numbers queued and not yet applied.
    incomplete: BTreeSet<u64>,
}

pub struct Apply {
    shared: Arc<Shared>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
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
    /// push.
    pushed: AtomicU64,
    /// The watermark: every job numbered up to here has been applied. Jobs
    /// finish out of order, so this is not a count — it is the number below
    /// the oldest job still outstanding.
    applied: AtomicU64,
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
                retired: false,
                serial_running: false,
                slots: HashMap::new(),
                ready: VecDeque::new(),
                tails: HashMap::new(),
                incomplete: BTreeSet::new(),
            }),
            arrived: Condvar::new(),
            settled: Condvar::new(),
            depth: AtomicUsize::new(0),
            pushed: AtomicU64::new(0),
            applied: AtomicU64::new(0),
            bytes: AtomicUsize::new(0),
            free: AtomicU64::new(u64::MAX),
            window: jobs_cap(),
            kind_count: std::array::from_fn(|_| AtomicU64::new(0)),
            kind_nanos: std::array::from_fn(|_| AtomicU64::new(0)),
            jobs_done: AtomicU64::new(0),
        });
        shared.refresh_free(&root);
        let mut handles = Vec::new();
        if on {
            for index in 0..workers() {
                let shared = shared.clone();
                let root = root.clone();
                handles.push(
                    std::thread::Builder::new()
                        .name(format!("fs-apply-{index}"))
                        .spawn(move || shared.run(&root))
                        .expect("failed to spawn a filesystem apply thread"),
                );
            }
        }
        Apply {
            shared,
            workers: Mutex::new(handles),
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
    /// Blocks only when the queue holds more payload than [`BYTES_CAP`] or
    /// more jobs than the window.
    pub fn push(&self, job: Job) -> u64 {
        let shared = &self.shared;
        let mut state = shared.state.lock().expect("apply queue poisoned");
        while shared.bytes.load(Ordering::Relaxed) > BYTES_CAP
            || shared.depth.load(Ordering::Relaxed) > shared.window
        {
            state = shared.settled.wait(state).expect("apply queue poisoned");
        }
        if state.retired {
            // Retired mid-flight; nothing will drain, so do it here.
            drop(state);
            (job.run)();
            return shared.applied.load(Ordering::Relaxed);
        }
        shared.depth.fetch_add(1, Ordering::Relaxed);
        shared.bytes.fetch_add(job.bytes, Ordering::Relaxed);
        let seq = shared.pushed.fetch_add(1, Ordering::Relaxed) + 1;
        // Ordered behind the most recent job on each of its keys, if that
        // job is still held. One that has finished has left the table, and
        // took its tail entries with it.
        let mut waiting = 0;
        let mut seen: [u64; 4] = [0; 4];
        for key in job.keys.iter() {
            let Some(&prev) = state.tails.get(&key) else {
                continue;
            };
            if seen.contains(&prev) {
                continue;
            }
            seen[waiting] = prev;
            let slot = state.slots.get_mut(&prev).expect("a tail names a held job");
            slot.successors.push(seq);
            waiting += 1;
        }
        for key in job.keys.iter() {
            state.tails.insert(key, seq);
        }
        state.incomplete.insert(seq);
        state.slots.insert(
            seq,
            Slot {
                job: Some(job),
                waiting,
                successors: Vec::new(),
                done: false,
            },
        );
        if waiting == 0 {
            state.ready.push_back(seq);
        }
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
        if shared.applied.load(Ordering::Relaxed) >= seq {
            return;
        }
        let mut state = shared.state.lock().expect("apply queue poisoned");
        while shared.applied.load(Ordering::Relaxed) < seq && !state.retired {
            state = shared.settled.wait(state).expect("apply queue poisoned");
        }
    }

    /// Blocks while `still` holds and the queue has work in flight, waking
    /// on every completion to look again.
    ///
    /// The scoped barrier for an inode's own flags, now that jobs finish out
    /// of order: waiting for the watermark would wait for every job queued
    /// before this inode's — most of them for files it has never heard of.
    /// The flag is lowered by the job before its completion takes the lock,
    /// and the notification follows the completion, so a waiter that saw the
    /// flag up under the lock is woken to see it down.
    pub fn wait_while(&self, still: impl Fn() -> bool) {
        let shared = &self.shared;
        if !still() || shared.depth.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut state = shared.state.lock().expect("apply queue poisoned");
        while still() && shared.depth.load(Ordering::Relaxed) != 0 && !state.retired {
            state = shared.settled.wait(state).expect("apply queue poisoned");
        }
    }

    /// The drainer's histogram since the last call, one line per kind.
    pub fn report(&self) -> String {
        const NAMES: [&str; KINDS] = [
            "create", "write", "unlink", "rename", "clone", "setattr", "link", "mkdir", "symlink",
        ];
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

    /// The applied watermark: every job numbered up to this has landed.
    /// A job's number compared against it says whether that job could still
    /// be in flight — which is what a reader that consulted the host before
    /// it knew which file it had needs to know.
    pub fn applied(&self) -> u64 {
        self.shared.applied.load(Ordering::Relaxed)
    }
}

impl Drop for Apply {
    fn drop(&mut self) {
        self.drain();
        {
            let mut state = self.shared.state.lock().expect("apply queue poisoned");
            state.retired = true;
        }
        self.shared.arrived.notify_all();
        self.shared.settled.notify_all();
        for handle in self.workers.lock().expect("apply queue poisoned").drain(..) {
            let _ = handle.join();
        }
    }
}

impl Shared {
    fn run(&self, root: &std::path::Path) {
        loop {
            let (seq, job) = {
                let mut state = self.state.lock().expect("apply queue poisoned");
                loop {
                    if state.retired {
                        return;
                    }
                    // The oldest ready job this worker may take: any clone,
                    // or the oldest of the rest when none is running.
                    let pick = state.ready.iter().position(|seq| {
                        let concurrent = state
                            .slots
                            .get(seq)
                            .and_then(|slot| slot.job.as_ref())
                            .is_some_and(|job| matches!(job.kind, Kind::Clone));
                        concurrent || !state.serial_running
                    });
                    if let Some(index) = pick {
                        let seq = state
                            .ready
                            .remove(index)
                            .expect("a position in the ready queue");
                        let job = state
                            .slots
                            .get_mut(&seq)
                            .and_then(|slot| slot.job.take())
                            .expect("a ready job is held and untaken");
                        if !matches!(job.kind, Kind::Clone) {
                            state.serial_running = true;
                        }
                        break (seq, job);
                    }
                    state = self.arrived.wait(state).expect("apply queue poisoned");
                }
            };
            // A job that panics must not take a worker with it: with no
            // workers every barrier waits forever, which is a hung guest.
            let bytes = job.bytes;
            let kind = job.kind as usize;
            let keys = job.keys;
            let started = std::time::Instant::now();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || (job.run)())).is_err()
            {
                tracing::error!("an apply job panicked; the queue continues");
            }
            self.kind_count[kind].fetch_add(1, Ordering::Relaxed);
            self.kind_nanos[kind].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            self.bytes.fetch_sub(bytes, Ordering::Relaxed);
            let done = self.jobs_done.fetch_add(1, Ordering::Relaxed) + 1;
            if done.is_multiple_of(FREE_EVERY) {
                self.refresh_free(root);
            }
            {
                let mut state = self.state.lock().expect("apply queue poisoned");
                if !matches!(kind_of(kind), Kind::Clone) {
                    state.serial_running = false;
                }
                let slot = state.slots.remove(&seq).expect("a running job is held");
                debug_assert!(!slot.done);
                for next in slot.successors {
                    let successor = state
                        .slots
                        .get_mut(&next)
                        .expect("a successor is held until it runs");
                    successor.waiting -= 1;
                    if successor.waiting == 0 {
                        state.ready.push_back(next);
                    }
                }
                // A key still pointing here has no later job on it; with this
                // one gone, the next push on that key waits for nothing.
                for key in keys.iter() {
                    if state.tails.get(&key) == Some(&seq) {
                        state.tails.remove(&key);
                    }
                }
                state.incomplete.remove(&seq);
                let watermark = match state.incomplete.first() {
                    Some(&oldest) => oldest - 1,
                    None => self.pushed.load(Ordering::Relaxed),
                };
                self.applied.store(watermark, Ordering::Relaxed);
                // Depth is decremented under the lock, and the lock is held
                // while the counters are updated, and that is not decoration:
                // a waiter re-checks them under this same mutex, so it cannot
                // observe the old values and then miss the notification in
                // the gap before it sleeps — a lost wake-up, and a barrier
                // that sleeps forever on a queue that is already empty.
                self.depth.fetch_sub(1, Ordering::Relaxed);
            }
            self.arrived.notify_all();
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

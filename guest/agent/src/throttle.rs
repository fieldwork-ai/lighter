//! The containers' throttle, for a guest the host sizes (a virtio-mem range).
//!
//! Such a guest has only what is plugged, and a process in a container can
//! take memory faster than the host can be asked for more: a tmpfs writer
//! took a 3 GiB base in 140 ms, and the kernel's answer to memory it cannot
//! reclaim is the OOM killer, not a wait. So the containers' `memory.high`
//! sits at the edge of what the guest can give them, and a burst that
//! reaches it is slowed in reclaim, as the kernel does to any cgroup above
//! its high, instead of killed; the stall it causes is what `Stall` hears,
//! and the host plugs more in answer (the agent's line says `need`), and
//! the edge moves up with it on the next tick. Meta's Senpai and TMO steer
//! memory the same way, from pressure stall information and `memory.high`.
//!
//! The edge is the containers' usage less their own reclaimable cache, plus
//! what the guest has available, less a reserve for the kernel and the
//! engine: reclaimable cache counts as room (MemAvailable counts it) and not
//! as the containers' own, so a build filling the cache never comes near
//! it, and only memory nothing can reclaim (tmpfs, anonymous) reaches it.
//! A static bound on the same cgroup (`lighter.cachebound`) once throttled a
//! daily driver's every container for good; this one moves with the guest,
//! and gives up: the host at its ceiling, or not plugging, leaves no reason
//! to hold anything, so after `GIVE_UP` of throttling with no growth the
//! high is lifted until the guest grows again, and the kernel does what it
//! would have done without it.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long the containers may be throttled with no growth before the high
/// is lifted: a plug lands in tens of milliseconds, so seconds of none is a
/// host that will not.
const GIVE_UP: Duration = Duration::from_secs(3);

/// The trigger: this much stall in a window of this much, in microseconds.
/// The kernel's smallest window is half a second; 20 ms of it is a burst
/// already sleeping at the edge, not a page fault's reclaim.
const TRIGGER: &str = "some 20000 500000";

/// Wakes the agent's loop early: a stall at the edge, or anything else that
/// should not wait for the tick.
#[derive(Default)]
pub struct Wake {
    woken: Mutex<bool>,
    cond: Condvar,
}

impl Wake {
    pub fn wake(&self) {
        *self.woken.lock().expect("wake poisoned") = true;
        self.cond.notify_one();
    }

    /// Sleeps for `timeout` or until woken, whichever is first.
    pub fn sleep(&self, timeout: Duration) {
        let woken = self.woken.lock().expect("wake poisoned");
        let (mut woken, _) = self
            .cond
            .wait_timeout_while(woken, timeout, |w| !*w)
            .expect("wake poisoned");
        *woken = false;
    }
}

/// The containers stalled at the edge since the loop last asked.
#[derive(Clone, Default)]
pub struct Stall(Arc<AtomicBool>);

impl Stall {
    /// Watches `cgroup`'s memory pressure with a kernel trigger, marking the
    /// stall and waking the loop the moment it fires. A kernel without
    /// pressure accounting, or a cgroup that is not there yet, is retried.
    pub fn watch(cgroup: &str, wake: Arc<Wake>) -> Stall {
        let stall = Stall::default();
        let flag = stall.0.clone();
        let path = format!("{cgroup}/memory.pressure");
        std::thread::spawn(move || {
            loop {
                if let Some(fd) = arm(&path) {
                    watch_fd(&fd, &flag, &wake);
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        stall
    }

    /// Whether a stall fired since the last call.
    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}

fn arm(path: &str) -> Option<OwnedFd> {
    let c = std::ffi::CString::new(path).ok()?;
    // SAFETY: a NUL-terminated path; the descriptor is owned on success.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a descriptor just opened and owned by nothing else.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let text = format!("{TRIGGER}\0");
    // SAFETY: a write of a buffer we own to a descriptor we own; the kernel
    // takes the trigger with its terminator.
    let n = unsafe { libc::write(fd.as_raw_fd(), text.as_ptr().cast(), text.len()) };
    (n > 0).then_some(fd)
}

/// Waits on an armed trigger until its cgroup goes away.
fn watch_fd(fd: &OwnedFd, flag: &AtomicBool, wake: &Wake) {
    loop {
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        // SAFETY: one pollfd, owned on this stack.
        let n = unsafe { libc::poll(&mut pfd, 1, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if pfd.revents & libc::POLLERR != 0 {
            return;
        }
        if pfd.revents & libc::POLLPRI != 0 {
            flag.store(true, Ordering::Release);
            wake.wake();
        }
    }
}

/// The edge itself, moved each tick.
pub struct Throttle {
    cgroup: String,
    /// The edge as last written, `None` while lifted or never set.
    high: Option<u64>,
    /// When the containers were first seen throttled with the guest not
    /// growing since.
    throttled_since: Option<Instant>,
    /// The guest's size when the high was lifted: it comes back once the
    /// guest grows past it.
    lifted_at: Option<u64>,
    last_events: u64,
    last_total: u64,
}

impl Throttle {
    pub fn new(cgroup: &str) -> Throttle {
        Throttle {
            cgroup: cgroup.into(),
            high: None,
            throttled_since: None,
            lifted_at: None,
            last_events: 0,
            last_total: 0,
        }
    }

    /// One tick: `total` and `available` in bytes, from `/proc/meminfo`.
    pub fn tick(&mut self, total: u64, available: u64) {
        let now = Instant::now();
        let grew = total > self.last_total;
        self.last_total = total;
        let events = self.high_events();
        let throttled = events > self.last_events;
        self.last_events = events;
        if let Some(at) = self.lifted_at {
            if total <= at {
                return;
            }
            self.lifted_at = None;
        }
        if grew || !throttled {
            self.throttled_since = None;
        } else if self.throttled_since.is_none() {
            self.throttled_since = Some(now);
        }
        if self.throttled_since.is_some_and(|t| now.duration_since(t) >= GIVE_UP) {
            self.write("max");
            self.high = None;
            self.throttled_since = None;
            self.lifted_at = Some(total);
            return;
        }
        let Some((usage, reclaimable)) = self.usage() else { return };
        let high = edge(usage, reclaimable, available, total);
        // Rounded to a megabyte, and written only when it moves, so an idle
        // guest does not write the file four times a second.
        let high = high >> 20 << 20;
        if self.high != Some(high) {
            self.write(&high.to_string());
            self.high = Some(high);
        }
    }

    fn write(&self, value: &str) {
        let _ = std::fs::write(format!("{}/memory.high", self.cgroup), value);
    }

    /// Usage, and the part of it that is reclaimable cache (file less shmem).
    fn usage(&self) -> Option<(u64, u64)> {
        let current = std::fs::read_to_string(format!("{}/memory.current", self.cgroup))
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()?;
        let stat = std::fs::read_to_string(format!("{}/memory.stat", self.cgroup)).ok()?;
        let field = |name: &str| {
            stat.lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        Some((current, field("file").saturating_sub(field("shmem"))))
    }

    /// The `high` count in `memory.events`: each is a reclaim-and-throttle.
    fn high_events(&self) -> u64 {
        std::fs::read_to_string(format!("{}/memory.events", self.cgroup))
            .ok()
            .and_then(|e| {
                e.lines()
                    .find_map(|l| l.strip_prefix("high ")?.trim().parse::<u64>().ok())
            })
            .unwrap_or(0)
    }
}

/// The reserve the edge leaves the kernel and the engine: a thirty-second of
/// the guest, and a quarter-gigabyte at least.
fn reserve(total: u64) -> u64 {
    (total / 32).max(256 << 20)
}

/// Where the containers' high goes: their usage less their own reclaimable
/// cache, plus the guest's available memory, less the reserve.
fn edge(usage: u64, reclaimable: u64, available: u64, total: u64) -> u64 {
    usage
        .saturating_sub(reclaimable)
        .saturating_add(available)
        .saturating_sub(reserve(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// A build filling the cache: its cache is available memory, so the edge
    /// stays a guest's worth of room above it.
    #[test]
    fn cache_growth_is_not_near_the_edge() {
        let total = 8 * GIB;
        // 3 GiB used, 2.5 of it cache; 5 GiB available (cache included).
        let high = edge(3 * GIB, 5 * GIB / 2, 5 * GIB, total);
        assert_eq!(high, GIB / 2 + 5 * GIB - reserve(total));
        assert!(high > 3 * GIB, "the cache has room to grow into");
    }

    /// A tmpfs writer: nothing it holds is reclaimable, so the edge is where
    /// the guest runs out, less the reserve.
    #[test]
    fn unreclaimable_memory_meets_the_edge_at_the_guests_end() {
        let total = 4 * GIB;
        let high = edge(GIB, 0, GIB, total);
        assert_eq!(high, 2 * GIB - reserve(total));
    }

    #[test]
    fn the_reserve_is_a_thirty_second_or_a_quarter_gigabyte() {
        assert_eq!(reserve(4 * GIB), 256 << 20);
        assert_eq!(reserve(32 * GIB), GIB);
    }

    #[test]
    fn a_wake_ends_the_sleep_early() {
        let wake = Arc::new(Wake::default());
        let w = wake.clone();
        let t = std::thread::spawn(move || {
            let start = Instant::now();
            w.sleep(Duration::from_secs(10));
            start.elapsed()
        });
        std::thread::sleep(Duration::from_millis(20));
        wake.wake();
        assert!(t.join().unwrap() < Duration::from_secs(5));
    }
}

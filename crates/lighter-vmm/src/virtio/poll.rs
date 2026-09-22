//! Watching a virtqueue so the guest does not have to trap to use it.
//!
//! # What a request costs, and where it goes
//!
//! Submitting one virtio request normally costs a write to the notification
//! register. That write is an MMIO trap: the vCPU leaves the guest, our
//! handler runs, and the core re-enters. Measured on this machine that crossing
//! is about two and a half microseconds — nothing next to a disk, but a third
//! of a `stat` across a shared filesystem, and a package install makes several
//! hundred thousand of them.
//!
//! The driver already offers a way out. Before every notification it reads a
//! flag in the used ring, and skips the write if the device has set it. So a
//! host thread that is *already watching* the ring can set that flag and the
//! guest stops trapping altogether — the request appears in shared memory and
//! is picked up on the next turn of a loop that was running anyway.
//!
//! # The mistake that made the first version slower
//!
//! Asking "is there anything yet?" used to mean taking the transport lock. In
//! a spin loop the answer is no almost every time, and every one of those noes
//! cost the vCPU a lock it was about to want; the yield between attempts cost a
//! syscall on top. Measured on a package install it was 15.2 seconds against
//! 14.5 with the guest trapping normally — the watcher was paying more in
//! contention than the traps had cost in the first place.
//!
//! So the question is now answered without a lock at all.
//! [`crate::virtio::mmio::QueueSignal`] mirrors the ring's address and our
//! cursor into three relaxed atomics, and the probe is one read out of guest
//! memory. The lock is taken only when there is something to take it for.
//!
//! # Why it still parks
//!
//! A thread spinning on a ring burns a core, which is the wrong thing on a
//! laptop that is supposed to idle at nothing. So it only spins while the guest
//! is actually asking for things: after a short quiet period it clears the
//! flag, takes one last look, and sleeps until a real kick wakes it. An idle
//! guest sends no kicks and the thread stays asleep.
//!
//! **Clearing the flag is the delicate part.** A driver that looked at the flag
//! while it was set, and decided not to kick, will not look again. So the
//! order — clear, fence, then re-examine the ring — is what stands between this
//! and a request that sits in the queue until something unrelated happens
//! along. The fence is in [`crate::virtio::queue::Virtqueue::suppress_notifications`];
//! the re-examination is here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::memory::GuestMemory;
use crate::virtio::mmio::VirtioMmio;

/// How long to keep watching after the last request before going back to
/// sleep.
///
/// Long enough to cover the gap between one request and the next in a busy
/// workload — a package manager waiting on its own `stat` before issuing the
/// following one — and short enough that a burst which has genuinely ended
/// costs a fraction of a millisecond of one core.
///
/// Zero turns it off, which is what `LIGHTER_HOST_POLL_US=0` is for.
const IDLE_WINDOW: std::time::Duration = std::time::Duration::from_micros(200);

/// Whether watching is paying, from what the last windows caught.
///
/// A window that catches a request saves the guest a trap (about two and a
/// half microseconds) for up to [`IDLE_WINDOW`] of a core. In a package
/// install every window catches dozens; under a trickle (a home server's
/// camera frames, a heartbeat) nearly none catch anything, and a window per
/// kick was a percent and a half of a core spent watching an empty ring. So
/// after [`MISSES`] empty windows in a row the poller stops watching for a
/// while, doubling from 10 ms to [`BACKOFF_MAX`] as it keeps not paying, and
/// the first window that catches something puts it straight back.
#[derive(Debug, Default)]
pub(crate) struct Payoff {
    misses: u32,
    backoff: std::time::Duration,
    off_until: Option<std::time::Instant>,
}

const MISSES: u32 = 4;
const BACKOFF_MIN: std::time::Duration = std::time::Duration::from_millis(10);
const BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(250);

impl Payoff {
    pub(crate) fn should_watch(&self, now: std::time::Instant) -> bool {
        self.off_until.is_none_or(|until| now >= until)
    }

    pub(crate) fn record(&mut self, caught: usize, now: std::time::Instant) {
        if caught > 0 {
            *self = Payoff::default();
            return;
        }
        self.misses += 1;
        if self.misses >= MISSES {
            self.misses = 0;
            self.backoff = if self.backoff.is_zero() {
                BACKOFF_MIN
            } else {
                (self.backoff * 2).min(BACKOFF_MAX)
            };
            self.off_until = Some(now + self.backoff);
        }
    }
}

/// The handle a poller parks on, and that the transport pokes.
#[derive(Default)]
pub struct Kicks {
    pending: Mutex<bool>,
    arrived: Condvar,
    stopped: AtomicBool,
}

impl Kicks {
    pub fn new() -> Arc<Kicks> {
        Arc::new(Kicks::default())
    }

    /// The guest wrote the notification register.
    pub fn kicked(&self) {
        *self.pending.lock().expect("poller signal poisoned") = true;
        self.arrived.notify_one();
    }

    /// Retires the poller.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.kicked();
    }

    /// Sleeps until the guest asks for something. False means shut down.
    #[cfg(test)]
    fn wait(&self) -> bool {
        self.wait_until(None)
    }

    fn wait_until(&self, deadline: Option<std::time::Instant>) -> bool {
        let mut pending = self.pending.lock().expect("poller signal poisoned");
        while !*pending {
            if self.stopped.load(Ordering::Acquire) {
                return false;
            }
            if let Some(deadline) = deadline {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return true;
                }
                pending = self
                    .arrived
                    .wait_timeout(pending, left)
                    .expect("poller signal poisoned")
                    .0;
            } else {
                pending = self.arrived.wait(pending).expect("poller signal poisoned");
            }
        }
        *pending = false;
        !self.stopped.load(Ordering::Acquire)
    }
}

/// How long the poller watches after the last request, from the environment;
/// zero turns the watcher off.
fn idle_window() -> std::time::Duration {
    match std::env::var("LIGHTER_HOST_POLL_US")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(micros) => std::time::Duration::from_micros(micros),
        None => IDLE_WINDOW,
    }
}

/// Starts a thread that watches `queue` on `transport`.
pub fn spawn(
    name: &str,
    transport: Arc<Mutex<VirtioMmio>>,
    watched: Vec<u16>,
    kicks: Arc<Kicks>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_with_window(name, transport, watched, kicks, idle_window())
}

pub(crate) fn spawn_with_window(
    name: &str,
    transport: Arc<Mutex<VirtioMmio>>,
    watched: Vec<u16>,
    kicks: Arc<Kicks>,
    window: std::time::Duration,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let (signals, memory) = {
        let held = transport.lock().expect("polled transport poisoned");
        (
            watched
                .iter()
                .filter_map(|index| held.signal(*index).map(|signal| (*index, signal)))
                .collect::<Vec<_>>(),
            held.memory().clone(),
        )
    };
    if signals.is_empty() {
        return std::thread::Builder::new()
            .name(format!("poll-{name}"))
            .spawn(|| ());
    }
    std::thread::Builder::new()
        .name(format!("poll-{name}"))
        .spawn(move || {
            crate::virtio::fs::raise_server_qos();
            let adapt = std::env::var("LIGHTER_HOST_POLL_ADAPT").as_deref() != Ok("0");
            let mut payoff = Payoff::default();
            loop {
                let deadline = transport
                    .lock()
                    .expect("polled transport poisoned")
                    .retry_deadline();
                if !kicks.wait_until(deadline) {
                    break;
                }
                {
                    let mut held = transport.lock().expect("polled transport poisoned");
                    // A stopped/reset device has no retained work. Reading the
                    // deadline under the transport lock also rejects stale wakes.
                    if held.retry_deadline().is_some() {
                        held.retry_deferred(std::time::Instant::now());
                    }
                    if held.retry_deadline().is_some() || window.is_zero() {
                        continue;
                    }
                }
                if adapt && !payoff.should_watch(std::time::Instant::now()) {
                    continue;
                }
                // The kick that woke us has already been serviced by the vCPU
                // that made it; from here the guest is told to stop bothering.
                //
                // All of the watched queues, not just the one that kicked: a
                // driver spreads its requests across them by CPU, so the next
                // one is as likely to arrive on any other, and a queue left
                // un-suppressed traps for every request while this thread is
                // already watching it.
                {
                    let mut held = transport.lock().expect("polled transport poisoned");
                    for (index, _) in &signals {
                        held.suppress_notifications(*index, true);
                    }
                }

                let caught = watch(&transport, &signals, &memory, window);
                payoff.record(caught, std::time::Instant::now());

                // Clearing, then looking again — repeatedly. A driver that saw
                // the flag set and skipped its kick is relying on this, and one
                // look is not enough: the guest may publish a chain between the
                // clear and the look, having read the flag before the clear.
                // Going round until the ring is genuinely empty is the only
                // version of this with no window in it.
                let mut held = transport.lock().expect("polled transport poisoned");
                for (index, _) in &signals {
                    held.suppress_notifications(*index, false);
                }
                let mut stranded = 0;
                loop {
                    let mut left = 0;
                    for (index, _) in &signals {
                        held.poll_queue(*index);
                        left += held.outstanding(*index);
                    }
                    if left == 0 || held.retry_deadline().is_some() {
                        break;
                    }
                }
                if held.retry_deadline().is_none() {
                    for (index, _) in &signals {
                        stranded += held.outstanding(*index);
                    }
                }
                drop(held);
                if stranded != 0 {
                    // Benign by construction: the loop above only exits with an
                    // empty ring, so anything here arrived afterwards — and the
                    // flag is clear by then, so the guest will kick for it.
                    tracing::debug!(stranded, "a chain arrived as the poller parked");
                }
            }
        })
}

/// Spins on the ring until it has been quiet for `window`; returns how many
/// times it found work.
///
/// The probe is lock-free and the spin has no syscall in it. Both matter: the
/// thread being waited on is a vCPU, and anything this loop does that the
/// scheduler or the transport lock can see is taken directly out of the work it
/// is waiting for.
fn watch(
    transport: &Arc<Mutex<VirtioMmio>>,
    signals: &[(u16, Arc<crate::virtio::mmio::QueueSignal>)],
    memory: &GuestMemory,
    window: std::time::Duration,
) -> usize {
    let mut caught = 0;
    // The clock is read only on an empty iteration. While every look finds
    // work there is no quiet to time, and a sample of the egress case had
    // `Instant::now` at a quarter of this thread's busy samples.
    let mut idle_since: Option<std::time::Instant> = None;
    loop {
        let mut found = false;
        for (index, signal) in signals {
            if !signal.has_work(memory) {
                continue;
            }
            if transport
                .lock()
                .expect("polled transport poisoned")
                .poll_queue(*index)
            {
                found = true;
            }
        }
        if found {
            caught += 1;
            idle_since = None;
            continue;
        }
        let now = std::time::Instant::now();
        match idle_since {
            None => idle_since = Some(now),
            Some(since) if now.duration_since(since) >= window => return caught,
            Some(_) => {}
        }
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_kick_wakes_a_waiter() {
        let kicks = Kicks::new();
        let signal = kicks.clone();
        let waiter = std::thread::spawn(move || signal.wait());
        std::thread::sleep(Duration::from_millis(20));
        kicks.kicked();
        assert!(waiter.join().unwrap());
    }

    /// A kick that lands before anyone is waiting must not be lost, or the
    /// poller sleeps through the burst it was woken for.
    #[test]
    fn a_kick_before_the_wait_is_remembered() {
        let kicks = Kicks::new();
        kicks.kicked();
        assert!(kicks.wait());
    }

    #[test]
    fn empty_windows_back_off_and_a_catch_resumes() {
        let t0 = std::time::Instant::now();
        let mut p = Payoff::default();
        for _ in 0..MISSES - 1 {
            p.record(0, t0);
            assert!(p.should_watch(t0));
        }
        p.record(0, t0);
        assert!(!p.should_watch(t0));
        assert!(p.should_watch(t0 + BACKOFF_MIN));
        for _ in 0..MISSES {
            p.record(0, t0);
        }
        assert!(!p.should_watch(t0 + BACKOFF_MIN), "the backoff doubles");
        for _ in 0..64 {
            p.record(0, t0);
        }
        assert!(p.should_watch(t0 + BACKOFF_MAX), "and is capped");
        p.record(3, t0);
        assert!(
            p.should_watch(t0),
            "a window that catches something resumes"
        );
    }

    #[test]
    fn stopping_releases_a_waiter() {
        let kicks = Kicks::new();
        let signal = kicks.clone();
        let waiter = std::thread::spawn(move || signal.wait());
        std::thread::sleep(Duration::from_millis(20));
        kicks.stop();
        assert!(
            !waiter.join().unwrap(),
            "a stopped poller must not report work"
        );
    }
}

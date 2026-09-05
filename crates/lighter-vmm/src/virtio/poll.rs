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
    fn wait(&self) -> bool {
        let mut pending = self.pending.lock().expect("poller signal poisoned");
        while !*pending {
            if self.stopped.load(Ordering::Acquire) {
                return false;
            }
            pending = self.arrived.wait(pending).expect("poller signal poisoned");
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
    let window = idle_window();
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
            if window.is_zero() {
                return;
            }
            crate::virtio::fs::raise_server_qos();
            while kicks.wait() {
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

                watch(&transport, &signals, &memory, window);

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
                    if left == 0 {
                        break;
                    }
                }
                for (index, _) in &signals {
                    stranded += held.outstanding(*index);
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

/// Spins on the ring until it has been quiet for `window`.
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
) {
    let mut last_work = std::time::Instant::now();
    while last_work.elapsed() < window {
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
            last_work = std::time::Instant::now();
        } else {
            std::hint::spin_loop();
        }
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

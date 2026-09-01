//! Watching a virtqueue so the guest does not have to trap to use it.
//!
//! # What a request costs, and where it goes
//!
//! Submitting one virtio request normally costs a write to the notification
//! register. That write is an MMIO trap: the vCPU leaves the guest, our
//! handler runs, and the core re-enters. On Apple's hypervisor that crossing is
//! a few microseconds, which is nothing next to a disk but is most of the cost
//! of a `stat`. A package install makes several hundred thousand of them.
//!
//! The driver already offers a way out. Before every notification it reads a
//! flag in the used ring, and skips the write if the device has set it. So a
//! host thread that is *already watching* the ring can set that flag and the
//! guest stops trapping altogether — the request appears in shared memory and
//! is picked up on the next turn of a loop that was running anyway.
//!
//! # Why it parks, and why that is not a compromise
//!
//! A thread spinning on a ring burns a core, which is exactly the wrong thing
//! on a laptop that is supposed to idle at nothing. So it only spins while the
//! guest is actually asking for things: after a short quiet period it clears
//! the flag, takes one last look, and sleeps until a real kick wakes it. An
//! idle guest sends no kicks and the thread stays asleep.
//!
//! **Clearing the flag is the delicate part.** A driver that looked at the flag
//! while it was set, and decided not to kick, will not look again. So the
//! order — clear, fence, then re-examine the ring — is what stands between this
//! and a request that sits in the queue until something unrelated happens
//! along. The fence is in [`crate::virtio::queue::Virtqueue::suppress_notifications`];
//! the re-examination is here.
//!
//! # This is off, because it does not pay
//!
//! It works — the first version wedged the guest, which one look after clearing
//! the flag was not enough to prevent; draining until the ring is genuinely
//! empty fixes that. What it does not do is help. Measured on a package
//! install it is *slower*: 15.2 seconds against 14.5 with the guest trapping
//! normally.
//!
//! The reason is that the trap was never the expensive part. An MMIO exit on
//! Apple's hypervisor is a couple of microseconds out of a round trip that
//! costs thirteen, and a thread taking the transport lock in a loop to save
//! them costs more in contention than it removes.
//!
//! Kept, off, because the primitives are correct and tested, and because the
//! measurement is the point: the next person to think the guest's kick is
//! worth eliminating can turn it on with `LIGHTER_HOST_POLL_US` and see for
//! themselves in ten minutes rather than a day.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::virtio::mmio::VirtioMmio;

/// How long to keep watching after the last request before going back to
/// sleep.
///
/// Long enough to cover the gap between one request and the next in a busy
/// workload — a package manager waiting on its own `stat` before issuing the
/// following one — and short enough that a burst which has genuinely ended
/// costs a fraction of a millisecond of one core.
/// Zero: off. See the note at the top of the file about why.
const IDLE_WINDOW: std::time::Duration = std::time::Duration::ZERO;

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
            pending = self
                .arrived
                .wait(pending)
                .expect("poller signal poisoned");
        }
        *pending = false;
        !self.stopped.load(Ordering::Acquire)
    }
}

/// How long the poller watches after the last request, from the environment.
///
/// The default is zero, which is off. Setting it enables an experiment that
/// currently hangs the guest; see the note at the top of the file.
fn idle_window() -> std::time::Duration {
    match std::env::var("LIGHTER_HOST_POLL_US").ok().and_then(|v| v.parse().ok()) {
        Some(micros) => std::time::Duration::from_micros(micros),
        None => IDLE_WINDOW,
    }
}

/// Starts a thread that watches `queue` on `transport`.
pub fn spawn(
    name: &str,
    transport: Arc<Mutex<VirtioMmio>>,
    queue: u16,
    kicks: Arc<Kicks>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let window = idle_window();
    std::thread::Builder::new()
        .name(format!("poll-{name}"))
        .spawn(move || {
            if window.is_zero() {
                return;
            }
            while kicks.wait() {
                // The kick that woke us has already been serviced by the vCPU
                // that made it; from here the guest is told to stop bothering.
                transport
                    .lock()
                    .expect("polled transport poisoned")
                    .suppress_notifications(queue, true);

                let mut last_work = std::time::Instant::now();
                while last_work.elapsed() < window {
                    let worked = transport
                        .lock()
                        .expect("polled transport poisoned")
                        .poll_queue(queue);
                    if worked {
                        last_work = std::time::Instant::now();
                    } else {
                        // Nothing yet. Yielding rather than spinning hot: the
                        // thread that will fill this ring is a vCPU, and taking
                        // its core away to watch for it is self-defeating.
                        std::hint::spin_loop();
                        std::thread::yield_now();
                    }
                }

                // Clearing, then looking again — repeatedly. A driver that saw
                // the flag set and skipped its kick is relying on this, and one
                // look is not enough: the guest may publish a chain between the
                // clear and the look, having read the flag before the clear.
                // Going round until the ring is genuinely empty is the only
                // version of this with no window in it.
                let mut held = transport.lock().expect("polled transport poisoned");
                held.suppress_notifications(queue, false);
                loop {
                    held.poll_queue(queue);
                    if held.outstanding(queue) == 0 {
                        break;
                    }
                }
                let stranded = held.outstanding(queue);
                drop(held);
                if stranded != 0 {
                    // Benign by construction: the loop above only exits with an
                    // empty ring, so anything here arrived afterwards — and the
                    // flag is clear by then, so the guest will kick for it.
                    tracing::debug!(queue, stranded, "a chain arrived as the poller parked");
                }
            }
        })
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
        assert!(!waiter.join().unwrap(), "a stopped poller must not report work");
    }
}

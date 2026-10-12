//! Thread quality of service, for threads a user is waiting on.
//!
//! Apple silicon schedules a thread at the default class on whichever core
//! is free, efficiency cores included, and a byte copy that moves at a
//! performance core's pace one run and an efficiency core's the next reads
//! as noise of twenty gigabits. The threads that carry a stream's bytes ask
//! for the interactive class and stay on the performance cores.
//!
//! Unconditional, unlike the filesystem server's raise, which is measured
//! and off: a request thread that is answered on the vCPU's own core does
//! not want it, a copy thread that is never on that core does.
//!
//! The vCPUs themselves run at the default class, so a guest building at
//! full tilt competes with the Mac's windows no harder than any other
//! process. The one exception is scoped: while a container's stream to one
//! of the accelerator servers (the Neural Engine, ggml on Metal, PyTorch on
//! MPS) runs a request loop, the vCPUs are overridden to the interactive
//! class, because such a loop is a wake of the guest per message, and a vCPU
//! thread at the default class on a busy Mac is woken late: llama.cpp over
//! the ggml server on an M1 ran at 63 tokens a second one run and 80 the
//! next until the override, and 82 to 85 with it, against 106 for a native
//! client. Nobody is waiting on anything else while a model answers. A
//! stream that is merely open, or answers a few times a second, is no such
//! loop (see [`Pace`]).

use std::ffi::c_void;
use std::net::SocketAddr;
use std::sync::Mutex;

/// The stack for a thread that carries one connection. A quarter megabyte
/// against macOS's half-megabyte default for a spawned thread: a
/// connection thread holds a few buffers and a stream's life, and the
/// stack mapping is most of what a spawn costs at thousands a second.
pub const CONNECTION_STACK: usize = 256 << 10;

/// `QOS_CLASS_DEFAULT`.
const DEFAULT: u32 = 0x15;
/// `QOS_CLASS_UTILITY`.
const UTILITY: u32 = 0x11;
/// `QOS_CLASS_USER_INTERACTIVE`.
const USER_INTERACTIVE: u32 = 0x21;

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    fn pthread_self() -> usize;
    fn pthread_override_qos_class_start_np(
        thread: usize,
        qos_class: u32,
        relative_priority: i32,
    ) -> *mut c_void;
    fn pthread_override_qos_class_end_np(over: *mut c_void) -> i32;
}

/// The vCPU threads, as pthreads, for [`Boost`]. Registered by each on its
/// way into the guest; a thread stays registered for the process's life,
/// which is the machine's.
static VCPUS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Loopback ports of the accelerator servers, so the reactor can recognise
/// a stream bound for one.
static ACCELERATOR_PORTS: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// Called on a vCPU thread before it runs the guest.
pub fn register_vcpu() {
    // `LIGHTER_VCPU_QOS=utility` (or `low`: the default class at its lowest
    // relative priority) runs the vCPUs below the default, for measuring.
    // Measured on the M5 with 18 vCPUs, every one of them busy: the Mac's
    // interactive thread woke 6.5 ms late at the 99th percentile behind
    // default-class vCPUs and 1.3 behind utility ones, but utility cost 20%
    // of the guest's all-core throughput on an idle Mac (efficiency cores)
    // and `low` 30%; and 18 native threads at the default class made the
    // same thread 6.1 ms late. The default class is a native build's, so
    // the vCPUs stay there: containers compete with the Mac as native work
    // does, and no harder (gate m6c).
    match std::env::var("LIGHTER_VCPU_QOS").as_deref() {
        // SAFETY (both): a plain call on the current thread with constant
        // arguments.
        Ok("utility") => unsafe {
            pthread_set_qos_class_self_np(UTILITY, 0);
        },
        Ok("low") => unsafe {
            pthread_set_qos_class_self_np(DEFAULT, -15);
        },
        _ => {}
    }
    // SAFETY: a plain query of the calling thread.
    let me = unsafe { pthread_self() };
    VCPUS.lock().expect("vcpu registry poisoned").push(me);
}

/// Names a loopback port one of the accelerator servers listens on.
pub fn register_accelerator_port(port: u16) {
    ACCELERATOR_PORTS
        .lock()
        .expect("port registry poisoned")
        .push(port);
}

/// Whether an outbound stream is bound for an accelerator server.
pub fn is_accelerator(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
        && ACCELERATOR_PORTS
            .lock()
            .expect("port registry poisoned")
            .contains(&addr.port())
}

/// Whether a stream to an accelerator server is running a request loop, and
/// the vCPUs held at the interactive class while it is.
///
/// Busy is [`BUSY`] replies within [`WINDOW`]: a token loop answers every
/// ten milliseconds or so, a detector a few times a second. Held for the
/// stream's life, the boost kept every vCPU of a Mac running Frigate at the
/// interactive class for as long as its detector ran, which never ends:
/// Frigate on one camera, 30 vCPUs, spent a fifth more energy with it than
/// without (powermetrics' energy impact, 558 against 448) and asked nothing
/// of it. The boost ends once replies have been quiet for [`HOLD`].
pub struct Pace {
    window: std::time::Instant,
    replies: u32,
    busy: Option<std::time::Instant>,
    boost: Option<Boost>,
}

/// How long [`BUSY`] replies may take and still be a request loop.
pub const WINDOW: std::time::Duration = std::time::Duration::from_millis(100);
/// Replies within [`WINDOW`] that make a request loop.
pub const BUSY: u32 = 4;
/// How long a request loop may go quiet before the vCPUs drop back. Long
/// enough to span the pause between one answer and the next prompt, so a
/// conversation does not start each answer on vCPUs at the default class: on
/// the M1 at half a second, llama-bench's generation, short runs with pauses
/// between them, read 78 tokens a second one run against 84 for 0.13.2.
pub const HOLD: std::time::Duration = std::time::Duration::from_secs(2);

impl Pace {
    pub fn new(now: std::time::Instant) -> Pace {
        Pace {
            window: now,
            replies: 0,
            busy: None,
            boost: None,
        }
    }

    /// A reply arrived. Returns true if this one started the boost.
    pub fn reply(&mut self, now: std::time::Instant) -> bool {
        if now.duration_since(self.window) >= WINDOW {
            self.window = now;
            self.replies = 0;
        }
        self.replies += 1;
        if self.replies < BUSY {
            return false;
        }
        self.busy = Some(now);
        if self.boost.is_some() {
            return false;
        }
        self.boost = Some(Boost::vcpus());
        tracing::debug!("accelerator request loop: vCPUs at the interactive class");
        true
    }

    /// Ends the boost once replies have been quiet for [`HOLD`]. Returns
    /// whether the vCPUs are still held.
    pub fn settle(&mut self, now: std::time::Instant) -> bool {
        if self
            .busy
            .is_some_and(|busy| now.duration_since(busy) >= HOLD)
        {
            self.busy = None;
            if self.boost.take().is_some() {
                tracing::debug!("accelerator request loop quiet: vCPUs back to the default class");
            }
        }
        self.boost.is_some()
    }
}

/// The vCPUs held at the interactive class until this is dropped. Each
/// holder has its own overrides, so two streams overlap without either
/// ending the other's early.
pub struct Boost(Vec<usize>);

impl Boost {
    /// Overrides every registered vCPU. `LIGHTER_VCPU_BOOST=0` makes it a
    /// no-op, for measuring.
    pub fn vcpus() -> Boost {
        if std::env::var_os("LIGHTER_VCPU_BOOST").is_some_and(|v| v == "0") {
            return Boost(Vec::new());
        }
        let vcpus = VCPUS.lock().expect("vcpu registry poisoned");
        let overrides = vcpus
            .iter()
            .filter_map(|&thread| {
                // SAFETY: a registered thread of this process; the override
                // holds its own reference to the thread.
                let over =
                    unsafe { pthread_override_qos_class_start_np(thread, USER_INTERACTIVE, 0) };
                (!over.is_null()).then_some(over as usize)
            })
            .collect();
        Boost(overrides)
    }
}

impl Drop for Boost {
    fn drop(&mut self) {
        for &over in &self.0 {
            // SAFETY: a handle this boost started and has not ended.
            unsafe { pthread_override_qos_class_end_np(over as *mut c_void) };
        }
    }
}

/// Raises the calling thread to the interactive class. Best effort.
pub fn raise_interactive() {
    // SAFETY: a plain call on the current thread with constant arguments.
    let rc = unsafe { pthread_set_qos_class_self_np(USER_INTERACTIVE, 0) };
    if rc != 0 {
        tracing::debug!(rc, "could not raise the thread's QoS");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn replies(pace: &mut Pace, start: Instant, every: Duration, n: u32) -> Instant {
        let mut now = start;
        for _ in 0..n {
            now += every;
            pace.reply(now);
        }
        now
    }

    #[test]
    fn a_token_loop_holds_the_vcpus_and_lets_go_once_quiet() {
        let start = Instant::now();
        let mut pace = Pace::new(start);
        let end = replies(&mut pace, start, Duration::from_millis(10), 50);
        assert!(pace.settle(end));
        // The last busy reply is the last that completed a window's BUSY.
        assert!(pace.settle(end + HOLD - Duration::from_millis(20)));
        assert!(!pace.settle(end + HOLD));
    }

    #[test]
    fn a_detector_a_few_times_a_second_never_does() {
        let start = Instant::now();
        let mut pace = Pace::new(start);
        let end = replies(&mut pace, start, Duration::from_millis(180), 200);
        assert!(!pace.settle(end));
    }

    #[test]
    fn a_loop_that_starts_again_is_held_again() {
        let start = Instant::now();
        let mut pace = Pace::new(start);
        let end = replies(&mut pace, start, Duration::from_millis(10), 10);
        assert!(!pace.settle(end + HOLD));
        let end = replies(&mut pace, end + HOLD, Duration::from_millis(10), 10);
        assert!(pace.settle(end));
    }
}

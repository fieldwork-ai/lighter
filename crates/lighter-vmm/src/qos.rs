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
//! process. The one exception is scoped: while a container has a stream
//! open to one of the accelerator servers (the Neural Engine, ggml on
//! Metal, PyTorch on MPS) the vCPUs are overridden to the interactive
//! class for that stream's life, because the request loop on such a
//! stream is a wake of the guest per message, and a vCPU thread at the
//! default class on a busy Mac is woken late: llama.cpp over the ggml
//! server on an M1 ran at 63 tokens a second one run and 80 the next until
//! the override, and 82 to 85 with it, against 106 for a native client.
//! Nobody is waiting on anything else while a model answers.

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

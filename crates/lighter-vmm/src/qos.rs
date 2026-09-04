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

/// The stack for a thread that carries one connection. A quarter megabyte
/// against macOS's half-megabyte default for a spawned thread: a
/// connection thread holds a few buffers and a stream's life, and the
/// stack mapping is most of what a spawn costs at thousands a second.
pub const CONNECTION_STACK: usize = 256 << 10;

/// `QOS_CLASS_USER_INTERACTIVE`.
const USER_INTERACTIVE: u32 = 0x21;

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// Raises the calling thread to the interactive class. Best effort.
pub fn raise_interactive() {
    // SAFETY: a plain call on the current thread with constant arguments.
    let rc = unsafe { pthread_set_qos_class_self_np(USER_INTERACTIVE, 0) };
    if rc != 0 {
        tracing::debug!(rc, "could not raise the thread's QoS");
    }
}

//! What the Mac thinks of its own memory.
//!
//! A virtual machine that holds on to eight gigabytes because it once needed
//! them is the single most common complaint about running containers this way,
//! and the fix has two halves. The guest gives pages back on its own through
//! free page reporting, which handles the ordinary case of a build finishing.
//! This is the other half: when the *host* is short, the guest is asked to give
//! up more than it would have volunteered.
//!
//! macOS publishes exactly the signal needed. `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`
//! reports the same three levels the kernel uses to decide whether to start
//! compressing and swapping, so a VM watching it reacts at the same moment
//! everything else on the machine does — rather than on a timer, or on a
//! guess about what "low memory" means.

use std::ffi::c_void;
use std::sync::Arc;

/// The levels macOS reports, and what each one means for a guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pressure {
    /// There is memory to spare. Give the guest back whatever it wants.
    Normal,
    /// The system has started reclaiming. Ask for some back before it has to
    /// compress ours.
    Warn,
    /// The system is about to swap or kill something. Ask for a lot.
    Critical,
}

const DISPATCH_MEMORYPRESSURE_NORMAL: usize = 0x01;
const DISPATCH_MEMORYPRESSURE_WARN: usize = 0x02;
const DISPATCH_MEMORYPRESSURE_CRITICAL: usize = 0x04;

unsafe extern "C" {
    static _dispatch_source_type_memorypressure: c_void;

    fn dispatch_source_create(
        kind: *const c_void,
        handle: usize,
        mask: usize,
        queue: *mut c_void,
    ) -> *mut c_void;
    fn dispatch_source_set_event_handler_f(source: *mut c_void, handler: extern "C" fn(*mut c_void));
    fn dispatch_source_get_data(source: *mut c_void) -> usize;
    fn dispatch_set_context(object: *mut c_void, context: *mut c_void);
    fn dispatch_resume(object: *mut c_void);
    fn dispatch_source_cancel(source: *mut c_void);
    fn dispatch_release(object: *mut c_void);
    fn dispatch_queue_create(label: *const i8, attr: *mut c_void) -> *mut c_void;
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

/// What a watcher does when the level changes.
pub trait Observer: Send + Sync {
    fn pressure(&self, level: Pressure);
}

/// What the dispatch handler is given: the source it must ask for the level,
/// and the observer it must tell.
struct Held {
    source: *mut c_void,
    observer: Box<dyn Observer>,
}

// SAFETY: the handler runs on one serial queue, and nothing else touches this
// until the `Watcher` is dropped — which waits for that queue to drain.
unsafe impl Send for Held {}
unsafe impl Sync for Held {}

/// A live subscription to the host's memory pressure.
pub struct Watcher {
    source: *mut c_void,
    queue: *mut c_void,
    /// Kept alive because the dispatch handler holds a pointer into it.
    _held: Arc<Held>,
}

// SAFETY: the fields are only touched on drop, and libdispatch's own
// operations are documented as safe from any thread.
unsafe impl Send for Watcher {}
unsafe impl Sync for Watcher {}

extern "C" fn on_pressure(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is what `Arc::as_ptr` returned for the `Held` the
    // `Watcher` keeps alive for the source's whole life.
    let held = unsafe { &*(context as *const Held) };
    // SAFETY: a live source, called from its own handler.
    let data = unsafe { dispatch_source_get_data(held.source) };
    let level = if data & DISPATCH_MEMORYPRESSURE_CRITICAL != 0 {
        Pressure::Critical
    } else if data & DISPATCH_MEMORYPRESSURE_WARN != 0 {
        Pressure::Warn
    } else {
        Pressure::Normal
    };
    held.observer.pressure(level);
}

extern "C" fn nothing(_: *mut c_void) {}

impl Watcher {
    /// Subscribes to host memory pressure.
    pub fn start(observer: Box<dyn Observer>) -> Result<Watcher, String> {
        let label = c"com.lighter.mempressure";
        // SAFETY: a static label and a null attribute, meaning a serial queue.
        let queue = unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null_mut()) };
        if queue.is_null() {
            return Err("could not create a dispatch queue".into());
        }
        // SAFETY: the documented way to build a memory-pressure source.
        let source = unsafe {
            dispatch_source_create(
                &_dispatch_source_type_memorypressure,
                0,
                DISPATCH_MEMORYPRESSURE_NORMAL
                    | DISPATCH_MEMORYPRESSURE_WARN
                    | DISPATCH_MEMORYPRESSURE_CRITICAL,
                queue,
            )
        };
        if source.is_null() {
            // SAFETY: a queue we created that nothing is using.
            unsafe { dispatch_release(queue) };
            return Err("the system refused a memory-pressure source".into());
        }

        let held = Arc::new(Held { source, observer });
        // SAFETY: a live source; the context outlives it because the `Watcher`
        // holds the allocation it points into, and the drop below waits for the
        // queue to drain before releasing anything.
        unsafe {
            dispatch_set_context(source, Arc::as_ptr(&held) as *mut c_void);
            dispatch_source_set_event_handler_f(source, on_pressure);
            dispatch_resume(source);
        }
        tracing::debug!("watching host memory pressure");
        Ok(Watcher {
            source,
            queue,
            _held: held,
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Same shape as the file-system watcher's teardown, and for the same
        // reason: a handler may be mid-flight on the queue, and releasing the
        // queue out from under it reads freed memory.
        // SAFETY: a live source and its own serial queue.
        unsafe {
            dispatch_source_cancel(self.source);
            dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing);
            dispatch_release(self.source);
            dispatch_release(self.queue);
        }
    }
}

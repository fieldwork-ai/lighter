//! Noticing that the Mac went to sleep.
//!
//! A lighter machine has no real-time clock. The guest is told the time once,
//! on the kernel command line, and counts from there — which works exactly
//! until the host suspends, because the guest's counter suspends with it and
//! wakes however many minutes or hours behind.
//!
//! Nothing about that fails loudly. TLS is what breaks first, and it breaks by
//! reporting that a certificate is not yet valid, which sends whoever is
//! debugging to look at the certificate. Containers that log timestamps quietly
//! log the wrong ones. `make` decides everything is up to date.
//!
//! macOS publishes the event: IOKit's root power domain notifies subscribers
//! before it sleeps and again once the machine is back, and
//! `kIOMessageSystemHasPoweredOn` is the one that matters here.

use std::ffi::c_void;
use std::sync::Arc;

/// `kIOMessageSystemWillSleep`, from `IOMessage.h`.
const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
/// `kIOMessageSystemHasPoweredOn`.
const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;
/// `kIOMessageCanSystemSleep`, which must be answered or the machine waits.
const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;

type IoServiceInterestCallback =
    extern "C" fn(refcon: *mut c_void, service: u32, message_type: u32, argument: *mut c_void);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        notification_port: *mut *mut c_void,
        callback: IoServiceInterestCallback,
        notifier: *mut u32,
    ) -> u32;
    fn IODeregisterForSystemPower(notifier: *mut u32) -> i32;
    fn IONotificationPortGetRunLoopSource(port: *mut c_void) -> *const c_void;
    fn IONotificationPortDestroy(port: *mut c_void);
    fn IOAllowPowerChange(root_port: u32, notification_id: isize) -> i32;

    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRunLoopStop(run_loop: *mut c_void);
    static kCFRunLoopCommonModes: *const c_void;
}

/// What a watcher reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    WillSleep,
    Woke,
}

/// What to do about it.
pub trait Observer: Send + Sync {
    fn power(&self, event: Power);
}

struct Held {
    /// Filled in immediately after registering, because IOKit only tells you
    /// the port you must acknowledge on once you have registered with it — and
    /// no notification can arrive before the run loop source is attached, which
    /// happens later still.
    root_port: std::sync::atomic::AtomicU32,
    observer: Box<dyn Observer>,
}

// SAFETY: only touched from the run loop thread this is bound to, and from the
// drop that stops that thread.
unsafe impl Send for Held {}
unsafe impl Sync for Held {}

impl Held {
    fn port(&self) -> u32 {
        self.root_port.load(std::sync::atomic::Ordering::Acquire)
    }
}

extern "C" fn on_power(refcon: *mut c_void, _service: u32, message: u32, argument: *mut c_void) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is what `Arc::as_ptr` returned for the `Held` the
    // watcher keeps alive for as long as the notification is registered.
    let held = unsafe { &*(refcon as *const Held) };
    match message {
        K_IO_MESSAGE_CAN_SYSTEM_SLEEP => {
            // Not answering this holds the whole machine awake for thirty
            // seconds while it waits for us. There is nothing we need to
            // veto: a suspended guest is fine, it is the waking that needs
            // attention.
            // SAFETY: the root port we registered with, and the notification
            // id the callback was given.
            unsafe { IOAllowPowerChange(held.port(), argument as isize) };
        }
        K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
            held.observer.power(Power::WillSleep);
            // SAFETY: as above. This one is not optional either — the system
            // waits for an acknowledgement before it actually sleeps.
            unsafe { IOAllowPowerChange(held.port(), argument as isize) };
        }
        K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => held.observer.power(Power::Woke),
        _ => {}
    }
}

/// A live subscription to the Mac's sleep and wake.
pub struct Watcher {
    thread: Option<std::thread::JoinHandle<()>>,
    run_loop: Arc<std::sync::Mutex<Option<usize>>>,
    _held: Arc<Held>,
}

impl Watcher {
    /// Starts watching. The callback runs on a thread of its own.
    pub fn start(observer: Box<dyn Observer>) -> Result<Watcher, String> {
        let run_loop: Arc<std::sync::Mutex<Option<usize>>> = Arc::new(std::sync::Mutex::new(None));
        let (ready, wait) = std::sync::mpsc::channel::<Result<Arc<Held>, String>>();
        let loop_slot = run_loop.clone();

        // IOKit's power notifications are delivered to a run loop, and a run
        // loop needs a thread that does nothing but turn it. This is the one
        // place in the VMM that needs one; everything else uses dispatch.
        let thread = std::thread::Builder::new()
            .name("power".into())
            .spawn(move || {
                let held = Arc::new(Held {
                    root_port: std::sync::atomic::AtomicU32::new(0),
                    observer,
                });
                let mut port: *mut c_void = std::ptr::null_mut();
                let mut notifier: u32 = 0;
                // SAFETY: output parameters we own, and a callback with the
                // signature IOKit documents. The context outlives the
                // registration because the watcher holds the same `Arc`.
                let root = unsafe {
                    IORegisterForSystemPower(
                        Arc::as_ptr(&held) as *mut c_void,
                        &mut port,
                        on_power,
                        &mut notifier,
                    )
                };
                if root == 0 || port.is_null() {
                    let _ = ready.send(Err("IOKit refused a power notification".into()));
                    return;
                }
                held.root_port
                    .store(root, std::sync::atomic::Ordering::Release);

                // SAFETY: a live notification port, and the current thread's
                // run loop, which exists for the life of this closure. Nothing
                // can be delivered before this line, which is why filling in
                // the port above is not a race.
                unsafe {
                    let source = IONotificationPortGetRunLoopSource(port);
                    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
                    *loop_slot.lock().expect("power loop slot poisoned") =
                        Some(CFRunLoopGetCurrent() as usize);
                }
                let _ = ready.send(Ok(held));
                // SAFETY: turns this thread's run loop until someone stops it.
                unsafe { CFRunLoopRun() };
                // SAFETY: tearing down what was registered above.
                unsafe {
                    IODeregisterForSystemPower(&mut notifier);
                    IONotificationPortDestroy(port);
                }
            })
            .map_err(|e| e.to_string())?;

        match wait.recv() {
            Ok(Ok(held)) => {
                tracing::debug!("watching for sleep and wake");
                Ok(Watcher {
                    thread: Some(thread),
                    run_loop,
                    _held: held,
                })
            }
            Ok(Err(why)) => Err(why),
            Err(_) => Err("the power watcher thread died before it started".into()),
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(address) = *self.run_loop.lock().expect("power loop slot poisoned") {
            // SAFETY: the run loop the watcher thread is turning, which is
            // alive until it returns from `CFRunLoopRun`.
            unsafe { CFRunLoopStop(address as *mut c_void) };
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

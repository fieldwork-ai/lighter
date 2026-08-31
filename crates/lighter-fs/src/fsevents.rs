//! Watching the host for changes, so the guest can be allowed to cache.
//!
//! # Why a filesystem this fast needs a watcher
//!
//! Caching in the guest is the only way to make a shared directory quick: with
//! nothing cached, resolving one six-component path costs six round trips, and
//! `npm ci` resolves millions of them. Caching is also the only way to make it
//! wrong, because the host can change a file the guest believes it knows.
//!
//! Every other implementation resolves that with a fixed timeout and lives with
//! the window. We can do better, because macOS will tell us: FSEvents reports a
//! host-side change within milliseconds, and a directory the host is touching
//! gets zero cache validity while a directory it is not gets a generous one.
//! The result is exact coherence exactly when it matters — while you are
//! editing — and full speed the rest of the time.
//!
//! # What we cannot do, and why the design is shaped around it
//!
//! FUSE has a reverse channel for invalidating what the guest already cached.
//! virtio-fs does not carry it: Linux's driver (6.12) has a high-priority queue
//! and request queues and nothing else, so a notification has nowhere to go.
//! Invalidation is therefore *pull*, not push — the guest asks again when the
//! validity we handed out expires, and all we control is that number. Which
//! makes the number the entire policy, and this module the thing that sets it.

use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;

/// Opaque Core Foundation and Core Services types. Only ever held as pointers.
#[allow(non_camel_case_types)]
type CFRef = *const c_void;
#[allow(non_camel_case_types)]
type FSEventStreamRef = *mut c_void;

type FSEventStreamCallback = extern "C" fn(
    stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
);

#[repr(C)]
struct FSEventStreamContext {
    version: i64,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW: u64 = 0xFFFF_FFFF_FFFF_FFFF;
/// Deliver events as `CFStringRef` paths, watch the directory recursively, and
/// report the deepest path involved rather than the watched root.
const K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER: u32 = 0x0000_0002;
const K_FS_EVENT_STREAM_CREATE_FLAG_WATCH_ROOT: u32 = 0x0000_0004;
/// Do not report changes this process made.
///
/// Load-bearing rather than an optimization: the guest's writes reach the disk
/// through this process, so without it a package install inside the container
/// would look like furious host activity and switch off the very caching that
/// makes it fast.
const K_FS_EVENT_STREAM_CREATE_FLAG_IGNORE_SELF: u32 = 0x0000_0008;
const K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS: u32 = 0x0000_0010;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFRef,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths_to_watch: CFRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: *mut c_void);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);

    fn CFStringCreateWithBytes(
        allocator: CFRef,
        bytes: *const u8,
        num_bytes: isize,
        encoding: u32,
        is_external_representation: u8,
    ) -> CFRef;
    fn CFArrayCreate(
        allocator: CFRef,
        values: *const CFRef,
        num_values: isize,
        callbacks: *const c_void,
    ) -> CFRef;
    fn CFRelease(cf: CFRef);
    fn CFStringGetCString(
        string: CFRef,
        buffer: *mut u8,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;

    static kCFTypeArrayCallBacks: c_void;

    fn dispatch_queue_create(label: *const i8, attr: *mut c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

/// What a watcher does with each changed path.
pub trait Observer: Send + Sync {
    fn changed(&self, path: &Path);
}

/// A running FSEvents stream.
///
/// Dropping it stops the stream and releases the queue, in that order — the
/// callback may be running on the queue at the moment of the drop, and
/// releasing the queue first would free it out from under the callback.
pub struct Watcher {
    stream: FSEventStreamRef,
    queue: *mut c_void,
    /// Kept alive because the callback holds a raw pointer into it.
    _observer: Arc<Box<dyn Observer>>,
}

// SAFETY: every field is only touched on drop, and FSEvents' own operations are
// documented as safe to call from any thread once the stream has a queue.
unsafe impl Send for Watcher {}
unsafe impl Sync for Watcher {}

extern "C" fn on_events(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    _event_flags: *const u32,
    _event_ids: *const u64,
) {
    if info.is_null() || num_events == 0 {
        return;
    }
    // SAFETY: `info` is what `Arc::as_ptr` returned, which points at the
    // `Box<dyn Observer>` *inside* the allocation — not at the `Arc` itself.
    // Reading it back as an `Arc` was this module's first bug and cost a
    // segfault with no message: the pointer is valid, it is simply the wrong
    // type, so the deref succeeds and the vtable is garbage.
    let observer = unsafe { &*(info as *const Box<dyn Observer>) };
    // With kFSEventStreamCreateFlagUseCFTypes unset, the paths arrive as a
    // plain C array of C strings, which is simpler to walk than a CFArray.
    let paths = event_paths as *const *const i8;
    for index in 0..num_events {
        // SAFETY: FSEvents guarantees `num_events` valid entries.
        let raw = unsafe { *paths.add(index) };
        if raw.is_null() {
            continue;
        }
        // SAFETY: each entry is a NUL-terminated path owned by the caller for
        // the duration of this callback.
        let bytes = unsafe { std::ffi::CStr::from_ptr(raw) }.to_bytes();
        let path = Path::new(std::str::from_utf8(bytes).unwrap_or(""));
        observer.changed(path);
    }
}

impl Watcher {
    /// Starts watching `root` and everything under it.
    ///
    /// `latency` is how long FSEvents may coalesce events before delivering
    /// them. It is the floor on how quickly the guest can be told to stop
    /// trusting its cache, so it is set small and paid for in wakeups.
    pub fn start(
        root: &Path,
        latency: std::time::Duration,
        observer: Box<dyn Observer>,
    ) -> Result<Watcher, String> {
        let observer = Arc::new(observer);
        let path = root.to_string_lossy();

        // SAFETY: a UTF-8 byte range of the stated length, and a null allocator
        // meaning the default. The returned string is released below.
        let cf_path = unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                path.as_bytes().as_ptr(),
                path.len() as isize,
                K_CF_STRING_ENCODING_UTF8,
                0,
            )
        };
        if cf_path.is_null() {
            return Err(format!("{} is not a path Core Foundation accepts", root.display()));
        }
        // SAFETY: a one-element array of the CFString just created, with Core
        // Foundation's own retain/release callbacks.
        let paths = unsafe {
            CFArrayCreate(
                std::ptr::null(),
                &cf_path,
                1,
                &kCFTypeArrayCallBacks as *const c_void,
            )
        };
        // SAFETY: the array retained it; our own reference is done with.
        unsafe { CFRelease(cf_path) };
        if paths.is_null() {
            return Err("could not build the watch list".into());
        }

        let context = FSEventStreamContext {
            version: 0,
            info: Arc::as_ptr(&observer) as *mut c_void,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };

        // SAFETY: all arguments are valid for the duration of the call, and the
        // context's `info` outlives the stream because the `Watcher` holds the
        // `Arc` it points into.
        let stream = unsafe {
            FSEventStreamCreate(
                std::ptr::null(),
                on_events,
                &context,
                paths,
                K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
                latency.as_secs_f64(),
                K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER
                    | K_FS_EVENT_STREAM_CREATE_FLAG_WATCH_ROOT
                    | K_FS_EVENT_STREAM_CREATE_FLAG_IGNORE_SELF
                    | K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS,
            )
        };
        // SAFETY: the stream retained the array.
        unsafe { CFRelease(paths) };
        if stream.is_null() {
            return Err("FSEvents refused to create a stream".into());
        }

        // A serial dispatch queue rather than a run loop: a run loop needs a
        // thread that does nothing but turn it, and the callback here is short
        // enough that serializing it costs nothing.
        let label = c"com.lighter.fs.events";
        // SAFETY: a static NUL-terminated label and a null attribute, which
        // means a serial queue.
        let queue = unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null_mut()) };
        // SAFETY: a live stream and a live queue.
        unsafe { FSEventStreamSetDispatchQueue(stream, queue) };
        // SAFETY: a live stream with a queue set.
        if unsafe { FSEventStreamStart(stream) } == 0 {
            // SAFETY: a created but unstarted stream.
            unsafe {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
            }
            return Err("FSEvents refused to start the stream".into());
        }

        tracing::debug!(root = %root.display(), "watching the host for changes");
        Ok(Watcher {
            stream,
            queue,
            _observer: observer,
        })
    }
}

extern "C" fn nothing(_: *mut c_void) {}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The order is the whole of this function, and getting it wrong is a
        // use-after-free that presents as an occasional SIGBUS in whichever
        // test happened to rename a file.
        //
        // `FSEventStreamInvalidate` promises that no *further* callback will be
        // dispatched. It promises nothing about one that is already running,
        // and that one holds a pointer into `_observer` — which this `Drop`
        // is about to release. So after invalidating, we push an empty piece of
        // work through the queue and wait for it: the queue is serial, so when
        // that returns, any callback ahead of it has finished.
        //
        // Only then may the queue be released, and only then may the fields
        // drop — which they do after this function returns, by which point
        // nothing can be looking at them.
        // SAFETY: a live stream and a live serial queue that this type owns.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing);
            dispatch_release(self.queue);
        }
    }
}

/// Unused, but kept declared: `CFStringGetCString` is the fallback path if a
/// future flag change makes FSEvents deliver CFStrings instead of C strings.
#[allow(dead_code)]
fn cf_string_unused() -> u8 {
    // SAFETY: never called; exists so the declaration is checked by the linker.
    unsafe { CFStringGetCString(std::ptr::null(), std::ptr::null_mut(), 0, 0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    struct Collector(Arc<Mutex<Vec<std::path::PathBuf>>>);

    impl Observer for Collector {
        fn changed(&self, path: &Path) {
            self.0.lock().unwrap().push(path.to_path_buf());
        }
    }

    fn watched_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("lighter-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // FSEvents reports resolved paths, and /var/folders is a symlink.
        std::fs::canonicalize(&root).unwrap()
    }

    fn wait_for(seen: &Arc<Mutex<Vec<std::path::PathBuf>>>, name: &str, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if seen.lock().unwrap().iter().any(|p| p.ends_with(name)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// The claim the whole caching policy rests on: a change made on the host,
    /// by something that is not us, is reported quickly enough to be useful as
    /// an invalidation signal.
    #[test]
    fn a_host_change_is_reported() {
        let root = watched_root("host");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let watcher = Watcher::start(
            &root,
            Duration::from_millis(5),
            Box::new(Collector(seen.clone())),
        )
        .expect("FSEvents should start on any Mac");

        // The stream is asynchronous: a write issued before it is genuinely
        // running is not reported, and that is a race the test would lose on a
        // loaded machine rather than a wrong one.
        std::thread::sleep(Duration::from_millis(300));
        // Another process, because our own writes are deliberately invisible.
        assert!(
            std::process::Command::new("/usr/bin/touch")
                .arg(root.join("touched"))
                .status()
                .unwrap()
                .success()
        );

        let found = wait_for(&seen, "touched", Duration::from_secs(10));
        let paths = seen.lock().unwrap().clone();
        drop(watcher);
        let _ = std::fs::remove_dir_all(&root);
        assert!(found, "FSEvents never reported the write; it reported {paths:?}");
    }

    /// The teardown race, run enough times to catch it. A stream torn down
    /// while its callback is mid-flight reads freed memory, and the symptom is
    /// an occasional SIGBUS with no connection to the code that caused it.
    #[test]
    fn a_watcher_can_be_dropped_while_events_are_arriving() {
        for round in 0..20 {
            let root = watched_root(&format!("teardown-{round}"));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let watcher = Watcher::start(
                &root,
                Duration::from_millis(1),
                Box::new(Collector(seen.clone())),
            )
            .unwrap();
            let mut writer = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "for i in $(seq 1 200); do touch {}/f$i; done",
                    root.display()
                ))
                .spawn()
                .unwrap();
            // Dropped with the shell still writing, which is the window.
            drop(watcher);
            let _ = writer.wait();
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// The other half, and the one that makes caching possible at all: writes
    /// *we* make are not reported. The guest's package install reaches the disk
    /// through this process, and if those writes came back as host activity
    /// they would switch off the caching they most need.
    #[test]
    fn our_own_writes_are_not_reported() {
        let root = watched_root("self");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let watcher = Watcher::start(
            &root,
            Duration::from_millis(5),
            Box::new(Collector(seen.clone())),
        )
        .expect("FSEvents should start on any Mac");

        std::thread::sleep(Duration::from_millis(300));
        for index in 0..50 {
            std::fs::write(root.join(format!("ours-{index}")), b"x").unwrap();
        }
        // A control, made by another process, so the test can tell "nothing was
        // reported because IgnoreSelf worked" from "nothing was reported
        // because the stream was not running".
        std::process::Command::new("/usr/bin/touch")
            .arg(root.join("theirs"))
            .status()
            .unwrap();

        let control = wait_for(&seen, "theirs", Duration::from_secs(10));
        let ours = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.to_string_lossy().contains("ours-"))
            .count();
        drop(watcher);
        let _ = std::fs::remove_dir_all(&root);

        assert!(control, "the stream was not delivering events at all");
        assert_eq!(ours, 0, "{ours} of our own writes came back as host activity");
    }
}

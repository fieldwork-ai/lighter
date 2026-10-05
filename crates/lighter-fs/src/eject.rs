//! Letting go of a volume as it is ejected.
//!
//! macOS will not unmount a volume while any process holds a file on it open,
//! and a share holds descriptors on purpose: the inodes the guest remembers
//! and the files it has been reading and writing, long after the container
//! that used them has exited. With `/Volumes` shared, every drive a container
//! had touched could not be ejected while the machine ran.
//!
//! Disk Arbitration asks a process that registers for it to approve each
//! unmount before attempting it, which is when Finder, `diskutil eject` and
//! `hdiutil detach` find out whether a volume is busy. The approval is the
//! moment to close what the shares hold on it, and then to approve.

use crate::inode::Pool;
use std::ffi::{c_char, c_void};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

type CFRef = *const c_void;
type Approval = extern "C" fn(disk: *mut c_void, context: *mut c_void) -> *mut c_void;

#[link(name = "DiskArbitration", kind = "framework")]
unsafe extern "C" {
    fn DASessionCreate(allocator: CFRef) -> *mut c_void;
    fn DASessionSetDispatchQueue(session: *mut c_void, queue: *mut c_void);
    fn DARegisterDiskUnmountApprovalCallback(
        session: *mut c_void,
        matching: CFRef,
        callback: Approval,
        context: *mut c_void,
    );
    fn DAUnregisterApprovalCallback(
        session: *mut c_void,
        callback: *mut c_void,
        context: *mut c_void,
    );
    fn DADiskCopyDescription(disk: *mut c_void) -> CFRef;
    static kDADiskDescriptionVolumePathKey: CFRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryGetValue(dictionary: CFRef, key: CFRef) -> CFRef;
    fn CFURLGetFileSystemRepresentation(
        url: CFRef,
        resolve: u8,
        buffer: *mut u8,
        length: isize,
    ) -> u8;
    fn CFRelease(cf: CFRef);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}

/// Approves every unmount, having had the pool's shares close what they hold
/// on the volume first. Dropping it stops the approvals.
pub struct Watch {
    session: *mut c_void,
    queue: *mut c_void,
    pool: *mut Pool,
}

// SAFETY: the session and queue are only touched in `drop`, and the pool the
// callback reads is `Sync`.
unsafe impl Send for Watch {}
unsafe impl Sync for Watch {}

impl Watch {
    pub fn start(pool: &Pool) -> std::io::Result<Watch> {
        // SAFETY: plain Disk Arbitration and libdispatch calls; the pool is
        // boxed so its address holds until `drop` unregisters the callback
        // and drains the queue it runs on.
        unsafe {
            let session = DASessionCreate(std::ptr::null());
            if session.is_null() {
                return Err(std::io::Error::other("no Disk Arbitration session"));
            }
            let queue = dispatch_queue_create(c"sh.lighter.eject".as_ptr(), std::ptr::null_mut());
            let pool = Box::into_raw(Box::new(pool.clone()));
            DARegisterDiskUnmountApprovalCallback(session, std::ptr::null(), approve, pool.cast());
            DASessionSetDispatchQueue(session, queue);
            Ok(Watch {
                session,
                queue,
                pool,
            })
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        extern "C" fn nothing(_: *mut c_void) {}
        // SAFETY: unregistered and detached from the queue, then the queue is
        // drained, so no callback holds the pool when it is freed.
        unsafe {
            DAUnregisterApprovalCallback(self.session, approve as *mut c_void, self.pool.cast());
            DASessionSetDispatchQueue(self.session, std::ptr::null_mut());
            dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing);
            CFRelease(self.session);
            dispatch_release(self.queue);
            drop(Box::from_raw(self.pool));
        }
    }
}

extern "C" fn approve(disk: *mut c_void, context: *mut c_void) -> *mut c_void {
    // SAFETY: the context is the pool `Watch::start` boxed, alive until the
    // callback is unregistered.
    let pool = unsafe { &*(context as *const Pool) };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(path) = volume_path(disk)
            && let Ok(meta) = std::fs::metadata(&path)
        {
            let closed = pool.release_device(meta.dev() as i64);
            tracing::info!(volume = %path.display(), closed, "a volume is being ejected");
        }
    }));
    // No dissenter: the unmount goes ahead.
    std::ptr::null_mut()
}

/// Where the disk is mounted, if it is.
fn volume_path(disk: *mut c_void) -> Option<PathBuf> {
    // SAFETY: the description is a copy we release; the URL is borrowed from
    // it and read before the release.
    unsafe {
        let description = DADiskCopyDescription(disk);
        if description.is_null() {
            return None;
        }
        let url = CFDictionaryGetValue(description, kDADiskDescriptionVolumePathKey);
        let mut buffer = [0u8; 1024];
        let path = (!url.is_null()
            && CFURLGetFileSystemRepresentation(
                url,
                1,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
            ) != 0)
            .then(|| {
                let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
                PathBuf::from(std::ffi::OsStr::from_encoded_bytes_unchecked(
                    &buffer[..end],
                ))
            });
        CFRelease(description);
        path
    }
}

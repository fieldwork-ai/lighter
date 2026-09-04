//! How much memory this process is actually costing the Mac.
//!
//! Not resident set size. A VM maps its guest's entire RAM up front, and RSS
//! counts every page the guest has ever touched whether or not it still wants
//! it — so a machine configured with 8 GiB that ran one build looks
//! permanently like 8 GiB, which is exactly the complaint people have about
//! virtual machines and exactly the thing this project exists to fix.
//!
//! `phys_footprint` is the number macOS itself uses for memory pressure and
//! for what Activity Monitor calls "Memory". Pages handed back with
//! `MADV_FREE_REUSABLE` leave it immediately, which makes it the honest
//! measure of whether the balloon and free page reporting are doing anything.

use std::sync::atomic::{AtomicBool, Ordering};

/// `TASK_VM_INFO`, from `<mach/task_info.h>`.
const TASK_VM_INFO: libc::c_int = 22;
/// `TASK_VM_INFO_COUNT`: the structure's size in 32-bit words.
const TASK_VM_INFO_COUNT: libc::c_uint = 87;

/// The prefix of `task_vm_info_data_t` up to and including `phys_footprint`.
///
/// Transcribed rather than bound because only one field is wanted and the
/// structure has grown several times; reading it by offset would be worse.
#[repr(C)]
#[derive(Default)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64,
    // The structure continues; `task_info` is told how many words we can hold
    // and fills only that many.
    tail: [u64; 16],
}

unsafe extern "C" {
    fn mach_task_self() -> libc::c_uint;
    fn task_info(
        target: libc::c_uint,
        flavor: libc::c_int,
        info: *mut libc::c_void,
        count: *mut libc::c_uint,
    ) -> libc::c_int;
}

/// This process's physical footprint, in bytes. Zero if it cannot be read.
pub fn bytes() -> u64 {
    let mut info = TaskVmInfo::default();
    let mut count = TASK_VM_INFO_COUNT
        .min((std::mem::size_of::<TaskVmInfo>() / std::mem::size_of::<u32>()) as libc::c_uint);
    // SAFETY: an output buffer we own, and a count that says how much of it the
    // kernel may fill — which is the whole contract of `task_info`.
    let rc = unsafe {
        task_info(
            mach_task_self(),
            TASK_VM_INFO,
            &mut info as *mut TaskVmInfo as *mut libc::c_void,
            &mut count,
        )
    };
    if rc != 0 { 0 } else { info.phys_footprint }
}

/// Resident, internal (anonymous) and reusable bytes, for a trace.
pub fn split() -> (u64, u64, u64) {
    let mut info = TaskVmInfo::default();
    let mut count = TASK_VM_INFO_COUNT
        .min((std::mem::size_of::<TaskVmInfo>() / std::mem::size_of::<u32>()) as libc::c_uint);
    // SAFETY: as in `bytes`.
    let rc = unsafe {
        task_info(
            mach_task_self(),
            TASK_VM_INFO,
            &mut info as *mut TaskVmInfo as *mut libc::c_void,
            &mut count,
        )
    };
    if rc != 0 {
        (0, 0, 0)
    } else {
        (info.resident_size, info.internal, info.reusable)
    }
}

/// Logs the footprint on an interval, for as long as the process lives.
///
/// Used by the memory gate, which has no other way to watch a number that only
/// this process can see — and by anyone wondering where their RAM went.
pub fn report_every(interval: std::time::Duration) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("footprint".into())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                tracing::info!(mib = bytes() / (1 << 20), "FOOTPRINT");
            }
        })
        .expect("failed to spawn the footprint reporter");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_process_has_a_footprint() {
        let before = bytes();
        assert!(before > 0, "task_info reported nothing");
        // Something large enough to move the number, and touched so it is
        // genuinely resident rather than merely reserved.
        let mut ballast = vec![0u8; 64 << 20];
        for page in ballast.chunks_mut(4096) {
            page[0] = 1;
        }
        let after = bytes();
        assert!(
            after > before,
            "footprint did not move: {before} then {after}"
        );
        drop(ballast);
    }
}

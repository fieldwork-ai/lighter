//! The balloon: how much of its memory the guest is asked to hand back.
//!
//! The framework's balloon device takes one number, the memory the guest may
//! keep; everything above it the guest's driver inflates into and the
//! framework releases to macOS. There is no free page reporting under the
//! framework (S1 read the device's feature bits: MUST_TELL_HOST and
//! DEFLATE_ON_OOM, nothing else), so this is the one channel memory goes
//! back through, and the policy in `memory_policy.rs` is what drives it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::vz::Vm;

pub struct Balloon {
    vm: Arc<Vm>,
    ram_bytes: u64,
    ballooned: AtomicU64,
    offered: AtomicU64,
}

impl Balloon {
    pub fn new(vm: Arc<Vm>, ram_bytes: u64) -> Balloon {
        Balloon {
            vm,
            ram_bytes,
            ballooned: AtomicU64::new(0),
            offered: AtomicU64::new(0),
        }
    }

    /// Asks the guest to give up `bytes` of its memory (rounded to a
    /// megabyte); zero deflates it entirely.
    pub fn set_ballooned_bytes(&self, bytes: u64) {
        let bytes = (bytes.min(self.ram_bytes) >> 20) << 20;
        if self.ballooned.swap(bytes, Ordering::Relaxed) == bytes {
            return;
        }
        self.vm.set_guest_memory(self.ram_bytes - bytes);
    }

    /// What the guest has been asked to give up.
    pub fn ballooned_bytes(&self) -> u64 {
        self.ballooned.load(Ordering::Relaxed)
    }

    /// The guest's most recent offer, for the record.
    pub fn note_offered(&self, bytes: u64) {
        self.offered.store(bytes, Ordering::Relaxed);
    }

    pub fn offered_bytes(&self) -> u64 {
        self.offered.load(Ordering::Relaxed)
    }

    pub fn ram_bytes(&self) -> u64 {
        self.ram_bytes
    }
}

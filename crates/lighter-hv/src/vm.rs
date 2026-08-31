//! The per-process virtual machine.

// Every `unsafe` block below is a single call into Hypervisor.framework whose
// safety argument is identical and is stated here once rather than repeated
// verbatim on each of them: the framework's calls are safe to make with
// well-formed arguments, and this module's types are what make the arguments
// well-formed — the VM exists (proved by holding a `Vm`), the vCPU handle came
// from `hv_vcpu_create` on this thread (proved by `Vcpu` being `!Send`), and
// out-parameters are stack locals of the right type.
//
// Blocks whose safety rests on anything more than that — raw pointers into
// guest memory, lifetimes the compiler cannot see — carry their own comment and
// live in modules where this allow is not in force.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Result, check};
use crate::sys;

/// Guest-physical memory permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPerms(u64);

impl MemoryPerms {
    pub const READ: MemoryPerms = MemoryPerms(sys::HV_MEMORY_READ);
    pub const WRITE: MemoryPerms = MemoryPerms(sys::HV_MEMORY_WRITE);
    pub const EXEC: MemoryPerms = MemoryPerms(sys::HV_MEMORY_EXEC);
    pub const RW: MemoryPerms = MemoryPerms(sys::HV_MEMORY_READ | sys::HV_MEMORY_WRITE);
    pub const RX: MemoryPerms = MemoryPerms(sys::HV_MEMORY_READ | sys::HV_MEMORY_EXEC);
    pub const RWX: MemoryPerms =
        MemoryPerms(sys::HV_MEMORY_READ | sys::HV_MEMORY_WRITE | sys::HV_MEMORY_EXEC);

    pub const fn bits(self) -> u64 {
        self.0
    }
}

impl std::ops::BitOr for MemoryPerms {
    type Output = MemoryPerms;
    fn bitor(self, rhs: MemoryPerms) -> MemoryPerms {
        MemoryPerms(self.0 | rhs.0)
    }
}

/// Only one VM may exist per process, so a second `Vm::create` must fail loudly
/// rather than return `HV_BUSY` from somewhere deep in a later call.
static VM_CREATED: AtomicBool = AtomicBool::new(false);

/// A virtual machine.
///
/// Exactly one may exist per process — an Apple constraint, not ours. `Vm` is
/// `Send + Sync` because mapping memory and creating vCPUs are callable from
/// any thread; it is the *vCPU* that is pinned (see [`crate::Vcpu`]).
#[derive(Debug)]
pub struct Vm {
    _private: (),
}

impl Vm {
    /// Creates the process's virtual machine with the default IPA size.
    pub fn create() -> Result<Vm> {
        Vm::create_with_ipa_bits(None)
    }

    /// Creates the VM, optionally widening the guest-physical address space.
    ///
    /// The default IPA size is generous on Apple silicon, but a VM that wants
    /// to place device windows high (or hot-plug a lot of RAM) has to ask.
    pub fn create_with_ipa_bits(ipa_bits: Option<u32>) -> Result<Vm> {
        if VM_CREATED.swap(true, Ordering::SeqCst) {
            return Err(crate::HvError::Busy);
        }

        let result = unsafe {
            match ipa_bits {
                None => check(sys::hv_vm_create(std::ptr::null_mut())),
                Some(bits) => {
                    let config = sys::hv_vm_config_create();
                    if config.is_null() {
                        return Err(crate::HvError::NoResources);
                    }
                    let r = check(sys::hv_vm_config_set_ipa_size(config, bits))
                        .and_then(|()| check(sys::hv_vm_create(config)));
                    sys::os_release(config);
                    r
                }
            }
        };

        match result {
            Ok(()) => Ok(Vm { _private: () }),
            Err(e) => {
                VM_CREATED.store(false, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    /// The largest IPA size this host supports, in bits.
    pub fn max_ipa_bits() -> Result<u32> {
        let mut bits = 0u32;
        unsafe { check(sys::hv_vm_config_get_max_ipa_size(&mut bits))? };
        Ok(bits)
    }

    /// The most vCPUs this host will allow in one VM.
    pub fn max_vcpu_count(&self) -> Result<u32> {
        let mut count = 0u32;
        unsafe { check(sys::hv_vm_get_max_vcpu_count(&mut count))? };
        Ok(count)
    }

    /// Maps host memory into the guest's physical address space.
    ///
    /// # Safety
    /// `addr` must point to at least `size` bytes of memory that stays valid,
    /// and stays at that address, until the region is unmapped. Guest code can
    /// write through this mapping at any time, so the host must not hold a
    /// `&`-reference to the same bytes across a `Vcpu::run`.
    pub unsafe fn map(
        &self,
        addr: *mut c_void,
        ipa: u64,
        size: usize,
        perms: MemoryPerms,
    ) -> Result<()> {
        unsafe { check(sys::hv_vm_map(addr, ipa, size, perms.bits())) }
    }

    /// Removes a guest-physical mapping.
    ///
    /// # Safety
    /// No vCPU may be executing code that touches `[ipa, ipa + size)`.
    pub unsafe fn unmap(&self, ipa: u64, size: usize) -> Result<()> {
        unsafe { check(sys::hv_vm_unmap(ipa, size)) }
    }

    /// Changes the permissions on an existing guest-physical mapping.
    ///
    /// Write-protecting a region is how dirty-page tracking is built: the guest
    /// then faults out on the next store, and the VMM records the page.
    pub fn protect(&self, ipa: u64, size: usize, perms: MemoryPerms) -> Result<()> {
        unsafe { check(sys::hv_vm_protect(ipa, size, perms.bits())) }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // Destroying the VM while a vCPU thread is still inside hv_vcpu_run is
        // undefined; the VMM is responsible for joining those threads first.
        unsafe {
            let _ = sys::hv_vm_destroy();
        }
        VM_CREATED.store(false, Ordering::SeqCst);
    }
}

// SAFETY: hv_vm_map/unmap/protect and hv_vcpu_create are callable from any
// thread. The per-thread constraint applies to a created vCPU, which is
// modelled separately and is deliberately neither Send nor Sync.
unsafe impl Send for Vm {}
unsafe impl Sync for Vm {}

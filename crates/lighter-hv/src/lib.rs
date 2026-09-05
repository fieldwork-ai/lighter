//! Safe bindings to Apple's `Hypervisor.framework` on Apple silicon.
//!
//! # The secret this crate keeps
//!
//! Everything macOS-specific about *how* we get a CPU and memory lives here.
//! Callers ask for a VM, guest-physical mappings, vCPUs and an interrupt
//! controller; they never see an `hv_*` symbol. That boundary is not decorative
//! — it is the escape hatch that lets the machine model move to a different
//! substrate without a rewrite, the way OrbStack moved off
//! Virtualization.framework in a point release.
//!
//! # The three rules the framework enforces
//!
//! 1. **One VM per process.** [`Vm::create`] returns [`HvError::Busy`] for a
//!    second call rather than letting a later, unrelated call fail.
//! 2. **A vCPU belongs to the thread that created it.** [`Vcpu`] is neither
//!    `Send` nor `Sync`, so "one thread per vCPU" is a compile error to get
//!    wrong. [`VcpuHandle`] is the sanctioned cross-thread capability and can
//!    do exactly one thing: force an exit.
//! 3. **The GIC is created between the VM and the first vCPU.** [`Gic::create`]
//!    takes `&Vm` for the first half; the second half is documented and
//!    asserted by the boot path, since the type system cannot see it.
//!
//! # Entitlement
//!
//! Every process touching this API needs `com.apple.security.hypervisor`. An
//! unsigned binary fails at [`Vm::create`] with [`HvError::Denied`]; `make sign`
//! ad-hoc signs the test and debug binaries.

#![deny(clippy::undocumented_unsafe_blocks)]

mod error;
mod gic;
mod regs;
mod vcpu;
mod vm;

pub mod sys;

pub use error::{HvError, Result};
pub use gic::{Gic, GicLayout, GicParameters};
pub use regs::{ACTLR_EL1_TSO, InterruptType, PSTATE_EL1H_DAIF_MASKED, Reg, SysReg};
pub use vcpu::{Exception, Exit, Vcpu, VcpuHandle};
pub use vm::{MemoryPerms, Vm};

/// Whether this host can run hardware-accelerated VMs at all.
///
/// False on hardware without the virtualization extensions, and — the case
/// that actually bites — inside a VM, which is why hosted CI cannot run the
/// integration gates.
pub fn hv_supported() -> bool {
    let mut supported: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    let name = c"kern.hv_support";
    // SAFETY: sysctlbyname writes at most `size` bytes into `supported`, and
    // the name is a valid NUL-terminated C string with 'static lifetime.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut supported).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    rc == 0 && supported == 1
}

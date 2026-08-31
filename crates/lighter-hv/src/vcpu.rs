//! A virtual CPU, pinned to the thread that created it.

use std::marker::PhantomData;

use crate::error::{Result, check};
use crate::regs::{InterruptType, Reg, SysReg};
use crate::sys;
use crate::vm::Vm;

/// Why the guest stopped executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Another thread called [`VcpuHandle::force_exit`].
    Canceled,
    /// The guest took a synchronous exception to EL2 — an MMIO access, a
    /// hypercall (`HVC`), a trapped system register, or a fault.
    Exception(Exception),
    /// The virtual timer became pending. The VMM must inject the guest's timer
    /// interrupt and clear the mask before this can fire again.
    VTimerActivated,
    /// The framework could not say. Treated as fatal by the VMM.
    Unknown,
}

/// Details of a guest exception exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exception {
    /// ESR_EL2 — exception class in bits 31:26, ISS in bits 24:0.
    pub syndrome: u64,
    /// FAR_EL2 — the faulting virtual address.
    pub virtual_address: u64,
    /// The faulting guest-physical address, valid for stage-2 aborts.
    pub physical_address: u64,
}

impl Exception {
    /// Exception class: `ESR_ELx.EC`, bits 31:26.
    #[inline]
    pub const fn class(self) -> u8 {
        ((self.syndrome >> 26) & 0x3f) as u8
    }

    /// Instruction-specific syndrome: `ESR_ELx.ISS`, bits 24:0.
    #[inline]
    pub const fn iss(self) -> u32 {
        (self.syndrome & 0x01ff_ffff) as u32
    }

    /// `EC == 0b100100`: a stage-2 data abort — the guest touched an address
    /// with no RAM behind it, which is how every MMIO access reaches us.
    pub const EC_DATA_ABORT_LOWER_EL: u8 = 0b100100;
    /// `EC == 0b100000`: a stage-2 instruction abort.
    pub const EC_INSN_ABORT_LOWER_EL: u8 = 0b100000;
    /// `EC == 0b010110`: `HVC` executed from AArch64 — PSCI and our own
    /// hypercalls arrive this way.
    pub const EC_HVC64: u8 = 0b010110;
    /// `EC == 0b011000`: a trapped MSR/MRS/system instruction.
    pub const EC_SYSREG_TRAP: u8 = 0b011000;
    /// `EC == 0b111100`: `BRK` — only seen when debug trapping is enabled.
    pub const EC_BRK64: u8 = 0b111100;
}

/// A handle to a vCPU that other threads may hold.
///
/// The only operation that is legal off-thread is forcing the vCPU out of
/// `run`, which is exactly what a VMM needs to stop a guest, deliver a signal,
/// or tear the machine down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcpuHandle(sys::hv_vcpu_t);

impl VcpuHandle {
    /// The vCPU's identifier, which is also its MPIDR affinity index and the
    /// key the GIC uses to address its redistributor.
    #[inline]
    pub const fn id(self) -> u64 {
        self.0
    }

    /// Forces the vCPU out of [`Vcpu::run`], which returns [`Exit::Canceled`].
    ///
    /// If the vCPU is not currently running, the next `run` returns
    /// immediately — the cancellation is latched, not lost, so a VMM shutting
    /// down never races the guest back into execution.
    pub fn force_exit(self) -> Result<()> {
        let ids = [self.0];
        unsafe { check(sys::hv_vcpus_exit(ids.as_ptr(), 1)) }
    }
}

// SAFETY: hv_vcpus_exit is explicitly callable from any thread; VcpuHandle
// exposes nothing else.
unsafe impl Send for VcpuHandle {}
unsafe impl Sync for VcpuHandle {}

/// A virtual CPU.
///
/// `hv_vcpu_create` binds the vCPU to the calling thread and every subsequent
/// call except `hv_vcpus_exit` must come from that same thread, so `Vcpu` is
/// deliberately neither `Send` nor `Sync`: the VMM's "one thread per vCPU"
/// structure is enforced by the compiler rather than by a comment.
#[derive(Debug)]
pub struct Vcpu {
    id: sys::hv_vcpu_t,
    exit: *mut sys::hv_vcpu_exit_t,
    /// Makes the type neither `Send` nor `Sync`.
    _not_send: PhantomData<*const ()>,
}

impl Vm {
    /// Creates a vCPU bound to the current thread.
    ///
    /// Must be called *after* the GIC exists, if there is going to be one:
    /// `hv_gic_create` allocates per-vCPU interrupt state and refuses to run
    /// once any vCPU has been created.
    pub fn create_vcpu(&self) -> Result<Vcpu> {
        let mut id: sys::hv_vcpu_t = 0;
        let mut exit: *mut sys::hv_vcpu_exit_t = std::ptr::null_mut();
        unsafe {
            check(sys::hv_vcpu_create(&mut id, &mut exit, std::ptr::null_mut()))?;
        }
        debug_assert!(!exit.is_null(), "framework returned a null exit pointer");
        Ok(Vcpu {
            id,
            exit,
            _not_send: PhantomData,
        })
    }
}

impl Vcpu {
    /// A handle that may be sent to other threads.
    #[inline]
    pub const fn handle(&self) -> VcpuHandle {
        VcpuHandle(self.id)
    }

    #[inline]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Runs the guest until it exits.
    pub fn run(&mut self) -> Result<Exit> {
        unsafe { check(sys::hv_vcpu_run(self.id))? };

        // SAFETY: the framework owns this allocation for the lifetime of the
        // vCPU and updates it in place on every return from hv_vcpu_run.
        let exit = unsafe { *self.exit };
        Ok(match exit.reason {
            sys::HV_EXIT_REASON_CANCELED => Exit::Canceled,
            sys::HV_EXIT_REASON_EXCEPTION => Exit::Exception(Exception {
                syndrome: exit.exception.syndrome,
                virtual_address: exit.exception.virtual_address,
                physical_address: exit.exception.physical_address,
            }),
            sys::HV_EXIT_REASON_VTIMER_ACTIVATED => Exit::VTimerActivated,
            _ => Exit::Unknown,
        })
    }

    pub fn reg(&self, reg: Reg) -> Result<u64> {
        let mut value = 0u64;
        unsafe { check(sys::hv_vcpu_get_reg(self.id, reg as u32, &mut value))? };
        Ok(value)
    }

    pub fn set_reg(&mut self, reg: Reg, value: u64) -> Result<()> {
        unsafe { check(sys::hv_vcpu_set_reg(self.id, reg as u32, value)) }
    }

    pub fn sys_reg(&self, reg: SysReg) -> Result<u64> {
        let mut value = 0u64;
        unsafe { check(sys::hv_vcpu_get_sys_reg(self.id, reg.0, &mut value))? };
        Ok(value)
    }

    pub fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()> {
        unsafe { check(sys::hv_vcpu_set_sys_reg(self.id, reg.0, value)) }
    }

    /// Makes an interrupt pending directly on the vCPU.
    ///
    /// With an in-kernel GIC this is only the virtual-timer path; device
    /// interrupts are raised as SPIs through [`crate::Gic::set_spi`].
    pub fn set_pending_interrupt(&mut self, typ: InterruptType, pending: bool) -> Result<()> {
        unsafe { check(sys::hv_vcpu_set_pending_interrupt(self.id, typ as u32, pending)) }
    }

    pub fn pending_interrupt(&self, typ: InterruptType) -> Result<bool> {
        let mut pending = false;
        unsafe {
            check(sys::hv_vcpu_get_pending_interrupt(
                self.id,
                typ as u32,
                &mut pending,
            ))?
        };
        Ok(pending)
    }

    /// Masks or unmasks the virtual timer.
    ///
    /// A [`Exit::VTimerActivated`] exit sets the mask implicitly; the VMM
    /// clears it when the guest acknowledges the timer interrupt, otherwise the
    /// exit never fires again and the guest's clock stops advancing.
    pub fn set_vtimer_mask(&mut self, masked: bool) -> Result<()> {
        unsafe { check(sys::hv_vcpu_set_vtimer_mask(self.id, masked)) }
    }

    pub fn vtimer_mask(&self) -> Result<bool> {
        let mut masked = false;
        unsafe { check(sys::hv_vcpu_get_vtimer_mask(self.id, &mut masked))? };
        Ok(masked)
    }

    /// Offsets the guest's view of the virtual counter.
    ///
    /// Advancing this by the time spent suspended is what keeps a resumed VM
    /// from believing hours passed inside one instruction.
    pub fn set_vtimer_offset(&mut self, offset: u64) -> Result<()> {
        unsafe { check(sys::hv_vcpu_set_vtimer_offset(self.id, offset)) }
    }

    pub fn vtimer_offset(&self) -> Result<u64> {
        let mut offset = 0u64;
        unsafe { check(sys::hv_vcpu_get_vtimer_offset(self.id, &mut offset))? };
        Ok(offset)
    }

    /// Nanoseconds of host CPU this vCPU has consumed executing guest code.
    pub fn exec_time(&self) -> Result<u64> {
        let mut time = 0u64;
        unsafe { check(sys::hv_vcpu_get_exec_time(self.id, &mut time))? };
        Ok(time)
    }

    /// Routes guest debug exceptions to the VMM instead of the guest.
    ///
    /// Needed to catch `BRK`, which is how the boot smoke test signals success
    /// without a console.
    pub fn set_trap_debug_exceptions(&mut self, trap: bool) -> Result<()> {
        unsafe { check(sys::hv_vcpu_set_trap_debug_exceptions(self.id, trap)) }
    }

    pub fn set_trap_debug_reg_accesses(&mut self, trap: bool) -> Result<()> {
        unsafe { check(sys::hv_vcpu_set_trap_debug_reg_accesses(self.id, trap)) }
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        unsafe {
            let _ = sys::hv_vcpu_destroy(self.id);
        }
    }
}

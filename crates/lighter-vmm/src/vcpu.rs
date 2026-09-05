//! The vCPU run loop.
//!
//! One of these runs on each vCPU thread. It is the hottest code in the VMM:
//! every device access the guest makes is a round trip through here, so the
//! structure is deliberately flat — decode the exit, service it, run again,
//! with no allocation and no locking beyond the device's own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lighter_hv::{Exception, Exit, Reg, SysReg, Vcpu, VcpuHandle};

use crate::bus::{FaultError, MmioBus, MmioFault};
use crate::exitstats;
use crate::psci::{AffinityState, PSCI_VERSION, PsciCall, PsciReturn};
use crate::smp::{CpuPark, StartRequest};
use crate::sysreg::{self, SysRegAccess, SysRegAction};

/// Why the run loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The guest asked to power the machine off.
    SystemOff,
    /// The guest asked to reboot.
    SystemReset,
    /// This core powered itself down via `CPU_OFF`.
    CpuOff,
    /// The VMM asked the guest to stop.
    Shutdown,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("hypervisor error running vCPU {vcpu}: {source}")]
    Hypervisor {
        vcpu: u64,
        #[source]
        source: lighter_hv::HvError,
    },
    #[error("vCPU {vcpu} took an unhandled {kind} at pc={pc:#x} (esr={esr:#x}, far={far:#x})")]
    UnhandledException {
        vcpu: u64,
        kind: &'static str,
        pc: u64,
        esr: u64,
        far: u64,
    },
    #[error("vCPU {vcpu}: {source}")]
    Fault {
        vcpu: u64,
        #[source]
        source: FaultError,
    },
    #[error("the hypervisor could not determine why vCPU {vcpu} exited")]
    UnknownExit { vcpu: u64 },
    #[error(
        "core {expected} was given vCPU id {actual}: the framework hands out ids \
         in call order and the GIC assigns redistributors by id, so a mismatch \
         means the device tree describes a different machine than the one running"
    )]
    VcpuIdMismatch { expected: u64, actual: u64 },
}

/// Shared state every vCPU thread needs.
pub struct RunContext {
    pub bus: MmioBus,
    pub park: Arc<CpuPark>,
    pub shutdown: Arc<AtomicBool>,
    /// Every running core's cross-thread handle.
    ///
    /// A vCPU sitting inside `hv_vcpu_run` cannot see the shutdown flag — that
    /// is only checked between exits — so stopping the machine means forcing
    /// each core out of the guest. Threads publish their handle here as soon as
    /// they have one, because a `Vcpu` cannot leave its own thread.
    pub handles: Mutex<Vec<VcpuHandle>>,
}

impl RunContext {
    /// Signals shutdown and forces every core out of the guest.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.park.shutdown();
        for handle in self
            .handles
            .lock()
            .expect("handle registry poisoned")
            .iter()
        {
            // A core that has already exited is not an error: the cancellation
            // latches and its next run returns immediately.
            let _ = handle.force_exit();
        }
    }
}

/// Drives one vCPU.
pub struct VcpuRunner {
    vcpu: Vcpu,
    index: u32,
    ctx: Arc<RunContext>,
}

impl VcpuRunner {
    pub fn new(vcpu: Vcpu, index: u32, ctx: Arc<RunContext>) -> VcpuRunner {
        VcpuRunner { vcpu, index, ctx }
    }

    /// Puts the core into the state the arm64 boot protocol specifies.
    ///
    /// `Documentation/arch/arm64/booting.rst`: MMU off, caches off, x0 holding
    /// the device tree address, x1..x3 zero, and the CPU at EL1 with interrupts
    /// masked so the kernel's own entry code decides when to take them.
    pub fn prepare_boot(&mut self, entry: u64, dtb: u64) -> Result<(), RunError> {
        self.set_reg(Reg::Pc, entry)?;
        self.set_reg(Reg::X0, dtb)?;
        self.set_reg(Reg::X1, 0)?;
        self.set_reg(Reg::X2, 0)?;
        self.set_reg(Reg::X3, 0)?;
        self.set_reg(Reg::Cpsr, lighter_hv::PSTATE_EL1H_DAIF_MASKED)?;
        self.set_affinity()?;
        Ok(())
    }

    /// Puts a secondary core at the entry point PSCI `CPU_ON` named.
    fn prepare_secondary(&mut self, req: StartRequest) -> Result<(), RunError> {
        self.set_reg(Reg::Pc, req.entry_point)?;
        // The context id is handed straight back to the kernel in x0; it uses
        // it to find that core's per-CPU data.
        self.set_reg(Reg::X0, req.context_id)?;
        self.set_reg(Reg::X1, 0)?;
        self.set_reg(Reg::X2, 0)?;
        self.set_reg(Reg::X3, 0)?;
        self.set_reg(Reg::Cpsr, lighter_hv::PSTATE_EL1H_DAIF_MASKED)?;
        self.set_affinity()?;
        Ok(())
    }

    /// Makes MPIDR_EL1 agree with the `reg` this core has in the device tree.
    ///
    /// The kernel matches device-tree CPU nodes to running cores by affinity,
    /// so a mismatch means a secondary that starts and is never recognised.
    /// Bit 31 of MPIDR is RES1 on ARMv8.
    fn set_affinity(&mut self) -> Result<(), RunError> {
        let mpidr = (1u64 << 31) | u64::from(self.index);
        match self.vcpu.set_sys_reg(SysReg::MPIDR_EL1, mpidr) {
            Ok(()) => Ok(()),
            // Some hosts present MPIDR as read-only. If its existing value
            // already encodes this core's index we are fine; otherwise SMP
            // would silently half-work, so say so loudly.
            Err(_) => {
                let actual = self.vcpu.sys_reg(SysReg::MPIDR_EL1).map_err(|source| {
                    RunError::Hypervisor {
                        vcpu: self.vcpu.id(),
                        source,
                    }
                })?;
                if actual & 0xff != u64::from(self.index) {
                    tracing::warn!(
                        index = self.index,
                        mpidr = format_args!("{actual:#x}"),
                        "MPIDR_EL1 is read-only and disagrees with the device tree; \
                         secondary CPUs may not come up"
                    );
                }
                Ok(())
            }
        }
    }

    /// Runs until the guest or the VMM stops it.
    pub fn run(&mut self) -> Result<StopReason, RunError> {
        loop {
            if self.ctx.shutdown.load(Ordering::Relaxed) {
                return Ok(StopReason::Shutdown);
            }

            let exit = self.vcpu.run().map_err(|source| RunError::Hypervisor {
                vcpu: self.vcpu.id(),
                source,
            })?;

            match exit {
                Exit::Canceled => {
                    exitstats::bump(exitstats::Kind::Canceled);
                    if self.ctx.shutdown.load(Ordering::Relaxed) {
                        return Ok(StopReason::Shutdown);
                    }
                    // A cancellation we did not ask for is a spurious wakeup;
                    // re-entering the guest is always safe.
                }
                Exit::VTimerActivated => {
                    exitstats::bump(exitstats::Kind::VTimer);
                    self.handle_vtimer()?
                }
                Exit::Unknown => {
                    return Err(RunError::UnknownExit {
                        vcpu: self.vcpu.id(),
                    });
                }
                Exit::Exception(exception) => {
                    exitstats::bump(match exception.class() {
                        Exception::EC_DATA_ABORT_LOWER_EL => exitstats::Kind::Mmio,
                        Exception::EC_HVC64 => exitstats::Kind::Hvc,
                        Exception::EC_SYSREG_TRAP => exitstats::Kind::SysReg,
                        _ => exitstats::Kind::Other,
                    });
                    if let Some(stop) = self.handle_exception(exception)? {
                        return Ok(stop);
                    }
                }
            }
        }
    }

    /// The virtual timer fired.
    ///
    /// The exit latches a mask that must be cleared before the timer can fire
    /// again. With Apple's in-kernel GIC the controller delivers INTID 27 to
    /// the guest itself, so there is nothing to inject — clearing the mask is
    /// the whole job. Leaving it set is a guest whose clock silently stops.
    fn handle_vtimer(&mut self) -> Result<(), RunError> {
        self.vcpu
            .set_vtimer_mask(false)
            .map_err(|source| RunError::Hypervisor {
                vcpu: self.vcpu.id(),
                source,
            })
    }

    fn handle_exception(&mut self, exception: Exception) -> Result<Option<StopReason>, RunError> {
        match exception.class() {
            Exception::EC_DATA_ABORT_LOWER_EL => {
                self.handle_mmio(exception)?;
                Ok(None)
            }
            Exception::EC_HVC64 => self.handle_hvc(),
            Exception::EC_SYSREG_TRAP => {
                self.handle_sysreg_trap(exception)?;
                Ok(None)
            }
            other => {
                let kind = match other {
                    Exception::EC_INSN_ABORT_LOWER_EL => "instruction abort",
                    Exception::EC_BRK64 => "breakpoint",
                    _ => "exception",
                };
                Err(RunError::UnhandledException {
                    vcpu: self.vcpu.id(),
                    kind,
                    pc: self.reg(Reg::Pc)?,
                    esr: exception.syndrome,
                    far: exception.virtual_address,
                })
            }
        }
    }

    /// Services a device access.
    fn handle_mmio(&mut self, exception: Exception) -> Result<(), RunError> {
        let fault = MmioFault::decode(&exception).map_err(|source| RunError::Fault {
            vcpu: self.vcpu.id(),
            source,
        })?;

        if fault.is_write {
            // Register 31 is XZR in a store, not X31 — reading it as a GPR
            // would address a register that does not exist.
            let value = if fault.reg == 31 {
                0
            } else {
                self.reg(Reg::gpr(fault.reg).expect("syndrome register index is 5 bits"))?
            };
            let bytes = value.to_le_bytes();
            self.ctx.bus.write(fault.address, &bytes[..fault.size]);
        } else {
            let mut bytes = [0u8; 8];
            self.ctx.bus.read(fault.address, &mut bytes[..fault.size]);
            let raw = u64::from_le_bytes(bytes);
            // A load into register 31 discards its result.
            if fault.reg != 31 {
                let value = fault.extend_loaded_value(raw);
                self.set_reg(
                    Reg::gpr(fault.reg).expect("syndrome register index is 5 bits"),
                    value,
                )?;
            }
        }

        // The faulting instruction did not retire, so step past it. All A64
        // instructions are four bytes, which is why this is safe to do blind.
        let pc = self.reg(Reg::Pc)?;
        self.set_reg(Reg::Pc, pc + 4)?;
        Ok(())
    }

    /// Services a trapped system-register access.
    ///
    /// The hypervisor traps the debug and performance-monitor register space to
    /// EL2. These describe hardware the guest does not have, so they read as
    /// zero and swallow writes; see [`crate::sysreg`] for why that is the right
    /// answer rather than a shortcut.
    fn handle_sysreg_trap(&mut self, exception: Exception) -> Result<(), RunError> {
        let access = SysRegAccess::decode(exception.iss());
        match sysreg::policy_for(&access) {
            SysRegAction::ReadAsZero => {
                // Register 31 is XZR: a read into it is discarded.
                if access.rt != 31 {
                    self.set_reg(
                        Reg::gpr(access.rt).expect("syndrome register index is 5 bits"),
                        0,
                    )?;
                }
            }
            SysRegAction::Ignore => {}
        }

        // The trapped instruction did not retire.
        let pc = self.reg(Reg::Pc)?;
        self.set_reg(Reg::Pc, pc + 4)
    }

    /// Services a hypercall. Every `HVC` a Linux guest makes here is PSCI.
    fn handle_hvc(&mut self) -> Result<Option<StopReason>, RunError> {
        let args = [
            self.reg(Reg::X0)?,
            self.reg(Reg::X1)?,
            self.reg(Reg::X2)?,
            self.reg(Reg::X3)?,
        ];

        // Unlike a trapped instruction, HVC *does* retire — the return address
        // is already past it, so advancing PC here would skip an instruction.
        let result = match PsciCall::from_function_id(args) {
            PsciCall::Version => PSCI_VERSION,
            PsciCall::Features { query } => {
                if PsciCall::is_implemented(query) {
                    PsciReturn::Success.as_reg()
                } else {
                    PsciReturn::NotSupported.as_reg()
                }
            }
            // No trusted OS to migrate, which is what "2" means.
            PsciCall::MigrateInfoType => 2,
            PsciCall::CpuOn {
                target_cpu,
                entry_point,
                context_id,
            } => {
                let index = mpidr_to_index(target_cpu);
                self.ctx
                    .park
                    .power_on(
                        index,
                        StartRequest {
                            entry_point,
                            context_id,
                        },
                    )
                    .as_reg()
            }
            PsciCall::AffinityInfo {
                target_affinity, ..
            } => {
                let index = mpidr_to_index(target_affinity);
                match self.ctx.park.is_on(index) {
                    Some(true) => AffinityState::On as u64,
                    Some(false) => AffinityState::Off as u64,
                    None => PsciReturn::InvalidParams.as_reg(),
                }
            }
            PsciCall::CpuSuspend => {
                // Treated as a no-op that returns immediately. A guest idling a
                // core loops back into WFI, which the hardware handles without
                // exiting — so idle costs nothing without us modelling states.
                PsciReturn::Success.as_reg()
            }
            PsciCall::CpuOff => {
                self.ctx.park.power_off(self.index);
                return Ok(Some(StopReason::CpuOff));
            }
            PsciCall::SystemOff => return Ok(Some(StopReason::SystemOff)),
            PsciCall::SystemReset => return Ok(Some(StopReason::SystemReset)),
            PsciCall::NotSupported { function_id } => {
                tracing::debug!(
                    function_id = format_args!("{function_id:#x}"),
                    "guest called an unimplemented PSCI function"
                );
                PsciReturn::NotSupported.as_reg()
            }
        };

        self.set_reg(Reg::X0, result)?;
        Ok(None)
    }

    /// Parks a secondary core until PSCI powers it on, then runs it.
    ///
    /// Returns when the machine shuts down.
    pub fn run_secondary(&mut self) -> Result<StopReason, RunError> {
        loop {
            let Some(request) = self.ctx.park.wait_for_power_on(self.index) else {
                return Ok(StopReason::Shutdown);
            };
            self.prepare_secondary(request)?;
            match self.run()? {
                // A core that powers itself off goes back to waiting, which is
                // exactly what CPU hotplug expects.
                StopReason::CpuOff => continue,
                other => return Ok(other),
            }
        }
    }

    #[inline]
    fn reg(&self, reg: Reg) -> Result<u64, RunError> {
        self.vcpu.reg(reg).map_err(|source| RunError::Hypervisor {
            vcpu: self.vcpu.id(),
            source,
        })
    }

    #[inline]
    fn set_reg(&mut self, reg: Reg, value: u64) -> Result<(), RunError> {
        self.vcpu
            .set_reg(reg, value)
            .map_err(|source| RunError::Hypervisor {
                vcpu: self.vcpu.id(),
                source,
            })
    }
}

/// Extracts a core index from an MPIDR-style affinity value.
///
/// Our cores are flat: affinity level 0 only, so the index is the low byte.
#[inline]
fn mpidr_to_index(mpidr: u64) -> u32 {
    (mpidr & 0xff) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_ignores_the_res1_bit_and_upper_levels() {
        assert_eq!(mpidr_to_index(0), 0);
        assert_eq!(mpidr_to_index(3), 3);
        assert_eq!(mpidr_to_index(0x8000_0003), 3);
        assert_eq!(mpidr_to_index(0x0100_0002), 2);
    }
}

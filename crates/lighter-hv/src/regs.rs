//! Guest register identifiers.
//!
//! These mirror the ARM architecture rather than any macOS API, which is why
//! exposing them from this crate does not leak a design decision: the register
//! set is fixed by the hardware, not by our choice of hypervisor.

/// General-purpose and special registers addressable via `hv_vcpu_{get,set}_reg`.
///
/// The discriminants are `hv_reg_t`, which is a plain ordinal enumeration in
/// declaration order — X0..X30, then PC, FPCR, FPSR, CPSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
#[allow(dead_code)]
pub enum Reg {
    X0 = 0,
    X1,
    X2,
    X3,
    X4,
    X5,
    X6,
    X7,
    X8,
    X9,
    X10,
    X11,
    X12,
    X13,
    X14,
    X15,
    X16,
    X17,
    X18,
    X19,
    X20,
    X21,
    X22,
    X23,
    X24,
    X25,
    X26,
    X27,
    X28,
    X29,
    X30,
    Pc,
    Fpcr,
    Fpsr,
    Cpsr,
}

impl Reg {
    /// The frame pointer is an alias of X29.
    pub const FP: Reg = Reg::X29;
    /// The link register is an alias of X30.
    pub const LR: Reg = Reg::X30;

    /// X0..X30 by index, for bulk save/restore and PSCI argument shuffling.
    pub const fn gpr(index: u8) -> Option<Reg> {
        if index > 30 {
            return None;
        }
        // SAFETY: discriminants 0..=30 are exactly X0..X30, checked above.
        Some(unsafe { std::mem::transmute::<u32, Reg>(index as u32) })
    }
}

/// A system register, encoded the way both ARM and `hv_sys_reg_t` encode one.
///
/// `hv_sys_reg_t` is not an opaque enumeration: its values are the standard
/// `op0:op1:CRn:CRm:op2` packing, which is why we can name registers Apple's
/// headers never enumerated. Verified against the SDK: MPIDR_EL1 (3,0,0,0,5)
/// packs to 0xc005, SCTLR_EL1 (3,0,1,0,0) to 0xc080 — both match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysReg(pub u16);

impl SysReg {
    #[inline]
    pub const fn new(op0: u16, op1: u16, crn: u16, crm: u16, op2: u16) -> SysReg {
        SysReg((op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2)
    }

    pub const MIDR_EL1: SysReg = SysReg::new(3, 0, 0, 0, 0);
    pub const MPIDR_EL1: SysReg = SysReg::new(3, 0, 0, 0, 5);
    pub const ID_AA64PFR0_EL1: SysReg = SysReg::new(3, 0, 0, 4, 0);
    pub const ID_AA64MMFR0_EL1: SysReg = SysReg::new(3, 0, 0, 7, 0);
    pub const SCTLR_EL1: SysReg = SysReg::new(3, 0, 1, 0, 0);
    pub const CPACR_EL1: SysReg = SysReg::new(3, 0, 1, 0, 2);
    pub const TTBR0_EL1: SysReg = SysReg::new(3, 0, 2, 0, 0);
    pub const TTBR1_EL1: SysReg = SysReg::new(3, 0, 2, 0, 1);
    pub const TCR_EL1: SysReg = SysReg::new(3, 0, 2, 0, 2);
    pub const MAIR_EL1: SysReg = SysReg::new(3, 0, 10, 2, 0);
    pub const VBAR_EL1: SysReg = SysReg::new(3, 0, 12, 0, 0);
    pub const CNTV_CTL_EL0: SysReg = SysReg::new(3, 3, 14, 3, 1);
    pub const CNTV_CVAL_EL0: SysReg = SysReg::new(3, 3, 14, 3, 2);
    pub const CNTFRQ_EL0: SysReg = SysReg::new(3, 3, 14, 0, 0);

    /// `ACTLR_EL1` on Apple silicon carries the implementation-defined
    /// Total Store Ordering enable in bit 1. Rosetta's x86 memory-model
    /// guarantees depend on it; see `docs/rosetta.md`.
    pub const ACTLR_EL1: SysReg = SysReg::new(3, 0, 1, 0, 1);
}

/// Bit 1 of `ACTLR_EL1`: enable Total Store Ordering for this vCPU.
pub const ACTLR_EL1_TSO: u64 = 1 << 1;

/// Interrupt lines that can be made pending on a vCPU directly.
///
/// With an in-kernel GIC these are only used for the virtual timer path; a
/// guest with a GICv3 takes its device interrupts as SPIs instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum InterruptType {
    Irq = crate::sys::HV_INTERRUPT_TYPE_IRQ,
    Fiq = crate::sys::HV_INTERRUPT_TYPE_FIQ,
}

/// PSTATE value for entering the guest at EL1h with DAIF masked.
///
/// EL1h (bits[3:0] = 0b0101) selects EL1 with its own stack pointer; bits 6..9
/// mask FIQ, IRQ, SError and Debug so the kernel's own entry code decides when
/// to take interrupts.
pub const PSTATE_EL1H_DAIF_MASKED: u64 = 0x3c5;

#[cfg(test)]
mod tests {
    use super::*;

    // The whole SysReg scheme rests on the packing matching Apple's enum. If
    // this ever drifts, every sysreg access silently addresses the wrong
    // register, so pin the values the SDK actually publishes.
    #[test]
    fn sysreg_encoding_matches_sdk_values() {
        assert_eq!(SysReg::MIDR_EL1.0, 0xc000);
        assert_eq!(SysReg::MPIDR_EL1.0, 0xc005);
        assert_eq!(SysReg::SCTLR_EL1.0, 0xc080);
        assert_eq!(SysReg::CNTV_CTL_EL0.0, 0xdf19);
        assert_eq!(SysReg::CNTV_CVAL_EL0.0, 0xdf1a);
    }

    #[test]
    fn gpr_index_maps_to_declaration_order() {
        assert_eq!(Reg::gpr(0), Some(Reg::X0));
        assert_eq!(Reg::gpr(30), Some(Reg::X30));
        assert_eq!(Reg::gpr(31), None);
        assert_eq!(Reg::X30 as u32, 30);
        assert_eq!(Reg::Pc as u32, 31);
        assert_eq!(Reg::Cpsr as u32, 34);
    }
}

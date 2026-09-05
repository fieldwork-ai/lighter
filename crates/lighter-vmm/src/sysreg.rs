//! Trapped system-register accesses.
//!
//! Apple's hypervisor traps a slice of the system-register space to EL2 —
//! debug OS-lock registers, the performance monitors, and a few others — so a
//! guest that touches them exits to us instead of executing. Every one of these
//! is a register whose real behaviour concerns physical debug hardware the
//! guest does not have, which is why the answer is almost always the same:
//! reads see zero, writes are discarded.
//!
//! Getting this wrong is not subtle. Linux clears the OS lock unconditionally
//! during `debug_monitors` init, on every boot, before it has a console the
//! user can see — so a VMM that treats the trap as fatal dies at exactly the
//! same line every time with no clue why.

/// A decoded `MSR`/`MRS` trap (`EC == 0b011000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysRegAccess {
    pub op0: u8,
    pub op1: u8,
    pub crn: u8,
    pub crm: u8,
    pub op2: u8,
    /// The general-purpose register the value comes from or goes to.
    /// 31 means XZR, not X31.
    pub rt: u8,
    /// True for `MRS` (register read), false for `MSR` (register write).
    pub is_read: bool,
}

impl SysRegAccess {
    /// Decodes the ISS of a system-register trap.
    pub const fn decode(iss: u32) -> SysRegAccess {
        SysRegAccess {
            op0: ((iss >> 20) & 0x3) as u8,
            op2: ((iss >> 17) & 0x7) as u8,
            op1: ((iss >> 14) & 0x7) as u8,
            crn: ((iss >> 10) & 0xf) as u8,
            rt: ((iss >> 5) & 0x1f) as u8,
            crm: ((iss >> 1) & 0xf) as u8,
            is_read: iss & 1 == 1,
        }
    }

    /// The register's `op0:op1:CRn:CRm:op2` tuple, for matching and logging.
    pub const fn encoding(&self) -> (u8, u8, u8, u8, u8) {
        (self.op0, self.op1, self.crn, self.crm, self.op2)
    }

    /// A name for the registers we expect to see, for diagnostics.
    pub const fn name(&self) -> Option<&'static str> {
        Some(match self.encoding() {
            (2, 0, 1, 0, 4) => "OSLAR_EL1",
            (2, 0, 1, 1, 4) => "OSLSR_EL1",
            (2, 0, 1, 3, 4) => "OSDLR_EL1",
            (2, 0, 1, 4, 4) => "DBGPRCR_EL1",
            (2, 0, 0, 2, 2) => "MDSCR_EL1",
            (2, 0, 7, 8, 6) => "DBGCLAIMSET_EL1",
            (2, 0, 7, 9, 6) => "DBGCLAIMCLR_EL1",
            (2, 0, 7, 14, 6) => "DBGAUTHSTATUS_EL1",
            (3, 3, 9, 12, 0) => "PMCR_EL0",
            (3, 3, 9, 12, 1) => "PMCNTENSET_EL0",
            (3, 3, 9, 12, 2) => "PMCNTENCLR_EL0",
            (3, 3, 9, 12, 3) => "PMOVSCLR_EL0",
            (3, 3, 9, 12, 5) => "PMSELR_EL0",
            (3, 3, 9, 13, 0) => "PMCCNTR_EL0",
            (3, 3, 9, 14, 0) => "PMUSERENR_EL0",
            (3, 0, 9, 14, 1) => "PMINTENSET_EL1",
            (3, 0, 9, 14, 2) => "PMINTENCLR_EL1",
            _ => return None,
        })
    }

    /// Whether this register is one we expect to be trapped, and can safely
    /// treat as read-as-zero / write-ignored.
    ///
    /// The breakpoint and watchpoint register files (`CRn == 0`, `op0 == 2`)
    /// are included as ranges rather than enumerated: there are up to sixteen
    /// of each and their behaviour here is identical.
    pub const fn is_known_raz_wi(&self) -> bool {
        match self.encoding() {
            // Debug OS lock and claim registers.
            (2, 0, 1, _, 4) | (2, 0, 7, _, 6) => true,
            // MDSCR_EL1 and the rest of the CRn==0 debug file: DBGBVR, DBGBCR,
            // DBGWVR, DBGWCR all live here, indexed by CRm.
            (2, 0, 0, _, _) => true,
            // Performance monitors. A container guest reading zeroed counters
            // is correct-ish; a guest that dies because it looked is not.
            (3, 0 | 3, 9, _, _) => true,
            _ => false,
        }
    }
}

/// What the run loop should do with a trapped access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysRegAction {
    /// Write this value into the destination register, then advance past the
    /// instruction.
    ReadAsZero,
    /// Discard the value and advance past the instruction.
    Ignore,
}

/// Decides how to service a trapped access.
///
/// Unknown registers are still handled rather than fatal — an unimplemented
/// register reading as zero is what much real hardware does, and killing a
/// guest over one is a worse failure than answering it — but they are logged,
/// because an unexpected trap is a fact worth knowing.
pub fn policy_for(access: &SysRegAccess) -> SysRegAction {
    if !access.is_known_raz_wi() {
        let (op0, op1, crn, crm, op2) = access.encoding();
        tracing::warn!(
            register = access.name().unwrap_or("unknown"),
            encoding = format_args!("s{op0}_{op1}_c{crn}_c{crm}_{op2}"),
            is_read = access.is_read,
            "guest touched an unexpected trapped system register; \
             answering read-as-zero / write-ignored"
        );
    }

    if access.is_read {
        SysRegAction::ReadAsZero
    } else {
        SysRegAction::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an ISS the way the architecture lays it out, so the decoder is
    /// tested against the spec's field positions rather than against itself.
    const fn iss(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32, rt: u32, read: bool) -> u32 {
        (op0 << 20) | (op2 << 17) | (op1 << 14) | (crn << 10) | (rt << 5) | (crm << 1) | read as u32
    }

    /// The exact trap that stopped the first boot: `MSR OSDLR_EL1, XZR` from
    /// Linux's `clear_os_lock`, ESR 0x622807e6.
    #[test]
    fn decodes_the_trap_that_stopped_the_first_boot() {
        let access = SysRegAccess::decode(0x622807e6 & 0x01ff_ffff);
        assert_eq!(access.encoding(), (2, 0, 1, 3, 4));
        assert_eq!(access.name(), Some("OSDLR_EL1"));
        assert_eq!(access.rt, 31, "the kernel writes XZR");
        assert!(!access.is_read, "it is a write");
        assert!(access.is_known_raz_wi());
        assert_eq!(policy_for(&access), SysRegAction::Ignore);
    }

    #[test]
    fn decodes_every_field_independently() {
        let a = SysRegAccess::decode(iss(3, 3, 9, 12, 0, 7, true));
        assert_eq!(a.encoding(), (3, 3, 9, 12, 0));
        assert_eq!(a.rt, 7);
        assert!(a.is_read);
        assert_eq!(a.name(), Some("PMCR_EL0"));
    }

    #[test]
    fn os_lock_registers_are_all_recognised() {
        for crm in [0, 1, 3, 4] {
            let a = SysRegAccess::decode(iss(2, 0, 1, crm, 4, 0, false));
            assert!(a.is_known_raz_wi(), "op0=2 op1=0 crn=1 crm={crm} op2=4");
        }
    }

    #[test]
    fn reads_are_answered_with_zero_and_writes_are_dropped() {
        let read = SysRegAccess::decode(iss(2, 0, 1, 1, 4, 3, true));
        assert_eq!(policy_for(&read), SysRegAction::ReadAsZero);
        let write = SysRegAccess::decode(iss(2, 0, 1, 0, 4, 3, false));
        assert_eq!(policy_for(&write), SysRegAction::Ignore);
    }

    #[test]
    fn unknown_registers_are_answered_rather_than_fatal() {
        let a = SysRegAccess::decode(iss(3, 0, 12, 8, 0, 1, true));
        assert!(!a.is_known_raz_wi());
        assert_eq!(policy_for(&a), SysRegAction::ReadAsZero);
    }
}

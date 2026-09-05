//! PSCI — the Power State Coordination Interface.
//!
//! This is how the guest kernel brings up secondary cores, powers them down,
//! and asks to halt or reboot the machine. On real hardware it is implemented
//! by firmware below the kernel; here there is no firmware, so the VMM answers
//! the `HVC` directly.
//!
//! Only the subset a Linux guest actually calls is implemented, and
//! [`PsciCall::from_function_id`] returns `NotSupported` for the rest rather
//! than guessing — a wrong answer to `CPU_SUSPEND` is a guest that never wakes.

/// What the guest asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsciCall {
    Version,
    CpuOn {
        target_cpu: u64,
        entry_point: u64,
        context_id: u64,
    },
    CpuOff,
    AffinityInfo {
        target_affinity: u64,
        lowest_affinity_level: u32,
    },
    /// The guest is idling a core. We treat this as "run until an interrupt",
    /// which is what makes an idle guest cost no host CPU.
    CpuSuspend,
    SystemOff,
    SystemReset,
    Features {
        query: u32,
    },
    MigrateInfoType,
    NotSupported {
        function_id: u32,
    },
}

/// PSCI return codes, as the specification defines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum PsciReturn {
    Success = 0,
    NotSupported = -1,
    InvalidParams = -2,
    Denied = -3,
    AlreadyOn = -4,
    OnPending = -5,
    InternalFailure = -6,
    NotPresent = -7,
    Disabled = -8,
    InvalidAddress = -9,
}

impl PsciReturn {
    /// The value to place in X0, sign-extended the way the caller reads it.
    pub const fn as_reg(self) -> u64 {
        self as i32 as i64 as u64
    }
}

/// PSCI 1.1, reported as major 1, minor 1.
pub const PSCI_VERSION: u64 = (1 << 16) | 1;

// Function IDs. The 0x84 prefix is the 32-bit calling convention, 0xC4 the
// 64-bit one; a 64-bit guest uses the latter for anything taking an address,
// and both must be accepted for the calls that exist in both forms.
const PSCI_VERSION_ID: u32 = 0x8400_0000;
const CPU_SUSPEND_32: u32 = 0x8400_0001;
const CPU_SUSPEND_64: u32 = 0xc400_0001;
const CPU_OFF_ID: u32 = 0x8400_0002;
const CPU_ON_32: u32 = 0x8400_0003;
const CPU_ON_64: u32 = 0xc400_0003;
const AFFINITY_INFO_32: u32 = 0x8400_0004;
const AFFINITY_INFO_64: u32 = 0xc400_0004;
const MIGRATE_INFO_TYPE_ID: u32 = 0x8400_0006;
const SYSTEM_OFF_ID: u32 = 0x8400_0008;
const SYSTEM_RESET_ID: u32 = 0x8400_0009;
const PSCI_FEATURES_ID: u32 = 0x8400_000a;

impl PsciCall {
    /// Decodes a PSCI call from the guest's argument registers.
    ///
    /// `args` are X0..X3 at the point of the `HVC`.
    pub fn from_function_id(args: [u64; 4]) -> PsciCall {
        let function_id = args[0] as u32;
        match function_id {
            PSCI_VERSION_ID => PsciCall::Version,
            CPU_ON_32 | CPU_ON_64 => PsciCall::CpuOn {
                target_cpu: args[1],
                entry_point: args[2],
                context_id: args[3],
            },
            CPU_OFF_ID => PsciCall::CpuOff,
            CPU_SUSPEND_32 | CPU_SUSPEND_64 => PsciCall::CpuSuspend,
            AFFINITY_INFO_32 | AFFINITY_INFO_64 => PsciCall::AffinityInfo {
                target_affinity: args[1],
                lowest_affinity_level: args[2] as u32,
            },
            SYSTEM_OFF_ID => PsciCall::SystemOff,
            SYSTEM_RESET_ID => PsciCall::SystemReset,
            PSCI_FEATURES_ID => PsciCall::Features {
                query: args[1] as u32,
            },
            MIGRATE_INFO_TYPE_ID => PsciCall::MigrateInfoType,
            other => PsciCall::NotSupported { function_id: other },
        }
    }

    /// Whether `PSCI_FEATURES` should report this function as implemented.
    pub fn is_implemented(function_id: u32) -> bool {
        !matches!(
            PsciCall::from_function_id([u64::from(function_id), 0, 0, 0]),
            PsciCall::NotSupported { .. }
        )
    }
}

/// Affinity states reported by `AFFINITY_INFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum AffinityState {
    On = 0,
    Off = 1,
    OnPending = 2,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_both_calling_conventions() {
        // A 64-bit guest uses the 0xC4 form for CPU_ON; accepting only one
        // convention is a machine where secondary cores never start.
        for id in [CPU_ON_32, CPU_ON_64] {
            assert_eq!(
                PsciCall::from_function_id([u64::from(id), 3, 0x4008_0000, 0xabc]),
                PsciCall::CpuOn {
                    target_cpu: 3,
                    entry_point: 0x4008_0000,
                    context_id: 0xabc,
                }
            );
        }
        for id in [CPU_SUSPEND_32, CPU_SUSPEND_64] {
            assert_eq!(
                PsciCall::from_function_id([u64::from(id), 0, 0, 0]),
                PsciCall::CpuSuspend
            );
        }
    }

    #[test]
    fn unknown_functions_are_refused_not_guessed() {
        assert_eq!(
            PsciCall::from_function_id([0x8400_00ff, 0, 0, 0]),
            PsciCall::NotSupported {
                function_id: 0x8400_00ff
            }
        );
        assert!(!PsciCall::is_implemented(0x8400_00ff));
        assert!(PsciCall::is_implemented(CPU_ON_64));
    }

    #[test]
    fn return_codes_sign_extend_the_way_the_guest_reads_them() {
        assert_eq!(PsciReturn::Success.as_reg(), 0);
        assert_eq!(PsciReturn::NotSupported.as_reg(), u64::MAX);
        assert_eq!(PsciReturn::AlreadyOn.as_reg(), u64::MAX - 3);
    }

    #[test]
    fn version_is_one_point_one() {
        assert_eq!(PSCI_VERSION >> 16, 1);
        assert_eq!(PSCI_VERSION & 0xffff, 1);
    }
}

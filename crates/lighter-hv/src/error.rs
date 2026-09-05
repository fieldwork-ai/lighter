//! Error type for `Hypervisor.framework` calls.

use crate::sys;

/// A failure returned by `Hypervisor.framework`.
///
/// The framework's error vocabulary is small and unhelpfully generic, so each
/// variant carries the note that actually explains it in practice — most
/// first-run failures are `Denied` (missing entitlement) or `Busy` (a second VM
/// in one process), and saying so here saves the next person an afternoon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HvError {
    #[error("hypervisor: operation failed (HV_ERROR)")]
    Error,
    #[error(
        "hypervisor: resource busy (HV_BUSY) — a process may create only one VM, \
         and a vCPU may only be driven from the thread that created it"
    )]
    Busy,
    #[error("hypervisor: invalid argument (HV_BAD_ARGUMENT)")]
    BadArgument,
    #[error("hypervisor: out of resources (HV_NO_RESOURCES)")]
    NoResources,
    #[error("hypervisor: no such VM or vCPU (HV_NO_DEVICE)")]
    NoDevice,
    #[error(
        "hypervisor: denied (HV_DENIED) — the binary is missing the \
         com.apple.security.hypervisor entitlement; run `make sign`"
    )]
    Denied,
    #[error("hypervisor: fault (HV_FAULT)")]
    Fault,
    #[error(
        "hypervisor: unsupported (HV_UNSUPPORTED) — this call needs a newer \
         macOS than the host is running"
    )]
    Unsupported,
    #[error("hypervisor: unrecognized status {0:#x}")]
    Unknown(u32),
}

pub type Result<T> = std::result::Result<T, HvError>;

/// Converts a raw `hv_return_t` into a `Result`.
#[inline]
pub fn check(status: sys::hv_return_t) -> Result<()> {
    match status {
        sys::HV_SUCCESS => Ok(()),
        sys::HV_ERROR => Err(HvError::Error),
        sys::HV_BUSY => Err(HvError::Busy),
        sys::HV_BAD_ARGUMENT => Err(HvError::BadArgument),
        sys::HV_NO_RESOURCES => Err(HvError::NoResources),
        sys::HV_NO_DEVICE => Err(HvError::NoDevice),
        sys::HV_DENIED => Err(HvError::Denied),
        sys::HV_FAULT => Err(HvError::Fault),
        sys::HV_UNSUPPORTED => Err(HvError::Unsupported),
        other => Err(HvError::Unknown(other)),
    }
}

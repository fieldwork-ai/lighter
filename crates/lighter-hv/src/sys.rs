//! Raw FFI declarations for `Hypervisor.framework` on Apple Silicon.
//!
//! Transcribed by hand from the macOS 26.5 SDK headers rather than generated,
//! so the ABI contract is reviewable in one place and the build needs no clang.
//! Every declaration here is mirrored by an assertion in `tests/abi.rs` where a
//! layout can be checked at runtime.
//!
//! Nothing in this module is safe to call directly; `crate::vm`, `crate::vcpu`
//! and `crate::gic` own the invariants (one VM per process, vCPU pinned to its
//! creating thread, GIC created between the two).

#![allow(non_camel_case_types)]

use std::ffi::c_void;

pub type hv_return_t = u32;
pub type hv_ipa_t = u64;
pub type hv_memory_flags_t = u64;
pub type hv_vcpu_t = u64;
pub type hv_vm_config_t = *mut c_void;
pub type hv_vcpu_config_t = *mut c_void;
pub type hv_gic_config_t = *mut c_void;

pub const HV_SUCCESS: hv_return_t = 0;
pub const HV_ERROR: hv_return_t = 0xfae9_4001;
pub const HV_BUSY: hv_return_t = 0xfae9_4002;
pub const HV_BAD_ARGUMENT: hv_return_t = 0xfae9_4003;
pub const HV_NO_RESOURCES: hv_return_t = 0xfae9_4005;
pub const HV_NO_DEVICE: hv_return_t = 0xfae9_4006;
pub const HV_DENIED: hv_return_t = 0xfae9_4007;
pub const HV_FAULT: hv_return_t = 0xfae9_4008;
pub const HV_UNSUPPORTED: hv_return_t = 0xfae9_400f;

pub const HV_MEMORY_READ: hv_memory_flags_t = 1 << 0;
pub const HV_MEMORY_WRITE: hv_memory_flags_t = 1 << 1;
pub const HV_MEMORY_EXEC: hv_memory_flags_t = 1 << 2;

/// `hv_exit_reason_t`.
pub const HV_EXIT_REASON_CANCELED: u32 = 0;
pub const HV_EXIT_REASON_EXCEPTION: u32 = 1;
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;
pub const HV_EXIT_REASON_UNKNOWN: u32 = 3;

/// `hv_interrupt_type_t`.
pub const HV_INTERRUPT_TYPE_IRQ: u32 = 0;
pub const HV_INTERRUPT_TYPE_FIQ: u32 = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct hv_vcpu_exit_exception_t {
    /// ESR_ELx at the point of the exception.
    pub syndrome: u64,
    /// FAR_ELx — the faulting *virtual* address.
    pub virtual_address: u64,
    /// The faulting intermediate physical address (guest physical).
    pub physical_address: hv_ipa_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct hv_vcpu_exit_t {
    pub reason: u32,
    pub exception: hv_vcpu_exit_exception_t,
}

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    // --- VM lifecycle -----------------------------------------------------
    pub fn hv_vm_create(config: hv_vm_config_t) -> hv_return_t;
    pub fn hv_vm_destroy() -> hv_return_t;
    pub fn hv_vm_get_max_vcpu_count(max_vcpu_count: *mut u32) -> hv_return_t;

    // --- Guest physical memory -------------------------------------------
    pub fn hv_vm_map(
        addr: *mut c_void,
        ipa: hv_ipa_t,
        size: usize,
        flags: hv_memory_flags_t,
    ) -> hv_return_t;
    pub fn hv_vm_unmap(ipa: hv_ipa_t, size: usize) -> hv_return_t;
    pub fn hv_vm_protect(ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t;

    // --- VM configuration (macOS 13+) ------------------------------------
    pub fn hv_vm_config_create() -> hv_vm_config_t;
    pub fn hv_vm_config_set_ipa_size(config: hv_vm_config_t, ipa_size: u32) -> hv_return_t;
    pub fn hv_vm_config_get_max_ipa_size(ipa_size: *mut u32) -> hv_return_t;
    pub fn hv_vm_config_get_default_ipa_size(ipa_size: *mut u32) -> hv_return_t;

    // --- vCPU -------------------------------------------------------------
    // Creates a vCPU *for the calling thread*; the handle is only valid there.
    pub fn hv_vcpu_create(
        vcpu: *mut hv_vcpu_t,
        exit: *mut *mut hv_vcpu_exit_t,
        config: hv_vcpu_config_t,
    ) -> hv_return_t;
    pub fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;
    /// Forces the listed vCPUs out of `hv_vcpu_run`. Callable from any thread —
    /// this is the one vCPU entry point that is not thread-bound.
    pub fn hv_vcpus_exit(vcpus: *const hv_vcpu_t, vcpu_count: u32) -> hv_return_t;

    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: u32, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: u32, value: u64) -> hv_return_t;
    pub fn hv_vcpu_get_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: u64) -> hv_return_t;

    pub fn hv_vcpu_get_pending_interrupt(
        vcpu: hv_vcpu_t,
        typ: u32,
        pending: *mut bool,
    ) -> hv_return_t;
    pub fn hv_vcpu_set_pending_interrupt(vcpu: hv_vcpu_t, typ: u32, pending: bool) -> hv_return_t;

    pub fn hv_vcpu_set_trap_debug_exceptions(vcpu: hv_vcpu_t, value: bool) -> hv_return_t;
    pub fn hv_vcpu_set_trap_debug_reg_accesses(vcpu: hv_vcpu_t, value: bool) -> hv_return_t;

    pub fn hv_vcpu_get_exec_time(vcpu: hv_vcpu_t, time: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_get_vtimer_mask(vcpu: hv_vcpu_t, masked: *mut bool) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: hv_vcpu_t, masked: bool) -> hv_return_t;
    pub fn hv_vcpu_get_vtimer_offset(vcpu: hv_vcpu_t, offset: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, offset: u64) -> hv_return_t;

    // --- vCPU configuration ----------------------------------------------
    pub fn hv_vcpu_config_create() -> hv_vcpu_config_t;
    pub fn hv_vcpu_config_get_feature_reg(
        config: hv_vcpu_config_t,
        reg: u32,
        value: *mut u64,
    ) -> hv_return_t;

    // --- GICv3 (macOS 15+) -------------------------------------------------
    // Ordering is load-bearing: create the VM, then the GIC, then vCPUs.
    pub fn hv_gic_config_create() -> hv_gic_config_t;
    pub fn hv_gic_config_set_distributor_base(
        config: hv_gic_config_t,
        base: hv_ipa_t,
    ) -> hv_return_t;
    pub fn hv_gic_config_set_redistributor_base(
        config: hv_gic_config_t,
        base: hv_ipa_t,
    ) -> hv_return_t;
    pub fn hv_gic_config_set_msi_region_base(
        config: hv_gic_config_t,
        base: hv_ipa_t,
    ) -> hv_return_t;
    pub fn hv_gic_config_set_msi_interrupt_range(
        config: hv_gic_config_t,
        msi_intid_base: u32,
        msi_intid_count: u32,
    ) -> hv_return_t;

    pub fn hv_gic_create(config: hv_gic_config_t) -> hv_return_t;
    pub fn hv_gic_reset() -> hv_return_t;
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t;
    pub fn hv_gic_get_distributor_reg(reg: u16, value: *mut u64) -> hv_return_t;
    pub fn hv_gic_send_msi(address: hv_ipa_t, intid: u32) -> hv_return_t;

    pub fn hv_gic_get_distributor_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_distributor_base_alignment(alignment: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_region_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_base_alignment(alignment: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_base(vcpu: hv_vcpu_t, base: *mut hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_get_msi_region_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_msi_region_base_alignment(alignment: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_spi_interrupt_range(
        spi_intid_base: *mut u32,
        spi_intid_count: *mut u32,
    ) -> hv_return_t;
}

#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    /// Releases an `os_object` — the GIC/VM/vCPU config handles are all of this
    /// family, and leaking them leaks kernel-adjacent resources.
    pub fn os_release(object: *mut c_void);
}

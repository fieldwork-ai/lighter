//! The in-kernel GICv3 interrupt controller (macOS 15+).
//!
//! Apple emulates GICv3 in the kernel, which spares us writing a distributor
//! and redistributor model and — more to the point — spares the guest an exit
//! per interrupt acknowledge. The cost is a strict construction order that the
//! framework enforces with a bare `HV_BAD_ARGUMENT`, so [`Gic::create`] takes
//! `&Vm` to make "after the VM" a type error and documents "before any vCPU"
//! where it cannot.

use crate::error::{Result, check};
use crate::sys;
use crate::vm::Vm;

/// Where the GIC's MMIO windows sit in guest-physical space.
///
/// Defaults match the layout every aarch64 Linux kernel is already happy with
/// (the `virt` machine convention), which keeps the device tree unsurprising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GicLayout {
    /// Distributor (GICD) base.
    pub distributor_base: u64,
    /// Redistributor (GICR) region base — one redistributor per vCPU, packed.
    pub redistributor_base: u64,
    /// Optional MSI window base. `None` disables MSI support entirely.
    pub msi_region_base: Option<u64>,
    /// INTID range reserved for MSIs, when `msi_region_base` is set.
    pub msi_intid_range: (u32, u32),
}

impl Default for GicLayout {
    fn default() -> Self {
        GicLayout {
            distributor_base: 0x0800_0000,
            redistributor_base: 0x080a_0000,
            msi_region_base: None,
            msi_intid_range: (0, 0),
        }
    }
}

/// Sizes and alignments the host reports for its GIC implementation.
///
/// These are queried rather than assumed: they feed both the placement checks
/// and the `reg` properties in the device tree, and a hardcoded size that
/// disagrees with the host produces a guest that boots and then takes no
/// interrupts at all.
#[derive(Debug, Clone, Copy)]
pub struct GicParameters {
    pub distributor_size: usize,
    pub distributor_alignment: usize,
    /// Size of a single vCPU's redistributor.
    pub redistributor_size: usize,
    /// Size of the whole redistributor region for this VM's vCPU count.
    pub redistributor_region_size: usize,
    pub redistributor_alignment: usize,
    pub msi_region_size: usize,
    pub msi_region_alignment: usize,
    /// First shared-peripheral interrupt ID, and how many there are.
    pub spi_base: u32,
    pub spi_count: u32,
}

impl GicParameters {
    /// Queries the host's GIC geometry. Valid once the VM exists.
    pub fn query() -> Result<GicParameters> {
        let mut p = GicParameters {
            distributor_size: 0,
            distributor_alignment: 0,
            redistributor_size: 0,
            redistributor_region_size: 0,
            redistributor_alignment: 0,
            msi_region_size: 0,
            msi_region_alignment: 0,
            spi_base: 0,
            spi_count: 0,
        };
        unsafe {
            check(sys::hv_gic_get_distributor_size(&mut p.distributor_size))?;
            check(sys::hv_gic_get_distributor_base_alignment(
                &mut p.distributor_alignment,
            ))?;
            check(sys::hv_gic_get_redistributor_size(&mut p.redistributor_size))?;
            check(sys::hv_gic_get_redistributor_region_size(
                &mut p.redistributor_region_size,
            ))?;
            check(sys::hv_gic_get_redistributor_base_alignment(
                &mut p.redistributor_alignment,
            ))?;
            check(sys::hv_gic_get_msi_region_size(&mut p.msi_region_size))?;
            check(sys::hv_gic_get_msi_region_base_alignment(
                &mut p.msi_region_alignment,
            ))?;
            check(sys::hv_gic_get_spi_interrupt_range(
                &mut p.spi_base,
                &mut p.spi_count,
            ))?;
        }
        Ok(p)
    }
}

/// The VM's interrupt controller.
///
/// One per VM; dropping it is a no-op because the GIC's lifetime is the VM's.
#[derive(Debug)]
pub struct Gic {
    layout: GicLayout,
    params: GicParameters,
}

impl Gic {
    /// Creates the GICv3.
    ///
    /// Must be called after [`Vm::create`] and **before any vCPU exists** — the
    /// framework allocates per-vCPU interrupt state here and returns
    /// `HV_BAD_ARGUMENT` if a vCPU already claimed it.
    ///
    /// Since the Xcode 26 SDK the base addresses are mandatory: the older
    /// `hv_gic_create(NULL)` form, where the framework picked them, is gone.
    pub fn create(_vm: &Vm, layout: GicLayout) -> Result<Gic> {
        let params = GicParameters::query()?;

        if layout.distributor_base % params.distributor_alignment as u64 != 0 {
            return Err(crate::HvError::BadArgument);
        }
        if layout.redistributor_base % params.redistributor_alignment as u64 != 0 {
            return Err(crate::HvError::BadArgument);
        }
        // Overlapping windows are accepted by the config calls and only surface
        // as a guest that mysteriously takes no interrupts, so reject here.
        let gicd_end = layout.distributor_base + params.distributor_size as u64;
        if gicd_end > layout.redistributor_base {
            return Err(crate::HvError::BadArgument);
        }

        unsafe {
            let config = sys::hv_gic_config_create();
            if config.is_null() {
                return Err(crate::HvError::NoResources);
            }
            let result = (|| {
                check(sys::hv_gic_config_set_distributor_base(
                    config,
                    layout.distributor_base,
                ))?;
                check(sys::hv_gic_config_set_redistributor_base(
                    config,
                    layout.redistributor_base,
                ))?;
                if let Some(msi_base) = layout.msi_region_base {
                    check(sys::hv_gic_config_set_msi_region_base(config, msi_base))?;
                    let (base, count) = layout.msi_intid_range;
                    check(sys::hv_gic_config_set_msi_interrupt_range(
                        config, base, count,
                    ))?;
                }
                check(sys::hv_gic_create(config))
            })();
            sys::os_release(config);
            result?;
        }

        Ok(Gic { layout, params })
    }

    pub const fn layout(&self) -> GicLayout {
        self.layout
    }

    pub const fn params(&self) -> GicParameters {
        self.params
    }

    /// Asserts or deasserts a shared peripheral interrupt.
    ///
    /// `intid` is an absolute INTID, so a device wired to "SPI 4" raises
    /// `spi_base + 4` — off-by-32 here is the classic silent-device bug, which
    /// is why [`Gic::spi_intid`] exists.
    pub fn set_spi(&self, intid: u32, level: bool) -> Result<()> {
        unsafe { check(sys::hv_gic_set_spi(intid, level)) }
    }

    /// Translates a device's SPI index into an absolute INTID.
    pub fn spi_intid(&self, spi: u32) -> Result<u32> {
        if spi >= self.params.spi_count {
            return Err(crate::HvError::BadArgument);
        }
        Ok(self.params.spi_base + spi)
    }

    /// Sends a message-signalled interrupt. Only valid when the layout
    /// configured an MSI region.
    pub fn send_msi(&self, address: u64, intid: u32) -> Result<()> {
        unsafe { check(sys::hv_gic_send_msi(address, intid)) }
    }

    /// The guest-physical base of a given vCPU's redistributor.
    ///
    /// Only meaningful once that vCPU has been created.
    pub fn redistributor_base(&self, vcpu_id: u64) -> Result<u64> {
        let mut base = 0u64;
        unsafe { check(sys::hv_gic_get_redistributor_base(vcpu_id, &mut base))? };
        Ok(base)
    }

    /// Returns the GIC to its power-on state.
    pub fn reset(&self) -> Result<()> {
        unsafe { check(sys::hv_gic_reset()) }
    }
}

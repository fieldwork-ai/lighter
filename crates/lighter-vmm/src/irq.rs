//! Interrupt lines, as a device sees them.
//!
//! A device model knows it has an interrupt; it does not know that the
//! interrupt controller is Apple's in-kernel GICv3, or that raising one costs a
//! framework call. That indirection is what lets the GIC be replaced — and it
//! costs nothing, because raising an interrupt is already a syscall-class
//! operation.

use std::sync::Arc;

use lighter_hv::Gic;

/// A single interrupt line owned by one device.
pub trait IrqLine: Send + Sync {
    /// Drives a level-triggered line high or low.
    fn set_level(&self, level: bool);

    /// Raises an edge-triggered interrupt.
    fn pulse(&self);
}

/// An interrupt line backed by a GIC shared peripheral interrupt.
pub struct GicSpi {
    gic: Arc<Gic>,
    intid: u32,
}

impl GicSpi {
    /// Binds a device's SPI index (not INTID) to a line.
    ///
    /// Taking the SPI *index* and translating here is deliberate: the SPI base
    /// is 32 on this host but is queried rather than assumed, and an off-by-32
    /// is a device whose interrupts vanish with no other symptom.
    pub fn new(gic: Arc<Gic>, spi: u32) -> Result<GicSpi, lighter_hv::HvError> {
        let intid = gic.spi_intid(spi)?;
        Ok(GicSpi { gic, intid })
    }

    pub fn intid(&self) -> u32 {
        self.intid
    }
}

impl IrqLine for GicSpi {
    fn set_level(&self, level: bool) {
        // A failure here means the GIC rejected the INTID, which is a
        // programming error rather than a runtime condition; log rather than
        // unwind, because this runs on a device thread where a panic would
        // take the guest down with no diagnosis.
        if let Err(e) = self.gic.set_spi(self.intid, level) {
            tracing::error!(intid = self.intid, %e, "failed to drive interrupt line");
        }
    }

    fn pulse(&self) {
        // The GIC latches an edge from a low-to-high transition, so a pulse is
        // deassert-then-assert. Doing it in that order matters: a line left
        // high by a previous pulse would otherwise never produce a new edge.
        self.set_level(false);
        self.set_level(true);
    }
}

/// An interrupt line that goes nowhere, for tests and for devices instantiated
/// before their controller exists.
pub struct NullIrq;

impl IrqLine for NullIrq {
    fn set_level(&self, _level: bool) {}
    fn pulse(&self) {}
}

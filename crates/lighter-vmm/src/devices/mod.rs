//! Device models.
//!
//! Each answers MMIO through [`crate::bus::MmioDevice`] and raises interrupts
//! through [`crate::irq::IrqLine`], so none of them knows where it lives in the
//! address map or what kind of interrupt controller the machine has.

pub mod pl011;

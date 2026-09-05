//! The guest's physical address map.
//!
//! Nothing here is a free choice: aarch64 Linux expects RAM around the 1 GiB
//! mark, Apple's GIC dictates where its own windows can sit, and every address
//! has to appear identically in three places — the `hv_vm_map` calls, the
//! device tree, and the MMIO dispatch table. Deriving all three from one value
//! is the entire point of this module; a layout that disagrees with the device
//! tree produces a guest that boots and then quietly cannot reach a device.

use lighter_hv::GicParameters;

/// A device's MMIO window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub base: u64,
    pub size: u64,
}

impl Window {
    pub const fn end(&self) -> u64 {
        self.base + self.size
    }

    pub const fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.base + self.size
    }
}

/// How many virtio-mmio slots the machine advertises.
///
/// Each is a fixed 512-byte window with its own SPI. Slots are cheap — an
/// unused one costs the guest one failed magic-number probe at boot — and
/// running out means changing the memory map, so the number is generous.
pub const VIRTIO_MMIO_SLOTS: usize = 16;

/// Size of one virtio-mmio register window.
pub const VIRTIO_MMIO_SIZE: u64 = 0x200;

/// SPI index (not INTID) assigned to the PL011 UART.
pub const UART_SPI: u32 = 0;

/// First SPI index handed to virtio-mmio slots.
pub const VIRTIO_SPI_BASE: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error(
        "the host's GIC redistributor region ({size:#x} bytes at {base:#x}) runs past \
         the start of the device window at {device_base:#x}"
    )]
    GicOverlapsDevices {
        base: u64,
        size: u64,
        device_base: u64,
    },
    #[error("guest RAM of {0:#x} bytes is too small; the kernel needs at least 64 MiB")]
    RamTooSmall(u64),
}

/// The complete guest-physical map for one machine.
#[derive(Debug, Clone)]
pub struct GuestLayout {
    /// GIC distributor.
    pub gicd: Window,
    /// GIC redistributor region, sized for this machine's vCPU count.
    pub gicr: Window,
    /// PL011 UART.
    pub uart: Window,
    /// The virtio-mmio slot array.
    pub virtio_mmio: Window,
    /// Guest RAM.
    pub ram: Window,
    /// The range virtio-mem offers above RAM, when the machine has one:
    /// memory the guest plugs in and out in 128 MiB blocks (`virtio::mem`).
    pub hotplug: Option<Window>,
}

impl GuestLayout {
    /// The conventional aarch64 RAM base. Kernels are built expecting to find
    /// memory here, and moving it saves nothing.
    pub const RAM_BASE: u64 = 0x4000_0000;

    /// GIC distributor base, matching the layout aarch64 Linux already knows.
    pub const GICD_BASE: u64 = 0x0800_0000;

    /// GIC redistributor base.
    pub const GICR_BASE: u64 = 0x080a_0000;

    /// Device window base.
    ///
    /// Sits above the *maximum* redistributor region rather than the one this
    /// machine needs, so the map does not shift with vCPU count — a shifting
    /// map would mean a device tree that depends on core count for no reason.
    pub const DEVICE_BASE: u64 = 0x0c00_0000;

    /// Builds and validates the map for a machine of `vcpus` cores and
    /// `ram_bytes` of memory.
    pub fn new(
        gic: &GicParameters,
        vcpus: u32,
        ram_bytes: u64,
        hotplug_bytes: u64,
    ) -> Result<GuestLayout, LayoutError> {
        if ram_bytes < 64 * 1024 * 1024 {
            return Err(LayoutError::RamTooSmall(ram_bytes));
        }

        // The host reports the region size for the maximum vCPU count; this
        // machine only needs one redistributor per core it actually has.
        let gicr_size = (gic.redistributor_size as u64) * u64::from(vcpus);

        let gicd = Window {
            base: Self::GICD_BASE,
            size: gic.distributor_size as u64,
        };
        let gicr = Window {
            base: Self::GICR_BASE,
            size: gicr_size,
        };

        // The check that matters: Apple could widen the redistributor in a
        // future release, and the symptom would be a device that answers at an
        // address the GIC also claims.
        let max_gicr_end = Self::GICR_BASE + gic.redistributor_region_size as u64;
        if max_gicr_end > Self::DEVICE_BASE {
            return Err(LayoutError::GicOverlapsDevices {
                base: Self::GICR_BASE,
                size: gic.redistributor_region_size as u64,
                device_base: Self::DEVICE_BASE,
            });
        }

        let uart = Window {
            base: Self::DEVICE_BASE,
            size: 0x1000,
        };
        let virtio_mmio = Window {
            base: Self::DEVICE_BASE + 0x1_0000,
            size: VIRTIO_MMIO_SIZE * VIRTIO_MMIO_SLOTS as u64,
        };
        let ram = Window {
            base: Self::RAM_BASE,
            size: ram_bytes,
        };
        // The hot-plug range starts at the first block boundary past RAM:
        // Linux wants both ends of it aligned to its memory block, and RAM
        // need not be a whole number of blocks.
        let hotplug = (hotplug_bytes > 0).then(|| {
            let block = crate::virtio::mem::BLOCK_SIZE;
            Window {
                base: ram.end().div_ceil(block) * block,
                size: hotplug_bytes.div_ceil(block) * block,
            }
        });

        Ok(GuestLayout {
            gicd,
            gicr,
            uart,
            virtio_mmio,
            ram,
            hotplug,
        })
    }

    /// The MMIO window for one virtio-mmio slot.
    pub fn virtio_slot(&self, index: usize) -> Option<Window> {
        if index >= VIRTIO_MMIO_SLOTS {
            return None;
        }
        Some(Window {
            base: self.virtio_mmio.base + VIRTIO_MMIO_SIZE * index as u64,
            size: VIRTIO_MMIO_SIZE,
        })
    }

    /// SPI index for a virtio-mmio slot.
    pub fn virtio_spi(&self, index: usize) -> u32 {
        VIRTIO_SPI_BASE + index as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> GicParameters {
        // The values this M5 Pro reports; the point of the test is that the
        // derived map is sane for them, and that a hypothetical much larger
        // redistributor is rejected rather than silently overlapping.
        GicParameters {
            distributor_size: 0x1_0000,
            distributor_alignment: 0x1_0000,
            redistributor_size: 0x8_0000,
            redistributor_region_size: 0x200_0000,
            redistributor_alignment: 0x1_0000,
            msi_region_size: 0x1_0000,
            msi_region_alignment: 0x1_0000,
            spi_base: 32,
            spi_count: 988,
        }
    }

    #[test]
    fn windows_do_not_overlap() {
        let l = GuestLayout::new(&params(), 4, 2 << 30, 0).unwrap();
        assert!(l.gicd.end() <= l.gicr.base);
        assert!(l.gicr.end() <= l.uart.base);
        assert!(l.uart.end() <= l.virtio_mmio.base);
        assert!(l.virtio_mmio.end() <= l.ram.base);
    }

    #[test]
    fn device_window_clears_the_maximum_redistributor_region() {
        // Not just this machine's vCPU count: the map must not move when the
        // core count changes, so it clears the largest region the host allows.
        let l = GuestLayout::new(&params(), 1, 2 << 30, 0).unwrap();
        let max_end = GuestLayout::GICR_BASE + params().redistributor_region_size as u64;
        assert!(l.uart.base >= max_end);
    }

    #[test]
    fn rejects_a_redistributor_that_would_swallow_the_devices() {
        let mut p = params();
        p.redistributor_region_size = 0x1000_0000; // 256 MiB
        assert!(matches!(
            GuestLayout::new(&p, 4, 2 << 30, 0),
            Err(LayoutError::GicOverlapsDevices { .. })
        ));
    }

    #[test]
    fn virtio_slots_tile_their_window_without_gaps() {
        let l = GuestLayout::new(&params(), 4, 2 << 30, 0).unwrap();
        for i in 0..VIRTIO_MMIO_SLOTS {
            let w = l.virtio_slot(i).unwrap();
            assert!(w.base >= l.virtio_mmio.base && w.end() <= l.virtio_mmio.end());
            if i > 0 {
                assert_eq!(w.base, l.virtio_slot(i - 1).unwrap().end());
            }
        }
        assert!(l.virtio_slot(VIRTIO_MMIO_SLOTS).is_none());
    }
}

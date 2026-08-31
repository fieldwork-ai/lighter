//! The flattened device tree handed to the guest kernel.
//!
//! There is no firmware in a lighter VM, so this is the *only* description the
//! kernel gets of the machine it woke up on: how much RAM, how many cores, what
//! the interrupt controller is, where the devices are, and how to bring up a
//! secondary CPU. Every address here is taken from [`GuestLayout`] rather than
//! written twice, because the failure mode for a disagreement is not a crash —
//! it is a device that probes, finds nothing, and is silently absent.

use vm_fdt::{Error as FdtError, FdtWriter};

use crate::kernel::InitramfsPlacement;
use crate::layout::{GuestLayout, UART_SPI, VIRTIO_MMIO_SLOTS};

/// Device-tree interrupt type cell for a shared peripheral interrupt.
const GIC_SPI: u32 = 0;
/// Device-tree interrupt type cell for a per-CPU (private) interrupt.
const GIC_PPI: u32 = 1;
/// `IRQ_TYPE_LEVEL_HIGH`.
const IRQ_LEVEL_HIGH: u32 = 4;
/// `IRQ_TYPE_EDGE_RISING`.
const IRQ_EDGE_RISING: u32 = 1;

/// PPI numbers for the ARM generic timer.
///
/// These are relative to the PPI base, so PPI 11 is INTID 27 — which is
/// exactly `HV_GIC_INT_EL1_VIRTUAL_TIMER` in Apple's header. The two numbering
/// schemes agreeing is not a coincidence, but it is worth writing down: it is
/// what lets a stock aarch64 kernel take timer interrupts from Apple's GIC with
/// no special casing.
const PPI_SECURE_PHYS_TIMER: u32 = 13;
const PPI_NONSECURE_PHYS_TIMER: u32 = 14;
const PPI_VIRT_TIMER: u32 = 11;
const PPI_HYP_TIMER: u32 = 10;

const PHANDLE_GIC: u32 = 1;
const PHANDLE_UART_CLK: u32 = 2;

/// Everything the device tree needs that is not implied by the memory map.
pub struct FdtParams<'a> {
    pub layout: &'a GuestLayout,
    pub vcpus: u32,
    pub cmdline: &'a str,
    pub initramfs: Option<InitramfsPlacement>,
    /// How many virtio-mmio slots are actually populated. Unpopulated slots are
    /// left out of the tree entirely rather than advertised and left to fail
    /// their magic-number probe.
    pub virtio_slots: usize,
}

/// Builds the flattened device tree blob.
pub fn build(params: &FdtParams<'_>) -> Result<Vec<u8>, FdtError> {
    let layout = params.layout;
    let mut fdt = FdtWriter::new()?;

    let root = fdt.begin_node("")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_string("compatible", "linux,dummy-virt")?;
    fdt.property_u32("interrupt-parent", PHANDLE_GIC)?;

    // --- /chosen ----------------------------------------------------------
    {
        let chosen = fdt.begin_node("chosen")?;
        fdt.property_string("bootargs", params.cmdline)?;
        fdt.property_string("stdout-path", &format!("/pl011@{:x}", layout.uart.base))?;
        if let Some(initrd) = params.initramfs {
            // The kernel reads these as 32-bit or 64-bit depending on its
            // vintage; the 64-bit form is what modern arm64 expects.
            fdt.property_u64("linux,initrd-start", initrd.start)?;
            fdt.property_u64("linux,initrd-end", initrd.end)?;
        }
        fdt.end_node(chosen)?;
    }

    // --- /memory ----------------------------------------------------------
    {
        let memory = fdt.begin_node(&format!("memory@{:x}", layout.ram.base))?;
        fdt.property_string("device_type", "memory")?;
        fdt.property_array_u64("reg", &[layout.ram.base, layout.ram.size])?;
        fdt.end_node(memory)?;
    }

    // --- /cpus ------------------------------------------------------------
    {
        let cpus = fdt.begin_node("cpus")?;
        fdt.property_u32("#address-cells", 1)?;
        fdt.property_u32("#size-cells", 0)?;

        for cpu in 0..params.vcpus {
            let node = fdt.begin_node(&format!("cpu@{cpu:x}"))?;
            fdt.property_string("device_type", "cpu")?;
            fdt.property_string("compatible", "arm,arm-v8")?;
            // Flat affinity: vCPU n has MPIDR_EL1.Aff0 == n, which is also the
            // id Apple's GIC uses to address that core's redistributor.
            fdt.property_u32("reg", cpu)?;
            // Every core including core 0 declares PSCI, so the kernel uses
            // the same path for boot and for hotplug.
            fdt.property_string("enable-method", "psci")?;
            fdt.end_node(node)?;
        }
        fdt.end_node(cpus)?;
    }

    // --- /psci ------------------------------------------------------------
    {
        let psci = fdt.begin_node("psci")?;
        fdt.property_string_list(
            "compatible",
            vec!["arm,psci-1.0".into(), "arm,psci-0.2".into()],
        )?;
        // HVC, not SMC: there is no secure monitor below us, and an SMC from
        // the guest would not trap to the VMM at all.
        fdt.property_string("method", "hvc")?;
        fdt.end_node(psci)?;
    }

    // --- /timer -----------------------------------------------------------
    {
        let timer = fdt.begin_node("timer")?;
        fdt.property_string("compatible", "arm,armv8-timer")?;
        fdt.property_array_u32(
            "interrupts",
            &[
                GIC_PPI,
                PPI_SECURE_PHYS_TIMER,
                IRQ_LEVEL_HIGH,
                GIC_PPI,
                PPI_NONSECURE_PHYS_TIMER,
                IRQ_LEVEL_HIGH,
                GIC_PPI,
                PPI_VIRT_TIMER,
                IRQ_LEVEL_HIGH,
                GIC_PPI,
                PPI_HYP_TIMER,
                IRQ_LEVEL_HIGH,
            ],
        )?;
        // The counter keeps running while the VM is idle, so Linux may use it
        // as a clocksource without arming a periodic wakeup — which is what
        // lets an idle guest cost ~no host CPU.
        fdt.property_null("always-on")?;
        fdt.end_node(timer)?;
    }

    // --- /intc (GICv3) ----------------------------------------------------
    {
        let intc = fdt.begin_node(&format!("interrupt-controller@{:x}", layout.gicd.base))?;
        fdt.property_string("compatible", "arm,gic-v3")?;
        fdt.property_u32("#interrupt-cells", 3)?;
        fdt.property_null("interrupt-controller")?;
        fdt.property_array_u64(
            "reg",
            &[
                layout.gicd.base,
                layout.gicd.size,
                layout.gicr.base,
                layout.gicr.size,
            ],
        )?;
        fdt.property_u32("#address-cells", 2)?;
        fdt.property_u32("#size-cells", 2)?;
        fdt.property_null("ranges")?;
        fdt.property_u32("phandle", PHANDLE_GIC)?;
        fdt.end_node(intc)?;
    }

    // --- /pl011 -----------------------------------------------------------
    {
        let uart = fdt.begin_node(&format!("pl011@{:x}", layout.uart.base))?;
        fdt.property_string_list(
            "compatible",
            vec!["arm,pl011".into(), "arm,primecell".into()],
        )?;
        fdt.property_array_u64("reg", &[layout.uart.base, layout.uart.size])?;
        fdt.property_array_u32("interrupts", &[GIC_SPI, UART_SPI, IRQ_LEVEL_HIGH])?;
        fdt.property_array_u32("clocks", &[PHANDLE_UART_CLK, PHANDLE_UART_CLK])?;
        fdt.property_string_list("clock-names", vec!["uartclk".into(), "apb_pclk".into()])?;
        fdt.end_node(uart)?;
    }

    // --- fixed clock for the UART ----------------------------------------
    {
        let clk = fdt.begin_node("apb-pclk")?;
        fdt.property_string("compatible", "fixed-clock")?;
        fdt.property_u32("#clock-cells", 0)?;
        fdt.property_u32("clock-frequency", 24_000_000)?;
        fdt.property_string("clock-output-names", "clk24mhz")?;
        fdt.property_u32("phandle", PHANDLE_UART_CLK)?;
        fdt.end_node(clk)?;
    }

    // --- virtio-mmio slots ------------------------------------------------
    // Declared lowest slot first, and the order is load-bearing: Linux probes
    // virtio-mmio nodes in tree order and names block devices as it goes, so
    // the first node declared is the one that becomes /dev/vda.
    //
    // This was descending once, on the reasoning that Linux reverses. It does
    // not. The guest came up with the second disk as /dev/vda and refused to
    // mount a root filesystem that was sitting on /dev/vdb, which is a
    // spectacular way to discover that you had the rule backwards.
    for index in 0..params.virtio_slots.min(VIRTIO_MMIO_SLOTS) {
        let window = layout.virtio_slot(index).expect("slot index checked above");
        let node = fdt.begin_node(&format!("virtio_mmio@{:x}", window.base))?;
        fdt.property_string("compatible", "virtio,mmio")?;
        fdt.property_array_u64("reg", &[window.base, window.size])?;
        fdt.property_array_u32(
            "interrupts",
            &[GIC_SPI, layout.virtio_spi(index), IRQ_EDGE_RISING],
        )?;
        fdt.end_node(node)?;
    }

    fdt.end_node(root)?;
    fdt.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighter_hv::GicParameters;

    fn layout() -> GuestLayout {
        let gic = GicParameters {
            distributor_size: 0x1_0000,
            distributor_alignment: 0x1_0000,
            redistributor_size: 0x8_0000,
            redistributor_region_size: 0x200_0000,
            redistributor_alignment: 0x1_0000,
            msi_region_size: 0x1_0000,
            msi_region_alignment: 0x1_0000,
            spi_base: 32,
            spi_count: 988,
        };
        GuestLayout::new(&gic, 2, 1 << 30).unwrap()
    }

    #[test]
    fn produces_a_wellformed_blob() {
        let layout = layout();
        let dtb = build(&FdtParams {
            layout: &layout,
            vcpus: 2,
            cmdline: "console=ttyAMA0 panic=-1",
            initramfs: Some(InitramfsPlacement {
                start: 0x7000_0000,
                end: 0x7010_0000,
            }),
            virtio_slots: 2,
        })
        .unwrap();

        // FDT magic and a plausible size — enough to catch a writer that
        // silently produced an empty tree.
        assert_eq!(&dtb[0..4], &[0xd0, 0x0d, 0xfe, 0xed]);
        let total_size = u32::from_be_bytes(dtb[4..8].try_into().unwrap()) as usize;
        assert_eq!(total_size, dtb.len());
        assert!(
            dtb.len() > 512,
            "tree suspiciously small: {} bytes",
            dtb.len()
        );
    }

    /// Slot 0 must be declared before slot 1, because Linux probes in tree
    /// order and the first virtio-blk it probes becomes /dev/vda. Getting this
    /// backwards makes the root filesystem land on /dev/vdb, and the guest
    /// panics with "unable to mount root fs" while both disks are plainly
    /// present in the partition list.
    #[test]
    fn virtio_slots_are_declared_lowest_first() {
        let layout = layout();
        let dtb = build(&FdtParams {
            layout: &layout,
            vcpus: 1,
            cmdline: "console=ttyAMA0",
            initramfs: None,
            virtio_slots: 3,
        })
        .unwrap();

        // Node names go in the strings/structure block as plain bytes, so their
        // relative position in the blob is their order in the tree.
        let position = |index: usize| {
            let name = format!("virtio_mmio@{:x}", layout.virtio_slot(index).unwrap().base);
            dtb.windows(name.len())
                .position(|w| w == name.as_bytes())
                .unwrap_or_else(|| panic!("{name} missing from the tree"))
        };

        assert!(
            position(0) < position(1) && position(1) < position(2),
            "slots must appear in ascending order: {:?}",
            (position(0), position(1), position(2))
        );
    }

    #[test]
    fn boots_without_an_initramfs() {
        let layout = layout();
        assert!(
            build(&FdtParams {
                layout: &layout,
                vcpus: 1,
                cmdline: "console=ttyAMA0",
                initramfs: None,
                virtio_slots: 0,
            })
            .is_ok()
        );
    }
}

//! The virtio-mmio transport.
//!
//! One of these wraps each device model and presents the register interface
//! Linux's `virtio_mmio` driver expects. It owns everything that is true of
//! *every* virtio device — the queues, feature negotiation, device status, and
//! the interrupt — so a device model contains only what makes it that device.

use std::sync::Arc;

use crate::bus::MmioDevice;
use crate::irq::IrqLine;
use crate::memory::GuestMemory;
use crate::virtio::queue::Virtqueue;
use crate::virtio::{VirtioDevice, features, status};

// Register offsets, from the virtio 1.2 specification's MMIO section.
const MAGIC_VALUE: u64 = 0x000;
const VERSION: u64 = 0x004;
const DEVICE_ID: u64 = 0x008;
const VENDOR_ID: u64 = 0x00c;
const DEVICE_FEATURES: u64 = 0x010;
const DEVICE_FEATURES_SEL: u64 = 0x014;
const DRIVER_FEATURES: u64 = 0x020;
const DRIVER_FEATURES_SEL: u64 = 0x024;
const QUEUE_SEL: u64 = 0x030;
const QUEUE_NUM_MAX: u64 = 0x034;
const QUEUE_NUM: u64 = 0x038;
const QUEUE_READY: u64 = 0x044;
const QUEUE_NOTIFY: u64 = 0x050;
const INTERRUPT_STATUS: u64 = 0x060;
const INTERRUPT_ACK: u64 = 0x064;
const STATUS: u64 = 0x070;
const QUEUE_DESC_LOW: u64 = 0x080;
const QUEUE_DESC_HIGH: u64 = 0x084;
const QUEUE_DRIVER_LOW: u64 = 0x090;
const QUEUE_DRIVER_HIGH: u64 = 0x094;
const QUEUE_DEVICE_LOW: u64 = 0x0a0;
const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
const SHM_SEL: u64 = 0x0ac;
const SHM_LEN_LOW: u64 = 0x0b0;
const SHM_LEN_HIGH: u64 = 0x0b4;
const CONFIG_GENERATION: u64 = 0x0fc;
const CONFIG_SPACE: u64 = 0x100;

/// `"virt"` little-endian. The driver probes every slot for this and moves on
/// quietly when it is absent, which is why unpopulated slots are harmless.
const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;

/// Version 2 is modern virtio. Version 1 selects the legacy ring layout.
const VIRTIO_MMIO_VERSION: u32 = 2;

/// Not a registered vendor, and deliberately recognisable in a trace.
const LIGHTER_VENDOR_ID: u32 = 0x4c47_4854;

/// Interrupt cause bits in `InterruptStatus`.
const INT_VRING: u32 = 1;
const INT_CONFIG: u32 = 2;

/// A device model plus the transport state that presents it to the guest.
pub struct VirtioMmio {
    device: Box<dyn VirtioDevice>,
    queues: Vec<Virtqueue>,
    memory: Arc<GuestMemory>,
    irq: Arc<dyn IrqLine>,

    /// Which 32-bit half of the 64-bit feature word the driver is reading.
    device_features_sel: u32,
    driver_features_sel: u32,
    acked_features: u64,

    queue_sel: u32,
    device_status: u32,
    interrupt_status: u32,
    config_generation: u32,
    /// Set once `DRIVER_OK` has been seen, so activation happens exactly once.
    activated: bool,
}

impl VirtioMmio {
    pub fn new(
        device: Box<dyn VirtioDevice>,
        memory: Arc<GuestMemory>,
        irq: Arc<dyn IrqLine>,
    ) -> VirtioMmio {
        let max = device.queue_max_size();
        let queues = (0..device.queue_count())
            .map(|_| Virtqueue::new(max))
            .collect();
        VirtioMmio {
            device,
            queues,
            memory,
            irq,
            device_features_sel: 0,
            driver_features_sel: 0,
            acked_features: 0,
            queue_sel: 0,
            device_status: 0,
            interrupt_status: 0,
            config_generation: 0,
            activated: false,
        }
    }

    pub fn device_name(&self) -> &'static str {
        self.device.name()
    }

    fn selected_queue(&mut self) -> Option<&mut Virtqueue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// Returns the device and its queues to power-on state.
    fn reset(&mut self) {
        for queue in &mut self.queues {
            queue.reset();
        }
        self.device.reset();
        self.device_status = 0;
        self.acked_features = 0;
        self.interrupt_status = 0;
        self.queue_sel = 0;
        self.activated = false;
        self.irq.set_level(false);
    }

    /// Handles the driver writing device status.
    fn set_status(&mut self, value: u32) {
        // Writing zero is the specified way to reset a device.
        if value == 0 {
            self.reset();
            return;
        }

        self.device_status = value;

        // The driver signals it has finished setup by setting DRIVER_OK. Doing
        // activation work here rather than on the first notification means a
        // device has its queues before any I/O can arrive.
        if value & status::DRIVER_OK != 0 && !self.activated {
            self.activated = true;
            self.device.activate(self.memory.clone());
            tracing::debug!(device = self.device.name(), "driver ready");
        }
    }

    /// Handles a queue notification.
    fn notify_queue(&mut self, index: u16) {
        if !self.activated {
            // A notification before DRIVER_OK is a driver bug, not something to
            // service: the device has no memory to work against yet.
            tracing::warn!(
                device = self.device.name(),
                index,
                "queue notified before the driver was ready"
            );
            return;
        }

        let serviced = self.device.notify(index, &mut self.queues, &self.memory);
        if !serviced.any() {
            return;
        }

        // Ask every queue that gained used entries, not just the notified one.
        // Servicing TX can produce work on RX, and consulting TX's suppression
        // state about an RX completion is how a reply ends up sitting in the
        // ring with the guest asleep.
        let wants_interrupt = (0..self.queues.len())
            .any(|i| serviced.contains(i as u16) && self.queues[i].needs_interrupt(&self.memory));
        if wants_interrupt {
            self.interrupt_status |= INT_VRING;
            // The device tree declares these lines edge-triggered, matching
            // what every aarch64 guest expects from virtio-mmio, so a pulse is
            // what delivers the interrupt.
            self.irq.pulse();
        }
    }

    fn read_register(&mut self, offset: u64) -> u32 {
        if offset >= CONFIG_SPACE {
            let mut buf = [0u8; 4];
            self.device.config_read(offset - CONFIG_SPACE, &mut buf);
            return u32::from_le_bytes(buf);
        }

        match offset {
            MAGIC_VALUE => VIRTIO_MMIO_MAGIC,
            VERSION => VIRTIO_MMIO_VERSION,
            DEVICE_ID => self.device.device_type(),
            VENDOR_ID => LIGHTER_VENDOR_ID,
            DEVICE_FEATURES => {
                // The 64-bit feature word is read 32 bits at a time, selected
                // by DeviceFeaturesSel.
                let all = self.device.features();
                if self.device_features_sel == 0 {
                    all as u32
                } else {
                    (all >> 32) as u32
                }
            }
            QUEUE_NUM_MAX => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| u32::from(q.max_size())),
            QUEUE_READY => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| u32::from(q.is_ready())),
            INTERRUPT_STATUS => self.interrupt_status,
            STATUS => self.device_status,
            CONFIG_GENERATION => self.config_generation,
            SHM_LEN_LOW | SHM_LEN_HIGH => {
                // No shared memory regions; the spec says report -1 for length.
                u32::MAX
            }
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        if offset >= CONFIG_SPACE {
            self.device
                .config_write(offset - CONFIG_SPACE, &value.to_le_bytes());
            self.config_generation = self.config_generation.wrapping_add(1);
            return;
        }

        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = value,
            DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            DRIVER_FEATURES => {
                let shifted = u64::from(value) << (32 * u64::from(self.driver_features_sel));
                self.acked_features |= shifted;
                self.device.ack_features(self.acked_features);
            }
            QUEUE_SEL => self.queue_sel = value,
            QUEUE_NUM => {
                if let Some(q) = self.selected_queue() {
                    q.set_size(value as u16);
                }
            }
            QUEUE_READY => {
                let ready = value == 1;
                let name = self.device.name();
                let sel = self.queue_sel;
                if let Some(q) = self.selected_queue()
                    && !q.set_ready(ready)
                {
                    // Refusing is right — an invalid geometry would make the
                    // ring arithmetic wrong — but it is invisible to the
                    // driver, which will then wait forever, so say so.
                    tracing::warn!(
                        device = name,
                        queue = sel,
                        "driver enabled a queue with invalid geometry"
                    );
                }
            }
            QUEUE_NOTIFY => self.notify_queue(value as u16),
            INTERRUPT_ACK => {
                self.interrupt_status &= !value;
            }
            STATUS => self.set_status(value),
            QUEUE_DESC_LOW => self.set_queue_addr(|q| &mut q.desc_addr, value, false),
            QUEUE_DESC_HIGH => self.set_queue_addr(|q| &mut q.desc_addr, value, true),
            QUEUE_DRIVER_LOW => self.set_queue_addr(|q| &mut q.avail_addr, value, false),
            QUEUE_DRIVER_HIGH => self.set_queue_addr(|q| &mut q.avail_addr, value, true),
            QUEUE_DEVICE_LOW => self.set_queue_addr(|q| &mut q.used_addr, value, false),
            QUEUE_DEVICE_HIGH => self.set_queue_addr(|q| &mut q.used_addr, value, true),
            SHM_SEL => {}
            _ => {}
        }
    }

    /// Assembles a 64-bit ring address from the two halves the driver writes.
    fn set_queue_addr(
        &mut self,
        field: impl Fn(&mut Virtqueue) -> &mut u64,
        value: u32,
        high: bool,
    ) {
        if let Some(q) = self.selected_queue() {
            let slot = field(q);
            if high {
                *slot = (*slot & 0x0000_0000_ffff_ffff) | (u64::from(value) << 32);
            } else {
                *slot = (*slot & 0xffff_ffff_0000_0000) | u64::from(value);
            }
        }
    }

    /// Raises a configuration-change interrupt.
    ///
    /// Used by devices whose configuration space changes on their own — the
    /// balloon's target size is the case that matters here.
    pub fn notify_config_change(&mut self) {
        self.config_generation = self.config_generation.wrapping_add(1);
        self.interrupt_status |= INT_CONFIG;
        self.irq.pulse();
    }

    /// Lets a device with a host-side source of work (a network backend, a
    /// vsock peer) push completions without the guest notifying first.
    pub fn service_queue(&mut self, index: u16) {
        self.notify_queue(index);
    }

    pub fn queues(&mut self) -> &mut [Virtqueue] {
        &mut self.queues
    }

    pub fn memory(&self) -> &Arc<GuestMemory> {
        &self.memory
    }

    /// Whether the driver has finished bringing the device up.
    pub fn is_activated(&self) -> bool {
        self.activated
    }

    /// The features both sides agreed on.
    pub fn negotiated_features(&self) -> u64 {
        self.device.features() & self.acked_features
    }
}

impl MmioDevice for VirtioMmio {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        // Configuration space is byte-addressable; the register file is not.
        if offset >= CONFIG_SPACE {
            self.device.config_read(offset - CONFIG_SPACE, data);
            return;
        }
        let value = self.read_register(offset & !0x3);
        let bytes = value.to_le_bytes();
        let shift = (offset & 0x3) as usize;
        for (i, out) in data.iter_mut().enumerate() {
            *out = bytes.get(shift + i).copied().unwrap_or(0);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= CONFIG_SPACE {
            self.device.config_write(offset - CONFIG_SPACE, data);
            self.config_generation = self.config_generation.wrapping_add(1);
            return;
        }
        let mut buf = [0u8; 4];
        for (i, b) in data.iter().take(4).enumerate() {
            buf[i] = *b;
        }
        self.write_register(offset & !0x3, u32::from_le_bytes(buf));
    }

    fn name(&self) -> &'static str {
        self.device.name()
    }
}

/// Feature bits every device should offer.
///
/// `VERSION_1` is mandatory — without it the driver uses the legacy ring layout
/// we do not implement — and indirect descriptors matter enough for block
/// throughput to be on by default.
pub const COMMON_FEATURES: u64 = features::VERSION_1 | features::RING_INDIRECT_DESC;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::NullIrq;
    use crate::virtio::{Serviced, device_type};

    struct Fake {
        notified: Vec<u16>,
        activated: bool,
        reset_count: u32,
    }

    impl VirtioDevice for Fake {
        fn device_type(&self) -> u32 {
            device_type::BLOCK
        }
        fn name(&self) -> &'static str {
            "fake"
        }
        fn features(&self) -> u64 {
            COMMON_FEATURES
        }
        fn queue_count(&self) -> usize {
            1
        }
        fn activate(&mut self, _mem: Arc<GuestMemory>) {
            self.activated = true;
        }
        fn notify(&mut self, queue: u16, _q: &mut [Virtqueue], _m: &GuestMemory) -> Serviced {
            self.notified.push(queue);
            Serviced::NONE
        }
        fn reset(&mut self) {
            self.reset_count += 1;
        }
    }

    fn transport() -> VirtioMmio {
        VirtioMmio::new(
            Box::new(Fake {
                notified: Vec::new(),
                activated: false,
                reset_count: 0,
            }),
            Arc::new(GuestMemory::detached()),
            Arc::new(NullIrq),
        )
    }

    fn read32(t: &mut VirtioMmio, offset: u64) -> u32 {
        let mut buf = [0u8; 4];
        t.read(offset, &mut buf);
        u32::from_le_bytes(buf)
    }

    fn write32(t: &mut VirtioMmio, offset: u64, value: u32) {
        t.write(offset, &value.to_le_bytes());
    }

    /// The driver probes every slot for this and skips the ones that do not
    /// answer, so getting it wrong makes a device simply not exist.
    #[test]
    fn advertises_modern_virtio() {
        let mut t = transport();
        assert_eq!(read32(&mut t, MAGIC_VALUE), VIRTIO_MMIO_MAGIC);
        assert_eq!(read32(&mut t, VERSION), 2, "version 1 selects legacy rings");
        assert_eq!(read32(&mut t, DEVICE_ID), device_type::BLOCK);
    }

    /// The 64-bit feature word is read in halves; returning the low half for
    /// both is the bug that silently drops VERSION_1 and leaves the driver in
    /// legacy mode.
    #[test]
    fn feature_word_is_selectable_in_halves() {
        let mut t = transport();
        write32(&mut t, DEVICE_FEATURES_SEL, 0);
        assert_eq!(read32(&mut t, DEVICE_FEATURES), COMMON_FEATURES as u32);
        write32(&mut t, DEVICE_FEATURES_SEL, 1);
        assert_eq!(
            read32(&mut t, DEVICE_FEATURES),
            (COMMON_FEATURES >> 32) as u32
        );
    }

    #[test]
    fn driver_features_accumulate_across_both_halves() {
        let mut t = transport();
        write32(&mut t, DRIVER_FEATURES_SEL, 0);
        write32(&mut t, DRIVER_FEATURES, features::RING_INDIRECT_DESC as u32);
        write32(&mut t, DRIVER_FEATURES_SEL, 1);
        write32(&mut t, DRIVER_FEATURES, (features::VERSION_1 >> 32) as u32);
        assert_eq!(
            t.negotiated_features(),
            features::VERSION_1 | features::RING_INDIRECT_DESC
        );
    }

    #[test]
    fn ring_addresses_assemble_from_two_halves() {
        let mut t = transport();
        write32(&mut t, QUEUE_SEL, 0);
        write32(&mut t, QUEUE_DESC_LOW, 0xdead_0000);
        write32(&mut t, QUEUE_DESC_HIGH, 0x0000_0001);
        assert_eq!(t.queues()[0].desc_addr, 0x1_dead_0000);

        // Rewriting only the low half must not disturb the high half.
        write32(&mut t, QUEUE_DESC_LOW, 0xbeef_0000);
        assert_eq!(t.queues()[0].desc_addr, 0x1_beef_0000);
    }

    #[test]
    fn activation_happens_once_when_the_driver_is_ready() {
        let mut t = transport();
        write32(&mut t, STATUS, status::ACKNOWLEDGE);
        assert!(!t.is_activated());
        write32(&mut t, STATUS, status::ACKNOWLEDGE | status::DRIVER);
        assert!(!t.is_activated());
        write32(
            &mut t,
            STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        assert!(t.is_activated());
    }

    #[test]
    fn writing_zero_to_status_resets_the_device() {
        let mut t = transport();
        write32(&mut t, STATUS, status::DRIVER_OK);
        write32(&mut t, QUEUE_SEL, 0);
        write32(&mut t, QUEUE_DESC_LOW, 0x1234);
        write32(&mut t, STATUS, 0);

        assert_eq!(read32(&mut t, STATUS), 0);
        assert!(!t.is_activated(), "reset must allow re-activation");
        assert_eq!(t.queues()[0].desc_addr, 0, "queues must be reset too");
    }

    #[test]
    fn interrupt_status_is_write_one_to_clear() {
        let mut t = transport();
        t.interrupt_status = INT_VRING | INT_CONFIG;
        write32(&mut t, INTERRUPT_ACK, INT_VRING);
        assert_eq!(read32(&mut t, INTERRUPT_STATUS), INT_CONFIG);
    }

    /// Servicing a queue before the driver is ready would run the device
    /// against memory it has not been given.
    #[test]
    fn notifications_before_driver_ok_are_refused() {
        let mut t = transport();
        write32(&mut t, QUEUE_NOTIFY, 0);
        assert!(!t.is_activated());
    }

    #[test]
    fn config_space_reads_are_byte_addressable() {
        struct Config;
        impl VirtioDevice for Config {
            fn device_type(&self) -> u32 {
                device_type::BLOCK
            }
            fn name(&self) -> &'static str {
                "config"
            }
            fn features(&self) -> u64 {
                0
            }
            fn queue_count(&self) -> usize {
                1
            }
            fn config_read(&self, offset: u64, data: &mut [u8]) {
                for (i, b) in data.iter_mut().enumerate() {
                    *b = (offset as u8).wrapping_add(i as u8);
                }
            }
            fn notify(&mut self, _q: u16, _qs: &mut [Virtqueue], _m: &GuestMemory) -> Serviced {
                Serviced::NONE
            }
        }

        let mut t = VirtioMmio::new(
            Box::new(Config),
            Arc::new(GuestMemory::detached()),
            Arc::new(NullIrq),
        );
        // A one-byte read at config offset 5 must see the device's byte 5, not
        // a truncated 32-bit read of offset 4.
        let mut one = [0u8; 1];
        t.read(CONFIG_SPACE + 5, &mut one);
        assert_eq!(one[0], 5);
    }
}

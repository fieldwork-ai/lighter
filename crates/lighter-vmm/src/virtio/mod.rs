//! virtio devices.
//!
//! Everything the guest does for real work — disk, network, sockets, entropy,
//! memory reclaim — arrives through one of these. The transport is virtio-mmio
//! rather than PCI: there is no PCI host bridge to model, no enumeration, and
//! the device set is fixed at boot by the device tree, which is both simpler and
//! measurably faster to probe.

pub mod balloon;
pub mod block;
pub mod disk;
pub mod mmio;
pub mod queue;
pub mod rng;

use std::sync::Arc;

use crate::memory::GuestMemory;
use queue::Virtqueue;

/// virtio device type IDs, from the specification's device-id table.
pub mod device_type {
    pub const NET: u32 = 1;
    pub const BLOCK: u32 = 2;
    pub const CONSOLE: u32 = 3;
    pub const RNG: u32 = 4;
    pub const BALLOON: u32 = 5;
    pub const VSOCK: u32 = 19;
    pub const FS: u32 = 26;
}

/// Transport feature bits, shared by every device.
pub mod features {
    /// Descriptor chains may point at tables of further descriptors. Linux uses
    /// this heavily for block I/O, where it turns a long chain into one entry.
    pub const RING_INDIRECT_DESC: u64 = 1 << 28;
    /// Available/used event suppression.
    pub const RING_EVENT_IDX: u64 = 1 << 29;
    /// Modern virtio. Not optional: without it the guest falls back to the
    /// legacy layout, which places the rings differently and which we do not
    /// implement.
    pub const VERSION_1: u64 = 1 << 32;
}

/// Device status bits the driver writes as it brings a device up.
pub mod status {
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
    pub const DEVICE_NEEDS_RESET: u32 = 64;
    pub const FAILED: u32 = 128;
}

/// What a device wants done after servicing a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Serviced {
    /// Whether any descriptors were returned to the driver.
    pub used_any: bool,
}

/// A virtio device model.
///
/// The transport owns the queues and the negotiation state; a device only
/// implements what makes it that kind of device — its type, its features, its
/// configuration space, and what to do when a queue is notified.
pub trait VirtioDevice: Send {
    fn device_type(&self) -> u32;

    /// A short name for diagnostics.
    fn name(&self) -> &'static str;

    /// Feature bits this device offers, including transport bits.
    fn features(&self) -> u64;

    /// Records the subset the driver accepted.
    ///
    /// Devices whose behaviour depends on negotiation — read-only block, packed
    /// rings — must consult this rather than their own offer.
    fn ack_features(&mut self, features: u64) {
        let _ = features;
    }

    /// How many virtqueues this device has.
    fn queue_count(&self) -> usize;

    /// Largest size for each queue.
    fn queue_max_size(&self) -> u16 {
        queue::MAX_QUEUE_SIZE
    }

    /// Device-specific configuration space.
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let _ = offset;
        data.fill(0);
    }

    fn config_write(&mut self, offset: u64, data: &[u8]) {
        let _ = (offset, data);
    }

    /// Called once the driver sets `DRIVER_OK`.
    fn activate(&mut self, mem: Arc<GuestMemory>) {
        let _ = mem;
    }

    /// Services a notification on `queue`.
    fn notify(&mut self, queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced;

    /// Returns the device to its power-on state.
    fn reset(&mut self) {}
}

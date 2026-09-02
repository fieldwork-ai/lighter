//! The virtio-mmio transport.
//!
//! One of these wraps each device model and presents the register interface
//! Linux's `virtio_mmio` driver expects. It owns everything that is true of
//! *every* virtio device — the queues, feature negotiation, device status, and
//! the interrupt — so a device model contains only what makes it that device.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

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
    /// Shared with the bus, which serves INTERRUPT_STATUS reads and
    /// INTERRUPT_ACK writes from it without taking this device's lock: the
    /// vCPU acknowledging a completion otherwise queued behind the vCPU
    /// submitting the next request, and a stream of 4 KiB writes spent as
    /// long waiting for that lock as running the guest.
    interrupt_status: std::sync::Arc<std::sync::atomic::AtomicU32>,
    config_generation: u32,
    /// Set once `DRIVER_OK` has been seen, so activation happens exactly once.
    activated: bool,
    /// Called when the guest writes the notification register. See
    /// [`VirtioMmio::set_kick_observer`].
    kick_observer: Option<Arc<dyn Fn(u16) + Send + Sync>>,
    /// A lock-free mirror of each queue's cursor, for whoever is watching it.
    ///
    /// The point is that a watcher must not need this transport's lock to ask
    /// "is there anything yet?". Taking it in a spin loop is what made the
    /// first host poller *slower* than letting the guest trap: the answer was
    /// no ninety-nine times in a hundred, and every one of those cost the vCPU
    /// a lock it was about to need.
    signals: Vec<Arc<QueueSignal>>,
}

/// What a queue watcher needs to know, without a lock.
///
/// Three relaxed atomics and a read straight out of guest memory. Relaxed is
/// enough because this answers a *hint*: a stale "yes" costs one wasted trip
/// through `poll_queue`, which re-checks under the lock, and a stale "no" is
/// corrected on the next turn of the loop microseconds later. The correctness
/// of what is consumed is the queue's business, not this structure's.
#[derive(Debug, Default)]
pub struct QueueSignal {
    ready: AtomicBool,
    /// Split: the available ring. Packed: the descriptor ring.
    ring_addr: AtomicU64,
    next_avail: AtomicU16,
    /// Packed rings answer the question differently, and reading one as the
    /// other is not a crash — it is a watcher that spins on the driver's event
    /// flags believing they are an index, and so either never sleeps or never
    /// wakes.
    packed: AtomicBool,
    /// The device's lap counter, which is half of what makes a packed
    /// descriptor available.
    avail_wrap: AtomicBool,
}

impl QueueSignal {
    /// Whether the driver has offered anything we have not taken.
    pub fn has_work(&self, memory: &GuestMemory) -> bool {
        if !self.ready.load(Ordering::Relaxed) {
            return false;
        }
        let addr = self.ring_addr.load(Ordering::Relaxed);
        if self.packed.load(Ordering::Relaxed) {
            // The flags of the descriptor at the cursor, at offset 14 of a
            // sixteen-byte entry. Available means the availability bit matches
            // our lap and the used bit does not.
            let at = addr + u64::from(self.next_avail.load(Ordering::Relaxed)) * 16 + 14;
            let wrap = self.avail_wrap.load(Ordering::Relaxed);
            return memory.read_u16(at).is_ok_and(|flags| {
                let avail = flags & (1 << 7) != 0;
                let used = flags & (1 << 15) != 0;
                avail == wrap && used != avail
            });
        }
        // Offset 2 in the available ring: past `flags`, at `idx`.
        memory
            .read_u16(addr + 2)
            .is_ok_and(|idx| idx != self.next_avail.load(Ordering::Relaxed))
    }

    fn publish(&self, queue: &Virtqueue) {
        let packed = queue.is_packed();
        self.packed.store(packed, Ordering::Relaxed);
        self.ring_addr.store(
            if packed {
                queue.desc_addr
            } else {
                queue.avail_addr
            },
            Ordering::Relaxed,
        );
        self.next_avail.store(queue.next_avail(), Ordering::Relaxed);
        self.avail_wrap.store(queue.avail_wrap(), Ordering::Relaxed);
        // Published last: it is the field that authorizes reading the others.
        self.ready.store(queue.is_ready(), Ordering::Relaxed);
    }
}

/// Doorbell exits taken (every queue notification the guest made), and
/// queue pickups the host poller made without one. Diagnostics: the ratio
/// says whether the poller is sparing the guest its exits.
pub static NOTIFIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static POLLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl VirtioMmio {
    pub fn new(
        device: Box<dyn VirtioDevice>,
        memory: Arc<GuestMemory>,
        irq: Arc<dyn IrqLine>,
    ) -> VirtioMmio {
        let max = device.queue_max_size();
        let signals = (0..device.queue_count())
            .map(|_| Arc::new(QueueSignal::default()))
            .collect();
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
            interrupt_status: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            config_generation: 0,
            activated: false,
            kick_observer: None,
            signals,
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
        self.interrupt_status
            .store(0, std::sync::atomic::Ordering::Release);
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
        NOTIFIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

        let memory = self.memory.clone();
        let mut serviced = self.device.notify(index, &mut self.queues, &memory);
        loop {
            self.publish_signals();
            // Re-arm before deciding about the interrupt: the driver stops
            // kicking a queue whose event index it has overtaken, and it
            // overtakes one that is never written. A device that negotiates
            // the feature owes this on every pass — but only where the cursor
            // has actually moved, because the write carries a fence and
            // paying for one per queue per request costs more than the
            // notification it saves.
            for queue in &mut self.queues {
                queue.arm_notifications(&memory);
            }
            // Then the ring is read once more, and this is not optional. The
            // driver decides whether to kick by comparing against the event
            // index it can see, and a chain it published between our last
            // look at the ring and the arming above compared against the OLD
            // one — an index it had long since passed — and was not kicked
            // for. Nothing in the driver looks again: the chain sits in the
            // ring until its next unrelated submission crosses the new index.
            // For an ext4 journal commit blocked on that very chain there is
            // no next submission, and the guest's data disk is dead. That was
            // the hang: a container that had exited, `jbd2/vdb-8` in D state
            // in `__wait_on_buffer`, the host entirely idle, three times in
            // one night under exactly the load that makes the window widest.
            // The specification's own double check; the driver does the
            // mirror image of it when it re-enables interrupts.
            let mut moved = false;
            for i in 0..self.queues.len() {
                if !self.queues[i].has_work(&memory) {
                    continue;
                }
                let before = self.queues[i].next_avail();
                let more = self.device.notify(i as u16, &mut self.queues, &memory);
                serviced = serviced.and(more);
                // A queue that offers buffers rather than requests (a
                // receive ring) always "has work" and never moves; only a
                // cursor that advanced means there was something to take.
                if self.queues[i].next_avail() != before {
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        if !serviced.any() {
            return;
        }

        // Ask every queue that gained used entries, not just the notified one.
        // Servicing TX can produce work on RX, and consulting TX's suppression
        // state about an RX completion is how a reply ends up sitting in the
        // ring with the guest asleep.
        // Deliberately not `any`, and not `filter().any()` either:
        // `needs_interrupt` records that the driver has been told, so
        // short-circuiting would leave a later queue believing it had
        // signalled a used index it never mentioned.
        let mut wants_interrupt = false;
        for i in 0..self.queues.len() {
            if serviced.contains(i as u16) && self.queues[i].needs_interrupt(&memory) {
                wants_interrupt = true;
            }
        }
        if wants_interrupt {
            self.interrupt_status
                .fetch_or(INT_VRING, std::sync::atomic::Ordering::AcqRel);
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
                let all = offered_features(self.device.features());
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
            INTERRUPT_STATUS => self
                .interrupt_status
                .load(std::sync::atomic::Ordering::Acquire),
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
        // Any register write can move a queue's geometry — its addresses, its
        // size, whether it is live at all — so the mirror is refreshed after
        // all of them rather than after an enumerated few that would go stale
        // the first time a register was added.
        self.write_register_inner(offset, value);
        self.publish_signals();
    }

    fn write_register_inner(&mut self, offset: u64, value: u32) {
        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = value,
            DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            DRIVER_FEATURES => {
                let shifted = u64::from(value) << (32 * u64::from(self.driver_features_sel));
                self.acked_features |= shifted;
                self.device.ack_features(self.acked_features);
                // The event index changes what the ring's trailing fields
                // mean, so every queue has to learn it before the driver can
                // set one up.
                let negotiated = self.negotiated_features();
                let event_idx = negotiated & features::RING_EVENT_IDX != 0;
                let packed = negotiated & features::RING_PACKED != 0;
                for queue in &mut self.queues {
                    queue.set_event_idx(event_idx);
                    queue.set_packed(packed);
                }
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
            QUEUE_NOTIFY => {
                // A real kick, as distinct from a host thread deciding to look
                // at the ring. Only this tells us the guest has started asking
                // for things, which is when a poller is worth waking.
                if let Some(observer) = &self.kick_observer {
                    observer(value as u16);
                }
                self.notify_queue(value as u16)
            }
            INTERRUPT_ACK => {
                self.interrupt_status
                    .fetch_and(!value, std::sync::atomic::Ordering::AcqRel);
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
        self.interrupt_status
            .fetch_or(INT_CONFIG, std::sync::atomic::Ordering::AcqRel);
        self.irq.pulse();
    }

    /// Lets a device with a host-side source of work (a network backend, a
    /// vsock peer) push completions without the guest notifying first.
    pub fn service_queue(&mut self, index: u16) {
        self.notify_queue(index);
    }

    /// Services a queue if the driver has left anything on it, without having
    /// been notified. Returns whether there was anything to do.
    ///
    /// This is the host half of busy-polling: paired with
    /// [`VirtioMmio::suppress_notifications`], a request can cross into the
    /// VMM with no trap at all.
    /// Installs a callback for genuine guest notifications.
    ///
    /// Deliberately fired from the register write rather than from the device,
    /// because the device cannot tell a guest kick from a host thread's own
    /// polling — and a poller woken by its own work never sleeps again.
    pub fn set_kick_observer(&mut self, observer: Arc<dyn Fn(u16) + Send + Sync>) {
        self.kick_observer = Some(observer);
    }

    pub fn poll_queue(&mut self, index: u16) -> bool {
        let Some(queue) = self.queues.get(index as usize) else {
            return false;
        };
        if !queue.has_work(&self.memory) {
            return false;
        }
        // Counted here and again inside notify_queue: doorbell exits are
        // NOTIFIES minus POLLED.
        POLLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.notify_queue(index);
        true
    }

    /// The lock-free view of a queue, for a watcher.
    pub fn signal(&self, index: u16) -> Option<Arc<QueueSignal>> {
        self.signals.get(index as usize).cloned()
    }

    /// Republishes every queue's cursor to its signal.
    ///
    /// Called wherever the guest could have changed a queue's geometry and
    /// wherever we could have consumed from one — which is deliberately more
    /// often than strictly necessary. A signal is a hint, and the cost of
    /// refreshing one is three relaxed stores; the cost of forgetting to is a
    /// watcher that sleeps through a burst.
    fn publish_signals(&self) {
        for (signal, queue) in self.signals.iter().zip(self.queues.iter()) {
            signal.publish(queue);
        }
    }

    /// Chains the driver has offered that we have not taken.
    pub fn outstanding(&self, index: u16) -> u16 {
        self.queues
            .get(index as usize)
            .map(|queue| queue.outstanding(&self.memory))
            .unwrap_or(0)
    }

    /// Sets or clears the "do not kick us" flag on a queue.
    pub fn suppress_notifications(&mut self, index: u16, suppress: bool) {
        let memory = self.memory.clone();
        if let Some(queue) = self.queues.get_mut(index as usize) {
            queue.suppress_notifications(&memory, suppress);
        }
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
    fn lockfree_interrupt(&self) -> Option<crate::bus::LockfreeInterrupt> {
        Some(crate::bus::LockfreeInterrupt {
            status: self.interrupt_status.clone(),
            status_offset: INTERRUPT_STATUS,
            ack_offset: INTERRUPT_ACK,
        })
    }

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
pub const COMMON_FEATURES: u64 = features::VERSION_1
    | features::RING_INDIRECT_DESC
    | features::RING_EVENT_IDX
    | features::RING_PACKED;

/// What a device should actually offer.
///
/// The packed layout is withheld by `LIGHTER_VIRTIO_PACKED=0`, which exists
/// because the answer to "is it faster" turned out to be workload-dependent
/// and worth being able to re-ask without a rebuild. The driver chooses it
/// whenever it is offered, so withholding is the only way to compare.
pub fn offered_features(device: u64) -> u64 {
    if std::env::var("LIGHTER_VIRTIO_PACKED").as_deref() == Ok("0") {
        return device & !features::RING_PACKED;
    }
    device
}

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
        t.interrupt_status
            .store(INT_VRING | INT_CONFIG, std::sync::atomic::Ordering::Release);
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

//! Split virtqueues.
//!
//! Every virtio device moves data through one of these, so this is the piece
//! whose correctness matters most and whose bugs are hardest to see: a subtle
//! error here does not crash, it corrupts a block or drops a packet under load.
//!
//! # Trusting nothing
//!
//! The rings live in guest memory and the guest can rewrite them at any instant,
//! including while we walk them. Every field is therefore read once into a local
//! and validated before use — never read twice and assumed equal — and every
//! descriptor chain walk is bounded by the queue size, because a guest can
//! trivially build a descriptor loop. Those two rules are what stop a buggy or
//! hostile guest turning a device model into an infinite loop or an
//! out-of-bounds host write.

use crate::memory::GuestMemory;

/// Bytes per descriptor in the table: `addr` (8), `len` (4), `flags` (2),
/// `next` (2).
const DESC_SIZE: u64 = 16;

/// This descriptor continues into `next`.
const VIRTQ_DESC_F_NEXT: u16 = 1;
/// The device writes this buffer; without it, the device reads it.
const VIRTQ_DESC_F_WRITE: u16 = 2;
/// This descriptor points at a table of further descriptors.
const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// The largest queue we advertise.
///
/// Bounded because the guest chooses the queue size and every chain walk is
/// bounded by it; an unbounded value would hand the guest control of how long
/// we spin.
pub const MAX_QUEUE_SIZE: u16 = 256;

/// One entry of the descriptor table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    /// Guest-physical address of the buffer.
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

impl Descriptor {
    /// Whether the *device* writes this buffer.
    #[inline]
    pub const fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }

    #[inline]
    const fn has_next(&self) -> bool {
        self.flags & VIRTQ_DESC_F_NEXT != 0
    }

    #[inline]
    const fn is_indirect(&self) -> bool {
        self.flags & VIRTQ_DESC_F_INDIRECT != 0
    }
}

/// A virtqueue's guest-provided geometry and our cursor into it.
#[derive(Debug, Clone)]
pub struct Virtqueue {
    /// Largest size this device supports for the queue.
    max_size: u16,
    /// Size the driver actually chose.
    size: u16,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
    ready: bool,
    /// How far we have consumed the available ring.
    ///
    /// Ours, not the guest's: the guest's `avail.idx` only ever moves forward,
    /// and the difference between it and this is what is outstanding.
    next_avail: u16,
    /// The index we will write next into the used ring.
    next_used: u16,
}

impl Virtqueue {
    pub fn new(max_size: u16) -> Virtqueue {
        Virtqueue {
            max_size,
            size: max_size,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            ready: false,
            next_avail: 0,
            next_used: 0,
        }
    }

    pub const fn max_size(&self) -> u16 {
        self.max_size
    }

    pub const fn size(&self) -> u16 {
        self.size
    }

    /// Sets the size the driver selected, clamped to what we support.
    pub fn set_size(&mut self, size: u16) {
        self.size = size.min(self.max_size);
    }

    pub const fn is_ready(&self) -> bool {
        self.ready
    }

    /// Marks the queue live, if its geometry is usable.
    ///
    /// A queue whose size is zero or not a power of two would make the ring
    /// index arithmetic wrong rather than merely unusual, so it is refused.
    pub fn set_ready(&mut self, ready: bool) -> bool {
        if ready && !self.is_geometry_valid() {
            return false;
        }
        self.ready = ready;
        true
    }

    fn is_geometry_valid(&self) -> bool {
        self.size > 0
            && self.size <= self.max_size
            && self.size.is_power_of_two()
            && self.desc_addr != 0
            && self.avail_addr != 0
            && self.used_addr != 0
    }

    /// Returns the queue to its reset state, as a device reset requires.
    pub fn reset(&mut self) {
        let max = self.max_size;
        *self = Virtqueue::new(max);
    }

    /// The driver's `avail.idx`.
    fn avail_idx(&self, mem: &GuestMemory) -> Option<u16> {
        // Offset 2: after the 2-byte flags field.
        mem.read_u16(self.avail_addr + 2).ok()
    }

    /// Whether the driver has made anything available.
    pub fn has_work(&self, mem: &GuestMemory) -> bool {
        self.ready
            && self
                .avail_idx(mem)
                .is_some_and(|idx| idx != self.next_avail)
    }

    /// Takes the next available descriptor chain, if there is one.
    pub fn pop<'m>(&mut self, mem: &'m GuestMemory) -> Option<DescriptorChain<'m>> {
        if !self.ready {
            return None;
        }
        let avail_idx = self.avail_idx(mem)?;
        if avail_idx == self.next_avail {
            return None;
        }

        // The ring holds `size` entries and wraps; `idx` itself wraps at 2^16.
        let slot = u64::from(self.next_avail % self.size);
        // Offset 4: past flags and idx.
        let head = mem.read_u16(self.avail_addr + 4 + slot * 2).ok()?;
        if head >= self.size {
            tracing::warn!(
                head,
                size = self.size,
                "driver offered an out-of-range descriptor"
            );
            // Still consume it, or we would spin on the same bad entry forever.
            self.next_avail = self.next_avail.wrapping_add(1);
            return None;
        }

        self.next_avail = self.next_avail.wrapping_add(1);
        Some(DescriptorChain::new(mem, self.desc_addr, self.size, head))
    }

    /// Returns a consumed chain to the driver.
    ///
    /// `len` is the number of bytes the device wrote into the chain's
    /// device-writable buffers — not the chain's total size. Drivers use it to
    /// size the response, so an inflated value leaks host memory contents and a
    /// short one truncates the answer.
    pub fn push_used(&mut self, mem: &GuestMemory, head: u16, len: u32) {
        let slot = u64::from(self.next_used % self.size);
        // The used ring starts with 2-byte flags and 2-byte idx, then 8-byte
        // elements of {id: u32, len: u32}.
        let element = self.used_addr + 4 + slot * 8;
        let _ = mem.write_u32(element, u32::from(head));
        let _ = mem.write_u32(element + 4, len);

        self.next_used = self.next_used.wrapping_add(1);

        // The index must become visible only after the element it refers to.
        // On aarch64 the guest reads these with acquire semantics, so a release
        // fence here is what makes the pairing correct.
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        let _ = mem.write_u16(self.used_addr + 2, self.next_used);
    }

    /// How many chains the driver has made available that we have not taken.
    ///
    /// Diagnostics, and the one number that matters when a polled queue stops:
    /// a poller that goes to sleep while this is non-zero has lost a wake-up,
    /// and the guest will wait forever for a reply nobody is going to produce.
    pub fn outstanding(&self, mem: &GuestMemory) -> u16 {
        self.avail_idx(mem)
            .map(|idx| idx.wrapping_sub(self.next_avail))
            .unwrap_or(0)
    }

    /// Tells the driver not to bother kicking us.
    ///
    /// The driver reads this flag before every notification and skips the
    /// write when it is set. That write is an MMIO trap — a vCPU leaving the
    /// guest, our handler running, and the core re-entering — so suppressing it
    /// while a host thread is already watching the ring removes the whole
    /// crossing from the submission path.
    ///
    /// **Clearing it is the dangerous direction.** A driver that decided not to
    /// kick, because the flag was set when it looked, will not look again — so
    /// whoever clears this has to re-examine the ring afterwards or the request
    /// sits there until something unrelated happens along.
    pub fn suppress_notifications(&self, mem: &GuestMemory, suppress: bool) {
        const VRING_USED_F_NO_NOTIFY: u16 = 1;
        if self.used_addr == 0 {
            return;
        }
        let flags = if suppress { VRING_USED_F_NO_NOTIFY } else { 0 };
        let _ = mem.write_u16(self.used_addr, flags);
        // The driver must see the flag before we look at its ring, and must see
        // it cleared before we stop looking. Either way round, the ordering is
        // what makes the hand-off safe.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the driver wants an interrupt for the work just completed.
    ///
    /// Honouring `VIRTQ_AVAIL_F_NO_INTERRUPT` is not an optimization we can
    /// skip: a driver polling a busy queue sets it precisely to avoid the
    /// interrupt storm that ignoring it produces.
    pub fn needs_interrupt(&self, mem: &GuestMemory) -> bool {
        const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
        match mem.read_u16(self.avail_addr) {
            Ok(flags) => flags & VIRTQ_AVAIL_F_NO_INTERRUPT == 0,
            Err(_) => true,
        }
    }
}

/// Walks one descriptor chain.
///
/// Iteration is bounded by the queue size: a guest that builds a cycle gets a
/// truncated chain and a warning, not a hung vCPU thread.
pub struct DescriptorChain<'m> {
    mem: &'m GuestMemory,
    /// Table currently being walked — the queue's own, or an indirect one.
    table: u64,
    /// Number of descriptors in `table`.
    table_len: u16,
    next: Option<u16>,
    /// The chain's first descriptor index, which is what goes in the used ring.
    head: u16,
    /// Descriptors yielded so far, to bound the walk.
    walked: u32,
    /// Set once we descend into an indirect table, since they do not nest.
    indirect: bool,
    /// The descriptor that pointed at the indirect table, so we can resume the
    /// outer chain if it continues.
    outer_next: Option<u16>,
    outer_table: u64,
    outer_len: u16,
}

impl<'m> DescriptorChain<'m> {
    fn new(mem: &'m GuestMemory, table: u64, table_len: u16, head: u16) -> DescriptorChain<'m> {
        DescriptorChain {
            mem,
            table,
            table_len,
            next: Some(head),
            head,
            walked: 0,
            indirect: false,
            outer_next: None,
            outer_table: 0,
            outer_len: 0,
        }
    }

    /// The index to report in the used ring.
    pub const fn head(&self) -> u16 {
        self.head
    }

    fn read_descriptor(&self, table: u64, index: u16) -> Option<Descriptor> {
        let addr = table + u64::from(index) * DESC_SIZE;
        Some(Descriptor {
            addr: self.mem.read_u64(addr).ok()?,
            len: self.mem.read_u32(addr + 8).ok()?,
            flags: self.mem.read_u16(addr + 12).ok()?,
            next: self.mem.read_u16(addr + 14).ok()?,
        })
    }
}

impl Iterator for DescriptorChain<'_> {
    type Item = Descriptor;

    fn next(&mut self) -> Option<Descriptor> {
        loop {
            // Two bounds, because a chain may legitimately span an indirect
            // table larger than the queue: cap total work regardless.
            if self.walked > u32::from(MAX_QUEUE_SIZE) * 2 {
                tracing::warn!("descriptor chain exceeded its bound; treating as ended");
                return None;
            }

            let index = self.next?;
            if index >= self.table_len {
                tracing::warn!(index, len = self.table_len, "descriptor index out of range");
                return None;
            }

            let desc = self.read_descriptor(self.table, index)?;
            self.walked += 1;

            if desc.is_indirect() && !self.indirect {
                // An indirect descriptor's buffer is itself a descriptor table.
                // Remember where to resume, then continue inside it.
                self.outer_next = desc.has_next().then_some(desc.next);
                self.outer_table = self.table;
                self.outer_len = self.table_len;
                self.indirect = true;
                self.table = desc.addr;
                self.table_len =
                    (desc.len / DESC_SIZE as u32).min(u32::from(MAX_QUEUE_SIZE) * 2) as u16;
                self.next = Some(0);
                continue;
            }

            self.next = if desc.has_next() {
                Some(desc.next)
            } else if self.indirect {
                // The indirect table ended; resume the outer chain if it had
                // more, which a well-formed driver rarely does but is legal.
                self.indirect = false;
                self.table = self.outer_table;
                self.table_len = self.outer_len;
                self.outer_next.take()
            } else {
                None
            };

            return Some(desc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use lighter_hv::Vm;

    /// Builds a queue in real guest memory so the tests exercise the same reads
    /// the device does, rather than a mock that could agree with a wrong model.
    struct Harness {
        mem: GuestMemory,
        queue: Virtqueue,
        base: u64,
    }

    const BASE: u64 = 0x4000_0000;
    const DESC: u64 = BASE;
    const AVAIL: u64 = BASE + 0x1000;
    const USED: u64 = BASE + 0x2000;
    const DATA: u64 = BASE + 0x3000;

    impl Harness {
        fn new(vm: Arc<Vm>, size: u16) -> Harness {
            let mut mem = GuestMemory::new(vm);
            mem.add_region(BASE, 0x10_0000).unwrap();
            let mut queue = Virtqueue::new(size);
            queue.desc_addr = DESC;
            queue.avail_addr = AVAIL;
            queue.used_addr = USED;
            queue.set_size(size);
            assert!(queue.set_ready(true));
            Harness {
                mem,
                queue,
                base: BASE,
            }
        }

        fn write_desc(&self, index: u16, desc: Descriptor) {
            let a = DESC + u64::from(index) * DESC_SIZE;
            self.mem.write_u64(a, desc.addr).unwrap();
            self.mem.write_u32(a + 8, desc.len).unwrap();
            self.mem.write_u16(a + 12, desc.flags).unwrap();
            self.mem.write_u16(a + 14, desc.next).unwrap();
        }

        /// Publishes `head` in the available ring.
        fn make_available(&self, slot: u16, head: u16, new_idx: u16) {
            self.mem
                .write_u16(AVAIL + 4 + u64::from(slot) * 2, head)
                .unwrap();
            self.mem.write_u16(AVAIL + 2, new_idx).unwrap();
        }
    }

    /// One VM per process is an Apple constraint, so these tests share one.
    ///
    /// Handed out as an `Arc` rather than behind a lock: a lock would be
    /// poisoned by the first failing test and every later one would report that
    /// instead of its own result, which is how a single real bug once presented
    /// as five identical `PoisonError`s.
    fn shared_vm() -> Arc<Vm> {
        use std::sync::OnceLock;
        static VM: OnceLock<Option<Arc<Vm>>> = OnceLock::new();
        VM.get_or_init(|| Vm::create().ok().map(Arc::new))
            .clone()
            .expect("these tests need the hypervisor entitlement: run `make test-hv`")
    }

    fn with_vm<T>(f: impl FnOnce(Arc<Vm>) -> T) -> T {
        f(shared_vm())
    }

    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn pops_a_single_descriptor_chain() {
        with_vm(|vm| {
            let mut h = Harness::new(vm, 16);
            h.write_desc(
                0,
                Descriptor {
                    addr: DATA,
                    len: 512,
                    flags: VIRTQ_DESC_F_WRITE,
                    next: 0,
                },
            );
            h.make_available(0, 0, 1);

            let chain = h.queue.pop(&h.mem).expect("a chain should be available");
            assert_eq!(chain.head(), 0);
            let descs: Vec<_> = chain.collect();
            assert_eq!(descs.len(), 1);
            assert_eq!(descs[0].addr, DATA);
            assert!(descs[0].is_write_only());
            assert!(h.queue.pop(&h.mem).is_none(), "queue should now be empty");
        });
    }

    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn walks_a_multi_descriptor_chain_in_order() {
        with_vm(|vm| {
            let mut h = Harness::new(vm, 16);
            for i in 0..3u16 {
                h.write_desc(
                    i,
                    Descriptor {
                        addr: DATA + u64::from(i) * 0x100,
                        len: 0x100,
                        flags: if i < 2 { VIRTQ_DESC_F_NEXT } else { 0 },
                        next: i + 1,
                    },
                );
            }
            h.make_available(0, 0, 1);

            let addrs: Vec<u64> = h.queue.pop(&h.mem).unwrap().map(|d| d.addr).collect();
            assert_eq!(addrs, vec![DATA, DATA + 0x100, DATA + 0x200]);
        });
    }

    /// A guest can build a descriptor cycle trivially. The device must not spin.
    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn a_descriptor_loop_terminates() {
        with_vm(|vm| {
            let mut h = Harness::new(vm, 16);
            h.write_desc(
                0,
                Descriptor {
                    addr: DATA,
                    len: 16,
                    flags: VIRTQ_DESC_F_NEXT,
                    next: 1,
                },
            );
            h.write_desc(
                1,
                Descriptor {
                    addr: DATA,
                    len: 16,
                    flags: VIRTQ_DESC_F_NEXT,
                    next: 0,
                },
            );
            h.make_available(0, 0, 1);

            let count = h.queue.pop(&h.mem).unwrap().count();
            assert!(
                count <= usize::from(MAX_QUEUE_SIZE) * 2 + 1,
                "walk was unbounded"
            );
        });
    }

    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn an_out_of_range_head_is_consumed_not_retried() {
        with_vm(|vm| {
            let mut h = Harness::new(vm, 16);
            h.make_available(0, 99, 1);
            assert!(h.queue.pop(&h.mem).is_none());
            // Critically, the bad entry must not still be pending, or the
            // device would spin on it for the machine's lifetime.
            assert!(!h.queue.has_work(&h.mem));
        });
    }

    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn used_ring_records_head_and_written_length() {
        with_vm(|vm| {
            let mut h = Harness::new(vm, 16);
            h.queue.push_used(&h.mem, 3, 512);
            assert_eq!(h.mem.read_u32(USED + 4).unwrap(), 3, "used element id");
            assert_eq!(h.mem.read_u32(USED + 8).unwrap(), 512, "written length");
            assert_eq!(h.mem.read_u16(USED + 2).unwrap(), 1, "used idx advanced");
            let _ = h.base;
        });
    }

    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn indirect_descriptors_are_followed() {
        with_vm(|vm| {
            let mut h = Harness::new(vm, 16);
            // Descriptor 0 points at a two-entry table living at DATA.
            h.write_desc(
                0,
                Descriptor {
                    addr: DATA,
                    len: (DESC_SIZE * 2) as u32,
                    flags: VIRTQ_DESC_F_INDIRECT,
                    next: 0,
                },
            );
            for i in 0..2u64 {
                let a = DATA + i * DESC_SIZE;
                h.mem.write_u64(a, 0x5000_0000 + i * 0x100).unwrap();
                h.mem.write_u32(a + 8, 0x100).unwrap();
                h.mem
                    .write_u16(a + 12, if i == 0 { VIRTQ_DESC_F_NEXT } else { 0 })
                    .unwrap();
                h.mem.write_u16(a + 14, 1).unwrap();
            }
            h.make_available(0, 0, 1);

            let addrs: Vec<u64> = h.queue.pop(&h.mem).unwrap().map(|d| d.addr).collect();
            assert_eq!(addrs, vec![0x5000_0000, 0x5000_0100]);
        });
    }

    #[test]
    fn geometry_must_be_a_power_of_two() {
        let mut q = Virtqueue::new(256);
        q.desc_addr = 0x1000;
        q.avail_addr = 0x2000;
        q.used_addr = 0x3000;

        q.set_size(100);
        assert!(
            !q.set_ready(true),
            "a non-power-of-two size must be refused"
        );

        q.set_size(128);
        assert!(q.set_ready(true));

        // The driver may not ask for more than the device offers.
        q.set_size(1024);
        assert_eq!(q.size(), 256, "size must clamp to the device maximum");
    }

    #[test]
    fn a_queue_with_no_rings_is_not_ready() {
        let mut q = Virtqueue::new(16);
        q.set_size(16);
        assert!(!q.set_ready(true), "queue with null ring addresses");
    }

    #[test]
    fn reset_restores_the_cursor_as_well_as_the_geometry() {
        let mut q = Virtqueue::new(16);
        q.desc_addr = 0x1000;
        q.avail_addr = 0x2000;
        q.used_addr = 0x3000;
        q.set_size(16);
        q.set_ready(true);
        q.next_avail = 7;
        q.next_used = 7;

        q.reset();

        // A device reset that left the cursors behind would make the next boot
        // skip every request the driver made before catching up.
        assert_eq!(q.next_avail, 0);
        assert_eq!(q.next_used, 0);
        assert!(!q.is_ready());
        assert_eq!(q.max_size(), 16);
    }
}

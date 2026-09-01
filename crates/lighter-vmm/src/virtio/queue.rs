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

/// In a packed ring, the driver flips this bit to say a descriptor is its turn.
const VIRTQ_DESC_F_AVAIL: u16 = 1 << 7;
/// And this one, to match, once the device has finished with it.
const VIRTQ_DESC_F_USED: u16 = 1 << 15;

/// Packed-ring event suppression: the two ends tell each other whether they
/// want to be disturbed by writing one of these into a four-byte structure of
/// `{ off_wrap: u16, flags: u16 }`.
mod event {
    /// Always tell me.
    pub const ENABLE: u16 = 0;
    /// Never tell me.
    pub const DISABLE: u16 = 1;
    /// Tell me when you reach the descriptor named in `off_wrap`.
    pub const DESC: u16 = 2;
    /// Bit 15 of `off_wrap` carries the wrap counter; the rest is the offset.
    pub const WRAP_SHIFT: u32 = 15;
}

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
    /// Whether the driver chose the packed layout.
    ///
    /// One ring instead of three. A descriptor carries its own availability in
    /// two flag bits rather than being pointed at by a separate ring, so a
    /// request touches one cache line where the split layout touches three,
    /// and the two ends stop bouncing a pair of indices between them. On a
    /// workload that is hundreds of thousands of tiny requests that is the
    /// difference the layout was invented for.
    packed: bool,
    /// The device's wrap counter for reading. Starts set, flips each time the
    /// read cursor passes the end of the ring.
    ///
    /// The counters are the whole trick: with no separate available ring,
    /// "is this mine yet" is answered by comparing the descriptor's own flag
    /// bits against a counter each side keeps, and the flip is what stops a
    /// stale descriptor from a previous lap reading as fresh.
    avail_wrap: bool,
    /// The same, for writing completions.
    used_wrap: bool,
    /// Whether `VIRTIO_F_EVENT_IDX` was negotiated.
    ///
    /// It changes the meaning of two fields rather than adding a third, which
    /// is why it has to be known here: with it, the flag words at the head of
    /// each ring are ignored by both sides, and the trailing event indices are
    /// what say who wants telling about what. Get this wrong in the
    /// permissive direction and you get an interrupt per request; get it wrong
    /// in the other and the guest waits forever for one that never comes.
    event_idx: bool,
    /// Whether a watcher has asked the driver to stop kicking this queue.
    ///
    /// Kept here rather than inferred from the ring because the two writers
    /// would otherwise undo each other: the poller asks for silence, and the
    /// next completion re-arms and asks to be kicked again.
    suppressed: bool,
    /// The last value written to the driver's copy of the available event.
    ///
    /// Only so it need not be written again. It is a guest-memory store and a
    /// sequentially-consistent fence, and doing that for every queue on every
    /// request costs more than the notification it is trying to avoid.
    published_event: Option<u16>,
    /// The used index as of the last interrupt we raised.
    ///
    /// `VRING_NEED_EVENT` is a question about an interval — "did the index the
    /// driver is waiting for fall between the last one it heard about and this
    /// one" — so the device has to remember where it last told the driver it
    /// had got to.
    signalled_used: u16,
    /// How many ring slots each buffer id occupied when it was taken.
    ///
    /// Only the packed layout needs it, and it is not optional there: the
    /// device's completion cursor advances by the chain length, the driver's
    /// does the same, and a device that assumed one slot per buffer would
    /// drift out of step with it after the first scatter-gather request.
    chain_len: Vec<u16>,
    /// Which descriptor heads the driver has offered and we have not returned.
    ///
    /// A head is outstanding from the moment it is popped until the moment it
    /// is pushed used, exactly once each way. Returning one twice, or one that
    /// was never taken, corrupts the driver's own bookkeeping — Linux answers
    /// `id %u is not a head!`, marks the queue broken, and every subsequent
    /// submission fails with EIO. That surfaces as a filesystem that stops
    /// dead, several seconds and one subsystem away from whatever caused it,
    /// which is why the queue checks itself rather than trusting its callers.
    outstanding: Vec<bool>,
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
            packed: false,
            avail_wrap: true,
            used_wrap: true,
            event_idx: false,
            suppressed: false,
            published_event: None,
            signalled_used: 0,
            chain_len: Vec::new(),
            outstanding: Vec::new(),
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

    /// Records whether the driver accepted `VIRTIO_F_EVENT_IDX`.
    pub fn set_event_idx(&mut self, on: bool) {
        self.event_idx = on;
    }

    /// Records whether the driver chose the packed layout.
    ///
    /// It has to be known before the queue is used and cannot change after:
    /// the three address registers mean different things in each layout, and
    /// the cursors count different things.
    pub fn set_packed(&mut self, on: bool) {
        self.packed = on;
    }

    pub const fn is_packed(&self) -> bool {
        self.packed
    }

    /// The device's lap counter for reading, which a watcher needs to tell an
    /// available descriptor from one it completed a lap ago.
    pub const fn avail_wrap(&self) -> bool {
        self.avail_wrap
    }

    /// The address of ring entry `index`. Both layouts put 16 bytes per entry.
    const fn desc_at(&self, index: u16) -> u64 {
        self.desc_addr + index as u64 * DESC_SIZE
    }

    /// Where the driver publishes the used index it wants an interrupt at.
    ///
    /// It lives immediately past the available ring: two bytes of flags, two
    /// of index, then `size` entries of two bytes each.
    const fn used_event_addr(&self) -> u64 {
        self.avail_addr + 4 + self.size as u64 * 2
    }

    /// Where the device publishes the available index it wants a kick at.
    ///
    /// Past the used ring: two bytes of flags, two of index, then `size`
    /// entries of eight bytes each.
    const fn avail_event_addr(&self) -> u64 {
        self.used_addr + 4 + self.size as u64 * 8
    }

    /// How far the available ring has been consumed.
    ///
    /// Exposed so a watcher outside the transport lock can mirror it; see
    /// [`crate::virtio::mmio::QueueSignal`].
    pub const fn next_avail(&self) -> u16 {
        self.next_avail
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
        if self.packed {
            // The flags alone answer it; the rest of the descriptor is only
            // wanted once the answer is yes.
            return self.ready
                && mem
                    .read_u16(self.desc_at(self.next_avail) + 14)
                    .is_ok_and(|flags| self.is_available(flags));
        }
        self.ready
            && self
                .avail_idx(mem)
                .is_some_and(|idx| idx != self.next_avail)
    }

    /// Takes the next available descriptor chain, if there is one.
    /// Whether the driver has published another chain, without taking it.
    ///
    /// The filesystem device asks this to decide where to serve a request: a
    /// chain that arrived alone is answered on the vCPU thread, and one that
    /// arrived with company goes to the worker pool. Read-only — the cursor
    /// does not move — so a `pop` after it sees the same chain.
    pub fn more_available(&self, mem: &GuestMemory) -> bool {
        if !self.ready {
            return false;
        }
        if self.packed {
            let head_at = self.desc_at(self.next_avail);
            return mem
                .read_u16(head_at + 14)
                .ok()
                .is_some_and(|flags| self.is_available(flags));
        }
        self.avail_idx(mem)
            .is_some_and(|idx| idx != self.next_avail)
    }

    pub fn pop<'m>(&mut self, mem: &'m GuestMemory) -> Option<DescriptorChain<'m>> {
        if !self.ready {
            return None;
        }
        if self.packed {
            return self.pop_packed(mem);
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
        self.mark_taken(head);
        Some(DescriptorChain::new(mem, self.desc_addr, self.size, head))
    }

    /// Takes the next chain from a packed ring.
    ///
    /// There is no available ring to consult: the descriptor at the cursor
    /// says whether it is ours by carrying the availability flags, and the
    /// chain runs forward from there until one without `NEXT`. The last
    /// descriptor of the chain carries the buffer id the driver wants back —
    /// which is not the ring position, and confusing the two returns the
    /// wrong buffer to the wrong caller.
    fn pop_packed<'m>(&mut self, mem: &'m GuestMemory) -> Option<DescriptorChain<'m>> {
        let start = self.next_avail;
        // The flags first, on their own, and nothing else until they say the
        // descriptor is ours.
        //
        // The driver writes a descriptor's address, length and id, then a
        // release barrier, then the flags that publish it. Reading the fields
        // in that order rather than the reverse is reading them before they
        // were written: the flags come back set and the rest is whatever the
        // last lap left there. It survives an idle machine and fails a busy
        // one — here, fourteen seconds into a package install, as a driver
        // reporting a buffer id that was never a head.
        let head_at = self.desc_at(start);
        let head_flags = mem.read_u16(head_at + 14).ok()?;
        if !self.is_available(head_flags) {
            return None;
        }
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        let first = PackedDesc {
            addr: mem.read_u64(head_at).ok()?,
            len: mem.read_u32(head_at + 8).ok()?,
            id: mem.read_u16(head_at + 12).ok()?,
            flags: head_flags,
        };

        // An indirect descriptor occupies one ring slot and points at a table
        // of plain descriptors; the count there is implied by its length,
        // because a packed indirect table does not chain.
        let (slots, id) = if first.flags & VIRTQ_DESC_F_INDIRECT != 0 {
            (1u16, first.id)
        } else {
            // Only the flags and the id are needed to measure a chain, and
            // they are four bytes of the sixteen. The full descriptors are
            // read once, by the caller walking the chain — reading them twice
            // is four guest-memory loads per descriptor that nothing uses.
            let mut slots = 1u16;
            let mut last = (first.flags, first.id);
            while last.0 & VIRTQ_DESC_F_NEXT != 0 {
                if slots >= self.size {
                    tracing::warn!("packed chain longer than the ring; treating as ended");
                    break;
                }
                let at = self.desc_at((start + slots) % self.size);
                let (Ok(id), Ok(flags)) = (mem.read_u16(at + 12), mem.read_u16(at + 14)) else {
                    break;
                };
                last = (flags, id);
                slots += 1;
            }
            (slots, last.1)
        };

        if id >= self.size {
            tracing::warn!(id, size = self.size, "driver offered an out-of-range id");
            self.advance_avail(slots);
            return None;
        }

        self.advance_avail(slots);
        self.mark_taken(id);
        if self.chain_len.len() != usize::from(self.size) {
            self.chain_len = vec![1; usize::from(self.size)];
        }
        if let Some(slot) = self.chain_len.get_mut(usize::from(id)) {
            *slot = slots;
        }
        Some(DescriptorChain::packed(
            mem,
            self.desc_addr,
            self.size,
            start,
            slots,
            id,
        ))
    }

    /// Whether the driver has handed this descriptor over.
    ///
    /// Available means the availability bit matches our lap and the used bit
    /// does not. Testing only the first is the bug that makes a device read a
    /// descriptor it completed one lap ago as a fresh request.
    const fn is_available(&self, flags: u16) -> bool {
        let avail = flags & VIRTQ_DESC_F_AVAIL != 0;
        let used = flags & VIRTQ_DESC_F_USED != 0;
        avail == self.avail_wrap && used != avail
    }

    /// Moves the read cursor on by `slots`, flipping the lap counter on wrap.
    fn advance_avail(&mut self, slots: u16) {
        let next = self.next_avail as u32 + slots as u32;
        if next >= self.size as u32 {
            self.avail_wrap = !self.avail_wrap;
        }
        self.next_avail = (next % self.size as u32) as u16;
    }

    /// Records that a head has been taken, and complains if it already was.
    fn mark_taken(&mut self, head: u16) {
        if self.outstanding.len() != usize::from(self.size) {
            self.outstanding = vec![false; usize::from(self.size)];
        }
        match self.outstanding.get_mut(usize::from(head)) {
            Some(slot) if *slot => {
                tracing::error!(
                    head,
                    "the driver offered a head that is already outstanding"
                )
            }
            Some(slot) => *slot = true,
            None => tracing::error!(head, size = self.size, "head outside the ring"),
        }
    }

    /// Returns a consumed chain to the driver.
    ///
    /// `len` is the number of bytes the device wrote into the chain's
    /// device-writable buffers — not the chain's total size. Drivers use it to
    /// size the response, so an inflated value leaks host memory contents and a
    /// short one truncates the answer.
    pub fn push_used(&mut self, mem: &GuestMemory, head: u16, len: u32) {
        match self.outstanding.get_mut(usize::from(head)) {
            Some(slot) if *slot => *slot = false,
            // Returning a head twice is what breaks the driver's ring, so it is
            // refused here rather than written. Dropping the completion strands
            // one request; writing it strands the whole filesystem.
            Some(_) => {
                tracing::error!(head, "refusing to return a head that is not outstanding");
                return;
            }
            None => {
                tracing::error!(head, size = self.size, "refusing a head outside the ring");
                return;
            }
        }
        if self.packed {
            self.push_used_packed(mem, head, len);
            return;
        }
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

    /// Returns a chain to the driver in a packed ring.
    ///
    /// One entry is written, at the device's own cursor, carrying the buffer
    /// id and the bytes written; then both flag bits are set to the current
    /// lap, which is what the driver looks for. The cursor then moves on by
    /// the *chain length*, because the driver advances its own by the same
    /// amount and the two have to stay in step.
    ///
    /// Order matters and is not incidental: the id and length must be visible
    /// before the flags that publish them, or the driver reads a descriptor it
    /// believes is finished and finds the previous lap's contents in it.
    fn push_used_packed(&mut self, mem: &GuestMemory, id: u16, len: u32) {
        let slots = self
            .chain_len
            .get(usize::from(id))
            .copied()
            .unwrap_or(1)
            .max(1);
        let at = self.desc_at(self.next_used);
        let _ = mem.write_u16(at + 12, id);
        let _ = mem.write_u32(at + 8, len);
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        let flags = if self.used_wrap {
            VIRTQ_DESC_F_AVAIL | VIRTQ_DESC_F_USED
        } else {
            0
        };
        let _ = mem.write_u16(at + 14, flags);

        let next = self.next_used as u32 + slots as u32;
        if next >= self.size as u32 {
            self.used_wrap = !self.used_wrap;
        }
        self.next_used = (next % self.size as u32) as u16;
    }

    /// How many chains the driver has made available that we have not taken.
    ///
    /// Diagnostics, and the one number that matters when a polled queue stops:
    /// a poller that goes to sleep while this is non-zero has lost a wake-up,
    /// and the guest will wait forever for a reply nobody is going to produce.
    pub fn outstanding(&self, mem: &GuestMemory) -> u16 {
        // A packed ring has no index to subtract: whether there is anything
        // there is a question about one descriptor, and "some" is all the
        // callers need — they drain until it is none.
        if self.packed {
            return u16::from(self.has_work(mem));
        }
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
    pub fn suppress_notifications(&mut self, mem: &GuestMemory, suppress: bool) {
        const VRING_USED_F_NO_NOTIFY: u16 = 1;
        if self.used_addr == 0 {
            return;
        }
        self.suppressed = suppress;
        if self.packed {
            // The packed layout says this outright instead of encoding it as
            // an index far enough away to be behind you: one word, two values.
            let flags = if suppress {
                event::DISABLE
            } else {
                event::ENABLE
            };
            let _ = mem.write_u16(self.used_addr + 2, flags);
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            return;
        }
        if self.event_idx {
            self.publish_avail_event(mem);
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
    pub fn needs_interrupt(&mut self, mem: &GuestMemory) -> bool {
        const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
        if self.packed {
            return self.needs_interrupt_packed(mem);
        }
        if !self.event_idx {
            return match mem.read_u16(self.avail_addr) {
                Ok(flags) => flags & VIRTQ_AVAIL_F_NO_INTERRUPT == 0,
                Err(_) => true,
            };
        }
        // The driver names the used index it is waiting for. An interrupt is
        // owed only if that index falls in what we have added since the last
        // one — which is a wrapping-interval question, not a comparison, and
        // writing it as a comparison is how devices end up either silent or
        // interrupting on every single request.
        let Ok(wanted) = mem.read_u16(self.used_event_addr()) else {
            return true;
        };
        let owed = need_event(wanted, self.next_used, self.signalled_used);
        if owed {
            self.signalled_used = self.next_used;
        }
        owed
    }

    /// The packed layout's version of the same question.
    ///
    /// The driver writes a flag word rather than only an index: never tell me,
    /// always tell me, or tell me when you reach this descriptor. The third is
    /// what Linux uses once the event index is negotiated, and it is the same
    /// wrapping-interval test as the split ring — over ring *positions* this
    /// time, with the event position pulled back a full ring when the driver's
    /// lap counter and ours disagree.
    fn needs_interrupt_packed(&mut self, mem: &GuestMemory) -> bool {
        let Ok(off_wrap) = mem.read_u16(self.avail_addr) else {
            return true;
        };
        let Ok(flags) = mem.read_u16(self.avail_addr + 2) else {
            return true;
        };
        match flags {
            event::DISABLE => return false,
            event::DESC => {}
            // ENABLE, and anything a confused driver writes: telling it more
            // often than it asked is wasteful, telling it less is a hang.
            _ => return true,
        }

        let old = self.signalled_used;
        let new = self.next_used;
        self.signalled_used = new;

        let wrap = off_wrap >> event::WRAP_SHIFT != 0;
        let mut wanted = off_wrap & !(1 << event::WRAP_SHIFT);
        if wrap != self.used_wrap {
            wanted = wanted.wrapping_sub(self.size);
        }
        need_event(wanted, new, old)
    }

    /// Tells the driver we want to hear about the next thing it publishes.
    ///
    /// Only meaningful with the event index, and then it is not optional: the
    /// field starts at zero, and a driver comparing against zero stops kicking
    /// the moment its available index has moved a little way past it. A device
    /// that negotiates the feature and never writes this goes quiet after a
    /// few hundred requests, which looks exactly like a lost wake-up and is
    /// not one.
    pub fn arm_notifications(&mut self, mem: &GuestMemory) {
        if self.used_addr == 0 {
            return;
        }
        // A packed ring's request to be kicked is a flag word, and it does not
        // go stale as the cursor moves — so unlike the split layout there is
        // nothing to re-publish, and the suppression call is the only writer.
        if self.packed || !self.event_idx {
            return;
        }
        self.publish_avail_event(mem);
    }

    /// Writes the index we want to be kicked at, if it has moved.
    ///
    /// Naming the index we have already consumed asks to be told about the
    /// very next one. Naming something half a ring away asks not to be told at
    /// all: the driver's test is a wrapping interval, and a point that far
    /// ahead is behind it from every direction that matters.
    fn publish_avail_event(&mut self, mem: &GuestMemory) {
        let want = if self.suppressed {
            self.next_avail.wrapping_add(1 << 15)
        } else {
            self.next_avail
        };
        if self.published_event == Some(want) {
            return;
        }
        let _ = mem.write_u16(self.avail_event_addr(), want);
        // The driver must see this before we decide it is not going to kick.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        self.published_event = Some(want);
    }
}

/// The `vring_need_event` of the specification, which is one line and worth
/// spelling out because it is not the comparison it looks like.
///
/// The question is whether `wanted` lies in the half-open interval
/// `(last, now]`, on a ring of indices that wraps at 65,536. Written with `<`
/// or `>=` on the raw values it is wrong the moment the index wraps, and a
/// wrap is a few thousand requests away on a busy queue.
#[inline]
const fn need_event(wanted: u16, now: u16, last: u16) -> bool {
    now.wrapping_sub(wanted).wrapping_sub(1) < now.wrapping_sub(last)
}

/// Walks one descriptor chain.
///
/// Iteration is bounded by the queue size: a guest that builds a cycle gets a
/// truncated chain and a warning, not a hung vCPU thread.
/// One entry of a packed ring: `addr`, `len`, `id`, `flags`.
///
/// The same sixteen bytes as a split descriptor, arranged differently: where
/// the split layout spends two bytes on `next`, this spends them on the buffer
/// id, and availability moves into the flags.
#[derive(Debug, Clone, Copy)]
struct PackedDesc {
    addr: u64,
    len: u32,
    id: u16,
    flags: u16,
}

/// Which layout a chain is walking.
///
/// The two differ in what "the next descriptor" means: a split chain follows
/// the `next` field, a packed chain simply reads on until one without `NEXT`.
/// Everything after that is the same, which is why devices never have to know.
enum Walk {
    Split {
        next: Option<u16>,
        indirect: bool,
        outer_next: Option<u16>,
        outer_table: u64,
        outer_len: u16,
    },
    Packed {
        /// Where in the ring the chain starts.
        start: u16,
        /// How many entries it occupies there.
        slots: u16,
        /// How many have been yielded.
        taken: u16,
    },
    /// A packed chain whose one ring entry pointed at a table.
    PackedIndirect { taken: u16 },
}

pub struct DescriptorChain<'m> {
    mem: &'m GuestMemory,
    /// Table currently being walked — the queue's own, or an indirect one.
    table: u64,
    /// Number of descriptors in `table`.
    table_len: u16,
    /// What the driver gets back when the chain is returned: the first
    /// descriptor's index in a split ring, the buffer id in a packed one.
    head: u16,
    /// Descriptors yielded so far, to bound the walk.
    walked: u32,
    walk: Walk,
}

impl<'m> DescriptorChain<'m> {
    fn new(mem: &'m GuestMemory, table: u64, table_len: u16, head: u16) -> DescriptorChain<'m> {
        DescriptorChain {
            mem,
            table,
            table_len,
            head,
            walked: 0,
            walk: Walk::Split {
                next: Some(head),
                indirect: false,
                outer_next: None,
                outer_table: 0,
                outer_len: 0,
            },
        }
    }

    /// A chain of consecutive packed ring entries.
    ///
    /// `start` and `slots` come from the pass the queue already made to find
    /// the buffer id, so this walk is a read of known extent rather than a
    /// second search.
    fn packed(
        mem: &'m GuestMemory,
        table: u64,
        table_len: u16,
        start: u16,
        slots: u16,
        id: u16,
    ) -> DescriptorChain<'m> {
        DescriptorChain {
            mem,
            table,
            table_len,
            head: id,
            walked: 0,
            walk: Walk::Packed {
                start,
                slots,
                taken: 0,
            },
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
        // One bound over both layouts, because a chain may legitimately span
        // an indirect table larger than the queue: cap total work regardless.
        if self.walked > u32::from(MAX_QUEUE_SIZE) * 2 {
            tracing::warn!("descriptor chain exceeded its bound; treating as ended");
            return None;
        }
        match self.walk {
            Walk::Split { .. } => self.next_split(),
            Walk::Packed { .. } | Walk::PackedIndirect { .. } => self.next_packed(),
        }
    }
}

impl DescriptorChain<'_> {
    fn next_split(&mut self) -> Option<Descriptor> {
        loop {
            if self.walked > u32::from(MAX_QUEUE_SIZE) * 2 {
                return None;
            }
            let Walk::Split { next, indirect, .. } = self.walk else {
                return None;
            };
            let index = next?;
            if index >= self.table_len {
                tracing::warn!(index, len = self.table_len, "descriptor index out of range");
                return None;
            }

            let desc = self.read_descriptor(self.table, index)?;
            self.walked += 1;

            if desc.is_indirect() && !indirect {
                // An indirect descriptor's buffer is itself a descriptor table.
                // Remember where to resume, then continue inside it.
                self.walk = Walk::Split {
                    next: Some(0),
                    indirect: true,
                    outer_next: desc.has_next().then_some(desc.next),
                    outer_table: self.table,
                    outer_len: self.table_len,
                };
                self.table = desc.addr;
                self.table_len =
                    (desc.len / DESC_SIZE as u32).min(u32::from(MAX_QUEUE_SIZE) * 2) as u16;
                continue;
            }

            let Walk::Split {
                indirect,
                outer_next,
                outer_table,
                outer_len,
                ..
            } = self.walk
            else {
                return None;
            };
            if desc.has_next() {
                self.walk = Walk::Split {
                    next: Some(desc.next),
                    indirect,
                    outer_next,
                    outer_table,
                    outer_len,
                };
            } else if indirect {
                // The indirect table ended; resume the outer chain if it had
                // more, which a well-formed driver rarely does but is legal.
                self.table = outer_table;
                self.table_len = outer_len;
                self.walk = Walk::Split {
                    next: outer_next,
                    indirect: false,
                    outer_next: None,
                    outer_table: 0,
                    outer_len: 0,
                };
            } else {
                self.walk = Walk::Split {
                    next: None,
                    indirect,
                    outer_next,
                    outer_table,
                    outer_len,
                };
            }
            return Some(desc);
        }
    }

    /// The packed walk: consecutive ring entries, and no `next` to follow.
    ///
    /// The one shape that is not consecutive is the indirect table, which a
    /// packed ring reaches through a single entry whose length says how many
    /// descriptors are on the other side. Those do not chain either — the
    /// table *is* the chain — so both cases are a count and a cursor.
    fn next_packed(&mut self) -> Option<Descriptor> {
        loop {
            match self.walk {
                Walk::Packed {
                    start,
                    slots,
                    taken,
                } => {
                    if taken >= slots {
                        return None;
                    }
                    let index = (start.wrapping_add(taken)) % self.table_len.max(1);
                    let raw = self.read_packed(index)?;
                    self.walked += 1;

                    if raw.flags & VIRTQ_DESC_F_INDIRECT != 0 {
                        self.table = raw.addr;
                        self.table_len =
                            (raw.len / DESC_SIZE as u32).min(u32::from(MAX_QUEUE_SIZE) * 2) as u16;
                        self.walk = Walk::PackedIndirect { taken: 0 };
                        continue;
                    }

                    self.walk = Walk::Packed {
                        start,
                        slots,
                        taken: taken + 1,
                    };
                    return Some(Descriptor {
                        addr: raw.addr,
                        len: raw.len,
                        flags: raw.flags & (VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE),
                        next: 0,
                    });
                }
                Walk::PackedIndirect { taken } => {
                    if taken >= self.table_len {
                        return None;
                    }
                    let raw = self.read_packed(taken)?;
                    self.walked += 1;
                    self.walk = Walk::PackedIndirect { taken: taken + 1 };
                    return Some(Descriptor {
                        addr: raw.addr,
                        len: raw.len,
                        flags: raw.flags & (VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE),
                        next: 0,
                    });
                }
                Walk::Split { .. } => return None,
            }
        }
    }

    fn read_packed(&self, index: u16) -> Option<PackedDesc> {
        let at = self.table + u64::from(index) * DESC_SIZE;
        Some(PackedDesc {
            addr: self.mem.read_u64(at).ok()?,
            len: self.mem.read_u32(at + 8).ok()?,
            id: self.mem.read_u16(at + 12).ok()?,
            flags: self.mem.read_u16(at + 14).ok()?,
        })
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
            // Offered and taken first. A head is outstanding from the pop that
            // takes it until the push that returns it, and returning one that
            // was never taken is exactly what the queue now refuses — so a
            // test that skipped the pop would be asserting on a path that no
            // longer exists.
            h.write_desc(
                3,
                Descriptor {
                    addr: DATA,
                    len: 512,
                    flags: VIRTQ_DESC_F_WRITE,
                    next: 0,
                },
            );
            h.make_available(0, 3, 1);
            assert!(h.queue.pop(&h.mem).is_some());
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

    /// `vring_need_event` is an interval question and reads like a
    /// comparison. Written as a comparison it is right for the first sixty-five
    /// thousand requests and wrong afterwards, which on a busy queue is a bug
    /// that appears minutes into a workload and nowhere in a test that counts
    /// to ten.
    #[test]
    fn the_event_check_is_an_interval_and_survives_the_wrap() {
        // `wanted` is the last index the driver has *seen*, so what it is
        // asking to hear about is `wanted + 1`. Reading it as "the index it
        // wants" is off by one and inverts two of these.
        assert!(!need_event(5, 4, 3), "we have not reached 6 yet");
        assert!(
            !need_event(5, 5, 3),
            "it has already seen 5; 6 does not exist"
        );
        assert!(
            need_event(5, 6, 3),
            "6 is new, and it is what was asked for"
        );
        assert!(need_event(5, 9, 3), "6 is inside (3, 9]");
        assert!(!need_event(5, 9, 6), "it was already told at 6");

        // The same, straddling the wrap at 2^16.
        assert!(
            need_event(u16::MAX - 1, u16::MAX, u16::MAX - 1),
            "65535 is new"
        );
        assert!(
            need_event(u16::MAX, 1, u16::MAX - 1),
            "0 is inside (65534, 1] once the index has wrapped"
        );
        assert!(!need_event(10, 1, u16::MAX - 1), "11 is nowhere near it");
    }

    /// Suppression under the event index is a written number, not a flag: the
    /// driver ignores the flag word entirely once the feature is on, so a
    /// device that keeps setting it is asking to be kicked for every request
    /// while believing it has asked for silence.
    /// A packed harness: the same three registers, meaning different things.
    /// `DESC` is the one ring, `AVAIL` is where the driver says whether it
    /// wants interrupts, `USED` is where we say whether we want kicks.
    impl Harness {
        fn packed(vm: Arc<Vm>, size: u16) -> Harness {
            let mut h = Harness::new(vm, size);
            h.queue.set_packed(true);
            h
        }

        /// Writes a packed ring entry as the driver would.
        fn write_packed(&self, index: u16, addr: u64, len: u32, id: u16, flags: u16) {
            let at = DESC + u64::from(index) * 16;
            self.mem.write_u64(at, addr).unwrap();
            self.mem.write_u32(at + 8, len).unwrap();
            self.mem.write_u16(at + 12, id).unwrap();
            self.mem.write_u16(at + 14, flags).unwrap();
        }

        fn packed_flags(&self, index: u16) -> u16 {
            self.mem
                .read_u16(DESC + u64::from(index) * 16 + 14)
                .unwrap()
        }
    }

    /// The driver marks a descriptor as its turn by setting the availability
    /// bit to its lap and the used bit to the opposite. Both halves matter:
    /// checking only the first reads a descriptor completed one lap ago as a
    /// fresh request.
    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn a_packed_chain_is_taken_with_its_buffer_id() {
        with_vm(|vm| {
            let mut h = Harness::packed(vm, 16);
            // Two descriptors, the second carrying the id, as a driver writes
            // a request-and-reply pair.
            h.write_packed(0, DATA, 64, 0, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_AVAIL);
            h.write_packed(1, DATA + 64, 32, 7, VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_AVAIL);

            let chain = h.queue.pop(&h.mem).expect("the chain should be available");
            assert_eq!(chain.head(), 7, "the id comes from the last descriptor");
            let seen: Vec<(u64, u32)> = chain.map(|d| (d.addr, d.len)).collect();
            assert_eq!(seen, vec![(DATA, 64), (DATA + 64, 32)]);

            assert!(
                h.queue.pop(&h.mem).is_none(),
                "there is nothing else available"
            );
        });
    }

    /// A descriptor left over from the previous lap has both bits set to the
    /// old counter, and must read as not ours.
    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn a_packed_descriptor_from_the_last_lap_is_not_available() {
        with_vm(|vm| {
            let mut h = Harness::packed(vm, 16);
            // What a completed descriptor looks like: both bits set.
            h.write_packed(0, DATA, 64, 3, VIRTQ_DESC_F_AVAIL | VIRTQ_DESC_F_USED);
            assert!(h.queue.pop(&h.mem).is_none(), "used, not available");
            assert!(!h.queue.has_work(&h.mem));

            // And what an untouched one looks like: neither.
            h.write_packed(0, DATA, 64, 3, 0);
            assert!(h.queue.pop(&h.mem).is_none(), "never made available");
        });
    }

    /// Completion writes the id and length, then publishes with both flag bits
    /// at the device's lap — and moves on by the chain's length, because the
    /// driver advances its own cursor by exactly that and the two have to stay
    /// in step.
    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn a_packed_completion_publishes_the_id_and_advances_by_the_chain() {
        with_vm(|vm| {
            let mut h = Harness::packed(vm, 16);
            h.write_packed(0, DATA, 64, 0, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_AVAIL);
            h.write_packed(1, DATA + 64, 32, 9, VIRTQ_DESC_F_AVAIL);
            assert!(h.queue.pop(&h.mem).is_some());

            h.queue.push_used(&h.mem, 9, 24);
            assert_eq!(h.mem.read_u16(DESC + 12).unwrap(), 9, "id");
            assert_eq!(h.mem.read_u32(DESC + 8).unwrap(), 24, "bytes written");
            let flags = h.packed_flags(0);
            assert!(
                flags & VIRTQ_DESC_F_AVAIL != 0 && flags & VIRTQ_DESC_F_USED != 0,
                "used means both bits at the device's lap, not one"
            );

            // The next request lands two slots on, not one.
            h.write_packed(2, DATA, 8, 4, VIRTQ_DESC_F_AVAIL);
            let chain = h.queue.pop(&h.mem).expect("slot 2 is next");
            assert_eq!(chain.head(), 4);
        });
    }

    /// A packed indirect descriptor takes one ring slot and points at a table
    /// whose length says how many descriptors are on the other side — they do
    /// not chain, so a walk that looked for `NEXT` would yield exactly one.
    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn a_packed_indirect_table_expands_to_its_entries() {
        with_vm(|vm| {
            let mut h = Harness::packed(vm, 16);
            let table = DATA + 0x1000;
            for (i, len) in [16u32, 32, 48].iter().enumerate() {
                let at = table + i as u64 * 16;
                h.mem.write_u64(at, DATA + i as u64 * 256).unwrap();
                h.mem.write_u32(at + 8, *len).unwrap();
                h.mem.write_u16(at + 12, 0).unwrap();
                h.mem.write_u16(at + 14, 0).unwrap();
            }
            h.write_packed(
                0,
                table,
                3 * 16,
                5,
                VIRTQ_DESC_F_INDIRECT | VIRTQ_DESC_F_AVAIL,
            );

            let chain = h.queue.pop(&h.mem).expect("available");
            assert_eq!(chain.head(), 5, "the id is on the outer descriptor");
            let lens: Vec<u32> = chain.map(|d| d.len).collect();
            assert_eq!(lens, vec![16, 32, 48]);
        });
    }

    /// Returning a head twice is what breaks a driver's ring: Linux answers
    /// `id %u is not a head!`, marks the queue broken, and every submission
    /// after that fails with EIO — a filesystem that stops dead, several
    /// seconds and one subsystem away from the mistake. Stranding one request
    /// is recoverable; this is not.
    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn a_head_cannot_be_returned_twice() {
        with_vm(|vm| {
            let mut h = Harness::new(vm, 16);
            h.write_desc(
                2,
                Descriptor {
                    addr: DATA,
                    len: 8,
                    flags: VIRTQ_DESC_F_WRITE,
                    next: 0,
                },
            );
            h.make_available(0, 2, 1);
            assert!(h.queue.pop(&h.mem).is_some());

            h.queue.push_used(&h.mem, 2, 8);
            assert_eq!(h.mem.read_u16(USED + 2).unwrap(), 1, "the first is written");

            h.queue.push_used(&h.mem, 2, 8);
            assert_eq!(
                h.mem.read_u16(USED + 2).unwrap(),
                1,
                "the second must be refused rather than written"
            );
        });
    }

    #[test]
    #[ignore = "needs the hypervisor entitlement; run via `make test-hv`"]
    fn suppression_writes_the_event_index_when_it_is_negotiated() {
        with_vm(|vm| {
            let mut h = Harness::new(vm, 16);
            h.queue.set_event_idx(true);

            let avail_event = USED + 4 + 16 * 8;
            h.queue.suppress_notifications(&h.mem, false);
            assert_eq!(
                h.mem.read_u16(avail_event).unwrap(),
                h.queue.next_avail(),
                "asking to be told means naming the index we have reached"
            );

            h.queue.suppress_notifications(&h.mem, true);
            let quiet = h.mem.read_u16(avail_event).unwrap();
            let next = h.queue.next_avail();
            assert_ne!(quiet, next);
            assert!(
                !need_event(quiet, next.wrapping_add(1), next),
                "a driver publishing the next index must not decide to kick"
            );

            // And the flag word is left alone, because it means nothing now.
            assert_eq!(h.mem.read_u16(USED).unwrap(), 0);
        });
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

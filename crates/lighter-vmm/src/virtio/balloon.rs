//! virtio-balloon, with free page reporting.
//!
//! This is the device that makes a lighter VM's memory footprint track what the
//! guest is actually using rather than what it was configured with. Two
//! mechanisms, and the second is the one that matters most in practice:
//!
//! **Ballooning** is host-driven. The host raises a target, the guest hands
//! back pages to reach it, and we release them. It responds to host memory
//! pressure but only as fast as the guest can free things.
//!
//! **Free page reporting** is guest-driven and continuous. The guest's own
//! allocator tells us about free runs as they appear, with no target and no
//! negotiation. This is what returns memory after a build finishes without
//! anyone asking, and it is why the guest kernel is built with
//! `CONFIG_PAGE_REPORTING`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::memory::GuestMemory;
use crate::virtio::mmio::COMMON_FEATURES;
use crate::virtio::queue::Virtqueue;
use crate::virtio::{Serviced, VirtioDevice, device_type};

/// The balloon counts in 4 KiB units regardless of the guest's page size.
pub const BALLOON_PAGE_SIZE: u64 = 4096;

// Queue indices, which depend on the transport and not only on the device.
//
// The balloon driver asks for five queues by name — inflate, deflate, stats,
// free-page-hint, reporting — and leaves the names of the ones its negotiated
// features do not call for as NULL. What happens next is where the two virtio
// transports part company. PCI honours the positions, so an unoffered stats
// queue leaves a hole and reporting stays at index four. **virtio-mmio
// compacts**: `vm_find_vqs` advances its index only for queues that have a
// name, so with stats and free-page-hint unoffered, reporting arrives as index
// two.
//
// We are mmio, so reporting is two. Assuming otherwise costs nothing visible:
// the guest kicks a queue the device is not watching, the driver blocks
// forever in `wait_event` waiting for a buffer that is never returned, and
// free page reporting stops after its very first attempt — with no error
// anywhere and a guest that holds every page it has ever touched.
const QUEUE_INFLATE: u16 = 0;
const QUEUE_DEFLATE: u16 = 1;
const QUEUE_REPORTING: u16 = 2;

// Feature bits.
const F_MUST_TELL_HOST: u64 = 1 << 0;
const F_DEFLATE_ON_OOM: u64 = 1 << 2;
const F_REPORTING: u64 = 1 << 5;

/// Shared, observable balloon state.
///
/// Separate from the device so the policy loop can read and steer it from
/// another thread without holding the device lock across a decision.
#[derive(Debug, Default)]
pub struct BalloonState {
    /// Pages the host wants the guest to give up.
    target_pages: AtomicU32,
    /// Pages the guest has actually given up.
    actual_pages: AtomicU32,
    /// Bytes released through free page reporting since boot.
    reported_bytes: std::sync::atomic::AtomicU64,
    /// Bytes the guest *offered*, whether or not the host took them.
    ///
    /// The two apart are what tells "the guest is not reporting" from "the
    /// guest reported and macOS would not take the pages back" — which look
    /// identical from outside and have nothing in common as problems.
    offered_bytes: std::sync::atomic::AtomicU64,
}

impl BalloonState {
    pub fn target_pages(&self) -> u32 {
        self.target_pages.load(Ordering::Relaxed)
    }

    pub fn set_target_pages(&self, pages: u32) {
        self.target_pages.store(pages, Ordering::Relaxed);
    }

    pub fn actual_pages(&self) -> u32 {
        self.actual_pages.load(Ordering::Relaxed)
    }

    pub fn reported_bytes(&self) -> u64 {
        self.reported_bytes.load(Ordering::Relaxed)
    }

    /// Bytes the guest has offered back, whether or not the host took them.
    pub fn offered_bytes(&self) -> u64 {
        self.offered_bytes.load(Ordering::Relaxed)
    }

    /// Bytes the guest is currently holding out of use on our behalf.
    pub fn ballooned_bytes(&self) -> u64 {
        u64::from(self.actual_pages()) * BALLOON_PAGE_SIZE
    }
}

/// The balloon device.
pub struct Balloon {
    state: Arc<BalloonState>,
    acked: u64,
}

impl Balloon {
    pub fn new(state: Arc<BalloonState>) -> Balloon {
        Balloon { state, acked: 0 }
    }

    /// Reads a queue of 4-byte page frame numbers and releases what they cover.
    ///
    /// The PFNs arrive in no particular order, so they are sorted and coalesced
    /// before release: individual 4 KiB pages are never releasable on a 16 KiB
    /// host, but contiguous runs of four or more are. Skipping this step is the
    /// difference between a balloon that frees most of what it inflates and one
    /// that frees nothing at all.
    fn drain_pfn_queue(
        &mut self,
        queue: &mut Virtqueue,
        mem: &GuestMemory,
        inflating: bool,
    ) -> bool {
        let mut used_any = false;

        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let mut pfns: Vec<u32> = Vec::new();

            for desc in chain {
                if desc.is_write_only() {
                    continue;
                }
                let count = desc.len / 4;
                for i in 0..u64::from(count) {
                    if let Ok(pfn) = mem.read_u32(desc.addr + i * 4) {
                        pfns.push(pfn);
                    }
                }
            }

            if inflating {
                let released = self.release_runs(&pfns, mem);
                self.state
                    .actual_pages
                    .fetch_add(pfns.len() as u32, Ordering::Relaxed);
                if released > 0 {
                    tracing::debug!(
                        pages = pfns.len(),
                        released_kib = released / 1024,
                        "balloon inflated"
                    );
                }
            } else {
                // Deflation needs no host action: the pages were never unmapped,
                // so the guest touching them faults in fresh zeroes. All we do
                // is stop counting them as ours.
                self.state.actual_pages.fetch_sub(
                    (pfns.len() as u32).min(self.state.actual_pages()),
                    Ordering::Relaxed,
                );
            }

            queue.push_used(mem, head, 0);
            used_any = true;
        }
        used_any
    }

    /// Sorts and coalesces page frame numbers, releasing each contiguous run.
    fn release_runs(&self, pfns: &[u32], mem: &GuestMemory) -> u64 {
        if pfns.is_empty() {
            return 0;
        }
        let mut sorted: Vec<u32> = pfns.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        let mut released = 0u64;
        let mut run_start = sorted[0];
        let mut run_end = sorted[0];

        for &pfn in &sorted[1..] {
            if pfn == run_end + 1 {
                run_end = pfn;
                continue;
            }
            released += self.release_run(mem, run_start, run_end);
            run_start = pfn;
            run_end = pfn;
        }
        released + self.release_run(mem, run_start, run_end)
    }

    fn release_run(&self, mem: &GuestMemory, first: u32, last: u32) -> u64 {
        let gpa = u64::from(first) * BALLOON_PAGE_SIZE;
        let len = (u64::from(last) - u64::from(first) + 1) * BALLOON_PAGE_SIZE;
        mem.release(gpa, len).unwrap_or(0)
    }

    /// Handles the free page reporting queue.
    ///
    /// Unlike inflation, the buffers here describe memory the guest still owns
    /// and will keep using; it is simply telling us the contents are worthless
    /// right now. We release the pages and return the buffers immediately.
    fn drain_reporting_queue(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let mut released = 0u64;
            let mut offered = 0u64;
            for desc in chain {
                offered += u64::from(desc.len);
                released += mem.release(desc.addr, u64::from(desc.len)).unwrap_or(0);
            }
            self.state
                .offered_bytes
                .fetch_add(offered, Ordering::Relaxed);
            if released > 0 {
                self.state
                    .reported_bytes
                    .fetch_add(released, Ordering::Relaxed);
                tracing::trace!(released_kib = released / 1024, "free pages reported");
            }
            queue.push_used(mem, head, 0);
            used_any = true;
        }
        used_any
    }
}

impl VirtioDevice for Balloon {
    fn device_type(&self) -> u32 {
        device_type::BALLOON
    }

    fn name(&self) -> &'static str {
        "virtio-balloon"
    }

    fn features(&self) -> u64 {
        // DEFLATE_ON_OOM matters for a container host: a guest under memory
        // pressure may take pages back rather than OOM-killing a build.
        COMMON_FEATURES | F_REPORTING | F_DEFLATE_ON_OOM | F_MUST_TELL_HOST
    }

    fn ack_features(&mut self, features: u64) {
        self.acked = features;
    }

    fn queue_count(&self) -> usize {
        // Three: inflate, deflate and reporting, in the order this transport
        // hands them out. See the note on the constants — under mmio there is
        // no gap for the stats queue we do not offer.
        3
    }

    /// `struct virtio_balloon_config`: the target, then what the guest achieved.
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; 8];
        config[0..4].copy_from_slice(&self.state.target_pages().to_le_bytes());
        config[4..8].copy_from_slice(&self.state.actual_pages().to_le_bytes());
        let start = offset as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(start + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        // Indexed by the queue that was notified, never by a number written out
        // a second time. Writing it twice is how this came to match on
        // `QUEUE_REPORTING` and then drain a different queue entirely — which
        // costs nothing visible, because the queue it drained was one the guest
        // never makes ready, so the reporting simply never happened.
        let Some(ring) = queues.get_mut(queue as usize) else {
            tracing::debug!(queue, "balloon notified on a queue it does not have");
            return Serviced::NONE;
        };
        let used_any = match queue {
            QUEUE_INFLATE => self.drain_pfn_queue_at(ring, mem, true),
            QUEUE_DEFLATE => self.drain_pfn_queue_at(ring, mem, false),
            QUEUE_REPORTING => self.drain_reporting_queue_at(ring, mem),
            other => {
                tracing::debug!(queue = other, "balloon notified on an unused queue");
                false
            }
        };
        Serviced::queue_if(queue, used_any)
    }

    fn reset(&mut self) {
        self.acked = 0;
        self.state.actual_pages.store(0, Ordering::Relaxed);
        self.state.target_pages.store(0, Ordering::Relaxed);
    }
}

// Small shims so `notify` can borrow one queue mutably without borrowing self
// twice; the bodies are the methods above.
impl Balloon {
    fn drain_pfn_queue_at(
        &mut self,
        queue: &mut Virtqueue,
        mem: &GuestMemory,
        inflating: bool,
    ) -> bool {
        self.drain_pfn_queue(queue, mem, inflating)
    }

    fn drain_reporting_queue_at(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        self.drain_reporting_queue(queue, mem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_free_page_reporting() {
        let balloon = Balloon::new(Arc::new(BalloonState::default()));
        assert_ne!(
            balloon.features() & F_REPORTING,
            0,
            "reporting is the mechanism that returns memory without being asked"
        );
        assert_ne!(balloon.features() & F_DEFLATE_ON_OOM, 0);
    }

    #[test]
    fn config_reports_target_and_actual() {
        let state = Arc::new(BalloonState::default());
        state.set_target_pages(1024);
        state.actual_pages.store(512, Ordering::Relaxed);
        let balloon = Balloon::new(state);

        let mut config = [0u8; 8];
        balloon.config_read(0, &mut config);
        assert_eq!(u32::from_le_bytes(config[0..4].try_into().unwrap()), 1024);
        assert_eq!(u32::from_le_bytes(config[4..8].try_into().unwrap()), 512);
    }

    #[test]
    fn ballooned_bytes_counts_in_four_kib_pages() {
        let state = BalloonState::default();
        state.actual_pages.store(256, Ordering::Relaxed);
        assert_eq!(state.ballooned_bytes(), 256 * 4096);
    }

    /// Coalescing is what makes release possible at all on a 16 KiB host, so
    /// the run-finding must merge adjacent PFNs and split at gaps.
    #[test]
    fn page_frames_coalesce_into_contiguous_runs() {
        let balloon = Balloon::new(Arc::new(BalloonState::default()));
        let mem = GuestMemory::detached();
        // No regions, so nothing is actually released — the point here is that
        // the run arithmetic does not panic and terminates.
        assert_eq!(balloon.release_runs(&[], &mem), 0);
        assert_eq!(balloon.release_runs(&[5, 4, 6, 100, 101], &mem), 0);
    }
}

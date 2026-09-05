//! virtio-mem: memory the guest plugs in, not memory it is lent.
//!
//! The balloon lends pages back to the host, and it works for the pages; but
//! the guest kernel built its page array for every byte it was given at boot,
//! 1.5% of the configured RAM (190 MiB of a 12 GiB guest's 650 MB idle
//! footprint on an M5), and no amount of lending returns that. virtio-mem is
//! the other model: the guest boots with a base of RAM and a range beside it
//! that is empty, the host says how much of the range it may use
//! (`requested_size`), and the guest plugs blocks in and out to match. Linux
//! onlines a plugged block as ordinary memory and offlines an unplugged one,
//! and with `MHP_MEMMAP_ON_MEMORY` a block's page array lives inside the
//! block and leaves with it. What is not plugged costs the host nothing.
//!
//! One block is 128 MiB, the same as an arm64 Linux memory block with 4 KiB
//! pages, so the driver runs in its big-block mode with one device block per
//! Linux block and no partial blocks to reason about. The range is mapped
//! into the guest whole and lazily (`MAP_NORESERVE`, the same as RAM); a
//! plugged block is one the guest may touch, an unplugged one is released
//! with the balloon's `MADV_FREE_REUSABLE` path and, with
//! `VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE` offered, one the guest has promised
//! not to touch.
//!
//! The host decides the size (`MemState::set_requested_bytes`, then the
//! transport's configuration-change interrupt); the guest only ever asks to
//! plug or unplug within it. Who decides, and from what, is the memory
//! policy's business, not this file's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::mmio::{COMMON_FEATURES, VirtioMmio};
use super::queue::Virtqueue;
use super::{Serviced, VirtioDevice, device_type};
use crate::memory::GuestMemory;

/// One device block: an arm64 Linux memory block.
pub const BLOCK_SIZE: u64 = 128 << 20;

/// The guest never touches unplugged memory, so the host may release it.
const F_UNPLUGGED_INACCESSIBLE: u64 = 1 << 1;

const REQ_PLUG: u16 = 0;
const REQ_UNPLUG: u16 = 1;
const REQ_UNPLUG_ALL: u16 = 2;
const REQ_STATE: u16 = 3;

const RESP_ACK: u16 = 0;
const RESP_NACK: u16 = 1;
const RESP_ERROR: u16 = 3;

const STATE_PLUGGED: u16 = 0;
const STATE_UNPLUGGED: u16 = 1;
const STATE_MIXED: u16 = 2;

/// `struct virtio_mem_req`: type, three words of padding, then the address
/// and the block count for every request kind, and three more words.
const REQ_LEN: usize = 24;
/// `struct virtio_mem_resp`: type, three words of padding, the state.
const RESP_LEN: usize = 10;

/// What the host and the device share about the range: where it is, how
/// much of it the host has offered, how much the guest holds.
#[derive(Debug)]
pub struct MemState {
    addr: u64,
    region: u64,
    requested: AtomicU64,
    plugged: AtomicU64,
    /// One flag per block, set while the guest holds it.
    blocks: Mutex<Vec<bool>>,
}

impl MemState {
    /// A range of `region` bytes at guest-physical `addr`, both multiples of
    /// the block size, with `requested` bytes offered to begin with.
    pub fn new(addr: u64, region: u64, requested: u64) -> MemState {
        debug_assert_eq!(addr % BLOCK_SIZE, 0);
        debug_assert_eq!(region % BLOCK_SIZE, 0);
        MemState {
            addr,
            region,
            requested: AtomicU64::new(Self::round(requested.min(region))),
            plugged: AtomicU64::new(0),
            blocks: Mutex::new(vec![false; (region / BLOCK_SIZE) as usize]),
        }
    }

    fn round(bytes: u64) -> u64 {
        bytes / BLOCK_SIZE * BLOCK_SIZE
    }

    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn region_bytes(&self) -> u64 {
        self.region
    }

    pub fn requested_bytes(&self) -> u64 {
        self.requested.load(Ordering::Relaxed)
    }

    pub fn plugged_bytes(&self) -> u64 {
        self.plugged.load(Ordering::Relaxed)
    }

    /// Offers `bytes` of the range, rounded down to whole blocks and capped
    /// at the range. The guest reads it when told the configuration changed,
    /// so the caller raises that on the device's transport.
    pub fn set_requested_bytes(&self, bytes: u64) -> u64 {
        let rounded = Self::round(bytes.min(self.region));
        self.requested.store(rounded, Ordering::Relaxed);
        rounded
    }

    /// The block index of `gpa`, if it is a block boundary inside the range.
    fn block_of(&self, gpa: u64) -> Option<usize> {
        if gpa < self.addr || gpa >= self.addr + self.region || !gpa.is_multiple_of(BLOCK_SIZE) {
            return None;
        }
        Some(((gpa - self.addr) / BLOCK_SIZE) as usize)
    }
}

/// The host's handle on the range: the state, and the transport that tells
/// the guest when the offer changed.
#[derive(Clone)]
pub struct MemControl {
    state: Arc<MemState>,
    transport: Arc<Mutex<VirtioMmio>>,
}

impl MemControl {
    pub fn new(state: Arc<MemState>, transport: Arc<Mutex<VirtioMmio>>) -> MemControl {
        MemControl { state, transport }
    }

    pub fn state(&self) -> &MemState {
        &self.state
    }

    /// Offers `bytes` of the range and tells the guest. Returns what was
    /// offered after rounding; the guest plugs or unplugs towards it in its
    /// own time, and `plugged_bytes` says how far it has got.
    pub fn request(&self, bytes: u64) -> u64 {
        let rounded = self.state.set_requested_bytes(bytes);
        self.transport
            .lock()
            .expect("virtio-mem transport poisoned")
            .notify_config_change();
        tracing::info!(
            requested_mib = rounded >> 20,
            plugged_mib = self.state.plugged_bytes() >> 20,
            "virtio-mem offer"
        );
        rounded
    }
}

/// The device: one queue of requests, a configuration space, and the range.
pub struct Mem {
    state: Arc<MemState>,
}

impl Mem {
    pub fn new(state: Arc<MemState>) -> Mem {
        Mem { state }
    }

    /// Handles one request, returning the response's type and state.
    fn handle(&mut self, req: &[u8; REQ_LEN], mem: &GuestMemory) -> (u16, u16) {
        let kind = u16::from_le_bytes([req[0], req[1]]);
        let addr = u64::from_le_bytes(req[8..16].try_into().expect("8 bytes"));
        let count = u16::from_le_bytes([req[16], req[17]]) as usize;
        let Some(first) = self.state.block_of(addr) else {
            tracing::debug!(kind, addr, count, "virtio-mem: request outside the range");
            return (RESP_ERROR, 0);
        };
        let mut blocks = self
            .state
            .blocks
            .lock()
            .expect("virtio-mem blocks poisoned");
        if kind != REQ_UNPLUG_ALL && (count == 0 || first + count > blocks.len()) {
            tracing::debug!(kind, addr, count, "virtio-mem: request past the range");
            return (RESP_ERROR, 0);
        }
        let span = first..first + count;
        match kind {
            REQ_PLUG => {
                if blocks[span.clone()].iter().any(|&b| b) {
                    return (RESP_NACK, 0);
                }
                let plugged = self.state.plugged_bytes();
                let wanted = count as u64 * BLOCK_SIZE;
                if plugged + wanted > self.state.requested_bytes() {
                    return (RESP_NACK, 0);
                }
                blocks[span].fill(true);
                self.state.plugged.fetch_add(wanted, Ordering::Relaxed);
                (RESP_ACK, 0)
            }
            REQ_UNPLUG => {
                if blocks[span.clone()].iter().any(|&b| !b) {
                    return (RESP_NACK, 0);
                }
                blocks[span].fill(false);
                self.state
                    .plugged
                    .fetch_sub(count as u64 * BLOCK_SIZE, Ordering::Relaxed);
                let _ = mem.release(addr, count as u64 * BLOCK_SIZE);
                (RESP_ACK, 0)
            }
            REQ_UNPLUG_ALL => {
                blocks.fill(false);
                self.state.plugged.store(0, Ordering::Relaxed);
                let _ = mem.release(self.state.addr, self.state.region);
                (RESP_ACK, 0)
            }
            REQ_STATE => {
                let held = blocks[span].iter().filter(|&&b| b).count();
                let state = if held == 0 {
                    STATE_UNPLUGGED
                } else if held == count {
                    STATE_PLUGGED
                } else {
                    STATE_MIXED
                };
                (RESP_ACK, state)
            }
            other => {
                tracing::debug!(kind = other, "virtio-mem: unknown request");
                (RESP_ERROR, 0)
            }
        }
    }

    fn drain(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let mut request = [0u8; REQ_LEN];
            let mut have = 0usize;
            let mut response_at: Option<(u64, u32)> = None;
            for desc in chain {
                if desc.is_write_only() {
                    if response_at.is_none() {
                        response_at = Some((desc.addr, desc.len));
                    }
                    continue;
                }
                let take = (desc.len as usize).min(REQ_LEN - have);
                if take > 0 && mem.read(desc.addr, &mut request[have..have + take]).is_ok() {
                    have += take;
                }
            }
            let (kind, state) = if have == REQ_LEN {
                self.handle(&request, mem)
            } else {
                (RESP_ERROR, 0)
            };
            let mut written = 0u32;
            if let Some((addr, len)) = response_at {
                let mut resp = [0u8; RESP_LEN];
                resp[..2].copy_from_slice(&kind.to_le_bytes());
                resp[8..10].copy_from_slice(&state.to_le_bytes());
                let n = (len as usize).min(RESP_LEN);
                if mem.write(addr, &resp[..n]).is_ok() {
                    written = n as u32;
                }
            }
            queue.push_used(mem, head, written);
            used_any = true;
        }
        used_any
    }
}

impl VirtioDevice for Mem {
    fn device_type(&self) -> u32 {
        device_type::MEM
    }

    fn name(&self) -> &'static str {
        "mem"
    }

    fn features(&self) -> u64 {
        COMMON_FEATURES | F_UNPLUGGED_INACCESSIBLE
    }

    fn queue_count(&self) -> usize {
        1
    }

    /// `struct virtio_mem_config`: block size, node, padding, then the
    /// range's address and size, the usable size (the whole range here), what
    /// is plugged, and what is offered.
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; 56];
        config[0..8].copy_from_slice(&BLOCK_SIZE.to_le_bytes());
        config[16..24].copy_from_slice(&self.state.addr.to_le_bytes());
        config[24..32].copy_from_slice(&self.state.region.to_le_bytes());
        config[32..40].copy_from_slice(&self.state.region.to_le_bytes());
        config[40..48].copy_from_slice(&self.state.plugged_bytes().to_le_bytes());
        config[48..56].copy_from_slice(&self.state.requested_bytes().to_le_bytes());
        let start = offset as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(start + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        let Some(ring) = queues.get_mut(queue as usize) else {
            return Serviced::NONE;
        };
        let used_any = self.drain(ring, mem);
        Serviced::queue_if(queue, used_any)
    }

    /// A reset unplugs everything: the driver that comes back starts from
    /// nothing plugged, and so must the range on the host.
    fn reset(&mut self) {
        let mut blocks = self
            .state
            .blocks
            .lock()
            .expect("virtio-mem blocks poisoned");
        blocks.fill(false);
        self.state.plugged.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_is_whole_blocks_within_the_range() {
        let state = MemState::new(1 << 30, 4 * BLOCK_SIZE, 3 * BLOCK_SIZE + 1);
        assert_eq!(state.requested_bytes(), 3 * BLOCK_SIZE);
        assert_eq!(state.set_requested_bytes(u64::MAX), 4 * BLOCK_SIZE);
        assert_eq!(state.set_requested_bytes(0), 0);
    }

    #[test]
    fn plug_and_unplug_track_the_blocks() {
        let state = std::sync::Arc::new(MemState::new(1 << 30, 4 * BLOCK_SIZE, 2 * BLOCK_SIZE));
        let mut dev = Mem::new(state.clone());
        let mem = GuestMemory::detached();
        let req = |kind: u16, addr: u64, count: u16| {
            let mut r = [0u8; REQ_LEN];
            r[..2].copy_from_slice(&kind.to_le_bytes());
            r[8..16].copy_from_slice(&addr.to_le_bytes());
            r[16..18].copy_from_slice(&count.to_le_bytes());
            r
        };
        assert_eq!(dev.handle(&req(REQ_PLUG, 1 << 30, 2), &mem), (RESP_ACK, 0));
        assert_eq!(state.plugged_bytes(), 2 * BLOCK_SIZE);
        // Past what was offered.
        assert_eq!(
            dev.handle(&req(REQ_PLUG, (1 << 30) + 2 * BLOCK_SIZE, 1), &mem),
            (RESP_NACK, 0)
        );
        assert_eq!(
            dev.handle(&req(REQ_STATE, 1 << 30, 4), &mem),
            (RESP_ACK, STATE_MIXED)
        );
        assert_eq!(
            dev.handle(&req(REQ_STATE, 1 << 30, 2), &mem),
            (RESP_ACK, STATE_PLUGGED)
        );
        assert_eq!(
            dev.handle(&req(REQ_UNPLUG, 1 << 30, 1), &mem),
            (RESP_ACK, 0)
        );
        assert_eq!(state.plugged_bytes(), BLOCK_SIZE);
        // Outside the range, or off a block boundary.
        assert_eq!(
            dev.handle(&req(REQ_PLUG, 1 << 20, 1), &mem),
            (RESP_ERROR, 0)
        );
        assert_eq!(
            dev.handle(&req(REQ_PLUG, (1 << 30) + 4096, 1), &mem),
            (RESP_ERROR, 0)
        );
        assert_eq!(
            dev.handle(&req(REQ_UNPLUG_ALL, 1 << 30, 0), &mem),
            (RESP_ACK, 0)
        );
        assert_eq!(state.plugged_bytes(), 0);
    }
}

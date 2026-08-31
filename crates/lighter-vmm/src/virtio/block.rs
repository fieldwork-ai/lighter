//! virtio-blk.
//!
//! Each request is one descriptor chain of three parts: a header the device
//! reads, a data region whose direction depends on the request type, and a
//! one-byte status the device writes. The device's whole job is to honour that
//! shape without trusting it — a driver bug or a hostile guest can present a
//! chain with the header missing, the status buffer too short, or a data region
//! pointing anywhere at all.

use std::sync::Arc;

use crate::memory::GuestMemory;
use crate::virtio::disk::{Disk, SECTOR_SIZE};
use crate::virtio::mmio::COMMON_FEATURES;
use crate::virtio::queue::{Descriptor, Virtqueue};
use crate::virtio::{Serviced, VirtioDevice, device_type};

// Request types.
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const T_DISCARD: u32 = 11;
const T_WRITE_ZEROES: u32 = 13;

// Status codes written into the last byte of the chain.
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

// Feature bits.
const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;
const F_DISCARD: u64 = 1 << 13;
const F_WRITE_ZEROES: u64 = 1 << 14;

/// Bytes of request header: type, reserved, sector.
const HEADER_LEN: usize = 16;

/// Largest single transfer we will assemble, as a guard against a chain that
/// claims an absurd length.
const MAX_TRANSFER: usize = 4 << 20;

/// Sectors per discard request we advertise.
const MAX_DISCARD_SECTORS: u32 = 1 << 22;

/// The serial the guest reads via `GET_ID`, padded to the 20 bytes the
/// specification allocates.
const DEVICE_ID: &[u8] = b"lighter-blk";

/// A virtio block device over a sparse host file.
pub struct Block {
    disk: Arc<Disk>,
    read_only: bool,
    /// Features the driver accepted, which decides whether discard requests are
    /// legal at all.
    acked: u64,
}

impl Block {
    pub fn new(disk: Arc<Disk>) -> Block {
        let read_only = disk.is_read_only();
        Block {
            disk,
            read_only,
            acked: 0,
        }
    }

    /// Services every available request on the queue.
    fn process_queue(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let descriptors: Vec<Descriptor> = chain.collect();
            let (status, written) = self.execute(&descriptors, mem);

            // The status byte lives in the last device-writable descriptor.
            // Writing it is the only part that must happen even when everything
            // else failed, or the driver waits forever on a request that is
            // never completed.
            let total = match descriptors.last() {
                Some(last) if last.is_write_only() && last.len >= 1 => {
                    let _ = mem.write(last.addr, &[status]);
                    written + 1
                }
                _ => {
                    tracing::warn!("virtio-blk request had no status descriptor");
                    written
                }
            };

            queue.push_used(mem, head, total);
            used_any = true;
        }
        used_any
    }

    /// Carries out one request, returning its status and how many bytes were
    /// written into device-writable buffers.
    fn execute(&mut self, descriptors: &[Descriptor], mem: &GuestMemory) -> (u8, u32) {
        // Minimum viable chain: header, and a status byte.
        let Some(header) = descriptors.first() else {
            return (S_IOERR, 0);
        };
        if header.is_write_only() || (header.len as usize) < HEADER_LEN {
            tracing::warn!("virtio-blk request header is malformed");
            return (S_IOERR, 0);
        }

        let Ok(request_type) = mem.read_u32(header.addr) else {
            return (S_IOERR, 0);
        };
        let Ok(sector) = mem.read_u64(header.addr + 8) else {
            return (S_IOERR, 0);
        };

        // Everything between the header and the trailing status byte.
        let body = &descriptors[1..descriptors.len().saturating_sub(1)];

        match request_type {
            T_IN => self.read(sector, body, mem),
            T_OUT => (self.write(sector, body, mem), 0),
            T_FLUSH => (
                match self.disk.flush() {
                    Ok(()) => S_OK,
                    Err(e) => {
                        tracing::error!(%e, "virtio-blk flush failed");
                        S_IOERR
                    }
                },
                0,
            ),
            T_GET_ID => self.get_id(body, mem),
            T_DISCARD => (self.discard(body, mem, true), 0),
            T_WRITE_ZEROES => (self.discard(body, mem, false), 0),
            other => {
                tracing::debug!(request_type = other, "unsupported virtio-blk request");
                (S_UNSUPP, 0)
            }
        }
    }

    fn read(&self, sector: u64, body: &[Descriptor], mem: &GuestMemory) -> (u8, u32) {
        let mut offset = sector * SECTOR_SIZE;
        let mut written = 0u32;
        for desc in body {
            if !desc.is_write_only() {
                // A read request whose data region the device may not write is
                // malformed; serving it would write nowhere useful.
                return (S_IOERR, written);
            }
            let len = desc.len as usize;
            if len > MAX_TRANSFER {
                return (S_IOERR, written);
            }
            let mut buf = vec![0u8; len];
            if self.disk.read_at(offset, &mut buf).is_err() {
                return (S_IOERR, written);
            }
            if mem.write(desc.addr, &buf).is_err() {
                return (S_IOERR, written);
            }
            offset += len as u64;
            written += desc.len;
        }
        (S_OK, written)
    }

    fn write(&self, sector: u64, body: &[Descriptor], mem: &GuestMemory) -> u8 {
        if self.read_only {
            return S_IOERR;
        }
        let mut offset = sector * SECTOR_SIZE;
        for desc in body {
            if desc.is_write_only() {
                return S_IOERR;
            }
            let len = desc.len as usize;
            if len > MAX_TRANSFER {
                return S_IOERR;
            }
            let mut buf = vec![0u8; len];
            if mem.read(desc.addr, &mut buf).is_err() {
                return S_IOERR;
            }
            if self.disk.write_at(offset, &buf).is_err() {
                return S_IOERR;
            }
            offset += len as u64;
        }
        S_OK
    }

    fn get_id(&self, body: &[Descriptor], mem: &GuestMemory) -> (u8, u32) {
        let Some(target) = body.first() else {
            return (S_IOERR, 0);
        };
        if !target.is_write_only() {
            return (S_IOERR, 0);
        }
        let len = (target.len as usize).min(20);
        let mut id = [0u8; 20];
        let n = DEVICE_ID.len().min(len);
        id[..n].copy_from_slice(&DEVICE_ID[..n]);
        if mem.write(target.addr, &id[..len]).is_err() {
            return (S_IOERR, 0);
        }
        (S_OK, len as u32)
    }

    /// Handles `DISCARD` and `WRITE_ZEROES`, which share a payload shape.
    ///
    /// This is the path that makes the disk shrink: a discard becomes a hole
    /// punch, and the host gets its blocks back.
    fn discard(&self, body: &[Descriptor], mem: &GuestMemory, unmap: bool) -> u8 {
        let feature = if unmap { F_DISCARD } else { F_WRITE_ZEROES };
        if self.acked & feature == 0 {
            // The driver never negotiated this, so it should not be asking.
            return S_UNSUPP;
        }
        if self.read_only {
            return S_IOERR;
        }

        for desc in body {
            if desc.is_write_only() {
                return S_IOERR;
            }
            // The payload is an array of 16-byte segments.
            let count = desc.len / 16;
            for i in 0..u64::from(count) {
                let base = desc.addr + i * 16;
                let (Ok(sector), Ok(num_sectors), Ok(flags)) = (
                    mem.read_u64(base),
                    mem.read_u32(base + 8),
                    mem.read_u32(base + 12),
                ) else {
                    return S_IOERR;
                };

                let offset = sector * SECTOR_SIZE;
                let len = u64::from(num_sectors) * SECTOR_SIZE;
                if len == 0 {
                    continue;
                }

                let result = if unmap {
                    self.disk.punch_hole(offset, len)
                } else {
                    // Bit 0 of flags is `unmap`: the guest saying it does not
                    // mind whether the blocks stay allocated.
                    self.disk.write_zeroes(offset, len, flags & 1 != 0)
                };
                if let Err(e) = result {
                    tracing::warn!(%e, offset, len, "virtio-blk discard failed");
                    return S_IOERR;
                }
            }
        }
        S_OK
    }
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        device_type::BLOCK
    }

    fn name(&self) -> &'static str {
        "virtio-blk"
    }

    fn features(&self) -> u64 {
        let mut features =
            COMMON_FEATURES | F_SEG_MAX | F_BLK_SIZE | F_FLUSH | F_DISCARD | F_WRITE_ZEROES;
        if self.read_only {
            features |= F_RO;
        }
        features
    }

    fn ack_features(&mut self, features: u64) {
        self.acked = features;
    }

    fn queue_count(&self) -> usize {
        1
    }

    /// Configuration space, laid out as `struct virtio_blk_config`.
    ///
    /// Built as a fixed buffer and sliced, rather than matched offset by
    /// offset, because the driver reads it at arbitrary widths and alignments.
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; 60];
        config[0..8].copy_from_slice(&self.disk.capacity_sectors().to_le_bytes());
        // seg_max: one below the queue size, so a full chain always leaves room
        // for the header and status descriptors.
        config[12..16]
            .copy_from_slice(&(crate::virtio::queue::MAX_QUEUE_SIZE as u32 - 2).to_le_bytes());
        // blk_size: 512, matching the sector size we report capacity in.
        config[20..24].copy_from_slice(&512u32.to_le_bytes());
        // num_queues
        config[34..36].copy_from_slice(&1u16.to_le_bytes());
        // max_discard_sectors / max_discard_seg / discard_sector_alignment
        config[36..40].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
        config[40..44].copy_from_slice(&1u32.to_le_bytes());
        config[44..48].copy_from_slice(&1u32.to_le_bytes());
        // max_write_zeroes_sectors / seg / may_unmap
        config[48..52].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
        config[52..56].copy_from_slice(&1u32.to_le_bytes());
        config[56] = 1;

        let start = offset as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(start + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, _queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        let used_any = match queues.first_mut() {
            Some(queue) => self.process_queue(queue, mem),
            None => false,
        };
        Serviced::queue_if(0, used_any)
    }

    fn reset(&mut self) {
        self.acked = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk() -> Arc<Disk> {
        let path = std::env::temp_dir().join(format!(
            "lighter-blk-{}-{:?}.img",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let disk = Disk::open_or_create(&path, 16 << 20, false).unwrap();
        let _ = std::fs::remove_file(&path);
        Arc::new(disk)
    }

    #[test]
    fn reports_capacity_in_sectors() {
        let block = Block::new(disk());
        let mut config = [0u8; 8];
        block.config_read(0, &mut config);
        assert_eq!(u64::from_le_bytes(config), (16 << 20) / 512);
    }

    #[test]
    fn offers_discard_so_the_disk_can_shrink() {
        let block = Block::new(disk());
        assert_ne!(block.features() & F_DISCARD, 0);
        assert_ne!(block.features() & F_WRITE_ZEROES, 0);
        assert_ne!(block.features() & F_FLUSH, 0);
    }

    /// A read-only disk must advertise itself as such, or the guest will mount
    /// it writable and fail confusingly on the first write.
    #[test]
    fn a_read_only_disk_advertises_ro() {
        let path = std::env::temp_dir().join(format!("lighter-ro-{}.img", std::process::id()));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let disk = Arc::new(Disk::open_or_create(&path, 0, true).unwrap());
        let block = Block::new(disk);
        assert_ne!(block.features() & F_RO, 0);
        let _ = std::fs::remove_file(path);
    }

    /// Discard before the driver negotiated it must be refused rather than
    /// silently punching holes a driver did not ask for.
    #[test]
    fn discard_requires_negotiation() {
        let mut block = Block::new(disk());
        assert_eq!(block.discard(&[], &GuestMemory::detached(), true), S_UNSUPP);
        block.ack_features(F_DISCARD);
        // With no descriptors there is nothing to do, but it is now permitted.
        assert_eq!(block.discard(&[], &GuestMemory::detached(), true), S_OK);
    }

    #[test]
    fn config_reads_past_the_end_are_zero_filled() {
        let block = Block::new(disk());
        let mut data = [0xffu8; 16];
        block.config_read(200, &mut data);
        assert!(data.iter().all(|&b| b == 0));
    }
}

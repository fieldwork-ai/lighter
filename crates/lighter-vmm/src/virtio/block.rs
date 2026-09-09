//! virtio-blk.
//!
//! Each request is one descriptor chain of three parts: a header the device
//! reads, a data region whose direction depends on the request type, and a
//! one-byte status the device writes. The device's whole job is to honour that
//! shape without trusting it — a driver bug or a hostile guest can present a
//! chain with the header missing, the status buffer too short, or a data region
//! pointing anywhere at all.

use super::disk::DiskWait;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
const F_MQ: u64 = 1 << 12;

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

/// How long the host's side of a flush takes, summarised every hundred when
/// `LIGHTER_BLK_TRACE` is set: the guest's own flush accounting says how long
/// a flush took it, and this is the part of that which was ours.
static TRACE_ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static WRITTEN_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WRITE_OPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn trace_on() -> bool {
    *TRACE_ON.get_or_init(|| std::env::var_os("LIGHTER_BLK_TRACE").is_some())
}

fn flush_trace(took: std::time::Duration) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    static TOTAL_US: AtomicU64 = AtomicU64::new(0);
    static MAX_US: AtomicU64 = AtomicU64::new(0);
    if !trace_on() {
        return;
    }
    let us = took.as_micros() as u64;
    let n = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let total = TOTAL_US.fetch_add(us, Ordering::Relaxed) + us;
    MAX_US.fetch_max(us, Ordering::Relaxed);
    if n.is_multiple_of(100) {
        tracing::info!(
            flushes = n,
            mean_us = total / n,
            max_us = MAX_US.swap(0, Ordering::Relaxed),
            written_kib_per_flush = WRITTEN_BYTES.swap(0, Ordering::Relaxed) / 1024 / 100,
            writes_per_flush = WRITE_OPS.swap(0, Ordering::Relaxed) / 100,
            "BLKFLUSH"
        );
    }
}

const RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// Snapshot request metadata once. Never retain raw pointers into guest RAM.
enum Operation {
    Read {
        offset: u64,
        body: Vec<Descriptor>,
    },
    Write {
        offset: u64,
        body: Vec<Descriptor>,
        done: usize,
    },
    Flush,
    Id(Vec<Descriptor>),
    Discard {
        offset: u64,
        len: u64,
    },
    Zero {
        offset: u64,
        len: u64,
        unmap: bool,
        done: usize,
    },
}

impl Operation {
    fn name(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::Flush => "flush",
            Self::Id(_) => "get-id",
            Self::Discard { .. } => "discard",
            Self::Zero { .. } => "write-zeroes",
        }
    }
}

struct Pending {
    queue: u16,
    head: u16,
    status: Descriptor,
    operation: Operation,
    since: Instant,
    deadline: Instant,
    retries: u64,
}

/// A virtio block device over a sparse host file.
pub struct Block {
    disk: Arc<Disk>,
    read_only: bool,
    /// Features the driver accepted, which decides whether discard requests are
    /// legal at all.
    acked: u64,
    /// Request queues offered. One per vCPU, so that the driver maps each
    /// hardware queue to exactly one CPU: a completion found by that CPU's
    /// own poll then runs inline, where a shared queue's would be handed to
    /// the block softirq and a ksoftirqd wakeup — two to four microseconds on
    /// a request the host finished in under two.
    queues: usize,
    pending: Option<Pending>,
}

impl Block {
    pub fn new(disk: Arc<Disk>, queues: usize) -> Block {
        let read_only = disk.is_read_only();
        Block {
            disk,
            read_only,
            acked: 0,
            queues: queues.clamp(1, 32),
            pending: None,
        }
    }

    fn complete(
        queue: &mut Virtqueue,
        mem: &GuestMemory,
        head: u16,
        status: Descriptor,
        result: (u8, u32),
    ) {
        let _ = mem.write(status.addr, &[result.0]);
        queue.push_used(mem, head, result.1 + 1);
    }

    fn process_queue(&mut self, index: u16, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let descriptors: Vec<Descriptor> = chain.collect();
            let status = descriptors.last().copied().filter(|d| {
                descriptors.len() >= 2
                    && d.is_write_only()
                    && d.len >= 1
                    && mem.host_span(d.addr, 1).is_ok()
            });
            let Some(status) = status else {
                tracing::warn!("virtio-blk request had no valid status descriptor");
                queue.push_used(mem, head, 0);
                used_any = true;
                continue;
            };
            let result = match self.prepare(descriptors, mem) {
                Err(status) => (status, 0),
                Ok(mut operation) => match self.perform(&mut operation, mem) {
                    Ok(written) => (S_OK, written),
                    Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                        let now = Instant::now();
                        tracing::warn!(
                            operation = operation.name(),
                            "host disk full; disk I/O waiting for space"
                        );
                        *self.disk.waiting.lock().expect("disk status poisoned") = Some(DiskWait {
                            operation: operation.name(),
                            since: now,
                            retries: 0,
                        });
                        self.pending = Some(Pending {
                            queue: index,
                            head,
                            status,
                            operation,
                            since: now,
                            deadline: now + RETRY_INTERVAL,
                            retries: 0,
                        });
                        break;
                    }
                    Err(e) => {
                        tracing::error!(%e, operation = operation.name(), "virtio-blk operation failed");
                        (S_IOERR, 0)
                    }
                },
            };
            Self::complete(queue, mem, head, status, result);
            used_any = true;
        }
        used_any
    }

    fn prepare(&self, descriptors: Vec<Descriptor>, mem: &GuestMemory) -> Result<Operation, u8> {
        let header = descriptors.first().ok_or(S_IOERR)?;
        if descriptors.len() < 2 || header.is_write_only() || header.len < HEADER_LEN as u32 {
            return Err(S_IOERR);
        }
        // Validate the whole header before doing address arithmetic.
        mem.host_span(header.addr, HEADER_LEN)
            .map_err(|_| S_IOERR)?;
        let kind = mem.read_u32(header.addr).map_err(|_| S_IOERR)?;
        let sector = mem.read_u64(header.addr + 8).map_err(|_| S_IOERR)?;
        let offset = sector.checked_mul(SECTOR_SIZE).ok_or(S_IOERR)?;
        let body = &descriptors[1..descriptors.len() - 1];
        match kind {
            T_IN | T_OUT => {
                let total: u64 = body.iter().map(|d| u64::from(d.len)).sum();
                if offset
                    .checked_add(total)
                    .is_none_or(|end| end > self.disk.len())
                {
                    return Err(S_IOERR);
                }
                if kind == T_IN {
                    Ok(Operation::Read {
                        offset,
                        body: descriptors,
                    })
                } else if self.read_only {
                    Err(S_IOERR)
                } else {
                    Ok(Operation::Write {
                        offset,
                        body: descriptors,
                        done: 0,
                    })
                }
            }
            T_FLUSH => Ok(Operation::Flush),
            T_GET_ID => Ok(Operation::Id(descriptors)),
            T_DISCARD | T_WRITE_ZEROES => {
                let feature = if kind == T_DISCARD {
                    F_DISCARD
                } else {
                    F_WRITE_ZEROES
                };
                if self.acked & feature == 0 {
                    return Err(S_UNSUPP);
                }
                if self.read_only {
                    return Err(S_IOERR);
                }
                // We advertise exactly one segment for both operations.
                if body.len() != 1 || body[0].is_write_only() || body[0].len != 16 {
                    return Err(S_IOERR);
                }
                let base = body[0].addr;
                mem.host_span(base, 16).map_err(|_| S_IOERR)?;
                let offset = mem
                    .read_u64(base)
                    .map_err(|_| S_IOERR)?
                    .checked_mul(SECTOR_SIZE)
                    .ok_or(S_IOERR)?;
                let sectors = mem.read_u32(base + 8).map_err(|_| S_IOERR)?;
                let flags = mem.read_u32(base + 12).map_err(|_| S_IOERR)?;
                let len = u64::from(sectors) * SECTOR_SIZE;
                if sectors > MAX_DISCARD_SECTORS
                    || offset
                        .checked_add(len)
                        .is_none_or(|end| end > self.disk.len())
                {
                    return Err(S_IOERR);
                }
                if flags & !u32::from(kind == T_WRITE_ZEROES) != 0 {
                    return Err(S_UNSUPP);
                }
                if kind == T_DISCARD {
                    Ok(Operation::Discard { offset, len })
                } else {
                    Ok(Operation::Zero {
                        offset,
                        len,
                        unmap: flags & 1 != 0,
                        done: 0,
                    })
                }
            }
            _ => Err(S_UNSUPP),
        }
    }

    fn perform(&self, operation: &mut Operation, mem: &GuestMemory) -> io::Result<u32> {
        match operation {
            Operation::Read { offset, body } => {
                let mut spans = Self::spans(&body[1..body.len() - 1], mem, true)
                    .ok_or(io::ErrorKind::InvalidInput)?;
                let total = spans.iter().map(|s| s.iov_len as u32).sum();
                self.disk.read_vectored_at(*offset, &mut spans)?;
                return Ok(total);
            }
            Operation::Write { offset, body, done } => {
                let mut spans = Self::spans(&body[1..body.len() - 1], mem, false)
                    .ok_or(io::ErrorKind::InvalidInput)?;
                if trace_on() && *done == 0 {
                    WRITTEN_BYTES.fetch_add(
                        spans.iter().map(|s| s.iov_len as u64).sum(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    WRITE_OPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                self.disk
                    .write_vectored_progress(*offset, &mut spans, done)?;
            }
            Operation::Flush => {
                let started = Instant::now();
                let result = self.disk.flush();
                flush_trace(started.elapsed());
                result?;
            }
            Operation::Id(body) => {
                let (status, written) = self.get_id(&body[1..body.len() - 1], mem);
                return if status == S_OK {
                    Ok(written)
                } else {
                    Err(io::ErrorKind::InvalidInput.into())
                };
            }
            Operation::Discard { offset, len } => self.disk.punch_hole(*offset, *len)?,
            Operation::Zero {
                offset,
                len,
                unmap,
                done,
            } => self
                .disk
                .write_zeroes_progress(*offset, *len, *unmap, done)?,
        }
        Ok(0)
    }

    fn retry(&mut self, queues: &mut [Virtqueue], mem: &GuestMemory, now: Instant) -> Serviced {
        if self.pending.as_ref().is_none_or(|p| now < p.deadline) {
            return Serviced::NONE;
        }
        let mut pending = self.pending.take().unwrap();
        pending.retries += 1;
        let result = match self.perform(&mut pending.operation, mem) {
            Ok(written) => {
                tracing::info!(
                    retries = pending.retries,
                    waited_secs = pending.since.elapsed().as_secs(),
                    "host disk I/O recovered"
                );
                (S_OK, written)
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                // Time spent attempting I/O is not part of the next wait.
                pending.deadline = Instant::now().max(now) + RETRY_INTERVAL;
                *self.disk.waiting.lock().expect("disk status poisoned") = Some(DiskWait {
                    operation: pending.operation.name(),
                    since: pending.since,
                    retries: pending.retries,
                });
                self.pending = Some(pending);
                return Serviced::NONE;
            }
            Err(e) => {
                tracing::error!(%e, "deferred disk operation failed");
                (S_IOERR, 0)
            }
        };
        *self.disk.waiting.lock().expect("disk status poisoned") = None;
        Self::complete(
            &mut queues[pending.queue as usize],
            mem,
            pending.head,
            pending.status,
            result,
        );
        Serviced::queue(pending.queue)
    }

    /// The data descriptors of a request as iovecs over the guest's own
    /// pages, so the disk reads into or writes from them directly. `None`
    /// when a descriptor points outside guest memory, is too long, or runs
    /// the wrong way for the request — a read whose data region the device
    /// may not write is malformed, and serving it would write nowhere useful.
    fn spans(body: &[Descriptor], mem: &GuestMemory, writable: bool) -> Option<Vec<libc::iovec>> {
        let mut iovs = Vec::with_capacity(body.len());
        for desc in body {
            if desc.is_write_only() != writable {
                return None;
            }
            let len = desc.len as usize;
            if len > MAX_TRANSFER {
                return None;
            }
            let base = mem.host_span(desc.addr, len).ok()?;
            iovs.push(libc::iovec {
                iov_base: base.cast(),
                iov_len: len,
            });
        }
        Some(iovs)
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
        if self.queues > 1 {
            features |= F_MQ;
        }
        features
    }

    fn ack_features(&mut self, features: u64) {
        self.acked = features;
    }

    fn queue_count(&self) -> usize {
        self.queues
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
        config[34..36].copy_from_slice(&(self.queues as u16).to_le_bytes());
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

    fn notify(&mut self, queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        // Only the timer services retained operations, never a guest kick.
        if self.pending.is_some() {
            return Serviced::NONE;
        }
        let used = queues
            .get_mut(queue as usize)
            .is_some_and(|q| self.process_queue(queue, q, mem));
        Serviced::queue_if(queue, used)
    }

    fn retry_deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|p| p.deadline)
    }

    fn retry_deferred(
        &mut self,
        queues: &mut [Virtqueue],
        mem: &GuestMemory,
        now: Instant,
    ) -> Serviced {
        self.retry(queues, mem, now)
    }

    fn reset(&mut self) {
        self.acked = 0;
        self.pending = None;
        *self.disk.waiting.lock().expect("disk status poisoned") = None;
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
        let block = Block::new(disk(), 1);
        let mut config = [0u8; 8];
        block.config_read(0, &mut config);
        assert_eq!(u64::from_le_bytes(config), (16 << 20) / 512);
    }

    #[test]
    fn offers_discard_so_the_disk_can_shrink() {
        let block = Block::new(disk(), 1);
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
        let block = Block::new(disk, 1);
        assert_ne!(block.features() & F_RO, 0);
        let _ = std::fs::remove_file(path);
    }

    /// One queue per vCPU is only meaningful if the driver is told: the MQ
    /// feature and `num_queues` travel together, and a single queue offers
    /// neither.
    #[test]
    fn several_queues_are_advertised_with_mq() {
        let block = Block::new(disk(), 8);
        assert_eq!(block.queue_count(), 8);
        assert_ne!(block.features() & F_MQ, 0);
        let mut num_queues = [0u8; 2];
        block.config_read(34, &mut num_queues);
        assert_eq!(u16::from_le_bytes(num_queues), 8);

        let single = Block::new(disk(), 1);
        assert_eq!(single.features() & F_MQ, 0);
        single.config_read(34, &mut num_queues);
        assert_eq!(u16::from_le_bytes(num_queues), 1);
    }

    /// A request that is only a header has no status descriptor to answer
    /// into: refused, not sliced from 1 to 0 (which panicked, and in a
    /// release build took the machine with it).
    #[test]
    fn a_header_alone_is_refused() {
        let block = Block::new(disk(), 1);
        let header = Descriptor {
            addr: 0x1000,
            len: HEADER_LEN as u32,
            flags: 0,
            next: 0,
        };
        // A valid backed header reaches the old failing slice. An empty
        // address space would return IOERR earlier even without the fix.
        let mem = GuestMemory::test_region(0x1000, 0x4000);
        mem.write_u32(header.addr, T_FLUSH).unwrap();
        mem.write_u64(header.addr + 8, 0).unwrap();
        assert!(matches!(block.prepare(vec![header], &mem), Err(S_IOERR)));
    }

    /// Discard before the driver negotiated it must be refused rather than
    /// silently punching holes a driver did not ask for.
    #[test]
    fn discard_requires_negotiation() {
        let mut block = Block::new(disk(), 1);
        let mem = GuestMemory::test_region(0x1000, 0x4000);
        let descriptors = [
            Descriptor {
                addr: 0x1000,
                len: 16,
                flags: 0,
                next: 0,
            },
            Descriptor {
                addr: 0x2000,
                len: 16,
                flags: 0,
                next: 0,
            },
            Descriptor {
                addr: 0x3000,
                len: 1,
                flags: 2,
                next: 0,
            },
        ];
        mem.write_u32(0x1000, T_DISCARD).unwrap();
        assert!(matches!(
            block.prepare(descriptors.to_vec(), &mem),
            Err(S_UNSUPP)
        ));
        block.ack_features(F_DISCARD);
        assert!(block.prepare(descriptors.to_vec(), &mem).is_ok());
    }

    #[test]
    fn config_reads_past_the_end_are_zero_filled() {
        let block = Block::new(disk(), 1);
        let mut data = [0xffu8; 16];
        block.config_read(200, &mut data);
        assert!(data.iter().all(|&b| b == 0));
    }
    // Two real split rings in detached guest RAM; no hypervisor required.
    fn ring(mem: &GuestMemory, base: u64, kind: u32, value: u8) -> Virtqueue {
        let mut q = Virtqueue::new(8);
        q.set_size(8);
        q.desc_addr = base;
        q.avail_addr = base + 0x100;
        q.used_addr = base + 0x200;
        assert!(q.set_ready(true));
        let header = base + 0x400;
        let data = base + 0x500;
        let status = base + 0x900;
        mem.write_u32(header, kind).unwrap();
        mem.write_u64(header + 8, 0).unwrap();
        mem.write(data, &[value; 512]).unwrap();
        mem.write(status, &[0xff]).unwrap();
        let descs = [(header, 16, 1, 1), (data, 512, 1, 2), (status, 1, 2, 0)];
        for (i, (addr, len, flags, next)) in descs.into_iter().enumerate() {
            let at = base + i as u64 * 16;
            mem.write_u64(at, addr).unwrap();
            mem.write_u32(at + 8, len).unwrap();
            mem.write_u16(at + 12, flags).unwrap();
            mem.write_u16(at + 14, next).unwrap();
        }
        mem.write_u16(q.avail_addr + 4, 0).unwrap();
        mem.write_u16(q.avail_addr + 2, 1).unwrap();
        q
    }

    #[test]
    fn partial_write_waits_without_completing_or_rewriting_the_prefix() {
        let disk = disk();
        disk.inject_fault(128, libc::ENOSPC);
        disk.inject_fault(0, libc::ENOSPC);
        let mut block = Block::new(disk.clone(), 2);
        let mem = GuestMemory::test_region(0x1000, 0x5000);
        let mut queues = vec![
            ring(&mem, 0x1000, T_OUT, 0x11),
            ring(&mem, 0x3000, T_OUT, 0x22),
        ];
        assert!(!block.notify(0, &mut queues, &mem).any());
        assert_eq!(mem.read_u16(0x1202).unwrap(), 0);
        let deadline = block.retry_deadline().unwrap();
        // Guest kicks cannot retry, and the second overlapping writer cannot pass.
        for _ in 0..100 {
            assert!(!block.notify(1, &mut queues, &mem).any());
        }
        assert_eq!(queues[1].outstanding(&mem), 1);
        assert!(
            !block
                .retry(&mut queues, &mem, deadline - Duration::from_nanos(1))
                .any()
        );
        assert_eq!(disk.waiting().unwrap().retries, 0);
        assert!(!block.retry(&mut queues, &mem, deadline).any());
        assert_eq!(disk.waiting().unwrap().retries, 1);
        assert_eq!(block.retry_deadline().unwrap(), deadline + RETRY_INTERVAL);
        // The already-written prefix must not be read again on retry.
        mem.write(0x1500, &[0x99; 128]).unwrap();
        let deadline = block.retry_deadline().unwrap();
        assert!(block.retry(&mut queues, &mem, deadline).contains(0));
        assert!(disk.waiting().is_none());
        let mut bytes = [0; 512];
        disk.read_at(0, &mut bytes).unwrap();
        assert_eq!(bytes, [0x11; 512]);
        assert_eq!(mem.read_u16(0x1202).unwrap(), 1);
        assert!(!block.retry(&mut queues, &mem, deadline).any());
        assert_eq!(mem.read_u16(0x1202).unwrap(), 1);
        assert!(block.notify(1, &mut queues, &mem).contains(1));
        disk.read_at(0, &mut bytes).unwrap();
        assert_eq!(bytes, [0x22; 512]);
    }

    #[test]
    fn deferred_flush_holds_other_queues_and_only_enospc_is_retried() {
        let disk = disk();
        disk.inject_fault(0, libc::ENOSPC);
        disk.inject_fault(0, libc::EIO);
        let mut block = Block::new(disk.clone(), 2);
        let mem = GuestMemory::test_region(0x1000, 0x5000);
        let mut queues = vec![
            ring(&mem, 0x1000, T_FLUSH, 0),
            ring(&mem, 0x3000, T_OUT, 0x22),
        ];
        block.notify(0, &mut queues, &mem);
        assert_eq!(disk.waiting().unwrap().operation, "flush");
        assert!(!block.notify(1, &mut queues, &mem).any());
        let deadline = block.retry_deadline().unwrap();
        assert!(block.retry(&mut queues, &mem, deadline).contains(0));
        let mut status = [0];
        mem.read(0x1900, &mut status).unwrap();
        assert_eq!(status[0], S_IOERR);
        assert!(block.retry_deadline().is_none());
        assert!(block.notify(1, &mut queues, &mem).any());
    }

    #[test]
    fn reset_cancels_retained_operations() {
        let disk = disk();
        disk.inject_fault(0, libc::ENOSPC);
        let mut block = Block::new(disk.clone(), 1);
        let mem = GuestMemory::test_region(0x1000, 0x4000);
        let mut queues = vec![ring(&mem, 0x1000, T_OUT, 9)];
        block.notify(0, &mut queues, &mem);
        let deadline = block.retry_deadline().unwrap();
        block.reset();
        queues[0].reset();
        assert!(disk.waiting().is_none());
        assert!(!block.retry(&mut queues, &mem, deadline).any());
        assert_eq!(mem.read_u16(0x1202).unwrap(), 0);
        let mut bytes = [0; 512];
        disk.read_at(0, &mut bytes).unwrap();
        assert_eq!(bytes, [0; 512]);
    }

    #[test]
    fn zero_fallback_retains_progress_across_exhaustion() {
        let disk = disk();
        disk.write_at(0, &[0xff; 4096]).unwrap();
        disk.inject_fault(513, libc::ENOSPC);
        let mut done = 0;
        assert_eq!(
            disk.write_zeroes_progress(0, 4096, false, &mut done)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );
        assert_eq!(done, 513);
        // Ensure retry does not overwrite the successfully completed prefix.
        disk.write_at(0, &[0x11; 513]).unwrap();
        disk.write_zeroes_progress(0, 4096, false, &mut done)
            .unwrap();
        let mut bytes = [0xff; 4096];
        disk.read_at(0, &mut bytes).unwrap();
        assert_eq!(&bytes[..513], &[0x11; 513]);
        assert!(bytes[513..].iter().all(|&b| b == 0));
    }
    fn timer_recovers_without_a_guest_kick(window: Duration) {
        use crate::bus::MmioDevice;
        use crate::virtio::{mmio::VirtioMmio, poll};
        use std::sync::Mutex;
        let disk = disk();
        disk.inject_fault(0, libc::ENOSPC);
        let mem = Arc::new(GuestMemory::test_region(0x1000, 0x5000));
        let mut transport = VirtioMmio::new(
            Box::new(Block::new(disk.clone(), 2)),
            mem.clone(),
            Arc::new(crate::irq::NullIrq),
        );
        transport.queues()[0] = ring(&mem, 0x1000, T_OUT, 0x11);
        transport.queues()[1] = ring(&mem, 0x3000, T_OUT, 0x22);
        transport.write(0x70, &4u32.to_le_bytes()); // DRIVER_OK
        let kicks = poll::Kicks::new();
        let signal = kicks.clone();
        transport.set_kick_observer(Arc::new(move |_| signal.kicked()));
        let transport = Arc::new(Mutex::new(transport));
        let worker = poll::spawn_with_window(
            "test-retry",
            transport.clone(),
            vec![0, 1],
            kicks.clone(),
            window,
        )
        .unwrap();
        transport.lock().unwrap().write(0x50, &0u32.to_le_bytes());
        let began = Instant::now();
        while disk.waiting().is_some() && began.elapsed() < Duration::from_secs(5) {
            // A blocked device must not monopolize the transport mutex.
            let deadline = Instant::now() + Duration::from_millis(250);
            loop {
                if let Ok(guard) = transport.try_lock() {
                    drop(guard);
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "blocked worker retained the transport lock"
                );
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        kicks.stop();
        worker.join().unwrap();
        assert!(began.elapsed() >= RETRY_INTERVAL);
        assert!(
            disk.waiting().is_none(),
            "timer never retried the retained request"
        );
        assert_eq!(mem.read_u16(0x1202).unwrap(), 1);
        assert_eq!(
            mem.read_u16(0x3202).unwrap(),
            1,
            "queued request was stranded after recovery"
        );
        let mut bytes = [0; 512];
        disk.read_at(0, &mut bytes).unwrap();
        assert_eq!(bytes, [0x22; 512]);
    }

    #[test]
    fn timer_recovers_polled_disk_without_a_new_kick() {
        timer_recovers_without_a_guest_kick(Duration::from_micros(200));
    }

    #[test]
    fn timer_recovers_unpolled_disk_without_a_new_kick() {
        timer_recovers_without_a_guest_kick(Duration::ZERO);
    }
    #[test]
    fn unrelated_disk_errors_are_completed_and_other_disks_keep_running() {
        let first = disk();
        first.inject_fault(0, libc::ENOSPC);
        let mut a = Block::new(first, 1);
        let second = disk();
        let mut b = Block::new(second.clone(), 1);
        let mem = GuestMemory::test_region(0x1000, 0x5000);
        let mut qa = vec![ring(&mem, 0x1000, T_OUT, 1)];
        let mut qb = vec![ring(&mem, 0x3000, T_OUT, 2)];
        a.notify(0, &mut qa, &mem);
        assert!(a.retry_deadline().is_some());
        assert!(b.notify(0, &mut qb, &mem).any());
        assert!(b.retry_deadline().is_none());
        // A fresh failing operation returns IOERR, never an endless retry.
        let mut q = vec![ring(&mem, 0x3000, T_OUT, 3)];
        second.inject_fault(0, libc::EIO);
        assert!(b.notify(0, &mut q, &mem).any());
        let mut status = [0];
        mem.read(0x3900, &mut status).unwrap();
        assert_eq!(status, [S_IOERR]);
        assert!(b.retry_deadline().is_none());
    }

    #[test]
    fn partial_vectored_write_resumes_across_descriptor_boundaries() {
        let disk = disk();
        disk.inject_fault(300, libc::ENOSPC);
        let mut done = 0;
        let first = [0x11u8; 256];
        let second = [0x22u8; 256];
        let spans = || {
            [
                libc::iovec {
                    iov_base: first.as_ptr().cast_mut().cast(),
                    iov_len: 256,
                },
                libc::iovec {
                    iov_base: second.as_ptr().cast_mut().cast(),
                    iov_len: 256,
                },
            ]
        };
        assert_eq!(
            disk.write_vectored_progress(512, &mut spans(), &mut done)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );
        assert_eq!(done, 300);
        disk.write_vectored_progress(512, &mut spans(), &mut done)
            .unwrap();
        let mut bytes = [0; 512];
        disk.read_at(512, &mut bytes).unwrap();
        assert_eq!(&bytes[..256], &first);
        assert_eq!(&bytes[256..], &second);
    }
    #[test]
    fn stopping_a_worker_with_a_deferred_request_does_not_wait_for_retry() {
        use crate::bus::MmioDevice;
        use crate::virtio::{mmio::VirtioMmio, poll};
        use std::sync::Mutex;
        let disk = disk();
        disk.inject_fault(0, libc::ENOSPC);
        let mem = Arc::new(GuestMemory::test_region(0x1000, 0x4000));
        let mut transport = VirtioMmio::new(
            Box::new(Block::new(disk, 1)),
            mem.clone(),
            Arc::new(crate::irq::NullIrq),
        );
        transport.queues()[0] = ring(&mem, 0x1000, T_OUT, 1);
        transport.write(0x70, &4u32.to_le_bytes());
        let kicks = poll::Kicks::new();
        let signal = kicks.clone();
        transport.set_kick_observer(Arc::new(move |_| signal.kicked()));
        let transport = Arc::new(Mutex::new(transport));
        let worker = poll::spawn_with_window(
            "test-stop-retry",
            transport.clone(),
            vec![0],
            kicks.clone(),
            Duration::ZERO,
        )
        .unwrap();
        transport.lock().unwrap().write(0x50, &0u32.to_le_bytes());
        let began = Instant::now();
        kicks.stop();
        worker.join().unwrap();
        assert!(began.elapsed() < Duration::from_secs(1));
        assert_eq!(mem.read_u16(0x1202).unwrap(), 0);
    }
    #[test]
    fn unaligned_zero_unmap_falls_back_and_preserves_surrounding_bytes() {
        let disk = disk();
        disk.write_at(0, &[0xff; 4096]).unwrap();
        disk.inject_fault(17, libc::ENOSPC);
        let mut done = 0;
        assert_eq!(
            disk.write_zeroes_progress(1, 513, true, &mut done)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );
        assert_eq!(done, 17);
        disk.write_zeroes_progress(1, 513, true, &mut done).unwrap();
        let mut bytes = [0; 4096];
        disk.read_at(0, &mut bytes).unwrap();
        assert_eq!(bytes[0], 0xff);
        assert!(bytes[1..514].iter().all(|&b| b == 0));
        assert!(bytes[514..].iter().all(|&b| b == 0xff));
    }
}

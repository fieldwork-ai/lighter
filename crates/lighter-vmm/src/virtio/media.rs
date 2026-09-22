//! virtio-media: a V4L2 stateful decoder, decoded on the Mac.
//!
//! The guest sees `/dev/video0`, a memory-to-memory H.264 decoder of the
//! kind a Raspberry Pi or a phone has, and stock ffmpeg (`h264_v4l2m2m`),
//! GStreamer and Frigate's own ffmpeg drive it unchanged. virtio-media is
//! V4L2 as a virtio protocol (the guest driver is a relay; the v9 series on
//! the kernel list, carried as guest patch 0036), and the `virtio-media`
//! crate does the protocol, the ioctl dispatch and the stateful-decoder
//! state machine; what this module adds is the four adapters that crate
//! needs from a VMM, and what `crate::video` adds is the decoder behind it,
//! VideoToolbox.
//!
//! # Memory
//!
//! The decoder's buffers, both the bitstream the guest writes and the
//! frames it reads back, are MMAP buffers, which in virtio-media live on
//! the host: the guest maps them through the device's shared-memory region.
//! That region is an aperture of guest-physical space reserved by the
//! layout (`GuestLayout::video`), and a buffer is placed in it exactly as a
//! GPU blob is, by `GuestMemory::map_foreign`, at 16 KiB granularity. The
//! backing is POSIX shared memory, because the crate's mapper is handed a
//! file descriptor and macOS has no memfd; the decoder writes a frame
//! through its own mapping of the same object and the guest reads it
//! through the aperture. One copy per frame, VideoToolbox's buffer into the
//! guest's, until the driver's DMA-BUF support lands upstream.
//!
//! # Threads
//!
//! Everything happens in `notify`, under the transport lock, as the GPU
//! does it: a command is read from the chain, dispatched, its response
//! scattered back, and any events the decoder produced are written into
//! the descriptors the driver keeps posted on the event queue. Decoding is
//! synchronous inside the command that queued the bitstream, a few
//! milliseconds for a 1080p frame.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use virtio_media::devices::video_decoder::VideoDecoder;
use virtio_media::io::WriteToDescriptorChain;
use virtio_media::protocol::{V4l2Event, VirtioMediaDeviceConfig};
use virtio_media::{
    VirtioMediaDevice, VirtioMediaDeviceRunner, VirtioMediaEventQueue, VirtioMediaHostMemoryMapper,
};

use crate::layout::Window;
use crate::memory::{GuestMemory, HOST_PAGE};
use crate::video::decoder::VideoToolboxDecoder;
use crate::virtio::mmio::COMMON_FEATURES;
use crate::virtio::queue::{Descriptor, Virtqueue};
use crate::virtio::{Serviced, ShmRegion, VirtioDevice, device_type};

pub const COMMAND_QUEUE: u16 = 0;
pub const EVENT_QUEUE: u16 = 1;

/// The shared-memory region MMAP buffers are mapped through, by the id the
/// driver asks for (`VIRTIO_MEDIA_SHM_MMAP`).
const SHM_ID_MMAP: u8 = 0;

/// What the guest sees in `VIDIOC_QUERYCAP`.
const CARD: &[u8] = b"lighter VideoToolbox decoder";

// V4L2 capability bits, as `videodev2.h` has them.
const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
const V4L2_CAP_STREAMING: u32 = 0x0400_0000;
/// `VFL_TYPE_VIDEO`: a plain `/dev/videoN`.
const VFL_TYPE_VIDEO: u32 = 0;

/// The device-readable part of a chain, read into memory once.
type Reader = std::io::Cursor<Vec<u8>>;
/// The response, accumulated and scattered into the writable descriptors.
type Writer = Vec<u8>;

type Runner = VirtioMediaDeviceRunner<
    Reader,
    Writer,
    VideoDecoder<VideoToolboxDecoder, Events, Aperture>,
    NoPoller,
>;

pub struct Media {
    runner: Runner,
    aperture: Window,
    /// Events the decoder produced, waiting for event-queue descriptors.
    pending: PendingEvents,
}

impl Media {
    pub fn new(aperture: Window, memory: std::sync::Arc<GuestMemory>) -> Media {
        let pending = PendingEvents::default();
        let device = VideoDecoder::new(
            VideoToolboxDecoder::new(),
            Events(pending.clone()),
            Aperture::new(aperture, memory),
        );
        Media {
            runner: VirtioMediaDeviceRunner::new(device, NoPoller),
            aperture,
            pending,
        }
    }

    /// Splits a chain into the bytes the driver wrote and the descriptors
    /// it expects the response in.
    fn split(
        mem: &GuestMemory,
        chain: impl Iterator<Item = Descriptor>,
    ) -> (Vec<u8>, Vec<(u64, u32)>) {
        let mut request = Vec::new();
        let mut reply = Vec::new();
        for desc in chain {
            if desc.is_write_only() {
                reply.push((desc.addr, desc.len));
            } else {
                let at = request.len();
                request.resize(at + desc.len as usize, 0);
                if mem.read(desc.addr, &mut request[at..]).is_err() {
                    request.truncate(at);
                }
            }
        }
        (request, reply)
    }

    fn scatter(mem: &GuestMemory, bufs: &[(u64, u32)], data: &[u8]) -> u32 {
        let mut at = 0usize;
        for &(addr, len) in bufs {
            if at >= data.len() {
                break;
            }
            let n = (len as usize).min(data.len() - at);
            if mem.write(addr, &data[at..at + n]).is_err() {
                break;
            }
            at += n;
        }
        at as u32
    }

    fn serve_commands(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used = false;
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let (request, reply) = Media::split(mem, chain);
            let word = |i: usize| {
                request
                    .get(i * 4..i * 4 + 4)
                    .map_or(0, |b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
            };
            tracing::trace!(
                cmd = word(0),
                session = word(2),
                code = format_args!("{:#x}", word(3)),
                "virtio-media command"
            );
            let mut reader = std::io::Cursor::new(request);
            let mut writer = Vec::new();
            self.runner.handle_command(&mut reader, &mut writer);
            // The decoder answers synchronously, so whatever it produced
            // (a frame back, an input buffer released, a resolution change)
            // is ready the moment the command is.
            let sessions: Vec<u32> = self.runner.sessions.keys().copied().collect();
            for id in sessions {
                if let Some(session) = self.runner.sessions.get_mut(&id) {
                    loop {
                        let before = self.pending.len();
                        if <VideoDecoder<VideoToolboxDecoder, Events, Aperture> as VirtioMediaDevice<Reader, Writer>>::process_events(&mut self.runner.device, session).is_err()
                            || self.pending.len() == before
                        {
                            break;
                        }
                    }
                }
            }
            let len = Media::scatter(mem, &reply, &writer);
            queue.push_used(mem, head, len);
            used = true;
        }
        used
    }

    /// Writes waiting events into the descriptors the driver has posted.
    fn serve_events(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used = false;
        while !self.pending.is_empty() {
            let Some(chain) = queue.pop(mem) else {
                tracing::trace!(
                    pending = self.pending.len(),
                    "virtio-media events waiting for a descriptor"
                );
                break;
            };
            let head = chain.head();
            let (_, reply) = Media::split(mem, chain);
            let Some(bytes) = self.pending.pop() else {
                queue.push_used(mem, head, 0);
                break;
            };
            let len = Media::scatter(mem, &reply, &bytes);
            tracing::trace!(
                kind = u32::from_le_bytes(bytes[..4].try_into().unwrap_or_default()),
                bytes = bytes.len(),
                left = self.pending.len(),
                "virtio-media event out"
            );
            if (len as usize) < bytes.len() {
                tracing::warn!(
                    len,
                    need = bytes.len(),
                    "virtio-media event descriptor too small"
                );
            }
            queue.push_used(mem, head, len);
            used = true;
        }
        used
    }
}

impl VirtioDevice for Media {
    fn device_type(&self) -> u32 {
        device_type::MEDIA
    }

    fn name(&self) -> &'static str {
        "virtio-media"
    }

    fn features(&self) -> u64 {
        COMMON_FEATURES
    }

    fn queue_count(&self) -> usize {
        2
    }

    fn shm_regions(&self) -> Vec<ShmRegion> {
        vec![ShmRegion {
            id: SHM_ID_MMAP,
            base: self.aperture.base,
            len: self.aperture.size,
        }]
    }

    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let mut card = [0u8; 32];
        card[..CARD.len()].copy_from_slice(CARD);
        let config = VirtioMediaDeviceConfig {
            device_caps: V4L2_CAP_VIDEO_M2M_MPLANE | V4L2_CAP_STREAMING,
            device_type: VFL_TYPE_VIDEO,
            card,
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&config.device_caps.to_le_bytes());
        bytes.extend_from_slice(&config.device_type.to_le_bytes());
        bytes.extend_from_slice(&config.card);
        for (i, b) in data.iter_mut().enumerate() {
            *b = bytes.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        let mut serviced = Serviced::NONE;
        if queue == COMMAND_QUEUE
            && let Some(q) = queues.get_mut(COMMAND_QUEUE as usize)
            && self.serve_commands(q, mem)
        {
            serviced.queues |= Serviced::queue(COMMAND_QUEUE).queues;
        }
        // Whichever queue was kicked, events wait only for descriptors.
        if let Some(q) = queues.get_mut(EVENT_QUEUE as usize)
            && self.serve_events(q, mem)
        {
            serviced.queues |= Serviced::queue(EVENT_QUEUE).queues;
        }
        serviced
    }
}

/// Serialized events, in order, shared between the crate's event sink and
/// the device that drains them into descriptors.
#[derive(Clone, Default)]
struct PendingEvents(std::sync::Arc<std::sync::Mutex<VecDeque<Vec<u8>>>>);

impl PendingEvents {
    fn push(&self, bytes: Vec<u8>) {
        self.0
            .lock()
            .expect("media events poisoned")
            .push_back(bytes);
    }
    fn pop(&self) -> Option<Vec<u8>> {
        self.0.lock().expect("media events poisoned").pop_front()
    }
    fn len(&self) -> usize {
        self.0.lock().expect("media events poisoned").len()
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The crate's event sink: serializes each event as the driver reads it.
struct Events(PendingEvents);

impl VirtioMediaEventQueue for Events {
    fn send_event(&mut self, event: V4l2Event) {
        let mut bytes: Vec<u8> = Vec::new();
        let written = match event {
            V4l2Event::Error(e) => bytes.write_obj(e),
            V4l2Event::DequeueBuffer(e) => bytes.write_obj(e),
            V4l2Event::Event(e) => bytes.write_obj(e),
        };
        match written {
            Ok(()) => self.0.push(bytes),
            Err(e) => tracing::warn!(%e, "virtio-media event could not be serialized"),
        }
    }
}

/// The decoder has no file descriptor to poll: it answers within the
/// command, and its events are collected right after it.
#[derive(Clone)]
struct NoPoller;

impl virtio_media::poll::SessionPoller for NoPoller {
    fn add_session(&self, _session: BorrowedFd, _session_id: u32) -> Result<(), i32> {
        Ok(())
    }
    fn remove_session(&self, _session: BorrowedFd) {}
}

/// The aperture: where MMAP buffers appear to the guest. Offsets are handed
/// out at host-page granularity and returned when the driver unmaps; the
/// guest memory the pages are placed in is the machine's.
pub struct Aperture {
    window: Window,
    mem: std::sync::Arc<GuestMemory>,
    /// Offset in the aperture → (host mapping, length).
    live: HashMap<u64, (*mut u8, usize)>,
    free: Vec<(u64, u64)>,
}

// The raw pointers are host mappings this struct owns and unmaps.
unsafe impl Send for Aperture {}

impl Aperture {
    fn new(window: Window, mem: std::sync::Arc<GuestMemory>) -> Aperture {
        Aperture {
            window,
            mem,
            live: HashMap::new(),
            free: vec![(0, window.size)],
        }
    }

    fn allocate(&mut self, len: u64) -> Option<u64> {
        let i = self.free.iter().position(|&(_, size)| size >= len)?;
        let (offset, size) = self.free[i];
        if size == len {
            self.free.remove(i);
        } else {
            self.free[i] = (offset + len, size - len);
        }
        Some(offset)
    }

    fn release(&mut self, offset: u64, len: u64) {
        self.free.push((offset, len));
        self.free.sort_unstable();
        // Merge neighbours so a long session does not fragment the space.
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.free.len());
        for &(o, l) in &self.free {
            match merged.last_mut() {
                Some((mo, ml)) if *mo + *ml == o => *ml += l,
                _ => merged.push((o, l)),
            }
        }
        self.free = merged;
    }
}

impl VirtioMediaHostMemoryMapper for Aperture {
    fn add_mapping(
        &mut self,
        buffer: BorrowedFd,
        length: u64,
        _offset: u64,
        rw: bool,
    ) -> Result<u64, i32> {
        let mem = self.mem.clone();
        let len = (length as usize).div_ceil(HOST_PAGE) * HOST_PAGE;
        let prot = if rw {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        // SAFETY: a fresh shared mapping of the buffer's object; unmapped in
        // `remove_mapping`, after the guest's side is gone.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                buffer.as_fd().as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::ENOMEM));
        }
        let Some(offset) = self.allocate(len as u64) else {
            unsafe { libc::munmap(ptr, len) };
            tracing::warn!(len, "virtio-media aperture is full");
            return Err(libc::ENOMEM);
        };
        let gpa = self.window.base + offset;
        // SAFETY: the mapping stays until `remove_mapping`, which unmaps the
        // guest side first.
        if let Err(e) = unsafe { mem.map_foreign(gpa, ptr.cast(), len) } {
            tracing::warn!(%e, "virtio-media buffer could not be mapped into the guest");
            unsafe { libc::munmap(ptr, len) };
            self.release(offset, len as u64);
            return Err(libc::EIO);
        }
        self.live.insert(offset, (ptr.cast(), len));
        Ok(offset)
    }

    fn remove_mapping(&mut self, shm_offset: u64) -> Result<(), i32> {
        let Some((ptr, len)) = self.live.remove(&shm_offset) else {
            return Err(libc::EINVAL);
        };
        // SAFETY: the driver asked for the unmap; its own mapping is gone.
        if let Err(e) = unsafe { self.mem.unmap_foreign(self.window.base + shm_offset, len) } {
            tracing::warn!(%e, "virtio-media buffer unmap failed");
        }
        unsafe { libc::munmap(ptr.cast(), len) };
        self.release(shm_offset, len as u64);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_aperture_hands_out_and_merges_ranges() {
        let mut a = Aperture::new(
            Window {
                base: 0x1_0000_0000,
                size: 4 * HOST_PAGE as u64,
            },
            std::sync::Arc::new(GuestMemory::detached()),
        );
        let p = HOST_PAGE as u64;
        assert_eq!(a.allocate(p), Some(0));
        assert_eq!(a.allocate(2 * p), Some(p));
        assert_eq!(a.allocate(p), Some(3 * p));
        assert_eq!(a.allocate(p), None);
        a.release(p, 2 * p);
        a.release(0, p);
        assert_eq!(a.free, vec![(0, 3 * p)]);
        a.release(3 * p, p);
        assert_eq!(a.free, vec![(0, 4 * p)]);
    }
}

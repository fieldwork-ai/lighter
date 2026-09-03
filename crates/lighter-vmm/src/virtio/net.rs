//! virtio-net.
//!
//! Two queues: the guest hands us frames to transmit on one, and offers empty
//! buffers to receive into on the other. What sits behind the device — today a
//! userspace network stack in a sidecar process — is hidden behind
//! [`NetBackend`], because that is a piece we expect to replace: a native
//! in-process stack would swap the implementation and leave this file alone.
//!
//! # Nothing here blocks
//!
//! The first version wrote each transmitted frame to the backend from inside
//! the notification, which is to say on the vCPU thread that kicked, under the
//! transport lock. A backend that is a socket has a buffer, and a guest sending
//! faster than the sidecar drains it fills that buffer, at which point the
//! write blocks — with a vCPU inside it. The guest saw that as a CPU that
//! stopped answering: an RCU stall on core 0 every minute, a Docker socket
//! that no longer replied, and 1.5 Gbit/s while it lasted.
//!
//! So the device now touches the backend on no thread of the guest's. Frames
//! are copied out of the ring into an [`Outbox`] and a thread of the backend's
//! own drains that to the wire; frames from the wire land in an [`Inbox`] and
//! are moved into the guest's buffers by whoever is holding the transport. The
//! only thing a vCPU does on a kick is a memcpy. When the outbox is full the
//! device stops taking chains and leaves them in the ring — backpressure into
//! the guest's own queue, which is where a slow link belongs — and the drain
//! thread asks for the queue to be looked at again once there is room.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use crate::memory::GuestMemory;
use crate::virtio::mmio::COMMON_FEATURES;
use crate::virtio::queue::Virtqueue;
use crate::virtio::{Serviced, VirtioDevice, device_type};

/// Queue indices.
pub const RX_QUEUE: u16 = 0;
pub const TX_QUEUE: u16 = 1;

// Feature bits.
const F_CSUM: u64 = 1 << 0;
const F_MTU: u64 = 1 << 3;
const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;

/// `VIRTIO_NET_S_LINK_UP`.
const STATUS_LINK_UP: u16 = 1;

/// Size of `struct virtio_net_hdr_v1`.
///
/// Twelve, not ten: with `VIRTIO_F_VERSION_1` the header always carries
/// `num_buffers`, whether or not `MRG_RXBUF` was negotiated. Getting this wrong
/// shifts every frame by two bytes, which presents as a link that is up and
/// carries nothing but malformed packets.
pub const NET_HDR_LEN: usize = 12;

/// Ethernet MTU we advertise.
const MTU: u16 = 1500;

/// Largest frame we will move in either direction.
const MAX_FRAME: usize = 65_550;

/// How many received frames to hold when the guest is not consuming.
///
/// Bounded because the producer is the network and the consumer is a guest that
/// may be busy: an unbounded queue here is a memory leak driven by whoever is
/// sending us traffic. Dropping is also what a real NIC does when its ring is
/// full.
const RX_BACKLOG: usize = 1024;

/// How many bytes of transmitted frames to hold for the backend before the
/// device stops taking chains off the ring.
///
/// Large enough that a burst does not park the guest on every write to the
/// wire, small enough that a stalled backend does not turn into a memory
/// leak. Eight megabytes is under a millisecond of the link at the speeds we
/// are after.
const OUTBOX_BYTES: usize = 8 << 20;

/// Somewhere for frames to go, and come from.
pub trait NetBackend: Send {
    /// Sends one Ethernet frame.
    fn send(&mut self, frame: &[u8]) -> std::io::Result<()>;

    /// Sends a batch. The default is one call per frame; a backend with a
    /// cheaper way to move several at once overrides it.
    fn send_many(&mut self, frames: &[Vec<u8>]) -> std::io::Result<()> {
        for frame in frames {
            self.send(frame)?;
        }
        Ok(())
    }
}

/// Frames received from the backend, waiting for the guest to offer buffers.
pub type Inbox = Arc<Mutex<VecDeque<Vec<u8>>>>;

struct OutboxState {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    /// The device stopped taking chains because this was full. Whoever drains
    /// it owes the queue a look.
    parked: bool,
    closed: bool,
}

/// Frames the guest has transmitted, waiting for the backend's thread.
pub struct Outbox {
    state: Mutex<OutboxState>,
    filled: Condvar,
}

impl Outbox {
    pub fn new() -> Arc<Outbox> {
        Arc::new(Outbox {
            state: Mutex::new(OutboxState {
                frames: VecDeque::new(),
                bytes: 0,
                parked: false,
                closed: false,
            }),
            filled: Condvar::new(),
        })
    }

    /// Whether there is room for another frame.
    fn has_room(&self) -> bool {
        self.state.lock().expect("net outbox poisoned").bytes < OUTBOX_BYTES
    }

    fn push(&self, frame: Vec<u8>) {
        let mut state = self.state.lock().expect("net outbox poisoned");
        state.bytes += frame.len();
        state.frames.push_back(frame);
        drop(state);
        self.filled.notify_one();
    }

    /// Records that the device left chains on the ring for want of room.
    fn park(&self) {
        self.state.lock().expect("net outbox poisoned").parked = true;
    }

    /// Blocks until there are frames, then takes all of them. The flag says
    /// whether the device had parked on a full outbox, in which case the
    /// caller owes the transmit queue a look. None means closed.
    pub fn take(&self) -> Option<(Vec<Vec<u8>>, bool)> {
        let mut state = self.state.lock().expect("net outbox poisoned");
        while state.frames.is_empty() {
            if state.closed {
                return None;
            }
            state = self.filled.wait(state).expect("net outbox poisoned");
        }
        let frames: Vec<Vec<u8>> = state.frames.drain(..).collect();
        state.bytes = 0;
        let parked = std::mem::take(&mut state.parked);
        Some((frames, parked))
    }

    /// Releases a thread blocked in [`Outbox::take`].
    pub fn close(&self) {
        self.state.lock().expect("net outbox poisoned").closed = true;
        self.filled.notify_all();
    }
}

/// A virtio network device.
pub struct Net {
    outbox: Arc<Outbox>,
    inbox: Inbox,
    mac: [u8; 6],
    /// Frames dropped because the guest had no receive buffers posted.
    dropped: u64,
}

impl Net {
    pub fn new(outbox: Arc<Outbox>, mac: [u8; 6], inbox: Inbox) -> Net {
        Net {
            outbox,
            inbox,
            mac,
            dropped: 0,
        }
    }

    /// A shared inbox, to be filled by whatever reads from the backend.
    pub fn new_inbox() -> Inbox {
        Arc::new(Mutex::new(VecDeque::new()))
    }

    /// Queues a received frame for the guest, dropping it if the backlog is
    /// full. Returns false if the frame was dropped.
    pub fn enqueue_received(inbox: &Inbox, frame: Vec<u8>) -> bool {
        let mut inbox = inbox.lock().expect("net inbox poisoned");
        if inbox.len() >= RX_BACKLOG {
            return false;
        }
        inbox.push_back(frame);
        true
    }

    /// Copies frames the guest has queued into the outbox, without the
    /// virtio-net header the wire does not want. Stops, leaving the rest on
    /// the ring, when the outbox is full.
    fn transmit(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;

        while queue.more_available(mem) {
            if !self.outbox.has_room() {
                self.outbox.park();
                break;
            }
            let Some(chain) = queue.pop(mem) else {
                break;
            };
            let head = chain.head();
            let mut frame = Vec::with_capacity(1600);
            let mut skip = NET_HDR_LEN;

            for desc in chain {
                // Transmit buffers are device-readable; a writable one here is
                // a malformed request.
                if desc.is_write_only() {
                    continue;
                }
                let mut addr = desc.addr;
                let mut len = desc.len as usize;
                if skip > 0 {
                    let drop = skip.min(len);
                    addr += drop as u64;
                    len -= drop;
                    skip -= drop;
                    if len == 0 {
                        continue;
                    }
                }
                if frame.len() + len > MAX_FRAME {
                    break;
                }
                let start = frame.len();
                frame.resize(start + len, 0);
                if mem.read(addr, &mut frame[start..]).is_err() {
                    frame.truncate(start);
                    break;
                }
            }

            // A frame that was only a header carries nothing; it still has to
            // come back to the driver.
            if !frame.is_empty() {
                self.outbox.push(frame);
            }
            queue.push_used(mem, head, 0);
            used_any = true;
        }

        used_any
    }

    /// Fills guest receive buffers from the inbox.
    fn receive(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;

        loop {
            // Take one frame at a time and only when a buffer is available, so
            // a frame is never lost between the inbox and the guest.
            let has_frame = !self.inbox.lock().expect("net inbox poisoned").is_empty();
            if !has_frame {
                break;
            }
            let Some(chain) = queue.pop(mem) else {
                // No buffers posted. Leave the frame in the inbox; the guest
                // will notify us when it posts more.
                break;
            };
            let head = chain.head();
            let frame = self
                .inbox
                .lock()
                .expect("net inbox poisoned")
                .pop_front()
                .expect("checked non-empty above");

            let mut written = 0usize;
            let mut header_written = false;
            let mut offset = 0usize;

            for desc in chain {
                if !desc.is_write_only() {
                    continue;
                }
                let capacity = desc.len as usize;
                let mut buf = Vec::with_capacity(capacity);

                if !header_written {
                    // struct virtio_net_hdr_v1, all zero except num_buffers:
                    // no checksum offload and no segmentation, so there is
                    // nothing else to say about this frame.
                    let mut hdr = [0u8; NET_HDR_LEN];
                    hdr[10..12].copy_from_slice(&1u16.to_le_bytes());
                    buf.extend_from_slice(&hdr);
                    header_written = true;
                }

                let room = capacity.saturating_sub(buf.len());
                let take = room.min(frame.len() - offset);
                buf.extend_from_slice(&frame[offset..offset + take]);
                offset += take;

                if mem.write(desc.addr, &buf).is_err() {
                    break;
                }
                written += buf.len();

                if offset >= frame.len() {
                    break;
                }
            }

            if offset < frame.len() {
                // The guest's buffer was too small for the frame. Truncating is
                // the honest outcome; the alternative is holding it forever.
                self.dropped += 1;
                tracing::debug!(
                    frame_len = frame.len(),
                    written,
                    "receive buffer too small for frame"
                );
            }

            queue.push_used(mem, head, written as u32);
            used_any = true;
        }

        used_any
    }

    /// Frames dropped for want of a receive buffer.
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        device_type::NET
    }

    fn name(&self) -> &'static str {
        "virtio-net"
    }

    fn features(&self) -> u64 {
        // No offloads are advertised: the backend is a userspace stack that
        // wants ordinary complete frames, and claiming checksum or segmentation
        // support we do not implement produces corrupt traffic under load
        // rather than an honest failure at negotiation.
        COMMON_FEATURES | F_MAC | F_MTU | F_STATUS | F_CSUM
    }

    fn queue_count(&self) -> usize {
        2
    }

    /// `struct virtio_net_config`.
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; 12];
        config[0..6].copy_from_slice(&self.mac);
        config[6..8].copy_from_slice(&STATUS_LINK_UP.to_le_bytes());
        // max_virtqueue_pairs: one rx/tx pair.
        config[8..10].copy_from_slice(&1u16.to_le_bytes());
        config[10..12].copy_from_slice(&MTU.to_le_bytes());

        let start = offset as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = config.get(start + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        let used_any = match queue {
            TX_QUEUE => match queues.get_mut(TX_QUEUE as usize) {
                Some(q) => self.transmit(q, mem),
                None => false,
            },
            RX_QUEUE => match queues.get_mut(RX_QUEUE as usize) {
                Some(q) => self.receive(q, mem),
                None => false,
            },
            other => {
                tracing::debug!(queue = other, "net notified on an unknown queue");
                false
            }
        };
        Serviced::queue_if(queue, used_any)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> (Net, Arc<Outbox>, Inbox) {
        let outbox = Outbox::new();
        let inbox = Net::new_inbox();
        let net = Net::new(
            outbox.clone(),
            [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee],
            inbox.clone(),
        );
        (net, outbox, inbox)
    }

    #[test]
    fn config_reports_mac_link_and_mtu() {
        let (net, _, _) = net();
        let mut config = [0u8; 12];
        net.config_read(0, &mut config);
        assert_eq!(&config[0..6], &[0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee]);
        assert_eq!(
            u16::from_le_bytes(config[6..8].try_into().unwrap()),
            STATUS_LINK_UP
        );
        assert_eq!(u16::from_le_bytes(config[10..12].try_into().unwrap()), 1500);
    }

    /// The header is 12 bytes under VERSION_1 even without MRG_RXBUF. A
    /// 10-byte assumption shifts every frame and is invisible until traffic
    /// flows, so pin it.
    #[test]
    fn header_length_is_the_version_1_size() {
        assert_eq!(NET_HDR_LEN, 12);
    }

    #[test]
    fn does_not_advertise_offloads_it_cannot_perform() {
        let (net, _, _) = net();
        // Segmentation offload would have the guest hand us oversized frames
        // the backend cannot send.
        const F_HOST_TSO4: u64 = 1 << 11;
        const F_GUEST_TSO4: u64 = 1 << 7;
        const F_MRG_RXBUF: u64 = 1 << 15;
        assert_eq!(
            net.features() & (F_HOST_TSO4 | F_GUEST_TSO4 | F_MRG_RXBUF),
            0
        );
    }

    #[test]
    fn receive_backlog_is_bounded() {
        let (_net, _, inbox) = net();
        for _ in 0..RX_BACKLOG {
            assert!(Net::enqueue_received(&inbox, vec![0u8; 64]));
        }
        assert!(
            !Net::enqueue_received(&inbox, vec![0u8; 64]),
            "a full backlog must drop rather than grow"
        );
    }

    /// The outbox hands over everything it holds in one take, and reports
    /// whether the device had parked on it — which is what tells the drain
    /// thread to have the ring looked at again.
    #[test]
    fn outbox_takes_a_batch_and_reports_a_park() {
        let outbox = Outbox::new();
        outbox.push(vec![1, 2, 3]);
        outbox.push(vec![4]);
        let (frames, parked) = outbox.take().unwrap();
        assert_eq!(frames, vec![vec![1, 2, 3], vec![4]]);
        assert!(!parked);

        outbox.push(vec![5]);
        outbox.park();
        let (frames, parked) = outbox.take().unwrap();
        assert_eq!(frames, vec![vec![5]]);
        assert!(parked, "a park must be reported with the next batch");
        assert!(outbox.has_room());
    }

    /// A full outbox refuses room, and taking from it makes room again: the
    /// bound is bytes, not frames, because a frame is anything up to 64 KiB.
    #[test]
    fn outbox_is_bounded_by_bytes() {
        let outbox = Outbox::new();
        let frames = OUTBOX_BYTES / 65_536;
        for _ in 0..frames {
            assert!(outbox.has_room());
            outbox.push(vec![0u8; 65_536]);
        }
        assert!(!outbox.has_room(), "at the bound there is no room");
        let (taken, _) = outbox.take().unwrap();
        assert_eq!(taken.len(), frames);
        assert!(outbox.has_room());
    }

    /// Closing releases a blocked taker with None, so the drain thread exits
    /// when the machine does.
    #[test]
    fn closing_the_outbox_releases_a_taker() {
        let outbox = Outbox::new();
        let taker = {
            let outbox = outbox.clone();
            std::thread::spawn(move || outbox.take())
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
        outbox.close();
        assert!(taker.join().unwrap().is_none());
    }
}

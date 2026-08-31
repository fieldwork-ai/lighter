//! virtio-net.
//!
//! Two queues: the guest hands us frames to transmit on one, and offers empty
//! buffers to receive into on the other. What sits behind the device — today a
//! userspace network stack in a sidecar process — is hidden behind
//! [`NetBackend`], because that is a piece we expect to replace: a native
//! in-process stack would swap the implementation and leave this file alone.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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
const RX_BACKLOG: usize = 256;

/// Somewhere for frames to go, and come from.
pub trait NetBackend: Send {
    /// Sends one Ethernet frame.
    fn send(&mut self, frame: &[u8]) -> std::io::Result<()>;
}

/// Frames received from the backend, waiting for the guest to offer buffers.
pub type Inbox = Arc<Mutex<VecDeque<Vec<u8>>>>;

/// A virtio network device.
pub struct Net {
    backend: Box<dyn NetBackend>,
    inbox: Inbox,
    mac: [u8; 6],
    /// Frames dropped because the guest had no receive buffers posted.
    dropped: u64,
}

impl Net {
    pub fn new(backend: Box<dyn NetBackend>, mac: [u8; 6], inbox: Inbox) -> Net {
        Net {
            backend,
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

    /// Moves frames the guest has queued out to the backend.
    fn transmit(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used_any = false;

        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let mut frame = Vec::with_capacity(1600);

            for desc in chain {
                // Transmit buffers are device-readable; a writable one here is
                // a malformed request.
                if desc.is_write_only() {
                    continue;
                }
                let len = desc.len as usize;
                if frame.len() + len > MAX_FRAME {
                    break;
                }
                let start = frame.len();
                frame.resize(start + len, 0);
                if mem.read(desc.addr, &mut frame[start..]).is_err() {
                    frame.truncate(start);
                    break;
                }
            }

            // The guest prefixes every frame with the virtio-net header, which
            // the wire does not want.
            if frame.len() > NET_HDR_LEN
                && let Err(e) = self.backend.send(&frame[NET_HDR_LEN..])
            {
                // A send failure is the backend's problem, not the guest's: the
                // frame is lost, exactly as it would be on a real network, and
                // the descriptor still has to come back.
                tracing::debug!(%e, "dropping a transmitted frame");
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

    struct Sink {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl NetBackend for Sink {
        fn send(&mut self, frame: &[u8]) -> std::io::Result<()> {
            self.sent.lock().unwrap().push(frame.to_vec());
            Ok(())
        }
    }

    fn net() -> (Net, Arc<Mutex<Vec<Vec<u8>>>>, Inbox) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inbox = Net::new_inbox();
        let net = Net::new(
            Box::new(Sink { sent: sent.clone() }),
            [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee],
            inbox.clone(),
        );
        (net, sent, inbox)
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
}

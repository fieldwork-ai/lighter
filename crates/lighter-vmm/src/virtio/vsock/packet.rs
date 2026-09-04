//! The virtio-vsock wire format.
//!
//! Every packet in either direction is a fixed 44-byte header optionally
//! followed by payload. The header is little-endian throughout, and its last
//! two fields carry the flow control that the whole protocol depends on.
//!
//! This module is deliberately pure: it parses and builds bytes and knows
//! nothing about queues, connections, or the host. That is what makes the
//! protocol testable without a VM.

/// Size of `struct virtio_vsock_hdr`.
pub const HDR_LEN: usize = 44;

/// The largest payload we will carry in one packet.
///
/// The specification's limit. Also the unit the credit accounting below is
/// denominated in, so a larger value here would silently change flow control.
pub const MAX_PAYLOAD: usize = 256 * 1024;

/// The host is always CID 2. CID 0 and 1 are reserved (any/hypervisor), so the
/// first address a guest can have is 3.
pub const HOST_CID: u64 = 2;
pub const GUEST_CID: u64 = 3;

/// Socket type. Only streams are implemented: it is what a Docker socket, a
/// control channel, and every other thing we want are.
pub const TYPE_STREAM: u16 = 1;

/// Operations, from the specification's `virtio_vsock_op`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Op {
    Invalid = 0,
    /// Open a connection.
    Request = 1,
    /// Accept one.
    Response = 2,
    /// Refuse one, or tear down an established one abruptly.
    Rst = 3,
    /// Close one half of an established one.
    Shutdown = 4,
    /// Payload.
    Rw = 5,
    /// Here is my current credit.
    CreditUpdate = 6,
    /// Tell me yours.
    CreditRequest = 7,
}

impl Op {
    pub const fn from_raw(value: u16) -> Op {
        match value {
            1 => Op::Request,
            2 => Op::Response,
            3 => Op::Rst,
            4 => Op::Shutdown,
            5 => Op::Rw,
            6 => Op::CreditUpdate,
            7 => Op::CreditRequest,
            _ => Op::Invalid,
        }
    }
}

/// Shutdown flags, meaningful only on [`Op::Shutdown`].
pub mod shutdown {
    /// The sender will read no more.
    pub const RCV: u32 = 1;
    /// The sender will write no more.
    pub const SEND: u32 = 2;
    /// Both, which is a full close.
    pub const BOTH: u32 = RCV | SEND;
}

/// A parsed packet header, plus whatever payload came with it.
#[derive(Debug, Clone)]
pub struct Packet {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub op: Op,
    pub flags: u32,
    /// The sender's total receive buffer size.
    pub buf_alloc: u32,
    /// How many bytes the sender's application has consumed, ever, mod 2^32.
    pub fwd_cnt: u32,
    pub payload: Vec<u8>,
}

impl Packet {
    /// A packet with no payload, addressed from the host to the guest.
    pub fn control(op: Op, src_port: u32, dst_port: u32) -> Packet {
        Packet {
            src_cid: HOST_CID,
            dst_cid: GUEST_CID,
            src_port,
            dst_port,
            op,
            flags: 0,
            buf_alloc: 0,
            fwd_cnt: 0,
            payload: Vec::new(),
        }
    }

    /// Parses a header and payload as they arrived from the guest.
    ///
    /// Returns `None` for anything malformed rather than a partially-populated
    /// packet: a short header or a length field that overruns the buffer is a
    /// driver bug or an attack, and neither deserves a best effort.
    pub fn parse(bytes: &[u8]) -> Option<Packet> {
        if bytes.len() < HDR_LEN {
            return None;
        }
        let u32_at = |off: usize| {
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        };
        let u64_at = |off: usize| {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[off..off + 8]);
            u64::from_le_bytes(buf)
        };
        let u16_at = |off: usize| u16::from_le_bytes([bytes[off], bytes[off + 1]]);

        let len = u32_at(24) as usize;
        // The declared payload length must match what actually arrived. A
        // larger one would have us read the guest's memory past the chain.
        if len > MAX_PAYLOAD || bytes.len() < HDR_LEN + len {
            return None;
        }

        Some(Packet {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            op: Op::from_raw(u16_at(30)),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
            payload: bytes[HDR_LEN..HDR_LEN + len].to_vec(),
        })
    }

    /// The header alone, for a writer that places the payload itself.
    pub fn header_bytes(&self) -> [u8; HDR_LEN] {
        let mut out = [0u8; HDR_LEN];
        out[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        out[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        out[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        out[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        out[24..28].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out[28..30].copy_from_slice(&TYPE_STREAM.to_le_bytes());
        out[30..32].copy_from_slice(&(self.op as u16).to_le_bytes());
        out[32..36].copy_from_slice(&self.flags.to_le_bytes());
        out[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        out[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        out
    }

    /// A packet from a header already read and a payload already owned,
    /// for a reader that copied the payload out of the guest exactly once.
    /// `None` when the header's length does not match the payload given.
    pub fn from_parts(header: &[u8; HDR_LEN], payload: Vec<u8>) -> Option<Packet> {
        let u32_at = |off: usize| {
            u32::from_le_bytes([
                header[off],
                header[off + 1],
                header[off + 2],
                header[off + 3],
            ])
        };
        let u64_at = |off: usize| {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&header[off..off + 8]);
            u64::from_le_bytes(buf)
        };
        let u16_at = |off: usize| u16::from_le_bytes([header[off], header[off + 1]]);
        let len = u32_at(24) as usize;
        if len > MAX_PAYLOAD || payload.len() != len {
            return None;
        }
        Some(Packet {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            op: Op::from_raw(u16_at(30)),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
            payload,
        })
    }

    /// Serializes the packet for the guest's receive queue.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![0u8; HDR_LEN + self.payload.len()];
        out[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        out[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        out[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        out[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        out[24..28].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out[28..30].copy_from_slice(&TYPE_STREAM.to_le_bytes());
        out[30..32].copy_from_slice(&(self.op as u16).to_le_bytes());
        out[32..36].copy_from_slice(&self.flags.to_le_bytes());
        out[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        out[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        out[HDR_LEN..].copy_from_slice(&self.payload);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_forty_four_bytes() {
        // Every offset in this file is written against this number, and it is
        // fixed by the specification rather than by us.
        assert_eq!(HDR_LEN, 44);
        assert_eq!(Packet::control(Op::Request, 1, 2).to_bytes().len(), HDR_LEN);
    }

    #[test]
    fn round_trips_through_the_wire_format() {
        let packet = Packet {
            src_cid: HOST_CID,
            dst_cid: GUEST_CID,
            src_port: 0xdead,
            dst_port: 2375,
            op: Op::Rw,
            flags: 0,
            buf_alloc: 262_144,
            fwd_cnt: 4096,
            payload: b"GET /_ping HTTP/1.1\r\n".to_vec(),
        };
        let parsed = Packet::parse(&packet.to_bytes()).expect("should parse");
        assert_eq!(parsed.src_port, 0xdead);
        assert_eq!(parsed.dst_port, 2375);
        assert_eq!(parsed.op, Op::Rw);
        assert_eq!(parsed.buf_alloc, 262_144);
        assert_eq!(parsed.fwd_cnt, 4096);
        assert_eq!(parsed.payload, packet.payload);
    }

    /// A length field that overruns the buffer would otherwise have us read
    /// past the descriptor chain the guest gave us.
    #[test]
    fn rejects_a_length_longer_than_the_buffer() {
        let mut bytes = Packet::control(Op::Rw, 1, 2).to_bytes();
        bytes[24..28].copy_from_slice(&4096u32.to_le_bytes());
        assert!(Packet::parse(&bytes).is_none());
    }

    #[test]
    fn rejects_a_short_header() {
        assert!(Packet::parse(&[0u8; HDR_LEN - 1]).is_none());
    }

    #[test]
    fn rejects_a_payload_over_the_maximum() {
        let mut bytes = Packet::control(Op::Rw, 1, 2).to_bytes();
        bytes[24..28].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        bytes.resize(HDR_LEN + MAX_PAYLOAD + 1, 0);
        assert!(Packet::parse(&bytes).is_none());
    }

    #[test]
    fn unknown_operations_become_invalid_rather_than_panicking() {
        assert_eq!(Op::from_raw(9999), Op::Invalid);
    }
}

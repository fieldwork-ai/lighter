//! The USB/IP wire format, as Linux's `vhci-hcd` speaks it once a device is
//! attached (`drivers/usb/usbip/usbip_common.h`).
//!
//! Only the URB layer crosses the stream: the `OP_REQ_IMPORT` handshake that
//! `usbip attach` does over TCP has no counterpart here, since the agent
//! hands the stream to `vhci` through sysfs with the device already chosen.
//! Every header is 48 bytes, big-endian: twenty bytes common to all four
//! commands, then the command's own. A submit that sends data is followed by
//! it; the reply to one that receives is followed by what was received.

use std::fmt;

pub const HEADER_LEN: usize = 48;

pub const CMD_SUBMIT: u32 = 1;
pub const CMD_UNLINK: u32 = 2;
pub const RET_SUBMIT: u32 = 3;
pub const RET_UNLINK: u32 = 4;

pub const DIR_OUT: u32 = 0;
pub const DIR_IN: u32 = 1;

/// The largest transfer a submit may ask for. A guest's URBs are bounded by
/// its drivers' buffers (a serial driver's are a few KiB, a mass storage
/// one's a few hundred); anything past this is a corrupt stream, not a URB.
pub const MAX_TRANSFER: usize = 16 << 20;
/// The most isochronous packets a submit may carry, for the same reason.
pub const MAX_ISO_PACKETS: usize = 1024;
const ISO_DESCRIPTOR_LEN: usize = 16;

/// Linux errno values, negated, as URB statuses.
pub mod status {
    pub const OK: i32 = 0;
    pub const ENOENT: i32 = -2;
    pub const EIO: i32 = -5;
    pub const ENOMEM: i32 = -12;
    pub const EINVAL: i32 = -22;
    pub const EPIPE: i32 = -32;
    pub const ENODATA: i32 = -61;
    pub const EPROTO: i32 = -71;
    pub const EOVERFLOW: i32 = -75;
    pub const EOPNOTSUPP: i32 = -95;
    pub const ECONNRESET: i32 = -104;
    pub const ESHUTDOWN: i32 = -108;
    pub const ETIMEDOUT: i32 = -110;
    pub const EREMOTEIO: i32 = -121;
}

/// A command from the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Submit(Submit),
    Unlink { seqnum: u32, victim: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submit {
    pub seqnum: u32,
    pub devid: u32,
    /// `DIR_IN` or `DIR_OUT`.
    pub direction: u32,
    /// The endpoint number, without its direction bit.
    pub ep: u8,
    pub transfer_flags: u32,
    /// Bytes to receive (in) or the length of `data` (out).
    pub length: usize,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub interval: i32,
    pub setup: [u8; 8],
    /// What an out transfer sends.
    pub data: Vec<u8>,
    /// Isochronous packet descriptors, raw.
    pub iso: Vec<u8>,
}

impl Submit {
    pub fn is_in(&self) -> bool {
        self.direction == DIR_IN
    }

    /// The endpoint address, direction bit included, as USB writes it.
    pub fn endpoint_address(&self) -> u8 {
        if self.is_in() { self.ep | 0x80 } else { self.ep }
    }

    pub fn is_iso(&self) -> bool {
        self.number_of_packets > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    UnknownCommand(u32),
    BadLength(i64),
    BadPackets(i32),
    BadEndpoint(u32),
    BadDirection(u32),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnknownCommand(c) => write!(f, "unknown USB/IP command {c}"),
            ParseError::BadLength(l) => write!(f, "transfer length {l} out of range"),
            ParseError::BadPackets(n) => write!(f, "isochronous packet count {n} out of range"),
            ParseError::BadEndpoint(e) => write!(f, "endpoint {e} out of range"),
            ParseError::BadDirection(d) => write!(f, "direction {d} is neither in nor out"),
        }
    }
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("four bytes"))
}

/// Takes one whole command off the front of `buf`: `Ok(None)` while more
/// bytes are needed, and the number of bytes the command used with it. A
/// command that cannot be valid is an error, and the stream is over, since
/// nothing after it can be framed.
pub fn parse(buf: &[u8]) -> Result<Option<(Command, usize)>, ParseError> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let command = be32(buf, 0);
    let seqnum = be32(buf, 4);
    match command {
        CMD_UNLINK => Ok(Some((Command::Unlink { seqnum, victim: be32(buf, 20) }, HEADER_LEN))),
        CMD_SUBMIT => {
            let direction = be32(buf, 12);
            if direction != DIR_IN && direction != DIR_OUT {
                return Err(ParseError::BadDirection(direction));
            }
            let ep = be32(buf, 16);
            if ep > 15 {
                return Err(ParseError::BadEndpoint(ep));
            }
            let length = be32(buf, 24) as i32;
            if length < 0 || length as usize > MAX_TRANSFER {
                return Err(ParseError::BadLength(length as i64));
            }
            let number_of_packets = be32(buf, 32) as i32;
            // A non-isochronous URB has 0 packets; some clients write -1.
            if number_of_packets < -1 || number_of_packets as i64 > MAX_ISO_PACKETS as i64 {
                return Err(ParseError::BadPackets(number_of_packets));
            }
            let data_len = if direction == DIR_OUT { length as usize } else { 0 };
            let iso_len = number_of_packets.max(0) as usize * ISO_DESCRIPTOR_LEN;
            let total = HEADER_LEN + data_len + iso_len;
            if buf.len() < total {
                return Ok(None);
            }
            let mut setup = [0u8; 8];
            setup.copy_from_slice(&buf[40..48]);
            Ok(Some((
                Command::Submit(Submit {
                    seqnum,
                    devid: be32(buf, 8),
                    direction,
                    ep: ep as u8,
                    transfer_flags: be32(buf, 20),
                    length: length as usize,
                    start_frame: be32(buf, 28) as i32,
                    number_of_packets,
                    interval: be32(buf, 36) as i32,
                    setup,
                    data: buf[HEADER_LEN..HEADER_LEN + data_len].to_vec(),
                    iso: buf[HEADER_LEN + data_len..total].to_vec(),
                }),
                total,
            )))
        }
        other => Err(ParseError::UnknownCommand(other)),
    }
}

/// The reply to a submit: its status, what was transferred and, for an in
/// transfer, the bytes themselves. The common header's devid, direction and
/// endpoint are zero, as Linux's own server sends them.
pub fn ret_submit(seqnum: u32, status: i32, actual: usize, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    out.extend_from_slice(&RET_SUBMIT.to_be_bytes());
    out.extend_from_slice(&seqnum.to_be_bytes());
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&(actual as i32).to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes()); // start_frame
    // number_of_packets: vhci copies it into the URB, and a non-isochronous
    // URB has none.
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes()); // error_count
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(data);
    out
}

/// The reply to an unlink: `-ECONNRESET` when the URB was stopped here, and
/// its own reply is never sent; `0` when it had already completed, and its
/// reply is on its way or gone.
pub fn ret_unlink(seqnum: u32, status: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(&RET_UNLINK.to_be_bytes());
    out.extend_from_slice(&seqnum.to_be_bytes());
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&[0u8; 24]);
    out
}

/// Encodes a command, for tests and for a client that speaks to this server.
pub fn encode(command: &Command) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    match command {
        Command::Unlink { seqnum, victim } => {
            out.extend_from_slice(&CMD_UNLINK.to_be_bytes());
            out.extend_from_slice(&seqnum.to_be_bytes());
            out.extend_from_slice(&[0u8; 12]);
            out.extend_from_slice(&victim.to_be_bytes());
            out.extend_from_slice(&[0u8; 24]);
        }
        Command::Submit(s) => {
            out.extend_from_slice(&CMD_SUBMIT.to_be_bytes());
            out.extend_from_slice(&s.seqnum.to_be_bytes());
            out.extend_from_slice(&s.devid.to_be_bytes());
            out.extend_from_slice(&s.direction.to_be_bytes());
            out.extend_from_slice(&(s.ep as u32).to_be_bytes());
            out.extend_from_slice(&s.transfer_flags.to_be_bytes());
            out.extend_from_slice(&(s.length as i32).to_be_bytes());
            out.extend_from_slice(&s.start_frame.to_be_bytes());
            out.extend_from_slice(&s.number_of_packets.to_be_bytes());
            out.extend_from_slice(&s.interval.to_be_bytes());
            out.extend_from_slice(&s.setup);
            out.extend_from_slice(&s.data);
            out.extend_from_slice(&s.iso);
        }
    }
    out
}

/// A reply as the guest reads it, for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Submit { seqnum: u32, status: i32, actual: usize, data: Vec<u8> },
    Unlink { seqnum: u32, status: i32 },
}

/// Parses one reply; an in transfer's data is taken as `actual` bytes when
/// `is_in` says the URB it answers was an in transfer.
pub fn parse_reply(buf: &[u8], is_in: impl Fn(u32) -> bool) -> Option<(Reply, usize)> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let seqnum = be32(buf, 4);
    let status = be32(buf, 20) as i32;
    match be32(buf, 0) {
        RET_UNLINK => Some((Reply::Unlink { seqnum, status }, HEADER_LEN)),
        RET_SUBMIT => {
            let actual = be32(buf, 24) as i32 as usize;
            let data_len = if is_in(seqnum) { actual } else { 0 };
            if buf.len() < HEADER_LEN + data_len {
                return None;
            }
            Some((
                Reply::Submit { seqnum, status, actual, data: buf[HEADER_LEN..HEADER_LEN + data_len].to_vec() },
                HEADER_LEN + data_len,
            ))
        }
        _ => None,
    }
}

/// The standard requests a device's server must carry out itself rather
/// than pass to the device as a control transfer, because the host stack
/// owns the state they change (`stub_rx.c`'s `tweak_special_requests`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    SetConfiguration(u8),
    SetInterface { interface: u8, alternate: u8 },
    ClearHalt { endpoint: u8 },
    /// A port reset the guest's hub driver asks for: the device is reset.
    ResetDevice,
}

pub fn special(setup: &[u8; 8]) -> Option<Special> {
    let (request_type, request) = (setup[0], setup[1]);
    let value = u16::from_le_bytes([setup[2], setup[3]]);
    let index = u16::from_le_bytes([setup[4], setup[5]]);
    const SET_CONFIGURATION: u8 = 9;
    const SET_INTERFACE: u8 = 11;
    const CLEAR_FEATURE: u8 = 1;
    const SET_FEATURE: u8 = 3;
    const ENDPOINT_HALT: u16 = 0;
    const PORT_RESET: u16 = 4;
    match (request_type, request) {
        (0x00, SET_CONFIGURATION) => Some(Special::SetConfiguration(value as u8)),
        (0x01, SET_INTERFACE) => Some(Special::SetInterface { interface: index as u8, alternate: value as u8 }),
        (0x02, CLEAR_FEATURE) if value == ENDPOINT_HALT => Some(Special::ClearHalt { endpoint: index as u8 }),
        (0x23, SET_FEATURE) if value == PORT_RESET => Some(Special::ResetDevice),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit(seqnum: u32, direction: u32, ep: u8, length: usize, data: Vec<u8>) -> Submit {
        Submit {
            seqnum,
            devid: 0x0001_0002,
            direction,
            ep,
            transfer_flags: 0,
            length,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0; 8],
            data,
            iso: Vec::new(),
        }
    }

    #[test]
    fn a_submit_round_trips_with_its_data() {
        let out = Command::Submit(submit(7, DIR_OUT, 2, 5, b"hello".to_vec()));
        let bytes = encode(&out);
        assert_eq!(bytes.len(), HEADER_LEN + 5);
        assert_eq!(parse(&bytes).unwrap(), Some((out, HEADER_LEN + 5)));
        let inward = Command::Submit(submit(8, DIR_IN, 1, 64, Vec::new()));
        assert_eq!(parse(&encode(&inward)).unwrap(), Some((inward, HEADER_LEN)));
    }

    #[test]
    fn a_partial_command_waits_for_the_rest() {
        let bytes = encode(&Command::Submit(submit(1, DIR_OUT, 2, 100, vec![7; 100])));
        for cut in [0, 1, HEADER_LEN - 1, HEADER_LEN, HEADER_LEN + 99] {
            assert_eq!(parse(&bytes[..cut]).unwrap(), None, "cut at {cut}");
        }
    }

    #[test]
    fn two_commands_back_to_back_are_taken_one_at_a_time() {
        let mut bytes = encode(&Command::Submit(submit(1, DIR_OUT, 2, 3, vec![1, 2, 3])));
        bytes.extend(encode(&Command::Unlink { seqnum: 2, victim: 1 }));
        let (first, used) = parse(&bytes).unwrap().unwrap();
        assert!(matches!(first, Command::Submit(s) if s.data == [1, 2, 3]));
        let (second, _) = parse(&bytes[used..]).unwrap().unwrap();
        assert_eq!(second, Command::Unlink { seqnum: 2, victim: 1 });
    }

    #[test]
    fn a_corrupt_header_ends_the_stream() {
        let mut bytes = encode(&Command::Submit(submit(1, DIR_IN, 1, 64, Vec::new())));
        bytes[3] = 9;
        assert_eq!(parse(&bytes), Err(ParseError::UnknownCommand(9)));
        let mut bytes = encode(&Command::Submit(submit(1, DIR_IN, 1, 64, Vec::new())));
        bytes[24..28].copy_from_slice(&(-5i32).to_be_bytes());
        assert!(matches!(parse(&bytes), Err(ParseError::BadLength(-5))));
        let mut bytes = encode(&Command::Submit(submit(1, DIR_IN, 1, 64, Vec::new())));
        bytes[16..20].copy_from_slice(&16u32.to_be_bytes());
        assert_eq!(parse(&bytes), Err(ParseError::BadEndpoint(16)));
    }

    #[test]
    fn replies_are_what_the_guest_reads() {
        let reply = ret_submit(9, status::OK, 3, b"abc");
        assert_eq!(reply.len(), HEADER_LEN + 3);
        assert_eq!(
            parse_reply(&reply, |_| true),
            Some((Reply::Submit { seqnum: 9, status: 0, actual: 3, data: b"abc".to_vec() }, HEADER_LEN + 3))
        );
        let reply = ret_unlink(10, status::ECONNRESET);
        assert_eq!(parse_reply(&reply, |_| false), Some((Reply::Unlink { seqnum: 10, status: -104 }, HEADER_LEN)));
    }

    #[test]
    fn the_requests_the_server_carries_out_itself_are_recognised() {
        assert_eq!(special(&[0x00, 9, 1, 0, 0, 0, 0, 0]), Some(Special::SetConfiguration(1)));
        assert_eq!(special(&[0x01, 11, 2, 0, 3, 0, 0, 0]), Some(Special::SetInterface { interface: 3, alternate: 2 }));
        assert_eq!(special(&[0x02, 1, 0, 0, 0x81, 0, 0, 0]), Some(Special::ClearHalt { endpoint: 0x81 }));
        assert_eq!(special(&[0x23, 3, 4, 0, 1, 0, 0, 0]), Some(Special::ResetDevice));
        // A class request to an interface (CDC-ACM's SET_LINE_CODING) is the device's.
        assert_eq!(special(&[0x21, 0x20, 0, 0, 0, 0, 7, 0]), None);
        // CLEAR_FEATURE of anything but ENDPOINT_HALT is the device's.
        assert_eq!(special(&[0x02, 1, 1, 0, 0x81, 0, 0, 0]), None);
    }
}

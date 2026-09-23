//! VP9: a keyframe's uncompressed header, the `vpcC` record VideoToolbox
//! builds a session from, and nothing to reorder.
//!
//! A V4L2 VP9 buffer is one packet as a container stores it: a frame, or a
//! superframe (hidden frames bundled with the one shown after them), which is
//! also what VideoToolbox takes, one shown frame a sample. VP9 shows frames
//! in the order it decodes them, so order is decode order and the depth is
//! zero. VideoToolbox decodes it only once its supplemental decoder is
//! registered (`available`), and then in hardware on Apple silicon.

use super::bits::Bits;
use super::codec::{FormatDescription, Order, Parser, Unit};
use super::vt_sys as vt;

/// Registers VideoToolbox's VP9 decoder, once, and says whether this Mac
/// decodes VP9 in hardware.
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| unsafe {
        vt::VTRegisterSupplementalVideoDecoderIfAvailable(vt::kCMVideoCodecType_VP9);
        vt::VTIsHardwareDecodeSupported(vt::kCMVideoCodecType_VP9) != 0
    })
}

/// What a keyframe's uncompressed header says about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub profile: u8,
    pub bit_depth: u8,
    /// 4:2:0 is 1 in `vpcC`'s terms (co-located with luma is 0, 4:2:2 2,
    /// 4:4:4 3).
    pub chroma_subsampling: u8,
    pub full_range: bool,
    pub width: u32,
    pub height: u32,
}

/// Reads a keyframe's uncompressed header (VP9 bitstream 6.2). `None` for
/// a frame that is not a keyframe, or a header that ends first.
pub fn keyframe_header(data: &[u8]) -> Option<Header> {
    let mut r = Bits::new(data);
    if r.bits(2)? != 2 {
        return None; // frame_marker
    }
    let low = r.bit()? as u8;
    let high = r.bit()? as u8;
    let profile = (high << 1) | low;
    if profile == 3 {
        r.bit()?; // reserved_zero
    }
    if r.bit()? {
        return None; // show_existing_frame
    }
    if r.bit()? {
        return None; // frame_type: not a keyframe
    }
    r.bit()?; // show_frame
    r.bit()?; // error_resilient_mode
    if r.bits(24)? != 0x49_83_42 {
        return None; // frame_sync_code
    }
    let bit_depth = if profile >= 2 {
        if r.bit()? { 12 } else { 10 }
    } else {
        8
    };
    let color_space = r.bits(3)?;
    let (full_range, chroma_subsampling) = if color_space != 7 {
        // Not sRGB.
        let full_range = r.bit()?;
        let mut chroma_subsampling = 1;
        if profile == 1 || profile == 3 {
            let x = r.bit()?;
            let y = r.bit()?;
            r.bit()?; // reserved_zero
            chroma_subsampling = match (x, y) {
                (true, true) => 1,
                (true, false) => 2,
                _ => 3,
            };
        }
        (full_range, chroma_subsampling)
    } else {
        if profile == 1 || profile == 3 {
            r.bit()?; // reserved_zero
        }
        (true, 3)
    };
    let width = r.bits(16)? as u32 + 1;
    let height = r.bits(16)? as u32 + 1;
    Some(Header {
        profile,
        bit_depth,
        chroma_subsampling,
        full_range,
        width,
        height,
    })
}

/// The `vpcC` box's payload (VP Codec ISO Media File Format Binding,
/// version 1): what VideoToolbox reads a VP9 session's parameters from.
pub fn vpcc(h: &Header) -> Vec<u8> {
    vec![
        1, // version
        0,
        0,
        0, // flags
        h.profile,
        // A level of 0 says nothing, which VideoToolbox takes.
        0,
        (h.bit_depth << 4) | (h.chroma_subsampling << 1) | h.full_range as u8,
        2, // colour_primaries: unspecified
        2, // transfer_characteristics: unspecified
        2, // matrix_coefficients: unspecified
        0,
        0, // codec_initialization_data_size
    ]
}

/// A VP9 stream.
#[derive(Default)]
pub struct Stream {
    header: Option<Header>,
}

impl Parser for Stream {
    fn unit(&mut self, data: &[u8], seq: i64) -> Unit {
        let mut changed = false;
        if let Some(h) = keyframe_header(data)
            && self.header != Some(h)
        {
            self.header = Some(h);
            changed = true;
        }
        // Nothing decodes before the first keyframe.
        let sample = if self.header.is_some() {
            data.to_vec()
        } else {
            Vec::new()
        };
        let order: Order = (0, seq);
        Unit {
            changed,
            sample,
            order,
        }
    }

    fn format_description(&self) -> Option<Result<FormatDescription, vt::OSStatus>> {
        let h = self.header?;
        Some(FormatDescription::vp9(h.width, h.height, &vpcc(&h)))
    }

    fn reorder_depth(&self) -> Option<u32> {
        Some(0)
    }

    fn ten_bit(&self) -> bool {
        self.header.is_some_and(|h| h.bit_depth > 8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyframe_header_reads() {
        // Profile 0 keyframe, shown, 640x360: marker 10, profile 00,
        // show_existing 0, frame_type 0, show 1, error_res 0, sync code,
        // color_space 2 (bt709), color_range 0, 639 and 359.
        let mut bits = String::from("10" /* marker */);
        bits += "00"; // profile
        bits += "0"; // show_existing_frame
        bits += "0"; // frame_type key
        bits += "1"; // show_frame
        bits += "0"; // error_resilient
        bits += &format!("{:024b}", 0x49_83_42);
        bits += "010"; // color_space
        bits += "0"; // color_range
        bits += &format!("{:016b}", 639);
        bits += &format!("{:016b}", 359);
        while bits.len() % 8 != 0 {
            bits.push('0');
        }
        let bytes: Vec<u8> = bits
            .as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect();
        let h = keyframe_header(&bytes).expect("a keyframe");
        assert_eq!(
            (h.profile, h.bit_depth, h.width, h.height),
            (0, 8, 640, 360)
        );
        assert_eq!(vpcc(&h)[6], (8 << 4) | (1 << 1));
        // An inter frame (frame_type 1) is not a keyframe.
        let mut inter = bytes.clone();
        inter[0] |= 0b0000_0100;
        assert!(keyframe_header(&inter).is_none());
    }
}

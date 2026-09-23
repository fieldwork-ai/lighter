//! What the decoder needs from a codec: turning one input buffer into what
//! VideoToolbox decodes, where the picture sorts, and the format description
//! a session is created from.
//!
//! The session (`decoder.rs`) is codec-blind: it numbers access units, hands
//! them to VideoToolbox, reorders what comes back and fills CAPTURE buffers.
//! Everything that depends on the bitstream's syntax is behind [`Parser`].

use super::vt_sys as vt;

/// Presentation order: an epoch that each random-access restart begins,
/// then the picture's order within it.
pub type Order = (u64, i64);

/// One input buffer, as the codec read it.
pub struct Unit {
    /// The stream's parameters changed with this buffer, so the
    /// VideoToolbox session must be rebuilt before it is decoded.
    pub changed: bool,
    /// What VideoToolbox decodes, in the layout the format description
    /// says; empty when there is nothing to decode (parameter sets alone).
    pub sample: Vec<u8>,
    /// Where the picture sorts.
    pub order: Order,
}

pub trait Parser: Send {
    /// Reads one input buffer, the access unit numbered `seq` in decode
    /// order.
    fn unit(&mut self, data: &[u8], seq: i64) -> Unit;
    /// A format description for the stream as it stands; `None` until the
    /// parameter sets needed have been seen.
    fn format_description(&self) -> Option<Result<FormatDescription, vt::OSStatus>>;
    /// Frames the stream may hold back for reordering, when it says.
    fn reorder_depth(&self) -> Option<u32>;
    /// Whether the stream carries more than eight bits a sample.
    fn ten_bit(&self) -> bool;
}

/// A `CMFormatDescription`, released when dropped.
pub struct FormatDescription(pub vt::CMFormatDescriptionRef);

unsafe impl Send for FormatDescription {}

impl FormatDescription {
    /// The coded picture size.
    pub fn dimensions(&self) -> (u32, u32) {
        let d = unsafe { vt::CMVideoFormatDescriptionGetDimensions(self.0) };
        (d.width.max(0) as u32, d.height.max(0) as u32)
    }

    /// From parameter-set NAL units, headers included: two for H.264 (SPS,
    /// PPS), three for HEVC (VPS, SPS, PPS). Samples then carry four-byte
    /// lengths.
    pub fn from_parameter_sets(
        hevc: bool,
        sets: &[&[u8]],
    ) -> Result<FormatDescription, vt::OSStatus> {
        let ptrs: Vec<*const u8> = sets.iter().map(|s| s.as_ptr()).collect();
        let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
        let mut desc: vt::CMFormatDescriptionRef = std::ptr::null();
        let st = unsafe {
            if hevc {
                vt::CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    vt::kCFAllocatorDefault,
                    sets.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    std::ptr::null(),
                    &mut desc,
                )
            } else {
                vt::CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    vt::kCFAllocatorDefault,
                    sets.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut desc,
                )
            }
        };
        if st != 0 || desc.is_null() {
            return Err(st);
        }
        Ok(FormatDescription(desc))
    }
}

impl FormatDescription {
    /// A VP9 stream's, from its size and `vpcC` record, which VideoToolbox
    /// reads from the sample description's extension atoms.
    pub fn vp9(width: u32, height: u32, vpcc: &[u8]) -> Result<FormatDescription, vt::OSStatus> {
        let mut desc: vt::CMFormatDescriptionRef = std::ptr::null();
        let st = unsafe {
            let data = vt::CFDataCreate(
                vt::kCFAllocatorDefault,
                vpcc.as_ptr(),
                vpcc.len() as vt::CFIndex,
            );
            let key = vt::CFStringCreateWithCString(
                vt::kCFAllocatorDefault,
                c"vpcC".as_ptr(),
                vt::kCFStringEncodingUTF8,
            );
            let atoms = dictionary(&[key], &[data]);
            let extensions = dictionary(
                &[vt::kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms],
                &[atoms],
            );
            let st = vt::CMVideoFormatDescriptionCreate(
                vt::kCFAllocatorDefault,
                vt::kCMVideoCodecType_VP9,
                width as i32,
                height as i32,
                extensions,
                &mut desc,
            );
            for owned in [extensions, atoms, key, data] {
                vt::CFRelease(owned);
            }
            st
        };
        if st != 0 || desc.is_null() {
            return Err(st);
        }
        Ok(FormatDescription(desc))
    }
}

/// A CFDictionary of CF objects; the caller releases it.
unsafe fn dictionary(
    keys: &[*const std::ffi::c_void],
    values: &[*const std::ffi::c_void],
) -> vt::CFDictionaryRef {
    unsafe {
        vt::CFDictionaryCreate(
            vt::kCFAllocatorDefault,
            keys.as_ptr(),
            values.as_ptr(),
            keys.len() as vt::CFIndex,
            &raw const vt::kCFTypeDictionaryKeyCallBacks,
            &raw const vt::kCFTypeDictionaryValueCallBacks,
        )
    }
}

impl Drop for FormatDescription {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { vt::CFRelease(self.0) };
        }
    }
}

/// The NAL units of an Annex B byte stream, without their start codes.
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0usize;
    let mut start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                // Trim the zero byte of a four-byte start code, and any
                // trailing zero bytes, which the standard allows.
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                if end > s {
                    nals.push(&data[s..end]);
                }
            }
            i += 3;
            start = Some(i);
            continue;
        }
        i += 1;
    }
    if let Some(s) = start
        && s < data.len()
    {
        nals.push(&data[s..]);
    }
    nals
}

/// NAL units with a four-byte big-endian length in front of each, the
/// layout a format description built with a length size of four expects.
pub fn length_prefixed<'a>(nals: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_splits_at_both_start_codes_and_drops_trailing_zeros() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        assert_eq!(
            annexb_nals(&data),
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]
        );
        assert!(annexb_nals(&[]).is_empty());
        assert!(annexb_nals(&[0, 0, 1]).is_empty());
    }

    #[test]
    fn length_prefixed_puts_four_byte_lengths_in_front() {
        let nals: [&[u8]; 2] = [&[0x65, 1, 2, 3], &[0x06, 7]];
        assert_eq!(
            length_prefixed(nals.into_iter()),
            vec![0, 0, 0, 4, 0x65, 1, 2, 3, 0, 0, 0, 2, 0x06, 7]
        );
    }
}

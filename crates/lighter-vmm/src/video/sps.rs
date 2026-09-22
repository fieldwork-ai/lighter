//! Just enough of an H.264 sequence parameter set to know how many frames
//! the decoder must hold back to hand them out in presentation order.
//!
//! VideoToolbox returns frames in decode order when it is driven one sample
//! at a time, so a stream with B-frames comes back scrambled unless the
//! decoder reorders; how deep that reorder buffer must be is the stream's to
//! say, and it says it in the SPS (H.264 7.3.2.1.1 and E.1.1). Three answers,
//! in order of authority:
//!
//! - `max_num_reorder_frames` from the VUI's bitstream restriction, which
//!   x264, x265-in-H.264 and most encoders that emit B-frames write;
//! - zero when the stream cannot reorder at all: `pic_order_cnt_type` 2
//!   (output order is decode order, by definition) or a Baseline profile,
//!   which has no B-slices; cameras are mostly one or the other;
//! - otherwise unknown, and the decoder learns it from the stream.

/// What the decoder needs from an SPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    /// Frames to hold back; `None` when the SPS does not say.
    pub reorder: Option<u32>,
}

/// Parses an SPS NAL (header byte included). `None` if it is not one, or
/// ends before the fields this needs.
pub fn parse(nal: &[u8]) -> Option<Sps> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let mut r = Bits::new(&rbsp);
    let profile_idc = r.bits(8)? as u8;
    r.bits(8)?; // constraint flags
    r.bits(8)?; // level_idc
    r.ue()?; // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.bit()?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    let poc_type = r.ue()?;
    match poc_type {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            let n = r.ue()?;
            for _ in 0..n.min(255) {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if !r.bit()? {
        // frame_mbs_only_flag
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    if r.bit()? {
        // frame_cropping_flag
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let implied = (poc_type == 2 || profile_idc == 66).then_some(0);
    let vui = if r.bit().unwrap_or(false) {
        vui_reorder(&mut r)
    } else {
        None
    };
    Some(Sps {
        profile_idc,
        reorder: vui.or(implied),
    })
}

/// `max_num_reorder_frames`, if the VUI carries a bitstream restriction.
fn vui_reorder(r: &mut Bits) -> Option<u32> {
    if r.bit()? {
        // aspect_ratio_info_present_flag
        if r.bits(8)? == 255 {
            r.bits(16)?;
            r.bits(16)?;
        }
    }
    if r.bit()? {
        // overscan_info_present_flag
        r.bit()?;
    }
    if r.bit()? {
        // video_signal_type_present_flag
        r.bits(3)?;
        r.bit()?;
        if r.bit()? {
            r.bits(24)?;
        }
    }
    if r.bit()? {
        // chroma_loc_info_present_flag
        r.ue()?;
        r.ue()?;
    }
    if r.bit()? {
        // timing_info_present_flag
        r.bits(32)?;
        r.bits(32)?;
        r.bit()?;
    }
    let nal_hrd = r.bit()?;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.bit()?;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.bit()?; // low_delay_hrd_flag
    }
    r.bit()?; // pic_struct_present_flag
    if !r.bit()? {
        // bitstream_restriction_flag
        return None;
    }
    r.bit()?; // motion_vectors_over_pic_boundaries_flag
    r.ue()?; // max_bytes_per_pic_denom
    r.ue()?; // max_bits_per_mb_denom
    r.ue()?; // log2_max_mv_length_horizontal
    r.ue()?; // log2_max_mv_length_vertical
    let reorder = r.ue()?;
    r.ue()?; // max_dec_frame_buffering
    Some(reorder.min(16))
}

fn skip_hrd(r: &mut Bits) -> Option<()> {
    let count = r.ue()? + 1;
    r.bits(4)?;
    r.bits(4)?;
    for _ in 0..count.min(32) {
        r.ue()?;
        r.ue()?;
        r.bit()?;
    }
    r.bits(20)?; // four 5-bit length fields
    Some(())
}

fn skip_scaling_list(r: &mut Bits, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

/// The RBSP: the payload with emulation-prevention bytes (`00 00 03`)
/// removed.
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b == 1)
    }

    fn bits(&mut self, n: u32) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        Some(v)
    }

    /// Exp-Golomb, unsigned.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + rest) as u32)
    }

    /// Exp-Golomb, signed.
    fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // x264 (ffmpeg 8's libx264), High profile, 1920x1080, `-bf 2`, taken
    // from the gate's own clip: VUI with a bitstream restriction.
    const X264_HIGH_BF2: &[u8] = &[
        0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
    ];

    #[test]
    fn exp_golomb_reads_as_the_standard_writes_it() {
        // 1 -> 0; 010 -> 1; 011 -> 2; 00100 -> 3
        let data = [0b1010_0110, 0b0100_0000];
        let mut r = Bits::new(&data);
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.ue(), Some(1));
        assert_eq!(r.ue(), Some(2));
        assert_eq!(r.ue(), Some(3));
        let data = [0b0100_1100];
        let mut r = Bits::new(&data);
        assert_eq!(r.se(), Some(1));
        assert_eq!(r.se(), Some(-1));
    }

    #[test]
    fn emulation_prevention_is_removed() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0]), vec![0, 0, 1, 0, 0, 0]);
        assert_eq!(unescape(&[1, 0, 3]), vec![1, 0, 3]);
    }

    #[test]
    fn a_high_profile_sps_is_read_to_its_reorder_depth() {
        let sps = parse(X264_HIGH_BF2).expect("parses");
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.reorder, Some(2), "{sps:?}");
    }

    #[test]
    fn baseline_without_vui_reorders_nothing() {
        // Baseline, level 3.0, sps_id 0, log2_max_frame_num 0 (ue 1), poc
        // type 0, lsb 0, 1 ref, no gaps, 40x30 MBs, frame_mbs_only,
        // direct_8x8, no cropping, no VUI.
        let mut w = Writer::default();
        w.bits(66, 8);
        w.bits(0xc0, 8);
        w.bits(30, 8);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(1);
        w.bits(0, 1);
        w.ue(39);
        w.ue(29);
        w.bits(1, 1);
        w.bits(1, 1);
        w.bits(0, 1);
        w.bits(0, 1);
        let mut nal = vec![0x67];
        nal.extend(w.finish());
        assert_eq!(
            parse(&nal),
            Some(Sps {
                profile_idc: 66,
                reorder: Some(0)
            })
        );
        nal[1] = 77; // Main: may carry B-frames, and says nothing
        assert_eq!(
            parse(&nal),
            Some(Sps {
                profile_idc: 77,
                reorder: None
            })
        );
    }

    #[test]
    fn not_an_sps_is_none() {
        assert_eq!(parse(&[0x68, 1, 2]), None);
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0x67]), None);
    }

    #[derive(Default)]
    struct Writer {
        out: Vec<u8>,
        n: u32,
    }

    impl Writer {
        fn bits(&mut self, v: u64, n: u32) {
            for i in (0..n).rev() {
                if self.n.is_multiple_of(8) {
                    self.out.push(0);
                }
                let b = ((v >> i) & 1) as u8;
                *self.out.last_mut().unwrap() |= b << (7 - self.n % 8);
                self.n += 1;
            }
        }
        fn ue(&mut self, v: u32) {
            let x = u64::from(v) + 1;
            let len = 64 - x.leading_zeros();
            self.bits(0, len - 1);
            self.bits(x, len);
        }
        fn finish(mut self) -> Vec<u8> {
            self.bits(1, 1); // rbsp stop bit
            self.out
        }
    }
}

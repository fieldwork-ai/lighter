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
//!
//! What that order IS comes from each slice header's picture order count
//! (8.2.1), not from the timestamps the guest put on the packets, which a
//! client may set to anything.

use super::bits::{BitWriter, Bits, escape, unescape};
use super::codec::{FormatDescription, Order, Parser, Unit, annexb_nals, length_prefixed};
use super::vt_sys as vt;

/// What the decoder needs from an SPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sps {
    pub profile_idc: u8,
    /// Frames to hold back; `None` when the SPS does not say.
    pub reorder: Option<u32>,
    /// What a slice header is read with (7.3.3).
    pub separate_colour_plane: bool,
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub frame_mbs_only: bool,
    /// Where `vui_parameters_present_flag` sits, in bits into the RBSP.
    vui_at: usize,
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
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit()?;
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
    let log2_max_frame_num = r.ue()? + 4;
    let poc_type = r.ue()?;
    let mut log2_max_poc_lsb = 0;
    match poc_type {
        0 => {
            log2_max_poc_lsb = r.ue()? + 4;
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
    let frame_mbs_only = r.bit()?;
    if !frame_mbs_only {
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
    let vui_at = r.pos;
    let vui = if r.bit().unwrap_or(false) {
        vui_reorder(&mut r)
    } else {
        None
    };
    Some(Sps {
        profile_idc,
        reorder: vui.or(implied),
        separate_colour_plane,
        log2_max_frame_num,
        poc_type,
        log2_max_poc_lsb,
        frame_mbs_only,
        vui_at,
    })
}

/// The SPS with its VUI taken out: everything up to the VUI flag, the flag
/// cleared, and the stop bit. The VUI is colour, timing and buffering
/// advice the decoder does not need, and some cameras write it wrong: the
/// Reolink E1 Pro's runs 8 bits past its end, which ffmpeg shrugs off and
/// VideoToolbox refuses the whole stream over (-12710, native ffmpeg
/// included).
pub fn without_vui(nal: &[u8]) -> Option<Vec<u8>> {
    let sps = parse(nal)?;
    let rbsp = unescape(&nal[1..]);
    let mut r = Bits::new(&rbsp);
    let mut out = BitWriter::default();
    for _ in 0..sps.vui_at {
        out.bit(r.bit()?);
    }
    out.bit(false); // vui_parameters_present_flag
    out.bit(true); // rbsp_stop_one_bit
    let mut nal_out = vec![nal[0]];
    nal_out.extend(escape(&out.bytes));
    Some(nal_out)
}

/// What presentation order needs from a slice header (7.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slice {
    pub idr: bool,
    /// `nal_ref_idc` is nonzero.
    pub reference: bool,
    /// `pic_order_cnt_lsb`, when the SPS counts in type 0; else 0.
    pub poc_lsb: u32,
}

/// Reads a coded slice NAL's header (header byte included). `None` for any
/// other NAL, or one that ends first.
pub fn slice(nal: &[u8], sps: &Sps) -> Option<Slice> {
    let header = *nal.first()?;
    let kind = header & 0x1f;
    if kind != 1 && kind != 5 {
        return None;
    }
    // The fields sit in the first few bytes; 64 is generous.
    let rbsp = unescape(&nal[1..nal.len().min(64)]);
    let mut r = Bits::new(&rbsp);
    r.ue()?; // first_mb_in_slice
    r.ue()?; // slice_type
    r.ue()?; // pic_parameter_set_id
    if sps.separate_colour_plane {
        r.bits(2)?; // colour_plane_id
    }
    r.bits(sps.log2_max_frame_num)?; // frame_num
    if !sps.frame_mbs_only && r.bit()? {
        // field_pic_flag
        r.bit()?; // bottom_field_flag
    }
    if kind == 5 {
        r.ue()?; // idr_pic_id
    }
    let poc_lsb = if sps.poc_type == 0 {
        r.bits(sps.log2_max_poc_lsb)? as u32
    } else {
        0
    };
    Some(Slice {
        idr: kind == 5,
        reference: header & 0x60 != 0,
        poc_lsb,
    })
}

/// Picture order count, type 0 (8.2.1.1): the slice header's lsb, and an msb
/// carried from the previous reference picture across the lsb's wrap, both
/// reset by an IDR.
#[derive(Debug, Default)]
pub struct PocCounter {
    prev_msb: i64,
    prev_lsb: i64,
}

impl PocCounter {
    pub fn next(&mut self, slice: &Slice, log2_max_lsb: u32) -> i64 {
        if slice.idr {
            self.prev_msb = 0;
            self.prev_lsb = 0;
        }
        let max = 1i64 << log2_max_lsb;
        let lsb = i64::from(slice.poc_lsb);
        let msb = if lsb < self.prev_lsb && self.prev_lsb - lsb >= max / 2 {
            self.prev_msb + max
        } else if lsb > self.prev_lsb && lsb - self.prev_lsb > max / 2 {
            self.prev_msb - max
        } else {
            self.prev_msb
        };
        if slice.reference {
            self.prev_msb = msb;
            self.prev_lsb = lsb;
        }
        msb + lsb
    }
}

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// An H.264 stream: its parameter sets, and the counting that orders its
/// pictures.
#[derive(Default)]
pub struct Stream {
    sps: Vec<u8>,
    pps: Vec<u8>,
    info: Option<Sps>,
    /// IDRs seen: each restarts the picture order count.
    epoch: u64,
    poc: PocCounter,
}

impl Stream {
    /// Where an access unit sorts, from its first slice header: by picture
    /// order count for type 0, and by decode order otherwise, which is
    /// presentation order for type 2 by definition. Type 1 derives the
    /// count from `frame_num` and offsets in the SPS, and nothing seen in
    /// practice uses it with B-frames; it is left in decode order.
    fn order_of(&mut self, nals: &[&[u8]], seq: i64) -> Order {
        let Some(info) = self.info else {
            return (self.epoch, seq);
        };
        let Some(slice) = nals.iter().find_map(|n| slice(n, &info)) else {
            return (self.epoch, seq);
        };
        if slice.idr {
            self.epoch += 1;
        }
        if info.poc_type == 0 {
            (self.epoch, self.poc.next(&slice, info.log2_max_poc_lsb))
        } else {
            (self.epoch, seq)
        }
    }
}

impl Parser for Stream {
    fn unit(&mut self, data: &[u8], seq: i64) -> Unit {
        let nals = annexb_nals(data);
        let mut changed = false;
        for nal in &nals {
            match nal_type(nal) {
                NAL_SPS if self.sps.as_slice() != *nal => {
                    self.sps = nal.to_vec();
                    self.info = parse(nal);
                    changed = true;
                }
                NAL_PPS if self.pps.as_slice() != *nal => {
                    self.pps = nal.to_vec();
                    changed = true;
                }
                _ => {}
            }
        }
        // Every slice NAL (and SEI) goes to VideoToolbox length-prefixed;
        // the parameter sets are in the format description.
        let sample = length_prefixed(
            nals.iter()
                .copied()
                .filter(|n| !matches!(nal_type(n), NAL_SPS | NAL_PPS | NAL_AUD)),
        );
        let order = self.order_of(&nals, seq);
        Unit {
            changed,
            sample,
            order,
        }
    }

    fn format_description(&self) -> Option<Result<FormatDescription, vt::OSStatus>> {
        if self.sps.is_empty() || self.pps.is_empty() {
            return None;
        }
        Some(
            FormatDescription::from_parameter_sets(false, &[&self.sps, &self.pps]).or_else(|st| {
                let stripped = without_vui(&self.sps).ok_or(st)?;
                let desc = FormatDescription::from_parameter_sets(false, &[&stripped, &self.pps])?;
                tracing::info!(
                    st,
                    "VideoToolbox refused the stream's SPS; decoding it without its VUI"
                );
                Ok(desc)
            }),
        )
    }

    fn reorder_depth(&self) -> Option<u32> {
        self.info.and_then(|i| i.reorder)
    }

    fn ten_bit(&self) -> bool {
        false
    }
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

#[cfg(test)]
pub(crate) mod tests {
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
        // Baseline, level 3.0, sps_id 0, log2_max_frame_num 4, poc
        // type 0, log2_max_poc_lsb 4, 1 ref, no gaps, 40x30 MBs, frame_mbs_only,
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
        let sps = parse(&nal).expect("parses");
        assert_eq!((sps.profile_idc, sps.reorder), (66, Some(0)));
        assert_eq!(
            (sps.log2_max_frame_num, sps.poc_type, sps.log2_max_poc_lsb),
            (4, 0, 4)
        );
        nal[1] = 77; // Main: may carry B-frames, and says nothing
        assert_eq!(parse(&nal).map(|s| s.reorder), Some(None));
    }

    #[test]
    fn picture_order_wraps_and_resets() {
        let s = |idr, reference, poc_lsb| Slice {
            idr,
            reference,
            poc_lsb,
        };
        let mut poc = PocCounter::default();
        // A 4-bit lsb wraps at 16: I0 P4 b2 P12, then P2 is 18 and b0 is 16.
        assert_eq!(poc.next(&s(true, true, 0), 4), 0);
        assert_eq!(poc.next(&s(false, true, 4), 4), 4);
        assert_eq!(poc.next(&s(false, false, 2), 4), 2);
        assert_eq!(poc.next(&s(false, true, 12), 4), 12);
        assert_eq!(poc.next(&s(false, true, 2), 4), 18);
        assert_eq!(poc.next(&s(false, false, 0), 4), 16);
        assert_eq!(poc.next(&s(true, true, 0), 4), 0);
    }

    #[test]
    fn a_slice_header_reads_to_its_lsb() {
        let sps = parse(X264_HIGH_BF2).expect("parses");
        assert_eq!(sps.poc_type, 0);
        // A non-IDR reference slice: first_mb 0, P (5), pps 0, frame_num 3,
        // lsb 6.
        let mut w = Writer::default();
        w.ue(0);
        w.ue(5);
        w.ue(0);
        w.bits(3, sps.log2_max_frame_num);
        w.bits(6, sps.log2_max_poc_lsb);
        let mut nal = vec![0x41];
        nal.extend(w.finish());
        assert_eq!(
            slice(&nal, &sps),
            Some(Slice {
                idr: false,
                reference: true,
                poc_lsb: 6
            })
        );
        assert_eq!(slice(&[0x68, 0xff], &sps), None);
    }

    // The Reolink E1 Pro's sub stream: High, 896x512, and a VUI that ends
    // 8 bits early.
    pub(crate) const REOLINK_SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x33, 0xac, 0x15, 0x14, 0xa0, 0xe0, 0x10, 0x68, 0x40, 0x00, 0x01, 0x3d,
        0x80, 0x00, 0x18, 0xce,
    ];
    pub(crate) const REOLINK_PPS: &[u8] = &[0x68, 0xee, 0x3c, 0xb0];

    #[test]
    fn a_vui_comes_out_and_the_rest_stays() {
        let before = parse(REOLINK_SPS).expect("parses up to the VUI");
        let stripped = without_vui(REOLINK_SPS).expect("strips");
        let after = parse(&stripped).expect("still an SPS");
        assert_eq!(after.vui_at, before.vui_at);
        assert_eq!(
            (
                after.profile_idc,
                after.poc_type,
                after.log2_max_frame_num,
                after.frame_mbs_only
            ),
            (
                before.profile_idc,
                before.poc_type,
                before.log2_max_frame_num,
                before.frame_mbs_only
            )
        );
        // Nothing after the flag but the stop bit and padding.
        let rbsp = unescape(&stripped[1..]);
        let mut r = Bits::new(&rbsp);
        r.bits(after.vui_at as u32).unwrap();
        assert_eq!(r.bit(), Some(false));
        assert_eq!(r.bit(), Some(true));
        while let Some(b) = r.bit() {
            assert!(!b);
        }
    }

    #[test]
    fn escaping_undoes_unescaping() {
        let raw = [0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00, 0x05];
        assert_eq!(escape(&unescape(&raw)), raw);
        assert_eq!(escape(&[0, 0, 0, 0]), vec![0, 0, 3, 0, 0]);
    }

    #[test]
    fn a_sample_carries_the_slices_and_leaves_parameter_sets_out() {
        let data = [
            &[0u8, 0, 0, 1, 0x67, 9][..],
            &[0, 0, 0, 1, 0x68, 9],
            &[0, 0, 0, 1, 0x09, 0xf0],
            &[0, 0, 0, 1, 0x65, 1, 2, 3],
            &[0, 0, 0, 1, 0x06, 7],
        ]
        .concat();
        let unit = Stream::default().unit(&data, 0);
        assert!(unit.changed);
        assert_eq!(
            unit.sample,
            vec![0, 0, 0, 4, 0x65, 1, 2, 3, 0, 0, 0, 2, 0x06, 7]
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

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
//!
//! A stream may carry several parameter sets at once, each under its own id,
//! and a slice names the one it is coded with (7.4.1.2.1): the slice its PPS,
//! the PPS its SPS. So they are kept by id, and the SPS a slice names is the
//! one its order is read with and its format description is made from.

use super::bits::{BitWriter, Bits, escape, unescape};
use super::codec::{FormatDescription, Order, Parser, Unit, annexb_nals, length_prefixed};
use super::vt_sys as vt;
use std::collections::BTreeMap;

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
    // Both widths are at most 16 bits (7.4.2.1.1); anything wider is not an
    // SPS a slice header can be read with.
    let log2_max_frame_num = r.ue().filter(|&n| n <= 12)? + 4;
    let poc_type = r.ue()?;
    let mut log2_max_poc_lsb = 0;
    match poc_type {
        0 => {
            log2_max_poc_lsb = r.ue().filter(|&n| n <= 12)? + 4;
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

/// The id an SPS is known by (header byte included), at most 31.
fn sps_id(nal: &[u8]) -> Option<u32> {
    if nal_type(nal) != NAL_SPS {
        return None;
    }
    let rbsp = unescape(&nal[1..nal.len().min(16)]);
    let mut r = Bits::new(&rbsp);
    r.bits(24)?; // profile_idc, constraint flags, level_idc
    r.ue().filter(|&id| id <= 31)
}

/// A PPS's own id, at most 255, and the id of the SPS it names.
fn pps_ids(nal: &[u8]) -> Option<(u32, u32)> {
    if nal_type(nal) != NAL_PPS {
        return None;
    }
    let rbsp = unescape(&nal[1..nal.len().min(16)]);
    let mut r = Bits::new(&rbsp);
    let pps = r.ue().filter(|&id| id <= 255)?;
    let sps = r.ue().filter(|&id| id <= 31)?;
    Some((pps, sps))
}

/// The id of the PPS a coded slice names.
fn slice_pps_id(nal: &[u8]) -> Option<u32> {
    if !matches!(nal_type(nal), 1 | 5) {
        return None;
    }
    let rbsp = unescape(&nal[1..nal.len().min(16)]);
    let mut r = Bits::new(&rbsp);
    r.ue()?; // first_mb_in_slice
    r.ue()?; // slice_type
    r.ue()
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
    /// Each SPS by id, as sent and as read.
    sps: BTreeMap<u32, (Vec<u8>, Option<Sps>)>,
    /// Each PPS by id, as sent, with the id of the SPS it names.
    pps: BTreeMap<u32, (Vec<u8>, u32)>,
    /// The PPS the last slice named, or before any slice the last one sent.
    active: Option<u32>,
    /// Whether a slice has named one yet.
    named: bool,
    /// The SPS the session's format description was last made from.
    described: Option<u32>,
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
        let Some(info) = self.active_sps().and_then(|(_, _, info)| info) else {
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

    /// The SPS the active PPS names: its id, as sent, and as read.
    fn active_sps(&self) -> Option<(u32, &[u8], Option<Sps>)> {
        let (_, id) = self.pps.get(&self.active?)?;
        let (sent, info) = self.sps.get(id)?;
        Some((*id, sent, *info))
    }
}

impl Parser for Stream {
    fn unit(&mut self, data: &[u8], seq: i64) -> Unit {
        let nals = annexb_nals(data);
        // A parameter set sent again as it was changes nothing; one sent
        // with new contents may change what the session decodes.
        let mut changed = false;
        for nal in &nals {
            if let Some(id) = sps_id(nal)
                && self.sps.get(&id).is_none_or(|(sent, _)| sent != nal)
            {
                self.sps.insert(id, (nal.to_vec(), parse(nal)));
                changed = true;
            } else if let Some((id, sps)) = pps_ids(nal) {
                if self.pps.get(&id).is_none_or(|(sent, _)| sent != nal) {
                    self.pps.insert(id, (nal.to_vec(), sps));
                    changed = true;
                }
                if !self.named {
                    self.active = Some(id);
                }
            }
        }
        if let Some(id) = nals.iter().find_map(|n| slice_pps_id(n))
            && self.pps.contains_key(&id)
        {
            self.active = Some(id);
            self.named = true;
        }
        let sps = self.active_sps().map(|(id, _, _)| id);
        changed |= sps != self.described;
        self.described = sps;
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

    /// The active SPS and every PPS that names it, so that a slice may name
    /// any of them without a new description.
    fn format_description(&self) -> Option<Result<FormatDescription, vt::OSStatus>> {
        let (id, sps, _) = self.active_sps()?;
        let pps: Vec<&[u8]> = self
            .pps
            .values()
            .filter(|(_, named)| *named == id)
            .map(|(sent, _)| sent.as_slice())
            .collect();
        let with = |sps: &[u8]| {
            let sets: Vec<&[u8]> = std::iter::once(sps).chain(pps.iter().copied()).collect();
            FormatDescription::from_parameter_sets(false, &sets)
        };
        Some(with(sps).or_else(|st| {
            let stripped = without_vui(sps).ok_or(st)?;
            let desc = with(&stripped)?;
            tracing::info!(
                st,
                "VideoToolbox refused the stream's SPS; decoding it without its VUI"
            );
            Ok(desc)
        }))
    }

    fn reorder_depth(&self) -> Option<u32> {
        self.active_sps().and_then(|(_, _, info)| info?.reorder)
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

    /// Baseline, level 3.0, the given id, log2_max_frame_num 4 + `frame`,
    /// poc type 0, log2_max_poc_lsb 4 + `poc`, 1 ref, no gaps, 40x30 MBs,
    /// frame_mbs_only, direct_8x8, no cropping, no VUI.
    pub(crate) fn baseline_sps(id: u32, frame: u32, poc: u32) -> Vec<u8> {
        let mut w = Writer::default();
        w.bits(66, 8);
        w.bits(0xc0, 8);
        w.bits(30, 8);
        w.ue(id);
        w.ue(frame);
        w.ue(0);
        w.ue(poc);
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
        nal
    }

    /// A CABAC PPS with the given id, naming the given SPS, and
    /// `pic_init_qp_minus26` written as the Exp-Golomb code `qp`.
    pub(crate) fn pps(id: u32, sps: u32, qp: u32) -> Vec<u8> {
        let mut w = Writer::default();
        w.ue(id);
        w.ue(sps);
        w.bits(1, 1); // entropy_coding_mode_flag
        w.bits(0, 1); // bottom_field_pic_order_in_frame_present_flag
        w.ue(0); // num_slice_groups_minus1
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.bits(0, 3); // weighted_pred_flag, weighted_bipred_idc
        w.ue(qp); // pic_init_qp_minus26
        w.ue(0); // pic_init_qs_minus26
        w.ue(0); // chroma_qp_index_offset
        w.bits(0b100, 3); // deblocking control, constrained intra, redundant_pic_cnt
        let mut nal = vec![0x68];
        nal.extend(w.finish());
        nal
    }

    /// A reference slice header naming PPS `pps`, read with `sps`: a P
    /// slice, or an I slice of an IDR picture.
    fn slice_nal(pps: u32, sps: &Sps, poc_lsb: u64, idr: bool) -> Vec<u8> {
        let mut w = Writer::default();
        w.ue(0); // first_mb_in_slice
        w.ue(if idr { 7 } else { 5 }); // slice_type
        w.ue(pps);
        w.bits(0, sps.log2_max_frame_num);
        if idr {
            w.ue(0); // idr_pic_id
        }
        w.bits(poc_lsb, sps.log2_max_poc_lsb);
        let mut nal = vec![if idr { 0x65 } else { 0x41 }];
        nal.extend(w.finish());
        nal
    }

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| [&[0u8, 0, 0, 1][..], nal].concat())
            .collect()
    }

    #[test]
    fn baseline_without_vui_reorders_nothing() {
        let mut nal = baseline_sps(0, 0, 0);
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
        let data = annexb(&[
            X264_HIGH_BF2,
            REOLINK_PPS,
            &[0x09, 0xf0],
            &[0x65, 0x88, 2, 3],
            &[0x06, 7],
        ]);
        let unit = Stream::default().unit(&data, 0);
        assert!(unit.changed);
        assert_eq!(
            unit.sample,
            vec![0, 0, 0, 4, 0x65, 0x88, 2, 3, 0, 0, 0, 2, 0x06, 7]
        );
    }

    #[test]
    fn parameter_sets_sent_again_as_they_were_change_nothing() {
        let mut stream = Stream::default();
        let info = parse(X264_HIGH_BF2).unwrap();
        let first = annexb(&[X264_HIGH_BF2, REOLINK_PPS, &slice_nal(0, &info, 0, false)]);
        assert!(stream.unit(&first, 0).changed);
        assert!(!stream.unit(&first, 1).changed);
        // The same id with new contents is a change.
        let other = pps(0, 0, 1);
        assert!(
            stream
                .unit(&annexb(&[&other, &slice_nal(0, &info, 2, false)]), 2)
                .changed
        );
    }

    #[test]
    fn a_slice_is_read_with_the_sps_its_pps_names() {
        // SPS 0 is x264's 1080p, reordering 2 frames, with a 6-bit lsb;
        // SPS 1 is a 640x480 Baseline that reorders none, with an 8-bit one.
        let high = parse(X264_HIGH_BF2).unwrap();
        let base_nal = baseline_sps(1, 0, 4);
        let base = parse(&base_nal).unwrap();
        assert_ne!(high.log2_max_poc_lsb, base.log2_max_poc_lsb);
        let (pps0, pps1) = (pps(0, 0, 0), pps(1, 1, 0));
        let mut stream = Stream::default();
        stream.unit(&annexb(&[X264_HIGH_BF2, &pps0]), 0);
        let unit = stream.unit(&annexb(&[&slice_nal(0, &high, 4, false)]), 1);
        assert_eq!(stream.reorder_depth(), Some(2));
        assert_eq!(unit.order.1, 4);
        // Announcing the second pair ahead of its slices describes the
        // stream no differently: its slices still name the first.
        stream.unit(&annexb(&[&base_nal, &pps1]), 2);
        let unit = stream.unit(&annexb(&[&slice_nal(0, &high, 6, false)]), 3);
        assert!(!unit.changed);
        assert_eq!(stream.reorder_depth(), Some(2));
        assert_eq!(unit.order.1, 6);
        assert_eq!(
            stream.format_description().unwrap().unwrap().dimensions(),
            (1920, 1080)
        );
        // A slice naming PPS 1 moves to SPS 1: a new description, the
        // smaller size, and its own lsb width to read the slice with.
        let unit = stream.unit(&annexb(&[&slice_nal(1, &base, 100, true)]), 4);
        assert!(unit.changed);
        assert_eq!(stream.reorder_depth(), Some(0));
        assert_eq!(unit.order.1, 100);
        assert_eq!(
            stream.format_description().unwrap().unwrap().dimensions(),
            (640, 480)
        );
    }

    #[test]
    fn every_pps_of_the_active_sps_is_in_the_description() {
        // So a slice may switch between them, and back, with no new one.
        let info = parse(X264_HIGH_BF2).unwrap();
        let (pps0, pps1) = (pps(0, 0, 0), pps(1, 0, 2));
        let mut stream = Stream::default();
        stream.unit(
            &annexb(&[X264_HIGH_BF2, &pps0, &pps1, &slice_nal(0, &info, 0, false)]),
            0,
        );
        for (seq, id) in [(1, 1), (2, 0), (3, 1)] {
            let unit = stream.unit(
                &annexb(&[&slice_nal(id, &info, 2 * seq as u64, false)]),
                seq,
            );
            assert!(!unit.changed);
        }
        let desc = stream.format_description().unwrap().unwrap();
        let mut count = 0usize;
        let st = unsafe {
            vt::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                desc.0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut count,
                std::ptr::null_mut(),
            )
        };
        assert_eq!((st, count), (0, 3));
    }

    #[test]
    fn an_sps_wider_than_the_standard_allows_is_refused() {
        // A guest may send anything; 16 bits is the most either field takes.
        assert!(parse(&baseline_sps(0, 12, 12)).is_some());
        assert!(parse(&baseline_sps(0, 13, 0)).is_none());
        assert!(parse(&baseline_sps(0, 0, 13)).is_none());
        assert!(parse(&baseline_sps(0, u32::MAX - 1, 0)).is_none());
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

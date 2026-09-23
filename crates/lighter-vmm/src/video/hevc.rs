//! HEVC (H.265): the parameter sets, and the picture order count that says
//! where each picture is shown.
//!
//! What H.264's `h264.rs` does, with HEVC's syntax (ITU-T H.265): NAL
//! headers are two bytes, there are three parameter sets (VPS, SPS, PPS),
//! the reorder depth is always declared (`sps_max_num_reorder_pics`), and
//! the order count is type 0's lsb-and-msb, with two rules H.264 does not
//! have. A random-access picture that starts a stream (an IDR, a BLA, or a
//! CRA with nothing before it) resets the count and begins a new epoch; the
//! leading pictures a CRA or BLA was coded with (RASL) reference pictures
//! from before it, so when decoding starts there they cannot be decoded and
//! are dropped rather than sent. And only pictures at the lowest temporal
//! layer that are neither leading nor sub-layer non-reference carry the count
//! forward (8.3.1).

use super::bits::{Bits, unescape};
use super::codec::{FormatDescription, Order, Parser, Unit, annexb_nals, length_prefixed};
use super::vt_sys as vt;

const RASL_N: u8 = 8;
const RASL_R: u8 = 9;
const RADL_N: u8 = 6;
const RADL_R: u8 = 7;
const BLA_W_LP: u8 = 16;
const BLA_N_LP: u8 = 18;
const IDR_W_RADL: u8 = 19;
const IDR_N_LP: u8 = 20;
const CRA_NUT: u8 = 21;
const VPS: u8 = 32;
const SPS: u8 = 33;
const PPS: u8 = 34;
const AUD: u8 = 35;
const EOS: u8 = 36;
const EOB: u8 = 37;
const FD: u8 = 38;

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| (b >> 1) & 0x3f)
}

fn temporal_id(nal: &[u8]) -> u8 {
    nal.get(1).map_or(0, |b| (b & 7).saturating_sub(1))
}

fn is_vcl(kind: u8) -> bool {
    kind < 32
}

fn is_irap(kind: u8) -> bool {
    (BLA_W_LP..=23).contains(&kind)
}

/// Sub-layer non-reference: the even VCL types below 16.
fn is_sub_layer_non_reference(kind: u8) -> bool {
    kind < 16 && kind.is_multiple_of(2)
}

/// What the parser needs from an SPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sps {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u32,
    pub separate_colour_plane: bool,
    pub log2_max_poc_lsb: u32,
    /// `sps_max_num_reorder_pics` at the highest temporal layer.
    pub reorder: u32,
}

/// Parses an SPS NAL (its two-byte header included).
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    if nal_type(nal) != SPS || nal.len() < 3 {
        return None;
    }
    let rbsp = unescape(&nal[2..]);
    let mut r = Bits::new(&rbsp);
    r.bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)? as u32;
    r.bit()?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut r, max_sub_layers_minus1)?;
    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.ue()?;
    let mut separate_colour_plane = false;
    if chroma_format_idc == 3 {
        separate_colour_plane = r.bit()?;
    }
    let width = r.ue()?;
    let height = r.ue()?;
    if r.bit()? {
        // conformance_window_flag
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let bit_depth = r.ue()? + 8;
    r.ue()?; // bit_depth_chroma_minus8
    let log2_max_poc_lsb = r.ue()? + 4;
    let sub_layer_ordering_info_present = r.bit()?;
    let first = if sub_layer_ordering_info_present {
        0
    } else {
        max_sub_layers_minus1
    };
    let mut reorder = 0;
    for _ in first..=max_sub_layers_minus1 {
        r.ue()?; // sps_max_dec_pic_buffering_minus1
        reorder = r.ue()?; // sps_max_num_reorder_pics
        r.ue()?; // sps_max_latency_increase_plus1
    }
    Some(Sps {
        width,
        height,
        bit_depth,
        separate_colour_plane,
        log2_max_poc_lsb,
        reorder: reorder.min(16),
    })
}

/// profile_tier_level(1, max_sub_layers_minus1) (7.3.3).
fn skip_profile_tier_level(r: &mut Bits, max_sub_layers_minus1: u32) -> Option<()> {
    // general_profile_space .. general_level_idc: 2 + 1 + 5 + 32 + 4 + 43
    // + 1 + 8 bits.
    r.bits(8)?;
    r.bits(32)?;
    r.bits(48)?;
    r.bits(8)?;
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.bit()?;
        level_present[i] = r.bit()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.bits(2)?; // reserved_zero_2bits
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.bits(8)?;
            r.bits(32)?;
            r.bits(48)?;
        }
        if level_present[i] {
            r.bits(8)?;
        }
    }
    Some(())
}

/// What a slice segment header needs from a PPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pps {
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
}

pub fn parse_pps(nal: &[u8]) -> Option<Pps> {
    if nal_type(nal) != PPS || nal.len() < 3 {
        return None;
    }
    let rbsp = unescape(&nal[2..]);
    let mut r = Bits::new(&rbsp);
    r.ue()?; // pps_pic_parameter_set_id
    r.ue()?; // pps_seq_parameter_set_id
    r.bit()?; // dependent_slice_segments_enabled_flag
    let output_flag_present = r.bit()?;
    let num_extra_slice_header_bits = r.bits(3)? as u32;
    Some(Pps {
        output_flag_present,
        num_extra_slice_header_bits,
    })
}

/// The first slice segment of a picture: its `slice_pic_order_cnt_lsb`
/// (0 for an IDR, which carries none). `None` for a segment that does not
/// begin its picture, or a header that ends first.
pub fn first_segment_poc_lsb(nal: &[u8], sps: &Sps, pps: &Pps) -> Option<u32> {
    let kind = nal_type(nal);
    let rbsp = unescape(&nal[2..nal.len().min(64)]);
    let mut r = Bits::new(&rbsp);
    if !r.bit()? {
        // first_slice_segment_in_pic_flag
        return None;
    }
    if is_irap(kind) {
        r.bit()?; // no_output_of_prior_pics_flag
    }
    r.ue()?; // slice_pic_parameter_set_id
    // The first segment is never dependent, so the rest follows.
    for _ in 0..pps.num_extra_slice_header_bits {
        r.bit()?; // slice_reserved_flag
    }
    r.ue()?; // slice_type
    if pps.output_flag_present {
        r.bit()?; // pic_output_flag
    }
    if sps.separate_colour_plane {
        r.bits(2)?; // colour_plane_id
    }
    if kind == IDR_W_RADL || kind == IDR_N_LP {
        return Some(0);
    }
    Some(r.bits(sps.log2_max_poc_lsb)? as u32)
}

/// An HEVC stream.
#[derive(Default)]
pub struct Stream {
    vps: Vec<u8>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    sps_info: Option<Sps>,
    pps_info: Option<Pps>,
    epoch: u64,
    /// The previous picture at temporal layer 0 that carries the count.
    prev_lsb: i64,
    prev_msb: i64,
    /// Nothing decoded yet, or an end of sequence since: the next CRA
    /// starts a stream.
    at_start: bool,
    /// The last random-access picture started decoding (a CRA or BLA with
    /// nothing usable before it), so its RASL pictures are dropped.
    skip_rasl: bool,
    started: bool,
}

impl Stream {
    fn order_of(&mut self, nals: &[&[u8]], seq: i64) -> (Order, bool) {
        let (Some(sps), Some(pps)) = (self.sps_info, self.pps_info) else {
            return ((self.epoch, seq), false);
        };
        let Some((nal, lsb)) = nals.iter().find_map(|n| {
            (is_vcl(nal_type(n)))
                .then(|| first_segment_poc_lsb(n, &sps, &pps).map(|lsb| (*n, lsb)))
                .flatten()
        }) else {
            return ((self.epoch, seq), false);
        };
        let kind = nal_type(nal);
        if is_irap(kind) {
            let no_rasl_output = kind <= BLA_N_LP
                || kind == IDR_W_RADL
                || kind == IDR_N_LP
                || !self.started
                || self.at_start;
            self.at_start = false;
            self.started = true;
            self.skip_rasl = no_rasl_output && (kind == CRA_NUT || kind <= BLA_N_LP);
            if no_rasl_output {
                self.epoch += 1;
                self.prev_lsb = 0;
                self.prev_msb = 0;
                let poc = i64::from(lsb);
                self.prev_lsb = poc;
                return ((self.epoch, poc), false);
            }
        } else if !self.started {
            // A stream joined between random-access pictures: nothing here
            // can be decoded until one comes.
            return ((self.epoch, seq), true);
        }
        if (kind == RASL_N || kind == RASL_R) && self.skip_rasl {
            return ((self.epoch, seq), true);
        }
        let max = 1i64 << sps.log2_max_poc_lsb;
        let lsb = i64::from(lsb);
        let msb = if lsb < self.prev_lsb && self.prev_lsb - lsb >= max / 2 {
            self.prev_msb + max
        } else if lsb > self.prev_lsb && lsb - self.prev_lsb > max / 2 {
            self.prev_msb - max
        } else {
            self.prev_msb
        };
        let leading = matches!(kind, RADL_N | RADL_R | RASL_N | RASL_R);
        if temporal_id(nal) == 0 && !leading && !is_sub_layer_non_reference(kind) {
            self.prev_lsb = lsb;
            self.prev_msb = msb;
        }
        ((self.epoch, msb + lsb), false)
    }
}

impl Parser for Stream {
    fn unit(&mut self, data: &[u8], seq: i64) -> Unit {
        let nals = annexb_nals(data);
        let mut changed = false;
        for nal in &nals {
            let kind = nal_type(nal);
            let (slot, info_changed) = match kind {
                VPS => (&mut self.vps, false),
                SPS => (&mut self.sps, true),
                PPS => (&mut self.pps, true),
                EOS => {
                    self.at_start = true;
                    continue;
                }
                _ => continue,
            };
            if slot.as_slice() != *nal {
                *slot = nal.to_vec();
                changed = true;
                if info_changed {
                    self.sps_info = parse_sps(&self.sps);
                    self.pps_info = parse_pps(&self.pps);
                }
            }
        }
        let (order, drop) = self.order_of(&nals, seq);
        let sample = if drop {
            Vec::new()
        } else {
            length_prefixed(
                nals.iter()
                    .copied()
                    .filter(|n| !matches!(nal_type(n), VPS | SPS | PPS | AUD | EOS | EOB | FD)),
            )
        };
        Unit {
            changed,
            sample,
            order,
        }
    }

    fn format_description(&self) -> Option<Result<FormatDescription, vt::OSStatus>> {
        if self.vps.is_empty() || self.sps.is_empty() || self.pps.is_empty() {
            return None;
        }
        Some(FormatDescription::from_parameter_sets(
            true,
            &[&self.vps, &self.sps, &self.pps],
        ))
    }

    fn reorder_depth(&self) -> Option<u32> {
        self.sps_info.map(|s| s.reorder)
    }

    fn ten_bit(&self) -> bool {
        self.sps_info.is_some_and(|s| s.bit_depth > 8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // x265 (ffmpeg 8's libx265), 1280x720, 3 B-frames, 8- and 10-bit.
    const VPS8: &str = "40010c01ffff01600000030090000003000003005d959409";
    const SPS8: &str =
        "42010101600000030090000003000003005da00280802d16595964932bc05a020000030002000003003c10";
    const SPS10: &str =
        "42010102200000030090000003000003005da00280802d136595964932bc05a020000003002000000303c1";
    const PPS: &str = "4401c172b46240";

    #[test]
    fn x265_parameter_sets_read() {
        let sps = parse_sps(&hex(SPS8)).expect("SPS");
        assert_eq!((sps.width, sps.height, sps.bit_depth), (1280, 720, 8));
        assert!(sps.reorder >= 1, "{sps:?}");
        let sps10 = parse_sps(&hex(SPS10)).expect("10-bit SPS");
        assert_eq!(sps10.bit_depth, 10);
        let pps = parse_pps(&hex(PPS)).expect("PPS");
        assert_eq!(pps.num_extra_slice_header_bits, 0);
        assert!(parse_sps(&hex(VPS8)).is_none());
    }

    #[test]
    fn the_idr_starts_an_epoch_at_zero_and_trailing_pictures_count_up() {
        let mut s = Stream::default();
        let params: Vec<u8> = [VPS8, SPS8, PPS]
            .iter()
            .flat_map(|p| [&[0u8, 0, 0, 1][..], &hex(p)].concat())
            .collect();
        let unit = s.unit(&params, 0);
        assert!(unit.changed && unit.sample.is_empty());
        let au = |nal: &str| [&[0u8, 0, 0, 1][..], &hex(nal)].concat();
        // IDR_N_LP, then the first trailing pictures of the clip's GOP in
        // decode order (TRAIL_R, TRAIL_R, TRAIL_N, TRAIL_N).
        let idr = s.unit(&au("2801af08460d36b8ec898633"), 0);
        assert_eq!(idr.order, (1, 0));
        let orders: Vec<i64> = [
            "0201d02149e10c20423068e1",
            "0201e044957820483060ea8a",
            "0001e024fd7e89029182c798",
            "0001e066b5fd420503058e70",
        ]
        .iter()
        .enumerate()
        .map(|(i, n)| s.unit(&au(n), i as i64 + 1).order.1)
        .collect();
        // A B-pyramid: the anchor furthest ahead first, then the ones
        // between, each shown before what was decoded ahead of it.
        assert!(orders[0] > orders[1] && orders[1] > orders[2], "{orders:?}");
        assert!(orders.iter().all(|&o| o > 0), "{orders:?}");
    }

    #[test]
    fn pictures_before_the_first_random_access_point_are_dropped() {
        let mut s = Stream::default();
        for p in [VPS8, SPS8, PPS] {
            s.unit(&[&[0u8, 0, 0, 1][..], &hex(p)].concat(), 0);
        }
        let trail = s.unit(
            &[&[0u8, 0, 0, 1][..], &hex("0201d02149e10c20423068e1")].concat(),
            0,
        );
        assert!(
            trail.sample.is_empty(),
            "joined mid-GOP: nothing to decode yet"
        );
    }
}

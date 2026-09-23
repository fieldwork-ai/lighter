//! H.264 and HEVC encode on VideoToolbox, for the guest's V4L2 encoder
//! (`encoder_device.rs`).
//!
//! A raw frame from the guest is copied into a pixel buffer from the
//! session's own pool and handed to a compression session, which encodes
//! it on the media engine and calls back on a thread of its own. The
//! callback turns the sample into what a V4L2 encoder hands back, an Annex B
//! byte stream, with the parameter sets in front of every keyframe (the
//! device decides whether they go out there or in a buffer of their own),
//! and rings the device's doorbell: the encoded frame is delivered from the
//! device's thread, never from VideoToolbox's.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};

use super::vt_sys as vt;

/// Rung when VideoToolbox has encoded something; the device's thread
/// waits on it and delivers.
#[derive(Default)]
pub struct Doorbell {
    rung: Mutex<bool>,
    arrived: Condvar,
}

impl Doorbell {
    pub fn ring(&self) {
        *self.rung.lock().expect("doorbell poisoned") = true;
        self.arrived.notify_one();
    }

    /// Blocks until rung since the last wait returned.
    pub fn wait(&self) {
        let mut rung = self.rung.lock().expect("doorbell poisoned");
        while !*rung {
            rung = self.arrived.wait(rung).expect("doorbell poisoned");
        }
        *rung = false;
    }
}

/// The codecs the encoder produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coded {
    H264,
    Hevc,
}

/// How a raw frame from the guest is laid out: one V4L2 plane, luma rows
/// `bytesperline` apart and chroma after them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raw {
    /// Luma, then interleaved Cb/Cr, eight bits a sample.
    Nv12,
    /// Luma, then Cb, then Cr, each chroma row half as long.
    Yu12,
    /// NV12 at sixteen bits a sample, the value in the top ten.
    P010,
}

impl Raw {
    pub fn sample_bytes(self) -> usize {
        if self == Raw::P010 { 2 } else { 1 }
    }
}

/// H.264 profiles by their V4L2 menu index.
pub const H264_BASELINE: i64 = 0;
pub const H264_CONSTRAINED_BASELINE: i64 = 1;
pub const H264_MAIN: i64 = 2;
pub const H264_HIGH: i64 = 4;

/// What a compression session is created with.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub coded: Coded,
    pub size: (u32, u32),
    /// Encode ten bits a sample (HEVC Main 10).
    pub ten_bit: bool,
    pub h264_profile: i64,
    pub bitrate: u32,
    pub constant_bitrate: bool,
    /// Frames from one keyframe to the next; 0 leaves it to VideoToolbox.
    pub gop: u32,
    /// Whether frames may be reordered (B-frames).
    pub reorder: bool,
    /// Frame duration as a fraction of a second.
    pub frame_interval: (u32, u32),
    pub qp: Option<(u32, u32)>,
}

/// One encoded frame as the callback hands it over.
pub struct Encoded {
    /// The number the frame was sent with.
    pub seq: i64,
    /// Annex B, parameter sets not included.
    pub data: Vec<u8>,
    /// The parameter sets, Annex B, for a keyframe.
    pub headers: Option<Vec<u8>>,
    pub keyframe: bool,
}

/// What the callback writes into and rings.
pub struct Shared {
    pub out: Mutex<VecDeque<Encoded>>,
    pub bell: Arc<Doorbell>,
}

impl Shared {
    pub fn new(bell: Arc<Doorbell>) -> Arc<Shared> {
        Arc::new(Shared {
            out: Mutex::new(VecDeque::new()),
            bell,
        })
    }

    pub fn take(&self) -> Vec<Encoded> {
        self.out
            .lock()
            .expect("encoder output poisoned")
            .drain(..)
            .collect()
    }
}

unsafe extern "C" fn on_encoded(
    refcon: *mut c_void,
    source: *mut c_void,
    status: vt::OSStatus,
    _info: vt::VTEncodeInfoFlags,
    sample: vt::CMSampleBufferRef,
) {
    // SAFETY: the session's `Arc<Shared>`, alive until the session is
    // completed and invalidated.
    let shared = unsafe { &*(refcon as *const Shared) };
    let seq = source as i64;
    if status != 0 || sample.is_null() {
        tracing::debug!(status, seq, "VideoToolbox encoded nothing for a frame");
        return;
    }
    let Some(encoded) = (unsafe { annex_b(sample, seq) }) else {
        return;
    };
    tracing::trace!(
        seq,
        bytes = encoded.data.len(),
        keyframe = encoded.keyframe,
        "video encode out"
    );
    shared
        .out
        .lock()
        .expect("encoder output poisoned")
        .push_back(encoded);
    shared.bell.ring();
}

/// A sample's NAL units with start codes for their four-byte lengths, and
/// the parameter sets if it is a keyframe.
unsafe fn annex_b(sample: vt::CMSampleBufferRef, seq: i64) -> Option<Encoded> {
    unsafe {
        let block = vt::CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            return None;
        }
        let len = vt::CMBlockBufferGetDataLength(block);
        let mut avcc = vec![0u8; len];
        if vt::CMBlockBufferCopyDataBytes(block, 0, len, avcc.as_mut_ptr().cast()) != 0 {
            return None;
        }
        let keyframe = is_keyframe(sample);
        let headers = if keyframe {
            parameter_sets(vt::CMSampleBufferGetFormatDescription(sample))
        } else {
            None
        };
        Some(Encoded {
            seq,
            data: start_codes(&avcc),
            headers,
            keyframe,
        })
    }
}

/// A sample is a keyframe unless its attachments say `NotSync`.
unsafe fn is_keyframe(sample: vt::CMSampleBufferRef) -> bool {
    unsafe {
        let attachments = vt::CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if attachments.is_null() || vt::CFArrayGetCount(attachments) < 1 {
            return true;
        }
        let first = vt::CFArrayGetValueAtIndex(attachments, 0);
        let not_sync = vt::CFDictionaryGetValue(first, vt::kCMSampleAttachmentKey_NotSync.cast());
        not_sync.is_null() || vt::CFBooleanGetValue(not_sync) == 0
    }
}

/// Every parameter set in a format description, Annex B: SPS and PPS for
/// H.264, VPS, SPS and PPS for HEVC.
unsafe fn parameter_sets(desc: vt::CMFormatDescriptionRef) -> Option<Vec<u8>> {
    if desc.is_null() {
        return None;
    }
    let hevc =
        unsafe { vt::CMFormatDescriptionGetMediaSubType(desc) } == vt::kCMVideoCodecType_HEVC;
    let get = |i: usize, ptr: &mut *const u8, size: &mut usize, count: &mut usize| unsafe {
        let mut nal_len = 0i32;
        if hevc {
            vt::CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                desc,
                i,
                ptr,
                size,
                count,
                &mut nal_len,
            )
        } else {
            vt::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                desc,
                i,
                ptr,
                size,
                count,
                &mut nal_len,
            )
        }
    };
    let (mut ptr, mut size, mut count) = (std::ptr::null(), 0usize, 0usize);
    if get(0, &mut ptr, &mut size, &mut count) != 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut n = 0;
    for i in 0..count {
        if get(i, &mut ptr, &mut size, &mut n) != 0 || ptr.is_null() {
            return None;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
    }
    Some(out)
}

/// Replaces four-byte big-endian NAL lengths with start codes.
pub fn start_codes(avcc: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len());
    let mut at = 0usize;
    while at + 4 <= avcc.len() {
        let n = u32::from_be_bytes(avcc[at..at + 4].try_into().expect("4 bytes")) as usize;
        at += 4;
        let end = (at + n).min(avcc.len());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[at..end]);
        at = end;
    }
    out
}

/// A compression session.
pub struct VtEncoder {
    session: vt::VTCompressionSessionRef,
    /// Kept alive for the callback's sake: its address is the refcon.
    shared: Arc<Shared>,
    params: Params,
}

// The session is VideoToolbox's and used from one thread at a time.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new(params: &Params, shared: Arc<Shared>) -> Result<VtEncoder, vt::OSStatus> {
        let (w, h) = params.size;
        let codec = match params.coded {
            Coded::H264 => vt::kCMVideoCodecType_H264,
            Coded::Hevc => vt::kCMVideoCodecType_HEVC,
        };
        let format = if params.ten_bit {
            vt::kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
        } else {
            vt::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        } as i32;
        let mut session: vt::VTCompressionSessionRef = std::ptr::null();
        let st = unsafe {
            let number = vt::CFNumberCreate(
                vt::kCFAllocatorDefault,
                vt::kCFNumberSInt32Type,
                (&format as *const i32).cast(),
            );
            let source = dictionary(&[vt::kCVPixelBufferPixelFormatTypeKey], &[number]);
            let spec = dictionary(
                &[vt::kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder],
                &[vt::kCFBooleanTrue],
            );
            let st = vt::VTCompressionSessionCreate(
                vt::kCFAllocatorDefault,
                w as i32,
                h as i32,
                codec,
                spec,
                source,
                std::ptr::null(),
                Some(on_encoded),
                Arc::as_ptr(&shared) as *mut c_void,
                &mut session,
            );
            for owned in [spec, source, number] {
                vt::CFRelease(owned);
            }
            st
        };
        if st != 0 || session.is_null() {
            return Err(st);
        }
        let encoder = VtEncoder {
            session,
            shared,
            params: params.clone(),
        };
        encoder.configure(params)?;
        unsafe { vt::VTCompressionSessionPrepareToEncodeFrames(session) };
        tracing::debug!(?params, "video encode session");
        Ok(encoder)
    }

    fn set(&self, key: vt::CFStringRef, value: vt::CFTypeRef) -> vt::OSStatus {
        unsafe { vt::VTSessionSetProperty(self.session, key, value) }
    }

    fn set_number(&self, key: vt::CFStringRef, value: f64) -> vt::OSStatus {
        unsafe {
            let n = vt::CFNumberCreate(
                vt::kCFAllocatorDefault,
                vt::kCFNumberFloat64Type,
                (&value as *const f64).cast(),
            );
            let st = self.set(key, n);
            vt::CFRelease(n);
            st
        }
    }

    fn configure(&self, p: &Params) -> Result<(), vt::OSStatus> {
        let profile = unsafe {
            match (p.coded, p.ten_bit) {
                (Coded::Hevc, true) => vt::kVTProfileLevel_HEVC_Main10_AutoLevel,
                (Coded::Hevc, false) => vt::kVTProfileLevel_HEVC_Main_AutoLevel,
                (Coded::H264, _) => match p.h264_profile {
                    H264_BASELINE | H264_CONSTRAINED_BASELINE => {
                        vt::kVTProfileLevel_H264_Baseline_AutoLevel
                    }
                    H264_MAIN => vt::kVTProfileLevel_H264_Main_AutoLevel,
                    _ => vt::kVTProfileLevel_H264_High_AutoLevel,
                },
            }
        };
        let st = self.set(
            unsafe { vt::kVTCompressionPropertyKey_ProfileLevel },
            profile,
        );
        if st != 0 {
            return Err(st);
        }
        let reorder = if p.reorder {
            unsafe { vt::kCFBooleanTrue }
        } else {
            unsafe { vt::kCFBooleanFalse }
        };
        self.set(
            unsafe { vt::kVTCompressionPropertyKey_AllowFrameReordering },
            reorder,
        );
        self.set_rate(p);
        self.set_gop(p.gop);
        let (num, den) = p.frame_interval;
        if num > 0 && den > 0 {
            self.set_number(
                unsafe { vt::kVTCompressionPropertyKey_ExpectedFrameRate },
                f64::from(den) / f64::from(num),
            );
        }
        if let Some((min, max)) = p.qp {
            self.set_number(
                unsafe { vt::kVTCompressionPropertyKey_MinAllowedFrameQP },
                f64::from(min),
            );
            self.set_number(
                unsafe { vt::kVTCompressionPropertyKey_MaxAllowedFrameQP },
                f64::from(max),
            );
        }
        Ok(())
    }

    /// Bitrate and its mode, which apply to a running session.
    pub fn set_rate(&self, p: &Params) {
        let bps = f64::from(p.bitrate.max(1));
        if p.constant_bitrate
            && self.set_number(
                unsafe { vt::kVTCompressionPropertyKey_ConstantBitRate },
                bps,
            ) == 0
        {
            return;
        }
        let st = self.set_number(unsafe { vt::kVTCompressionPropertyKey_AverageBitRate }, bps);
        if st != 0 {
            tracing::debug!(st, "VideoToolbox refused the bitrate");
        }
    }

    pub fn set_gop(&self, gop: u32) {
        if gop > 0 {
            self.set_number(
                unsafe { vt::kVTCompressionPropertyKey_MaxKeyFrameInterval },
                f64::from(gop),
            );
        }
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Encodes one frame from the guest's buffer; `seq` comes back with it.
    pub fn encode(
        &self,
        src: &[u8],
        raw: Raw,
        strides: Strides,
        seq: i64,
        force_keyframe: bool,
    ) -> Result<(), vt::OSStatus> {
        let pool = unsafe { vt::VTCompressionSessionGetPixelBufferPool(self.session) };
        if pool.is_null() {
            return Err(-1);
        }
        let mut pb: vt::CVPixelBufferRef = std::ptr::null();
        let st = unsafe {
            vt::CVPixelBufferPoolCreatePixelBuffer(vt::kCFAllocatorDefault, pool, &mut pb)
        };
        if st != 0 || pb.is_null() {
            return Err(st);
        }
        unsafe {
            vt::CVPixelBufferLockBaseAddress(pb, 0);
            let mut planes = [(std::ptr::null_mut(), 0usize, 0usize); 2];
            for (i, plane) in planes.iter_mut().enumerate() {
                *plane = (
                    vt::CVPixelBufferGetBaseAddressOfPlane(pb, i).cast::<u8>(),
                    vt::CVPixelBufferGetBytesPerRowOfPlane(pb, i),
                    vt::CVPixelBufferGetHeightOfPlane(pb, i),
                );
            }
            let dst = planes.map(|(ptr, stride, rows)| {
                if ptr.is_null() {
                    &mut [][..]
                } else {
                    std::slice::from_raw_parts_mut(ptr, stride * rows)
                }
            });
            let [y, uv] = dst;
            fill(
                src,
                raw,
                strides,
                self.params.size,
                (y, planes[0].1),
                (uv, planes[1].1),
                self.params.ten_bit,
            );
            vt::CVPixelBufferUnlockBaseAddress(pb, 0);
        }
        let (num, den) = self.params.frame_interval;
        let (num, den) = if num > 0 && den > 0 {
            (num, den)
        } else {
            (1, 30)
        };
        let pts = vt::CMTime {
            value: seq * i64::from(num),
            timescale: den as i32,
            flags: vt::kCMTimeFlags_Valid,
            epoch: 0,
        };
        let duration = vt::CMTime {
            value: i64::from(num),
            ..pts
        };
        let props = if force_keyframe {
            unsafe {
                dictionary(
                    &[vt::kVTEncodeFrameOptionKey_ForceKeyFrame],
                    &[vt::kCFBooleanTrue],
                )
            }
        } else {
            std::ptr::null()
        };
        let mut info = 0u32;
        let st = unsafe {
            vt::VTCompressionSessionEncodeFrame(
                self.session,
                pb,
                pts,
                duration,
                props,
                seq as *mut c_void,
                &mut info,
            )
        };
        unsafe {
            if !props.is_null() {
                vt::CFRelease(props);
            }
            vt::CVPixelBufferRelease(pb);
        }
        if st != 0 { Err(st) } else { Ok(()) }
    }

    /// Returns once every frame sent has come back through the callback.
    pub fn complete(&self) {
        unsafe { vt::VTCompressionSessionCompleteFrames(self.session, vt::CMTime::INVALID) };
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        unsafe {
            vt::VTCompressionSessionCompleteFrames(self.session, vt::CMTime::INVALID);
            vt::VTCompressionSessionInvalidate(self.session);
            vt::CFRelease(self.session);
        }
        // Frames completed after the device last looked are dropped with
        // the session.
        drop(self.shared.take());
    }
}

/// A CFDictionary of CF objects; the caller releases it.
unsafe fn dictionary(keys: &[*const c_void], values: &[*const c_void]) -> vt::CFDictionaryRef {
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

/// Row pitches of a guest frame, in bytes: luma, and one chroma plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strides {
    pub luma: usize,
    pub chroma: usize,
}

impl Strides {
    /// The layout V4L2 defines for `bytesperline`, unless `bytes_used` says
    /// the frame was written with another. ffmpeg 5.1 is the writer that
    /// does: it copies its planes in one after the other at its own
    /// linesizes and never reads `bytesperline`, and those depend on where
    /// the frame came from (unpadded from some filters, rounded up to 16 or
    /// more from buffer pools, chroma rounded on its own). So the pitches
    /// that account for every byte written are found by trying each
    /// power-of-two alignment of the rows, smallest first.
    pub fn of(raw: Raw, bytesperline: usize, (w, h): (u32, u32), bytes_used: usize) -> Strides {
        let (w, h) = (w as usize, h as usize);
        let rows = h / 2;
        let size = |s: Strides| match raw {
            Raw::Yu12 => s.luma * h + 2 * s.chroma * rows,
            _ => s.luma * h + s.chroma * rows,
        };
        let declared = match raw {
            Raw::Yu12 => Strides {
                luma: bytesperline,
                chroma: bytesperline / 2,
            },
            _ => Strides {
                luma: bytesperline,
                chroma: bytesperline,
            },
        };
        if size(declared) == bytes_used {
            return declared;
        }
        let row = w * raw.sample_bytes();
        (0..=8)
            .map(|shift| 1usize << shift)
            .map(|align| match raw {
                Raw::Yu12 => Strides {
                    luma: row.next_multiple_of(align),
                    chroma: (w / 2).next_multiple_of(align),
                },
                _ => Strides {
                    luma: row.next_multiple_of(align),
                    chroma: row.next_multiple_of(align),
                },
            })
            .find(|&s| size(s) == bytes_used)
            .unwrap_or(declared)
    }
}

/// Copies a guest frame into a two-plane 4:2:0 picture, eight bits a
/// sample or sixteen (`ten_bit`). Chroma planes are interleaved on the way
/// for YU12, and samples change width where the formats differ: the top
/// eight of sixteen bits going down, the value at the top of sixteen going
/// up.
pub fn fill(
    src: &[u8],
    raw: Raw,
    strides: Strides,
    size: (u32, u32),
    (y, y_stride): (&mut [u8], usize),
    (uv, uv_stride): (&mut [u8], usize),
    ten_bit: bool,
) {
    let (w, h) = (size.0 as usize, size.1 as usize);
    let sb = raw.sample_bytes();
    let db = if ten_bit { 2 } else { 1 };
    let bytesperline = strides.luma;
    let luma = bytesperline * h;
    for row in 0..h {
        let s = src.get(row * bytesperline..row * bytesperline + w * sb);
        let d = y.get_mut(row * y_stride..row * y_stride + w * db);
        if let (Some(s), Some(d)) = (s, d) {
            convert(s, sb, d, db);
        }
    }
    for row in 0..h / 2 {
        let Some(d) = uv.get_mut(row * uv_stride..row * uv_stride + w * db) else {
            break;
        };
        match raw {
            Raw::Nv12 | Raw::P010 => {
                let at = luma + row * bytesperline;
                if let Some(s) = src.get(at..at + w * sb) {
                    convert(s, sb, d, db);
                }
            }
            Raw::Yu12 => {
                let half = strides.chroma;
                let u_at = luma + row * half;
                let v_at = luma + half * (h / 2) + row * half;
                let (Some(u), Some(v)) = (src.get(u_at..u_at + w / 2), src.get(v_at..v_at + w / 2))
                else {
                    break;
                };
                for (i, (&cb, &cr)) in u.iter().zip(v).enumerate() {
                    if db == 1 {
                        d[2 * i] = cb;
                        d[2 * i + 1] = cr;
                    } else {
                        d[4 * i..4 * i + 4].copy_from_slice(&[0, cb, 0, cr]);
                    }
                }
            }
        }
    }
}

/// One row of samples from `sb` bytes each to `db` bytes each.
fn convert(s: &[u8], sb: usize, d: &mut [u8], db: usize) {
    match (sb, db) {
        (1, 1) | (2, 2) => d.copy_from_slice(s),
        (2, 1) => {
            for (o, pair) in d.iter_mut().zip(s.as_chunks::<2>().0) {
                *o = pair[1];
            }
        }
        _ => {
            for (o, &v) in d.as_chunks_mut::<2>().0.iter_mut().zip(s) {
                *o = [0, v];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_become_start_codes() {
        let avcc = [0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x06];
        assert_eq!(
            start_codes(&avcc),
            vec![0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x06]
        );
    }

    /// A 4x2 frame: luma 0..8, then chroma.
    fn frame(raw: Raw) -> Vec<u8> {
        let mut f: Vec<u8> = (0..8).collect();
        match raw {
            Raw::Nv12 => f.extend([10, 20, 11, 21]),
            Raw::Yu12 => f.extend([10, 11, 20, 21]),
            Raw::P010 => {
                f = f.iter().flat_map(|&v| [0, v]).collect();
                f.extend([0, 10, 0, 20, 0, 11, 0, 21]);
            }
        }
        f
    }

    fn filled(raw: Raw, ten_bit: bool) -> (Vec<u8>, Vec<u8>) {
        let db = if ten_bit { 2 } else { 1 };
        // Destination rows padded to 8 samples.
        let (ys, uvs) = (8 * db, 8 * db);
        let mut y = vec![0xff; ys * 2];
        let mut uv = vec![0xff; uvs];
        let bpl = 4 * raw.sample_bytes();
        fill(
            &frame(raw),
            raw,
            Strides::of(raw, bpl, (4, 2), bpl * 3),
            (4, 2),
            (&mut y, ys),
            (&mut uv, uvs),
            ten_bit,
        );
        (y, uv)
    }

    #[test]
    fn every_input_layout_lands_as_two_planes() {
        for raw in [Raw::Nv12, Raw::Yu12, Raw::P010] {
            let (y, uv) = filled(raw, false);
            assert_eq!(&y[..4], &[0, 1, 2, 3], "{raw:?}");
            assert_eq!(&y[8..12], &[4, 5, 6, 7], "{raw:?}");
            assert_eq!(&uv[..4], &[10, 20, 11, 21], "{raw:?}");
            // Padding untouched.
            assert_eq!(y[4], 0xff);
        }
    }

    #[test]
    fn strides_come_from_bytes_used_when_the_writer_ignored_bytesperline() {
        // Unpadded, as some ffmpeg filters hand frames over.
        assert_eq!(
            Strides::of(Raw::Yu12, 1008, (1000, 562), 1000 * 562 * 3 / 2),
            Strides {
                luma: 1000,
                chroma: 500
            }
        );
        // As V4L2 lays it out.
        assert_eq!(
            Strides::of(Raw::Yu12, 1008, (1000, 562), 1008 * 562 * 3 / 2),
            Strides {
                luma: 1008,
                chroma: 504
            }
        );
        // ffmpeg: chroma rows 512, not 504.
        assert_eq!(
            Strides::of(Raw::Yu12, 1008, (1000, 562), 1008 * 562 + 2 * 512 * 281),
            Strides {
                luma: 1008,
                chroma: 512
            }
        );
        // NV12 written at a wider pitch.
        assert_eq!(
            Strides::of(Raw::Nv12, 1920, (1918, 1078), 2048 * 1617),
            Strides {
                luma: 2048,
                chroma: 2048
            }
        );
        // A short buffer is read as declared.
        assert_eq!(
            Strides::of(Raw::Nv12, 1920, (1918, 1078), 100),
            Strides {
                luma: 1920,
                chroma: 1920
            }
        );
    }

    #[test]
    fn eight_bits_sit_at_the_top_of_sixteen() {
        let (y, uv) = filled(Raw::Nv12, true);
        assert_eq!(&y[..4], &[0, 0, 0, 1]);
        assert_eq!(&uv[..4], &[0, 10, 0, 20]);
        let (y, _) = filled(Raw::P010, true);
        assert_eq!(&y[..4], &[0, 0, 0, 1]);
    }
}

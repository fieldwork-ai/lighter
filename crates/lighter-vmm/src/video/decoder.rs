//! The stateful V4L2 decoder, on VideoToolbox.
//!
//! The guest's ffmpeg sends H.264 as Annex B, one access unit per OUTPUT
//! buffer, and expects NV12 frames back on CAPTURE buffers, in presentation
//! order, with the input's timestamp on each. VideoToolbox wants the same
//! stream the other way round: parameter sets in a format description and
//! each NAL length-prefixed. So a buffer is split at its start codes, an SPS
//! or PPS updates the description (and a new resolution is a source-change
//! event the guest must answer with new CAPTURE buffers before anything
//! more is read back), and the rest goes to the session as one sample.
//! Decoding is synchronous, and VideoToolbox driven a sample at a time
//! hands frames back in decode order, so the session reorders them itself,
//! as ffmpeg's own VideoToolbox path does: frames wait in a buffer sorted by
//! presentation time until it holds more than the stream's reorder depth
//! (`sps.rs`), then the earliest goes out, into a CAPTURE buffer large
//! enough to take it.
//!
//! What this mirrors is `extras/ffmpeg-decoder` in the virtio-media tree,
//! the reference backend, with VideoToolbox where it has libavcodec.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use virtio_media::devices::video_decoder::{
    StreamParams, VideoDecoderBackend, VideoDecoderBackendEvent, VideoDecoderBackendSession,
    VideoDecoderBufferBacking, VideoDecoderSession,
};
use virtio_media::ioctl::IoctlResult;
use virtio_media::v4l2r::bindings;
use virtio_media::v4l2r::ioctl::V4l2MplaneFormat;
use virtio_media::v4l2r::{PixelFormat, QueueClass, QueueDirection, QueueType, Rect};

use super::vt_sys as vt;
use crate::memory::HOST_PAGE;

const H264: u32 = PixelFormat::from_fourcc(b"H264").to_u32();
const NV12: u32 = PixelFormat::from_fourcc(b"NV12").to_u32();

/// Room for one access unit of input: a 4K keyframe at a high bitrate is
/// a couple of megabytes.
const INPUT_SIZE: u32 = 4 << 20;
/// Before the stream has said, and what an ffmpeg that never sets a size
/// gets: any real stream then changes it with a source-change event.
const DEFAULT_CODED_SIZE: (u32, u32) = (320, 240);
/// Frames VideoToolbox may hold back for reordering, plus the guest's own.
const MIN_OUTPUT_BUFFERS: u32 = 4;

// ------------------------------------------------------------ buffers ----

/// One plane of a buffer: POSIX shared memory, mapped here for the decoder
/// and, through its fd, into the guest by the aperture.
pub struct Shm {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

// The mapping is this struct's own.
unsafe impl Send for Shm {}

impl Shm {
    fn new(size: usize) -> IoctlResult<Shm> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let len = size.div_ceil(HOST_PAGE) * HOST_PAGE;
        let name = format!(
            "/lighter-video-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let cname = std::ffi::CString::new(name).map_err(|_| libc::EINVAL)?;
        // SAFETY: plain libc calls on a name this process just made up.
        let raw = unsafe {
            libc::shm_open(
                cname.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::ENOMEM));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // The name is only needed to open it; the fd keeps it alive.
        unsafe { libc::shm_unlink(cname.as_ptr()) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::ENOMEM));
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::ENOMEM));
        }
        Ok(Shm {
            fd,
            ptr: ptr.cast(),
            len,
        })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `len` bytes for the life of `self`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` is the only writer on this side.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// A V4L2 buffer's planes.
pub struct Backing {
    planes: Vec<Shm>,
}

impl VideoDecoderBufferBacking for Backing {
    fn new(_queue: QueueType, _index: u32, sizes: &[usize]) -> IoctlResult<Self> {
        let planes = sizes
            .iter()
            .map(|&size| Shm::new(size))
            .collect::<IoctlResult<_>>()?;
        Ok(Backing { planes })
    }

    fn fd_for_plane(&self, plane_idx: usize) -> Option<BorrowedFd<'_>> {
        self.planes.get(plane_idx).map(|p| p.fd.as_fd())
    }
}

// ------------------------------------------------------------ bitstream --

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

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

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

// ------------------------------------------------------- VideoToolbox ----

/// A frame VideoToolbox handed back, held until a CAPTURE buffer takes it.
struct Decoded {
    pixels: vt::CVPixelBufferRef,
    pts: vt::CMTime,
}

unsafe impl Send for Decoded {}

impl Drop for Decoded {
    fn drop(&mut self) {
        if !self.pixels.is_null() {
            unsafe { vt::CVPixelBufferRelease(self.pixels) };
        }
    }
}

/// Where the output callback puts frames. Boxed and leaked to the session
/// as its refcon; freed with the session.
type Sink = Mutex<VecDeque<Decoded>>;

unsafe extern "C" fn on_frame(
    refcon: *mut c_void,
    _source: *mut c_void,
    status: vt::OSStatus,
    _info: vt::VTDecodeInfoFlags,
    image: vt::CVImageBufferRef,
    pts: vt::CMTime,
    _duration: vt::CMTime,
) {
    if status != 0 || image.is_null() {
        tracing::debug!(status, "VideoToolbox produced no frame");
        return;
    }
    tracing::trace!(pts = pts.as_micros(), "VideoToolbox frame out");
    // SAFETY: `refcon` is the `Arc<Sink>` pointer the session registered and
    // keeps alive until it is invalidated and drained.
    let sink = unsafe { &*(refcon as *const Sink) };
    unsafe { vt::CVPixelBufferRetain(image) };
    sink.lock()
        .expect("decoder sink poisoned")
        .push_back(Decoded { pixels: image, pts });
}

/// One decompression session for one set of parameter sets.
struct VtSession {
    session: vt::VTDecompressionSessionRef,
    desc: vt::CMFormatDescriptionRef,
    /// Kept alive for the callback's sake: its address is the refcon.
    _sink: Arc<Sink>,
    /// The coded picture size the SPS declares.
    dims: (u32, u32),
}

unsafe impl Send for VtSession {}

impl VtSession {
    fn new(sps: &[u8], pps: &[u8], sink: Arc<Sink>) -> Result<VtSession, vt::OSStatus> {
        let ptrs = [sps.as_ptr(), pps.as_ptr()];
        let sizes = [sps.len(), pps.len()];
        let mut desc: vt::CMFormatDescriptionRef = std::ptr::null();
        let st = unsafe {
            vt::CMVideoFormatDescriptionCreateFromH264ParameterSets(
                vt::kCFAllocatorDefault,
                2,
                ptrs.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut desc,
            )
        };
        if st != 0 || desc.is_null() {
            return Err(st);
        }
        let d = unsafe { vt::CMVideoFormatDescriptionGetDimensions(desc) };
        let dims = (d.width.max(0) as u32, d.height.max(0) as u32);

        // NV12 out, whatever the stream's own layout.
        let format = vt::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32;
        let (attrs, number) = unsafe {
            let number = vt::CFNumberCreate(
                vt::kCFAllocatorDefault,
                vt::kCFNumberSInt32Type,
                (&format as *const i32).cast(),
            );
            let keys = [vt::kCVPixelBufferPixelFormatTypeKey];
            let values = [number];
            let attrs = vt::CFDictionaryCreate(
                vt::kCFAllocatorDefault,
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &raw const vt::kCFTypeDictionaryKeyCallBacks,
                &raw const vt::kCFTypeDictionaryValueCallBacks,
            );
            (attrs, number)
        };
        let record = vt::VTDecompressionOutputCallbackRecord {
            callback: Some(on_frame),
            refcon: Arc::as_ptr(&sink) as *mut c_void,
        };
        let mut session: vt::VTDecompressionSessionRef = std::ptr::null();
        let st = unsafe {
            vt::VTDecompressionSessionCreate(
                vt::kCFAllocatorDefault,
                desc,
                std::ptr::null(),
                attrs,
                &record,
                &mut session,
            )
        };
        unsafe {
            vt::CFRelease(attrs);
            vt::CFRelease(number);
        }
        if st != 0 || session.is_null() {
            unsafe { vt::CFRelease(desc) };
            return Err(st);
        }
        Ok(VtSession {
            session,
            desc,
            _sink: sink,
            dims,
        })
    }

    /// Decodes one access unit, its NALs length-prefixed in `avcc`.
    fn decode(&self, avcc: &mut [u8], pts_us: i64) -> Result<(), vt::OSStatus> {
        let mut block: vt::CMBlockBufferRef = std::ptr::null();
        let st = unsafe {
            vt::CMBlockBufferCreateWithMemoryBlock(
                vt::kCFAllocatorDefault,
                avcc.as_mut_ptr().cast(),
                avcc.len(),
                vt::kCFAllocatorNull,
                std::ptr::null(),
                0,
                avcc.len(),
                0,
                &mut block,
            )
        };
        if st != 0 || block.is_null() {
            return Err(st);
        }
        let timing = vt::CMSampleTimingInfo {
            duration: vt::CMTime::INVALID,
            presentation_time_stamp: vt::CMTime::micros(pts_us),
            decode_time_stamp: vt::CMTime::INVALID,
        };
        let size = avcc.len();
        let mut sample: vt::CMSampleBufferRef = std::ptr::null();
        let st = unsafe {
            vt::CMSampleBufferCreateReady(
                vt::kCFAllocatorDefault,
                block,
                self.desc,
                1,
                1,
                &timing,
                1,
                &size,
                &mut sample,
            )
        };
        if st != 0 || sample.is_null() {
            unsafe { vt::CFRelease(block) };
            return Err(st);
        }
        // Synchronous, and reordered into presentation order: the callback
        // runs on this thread before the call returns, for this frame or
        // for an earlier one the reordering released.
        let mut info = 0u32;
        let st = unsafe {
            vt::VTDecompressionSessionDecodeFrame(
                self.session,
                sample,
                vt::kVTDecodeFrame_EnableTemporalProcessing,
                std::ptr::null_mut(),
                &mut info,
            )
        };
        unsafe {
            vt::CFRelease(sample);
            vt::CFRelease(block);
        }
        if st != 0 { Err(st) } else { Ok(()) }
    }

    /// Releases every frame the reordering still holds.
    fn flush(&self) {
        unsafe {
            vt::VTDecompressionSessionFinishDelayedFrames(self.session);
            vt::VTDecompressionSessionWaitForAsynchronousFrames(self.session);
        }
    }
}

impl Drop for VtSession {
    fn drop(&mut self) {
        unsafe {
            vt::VTDecompressionSessionInvalidate(self.session);
            vt::CFRelease(self.session);
            vt::CFRelease(self.desc);
        }
    }
}

// ------------------------------------------------------------- session ---

/// A CAPTURE buffer the guest has queued, ready for a frame.
struct Available {
    index: u32,
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for Available {}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Drain {
    None,
    /// The pipeline is flushed; the next free CAPTURE buffer carries LAST.
    AwaitingLast,
}

pub struct Session {
    /// The parameter sets the stream last carried; a change rebuilds the
    /// session.
    sps: Vec<u8>,
    pps: Vec<u8>,
    vt: Option<VtSession>,
    sink: Arc<Sink>,
    /// What the CAPTURE queue is formatted for. Follows the stream unless
    /// the guest set something larger.
    coded_size: (u32, u32),
    stream: StreamParams,
    /// CAPTURE buffers the guest has queued, ready for a frame.
    available: VecDeque<Available>,
    /// Frames VideoToolbox has returned, held until the reorder depth says
    /// the earliest of them can go; sorted by presentation time.
    reorder: Vec<Decoded>,
    /// Frames in presentation order, waiting for a CAPTURE buffer.
    ready: VecDeque<Decoded>,
    /// How many frames to hold back: the SPS's word, or learned.
    depth: u32,
    /// Whether `depth` came from the SPS, which is then never second-guessed.
    depth_declared: bool,
    /// The presentation time of the last frame handed on, to notice a
    /// stream that reorders more than was assumed.
    last_out: Option<i64>,
    events: VecDeque<VideoDecoderBackendEvent>,
    accepting_output: bool,
    drain: Drain,
}

impl Session {
    fn new() -> Session {
        Session {
            sps: Vec::new(),
            pps: Vec::new(),
            vt: None,
            sink: Arc::new(Mutex::new(VecDeque::new())),
            coded_size: DEFAULT_CODED_SIZE,
            stream: StreamParams {
                min_output_buffers: MIN_OUTPUT_BUFFERS,
                coded_size: DEFAULT_CODED_SIZE,
                visible_rect: Rect::new(0, 0, DEFAULT_CODED_SIZE.0, DEFAULT_CODED_SIZE.1),
            },
            available: VecDeque::new(),
            reorder: Vec::new(),
            ready: VecDeque::new(),
            depth: 0,
            depth_declared: false,
            last_out: None,
            events: VecDeque::new(),
            accepting_output: true,
            drain: Drain::None,
        }
    }

    /// Takes the parameter sets out of an access unit and reports whether
    /// they changed.
    fn absorb_parameter_sets(&mut self, nals: &[&[u8]]) -> bool {
        let mut changed = false;
        for nal in nals {
            match nal_type(nal) {
                NAL_SPS if self.sps.as_slice() != *nal => {
                    self.sps = nal.to_vec();
                    changed = true;
                }
                NAL_PPS if self.pps.as_slice() != *nal => {
                    self.pps = nal.to_vec();
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    /// The access unit as VideoToolbox takes it: every slice NAL (and SEI)
    /// with a four-byte length in front.
    fn avcc(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::with_capacity(nals.iter().map(|n| n.len() + 4).sum());
        for nal in nals {
            if matches!(nal_type(nal), NAL_SPS | NAL_PPS | NAL_AUD) {
                continue;
            }
            out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            out.extend_from_slice(nal);
        }
        out
    }

    /// Decodes one access unit.
    fn feed(&mut self, data: Vec<u8>, index: u32, pts_us: i64) -> IoctlResult<()> {
        let nals = annexb_nals(&data);
        let changed = self.absorb_parameter_sets(&nals);
        if (changed || self.vt.is_none()) && !self.sps.is_empty() && !self.pps.is_empty() {
            if let Some(old) = self.vt.take() {
                old.flush();
                self.collect(true);
            }
            match super::sps::parse(&self.sps) {
                Some(super::sps::Sps {
                    reorder: Some(n), ..
                }) => {
                    self.depth = n;
                    self.depth_declared = true;
                }
                // Unsaid: start at none, which costs a camera without
                // B-frames nothing, and learn from the first frame that
                // arrives behind one already handed on.
                _ => {
                    self.depth = 0;
                    self.depth_declared = false;
                }
            }
            tracing::debug!(
                depth = self.depth,
                declared = self.depth_declared,
                "video reorder depth"
            );
            match VtSession::new(&self.sps, &self.pps, self.sink.clone()) {
                Ok(vt) => {
                    let dims = vt.dims;
                    self.vt = Some(vt);
                    if dims != self.stream.coded_size {
                        tracing::info!(
                            from = format_args!(
                                "{}x{}",
                                self.stream.coded_size.0, self.stream.coded_size.1
                            ),
                            to = format_args!("{}x{}", dims.0, dims.1),
                            "video stream resolution"
                        );
                        self.stream.coded_size = dims;
                        self.stream.visible_rect = Rect::new(0, 0, dims.0, dims.1);
                        if dims.0 > self.coded_size.0 || dims.1 > self.coded_size.1 {
                            self.coded_size = dims;
                        }
                        // The guest answers with buffers of the new size;
                        // frames decoded meanwhile wait for them.
                        self.events
                            .push_back(VideoDecoderBackendEvent::StreamFormatChanged);
                    }
                }
                Err(st) => {
                    tracing::warn!(st, "VideoToolbox refused the stream's parameter sets");
                    self.events
                        .push_back(VideoDecoderBackendEvent::InputBufferDone {
                            buffer_id: index,
                            error: libc::EINVAL,
                        });
                    return Ok(());
                }
            }
        }
        let Some(vt) = &self.vt else {
            // Nothing decodable yet (no parameter sets): the buffer is
            // consumed and nothing comes of it.
            self.events
                .push_back(VideoDecoderBackendEvent::InputBufferDone {
                    buffer_id: index,
                    error: 0,
                });
            return Ok(());
        };
        let mut avcc = Session::avcc(&nals);
        tracing::trace!(
            index,
            pts_us,
            nals = nals.len(),
            bytes = avcc.len(),
            "video decode in"
        );
        let error = if avcc.is_empty() {
            0
        } else {
            match vt.decode(&mut avcc, pts_us) {
                Ok(()) => 0,
                Err(st) => {
                    tracing::debug!(st, "VideoToolbox rejected a frame");
                    // A bad unit in a live stream is skipped, not fatal.
                    0
                }
            }
        };
        self.events
            .push_back(VideoDecoderBackendEvent::InputBufferDone {
                buffer_id: index,
                error,
            });
        self.collect_decoded();
        self.emit_frames();
        Ok(())
    }

    /// Moves frames from the callback's sink into the reorder buffer, and
    /// from there to `ready` whatever the depth releases. `flush` releases
    /// everything, for a drain or a new stream.
    fn collect_decoded(&mut self) {
        self.collect(false);
    }

    fn collect(&mut self, flush: bool) {
        let arrived: Vec<Decoded> = self
            .sink
            .lock()
            .expect("decoder sink poisoned")
            .drain(..)
            .collect();
        for frame in arrived {
            let pts = frame.pts.as_micros().unwrap_or(0);
            if let Some(last) = self.last_out
                && pts < last
                && !self.depth_declared
                && self.depth < 16
            {
                self.depth += 1;
                tracing::info!(
                    depth = self.depth,
                    "video stream reorders; holding back one more frame"
                );
            }
            let at = self
                .reorder
                .partition_point(|f| f.pts.as_micros().unwrap_or(0) <= pts);
            self.reorder.insert(at, frame);
        }
        while !self.reorder.is_empty() && (flush || self.reorder.len() as u32 > self.depth) {
            let frame = self.reorder.remove(0);
            self.last_out = frame.pts.as_micros();
            self.ready.push_back(frame);
        }
    }

    /// Copies ready frames into queued CAPTURE buffers, oldest first.
    fn emit_frames(&mut self) {
        loop {
            if self.ready.is_empty() {
                if self.drain == Drain::AwaitingLast
                    && let Some(out) = self.available.pop_front()
                {
                    self.drain = Drain::None;
                    self.events
                        .push_back(VideoDecoderBackendEvent::FrameCompleted {
                            buffer_id: out.index,
                            timestamp: bindings::timeval {
                                tv_sec: 0,
                                tv_usec: 0,
                            },
                            bytes_used: vec![],
                            is_last: true,
                        });
                }
                return;
            }
            let Some(out) = self.available.pop_front() else {
                return;
            };
            // A buffer from before a resolution change is too small for
            // what is now decoded: the frame waits for the new ones.
            if out.len < nv12_size(self.coded_size.0, self.coded_size.1) {
                self.available.push_front(out);
                return;
            }
            let frame = self.ready.pop_front().expect("checked");
            let used = copy_nv12(&frame, self.coded_size, out.ptr, out.len);
            let pts = frame.pts.as_micros().unwrap_or(0);
            tracing::trace!(buffer = out.index, pts, used, "video frame out");
            self.events
                .push_back(VideoDecoderBackendEvent::FrameCompleted {
                    buffer_id: out.index,
                    timestamp: bindings::timeval {
                        tv_sec: pts.div_euclid(1_000_000),
                        tv_usec: pts.rem_euclid(1_000_000),
                    },
                    bytes_used: vec![used as u32],
                    is_last: false,
                });
        }
    }

    fn capture_format(&self) -> bindings::v4l2_pix_format_mplane {
        let (w, h) = self.coded_size;
        let mut plane_fmt: [bindings::v4l2_plane_pix_format; bindings::VIDEO_MAX_PLANES as usize] =
            Default::default();
        plane_fmt[0] = bindings::v4l2_plane_pix_format {
            bytesperline: w,
            sizeimage: nv12_size(w, h) as u32,
            reserved: Default::default(),
        };
        bindings::v4l2_pix_format_mplane {
            width: w,
            height: h,
            pixelformat: NV12,
            plane_fmt,
            num_planes: 1,
            ..format_filler()
        }
    }

    fn output_format(&self) -> bindings::v4l2_pix_format_mplane {
        let (w, h) = self.stream.coded_size;
        let mut plane_fmt: [bindings::v4l2_plane_pix_format; bindings::VIDEO_MAX_PLANES as usize] =
            Default::default();
        plane_fmt[0] = bindings::v4l2_plane_pix_format {
            bytesperline: 0,
            sizeimage: INPUT_SIZE,
            reserved: Default::default(),
        };
        bindings::v4l2_pix_format_mplane {
            width: w,
            height: h,
            pixelformat: H264,
            plane_fmt,
            num_planes: 1,
            ..format_filler()
        }
    }
}

fn nv12_size(w: u32, h: u32) -> usize {
    (w as usize) * (h as usize) * 3 / 2
}

/// Copies a frame into a CAPTURE buffer laid out as NV12 at `coded`, and
/// returns the bytes used. Rows beyond the frame are left as they were.
fn copy_nv12(frame: &Decoded, coded: (u32, u32), dst: *mut u8, dst_len: usize) -> usize {
    let (cw, ch) = (coded.0 as usize, coded.1 as usize);
    let need = cw * ch * 3 / 2;
    if dst_len < need {
        tracing::warn!(dst_len, need, "CAPTURE buffer smaller than the frame");
        return 0;
    }
    let pb = frame.pixels;
    unsafe {
        vt::CVPixelBufferLockBaseAddress(pb, vt::kCVPixelBufferLock_ReadOnly);
        let planes = vt::CVPixelBufferGetPlaneCount(pb);
        let dst = std::slice::from_raw_parts_mut(dst, dst_len);
        for plane in 0..planes.min(2) {
            let src = vt::CVPixelBufferGetBaseAddressOfPlane(pb, plane) as *const u8;
            let stride = vt::CVPixelBufferGetBytesPerRowOfPlane(pb, plane);
            let w = vt::CVPixelBufferGetWidthOfPlane(pb, plane).min(cw);
            let rows = vt::CVPixelBufferGetHeightOfPlane(pb, plane);
            let (dst_off, dst_rows) = if plane == 0 {
                (0, ch)
            } else {
                (cw * ch, ch / 2)
            };
            let bytes = if plane == 0 { w } else { w * 2 };
            for row in 0..rows.min(dst_rows) {
                let s = std::slice::from_raw_parts(src.add(row * stride), bytes.min(cw));
                let d = &mut dst[dst_off + row * cw..dst_off + row * cw + s.len()];
                d.copy_from_slice(s);
            }
        }
        vt::CVPixelBufferUnlockBaseAddress(pb, vt::kCVPixelBufferLock_ReadOnly);
    }
    need
}

fn format_filler() -> bindings::v4l2_pix_format_mplane {
    bindings::v4l2_pix_format_mplane {
        field: bindings::v4l2_field_V4L2_FIELD_NONE,
        flags: 0,
        colorspace: bindings::v4l2_colorspace_V4L2_COLORSPACE_DEFAULT,
        __bindgen_anon_1: bindings::v4l2_pix_format_mplane__bindgen_ty_1 {
            ycbcr_enc: bindings::v4l2_ycbcr_encoding_V4L2_YCBCR_ENC_DEFAULT as u8,
        },
        quantization: bindings::v4l2_quantization_V4L2_QUANTIZATION_DEFAULT as u8,
        xfer_func: bindings::v4l2_xfer_func_V4L2_XFER_FUNC_DEFAULT as u8,
        ..Default::default()
    }
}

impl VideoDecoderBackendSession for Session {
    type BufferStorage = Backing;

    fn decode(
        &mut self,
        input: &Backing,
        index: u32,
        timestamp: bindings::timeval,
        bytes_used: u32,
    ) -> IoctlResult<()> {
        let plane = input.planes.first().ok_or(libc::EINVAL)?;
        let len = (bytes_used as usize).min(plane.len);
        let data = plane.bytes()[..len].to_vec();
        let pts_us = timestamp.tv_sec.saturating_mul(1_000_000) + timestamp.tv_usec;
        self.feed(data, index, pts_us)
    }

    fn use_as_output(&mut self, index: u32, backing: &mut Backing) -> IoctlResult<()> {
        if !self.accepting_output {
            return Ok(());
        }
        let plane = backing.planes.first_mut().ok_or(libc::EINVAL)?;
        let len = plane.len;
        tracing::trace!(
            index,
            len,
            waiting = self.ready.len(),
            "CAPTURE buffer offered"
        );
        self.available.push_back(Available {
            index,
            ptr: plane.bytes_mut().as_mut_ptr(),
            len,
        });
        self.emit_frames();
        Ok(())
    }

    fn drain(&mut self) -> IoctlResult<()> {
        if let Some(vt) = &self.vt {
            vt.flush();
        }
        self.collect(true);
        tracing::debug!(
            ready = self.ready.len(),
            buffers = self.available.len(),
            "video drain"
        );
        self.drain = Drain::AwaitingLast;
        self.emit_frames();
        Ok(())
    }

    fn clear_output_buffers(&mut self) -> IoctlResult<()> {
        self.available.clear();
        self.events
            .retain(|e| !matches!(e, VideoDecoderBackendEvent::FrameCompleted { .. }));
        Ok(())
    }

    fn next_event(&mut self) -> Option<VideoDecoderBackendEvent> {
        self.events.pop_front()
    }

    fn current_format(&self, direction: QueueDirection) -> V4l2MplaneFormat {
        let pix_mp = match direction {
            QueueDirection::Output => self.output_format(),
            QueueDirection::Capture => self.capture_format(),
        };
        V4l2MplaneFormat::from((direction, pix_mp))
    }

    fn stream_params(&self) -> StreamParams {
        self.stream.clone()
    }

    fn streaming_state(&mut self, direction: QueueDirection, streaming: bool) {
        if direction == QueueDirection::Capture && streaming {
            self.accepting_output = true;
        }
    }
}

// ------------------------------------------------------------- backend ---

/// The decoder device: one per machine, a session per open of the node.
pub struct VideoToolboxDecoder;

impl VideoToolboxDecoder {
    pub fn new() -> VideoToolboxDecoder {
        VideoToolboxDecoder
    }
}

impl Default for VideoToolboxDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoDecoderBackend for VideoToolboxDecoder {
    type Session = Session;

    fn new_session(&mut self, _id: u32) -> IoctlResult<Session> {
        Ok(Session::new())
    }

    fn close_session(&mut self, _session: Session) {}

    fn enum_formats(
        &self,
        _session: &VideoDecoderSession<Session>,
        direction: QueueDirection,
        index: u32,
    ) -> Option<bindings::v4l2_fmtdesc> {
        if index != 0 {
            return None;
        }
        let pixelformat = match direction {
            QueueDirection::Output => H264,
            QueueDirection::Capture => NV12,
        };
        Some(bindings::v4l2_fmtdesc {
            index,
            type_: QueueType::from_dir_and_class(direction, QueueClass::VideoMplane) as u32,
            pixelformat,
            ..Default::default()
        })
    }

    fn frame_sizes(&self, pixel_format: u32) -> Option<bindings::v4l2_frmsize_stepwise> {
        (pixel_format == NV12 || pixel_format == H264).then_some(bindings::v4l2_frmsize_stepwise {
            min_width: 16,
            max_width: 8192,
            step_width: 2,
            min_height: 16,
            max_height: 8192,
            step_height: 2,
        })
    }

    fn adjust_format(
        &self,
        session: &Session,
        direction: QueueDirection,
        format: V4l2MplaneFormat,
    ) -> V4l2MplaneFormat {
        let pix_mp = match direction {
            QueueDirection::Output => session.output_format(),
            QueueDirection::Capture => {
                // The guest may ask for larger buffers than the stream
                // needs, never smaller.
                let mut f = session.capture_format();
                let asked: &bindings::v4l2_pix_format_mplane = format.as_ref();
                let (w, h) = (
                    asked.width.max(session.stream.coded_size.0),
                    asked.height.max(session.stream.coded_size.1),
                );
                f.width = w;
                f.height = h;
                f.plane_fmt[0].bytesperline = w;
                f.plane_fmt[0].sizeimage = nv12_size(w, h) as u32;
                f
            }
        };
        V4l2MplaneFormat::from((direction, pix_mp))
    }

    fn apply_format(
        &self,
        session: &mut Session,
        direction: QueueDirection,
        format: &V4l2MplaneFormat,
    ) {
        if direction == QueueDirection::Capture {
            let pix_mp: &bindings::v4l2_pix_format_mplane = format.as_ref();
            session.coded_size = (pix_mp.width, pix_mp.height);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_splits_at_both_start_codes_and_drops_trailing_zeros() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let nals = annexb_nals(&data);
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]
        );
        assert_eq!(nal_type(nals[0]), NAL_SPS);
        assert_eq!(nal_type(nals[1]), NAL_PPS);
        assert_eq!(nal_type(nals[2]), 5);
        assert!(annexb_nals(&[]).is_empty());
        assert!(annexb_nals(&[0, 0, 1]).is_empty());
    }

    #[test]
    fn avcc_prefixes_lengths_and_leaves_parameter_sets_out() {
        let nals: Vec<&[u8]> = vec![
            &[0x67, 9],
            &[0x68, 9],
            &[0x09, 0xf0],
            &[0x65, 1, 2, 3],
            &[0x06, 7],
        ];
        assert_eq!(
            Session::avcc(&nals),
            vec![0, 0, 0, 4, 0x65, 1, 2, 3, 0, 0, 0, 2, 0x06, 7]
        );
    }

    #[test]
    fn a_unit_before_any_parameter_set_is_consumed_and_produces_nothing() {
        let mut s = Session::new();
        s.feed(vec![0, 0, 1, 0x65, 1, 2], 3, 40_000).unwrap();
        assert!(matches!(
            s.events.pop_front(),
            Some(VideoDecoderBackendEvent::InputBufferDone {
                buffer_id: 3,
                error: 0
            })
        ));
        assert!(s.events.is_empty());
    }

    fn frame(pts_ms: i64) -> Decoded {
        Decoded {
            pixels: std::ptr::null(),
            pts: vt::CMTime::micros(pts_ms * 1000),
        }
    }

    fn ready_pts(s: &Session) -> Vec<i64> {
        s.ready
            .iter()
            .map(|f| f.pts.as_micros().unwrap() / 1000)
            .collect()
    }

    #[test]
    fn decode_order_comes_out_in_presentation_order_at_the_declared_depth() {
        let mut s = Session::new();
        s.depth = 2;
        s.depth_declared = true;
        // I P B B as x264 -bf 2 emits them: 0, 100, 33, 67 in decode order.
        for pts in [0, 100, 33, 67] {
            s.sink.lock().unwrap().push_back(frame(pts));
            s.collect_decoded();
        }
        assert_eq!(ready_pts(&s), vec![0, 33]);
        s.collect(true);
        assert_eq!(ready_pts(&s), vec![0, 33, 67, 100]);
    }

    #[test]
    fn an_undeclared_depth_is_learned_from_the_first_late_frame() {
        let mut s = Session::new();
        for pts in [0, 100, 33, 67, 200, 133, 167] {
            s.sink.lock().unwrap().push_back(frame(pts));
            s.collect_decoded();
        }
        // Both B-frames of the first group arrived behind 100, each raising
        // the depth by one, to the 2 this stream's SPS would have declared;
        // the frames before that are lost to order, those after are not.
        assert_eq!(s.depth, 2);
        assert_eq!(ready_pts(&s), vec![0, 100, 33, 67, 133]);
    }

    #[test]
    fn shared_memory_is_page_sized_and_writable_through_its_fd() {
        let mut shm = Shm::new(100).unwrap();
        assert_eq!(shm.len, HOST_PAGE);
        shm.bytes_mut()[0] = 7;
        assert_eq!(shm.bytes()[0], 7);
        assert!(shm.fd.as_raw_fd() >= 0);
    }

    #[test]
    fn nal_time_round_trips() {
        assert_eq!(vt::CMTime::micros(1_500_000).as_micros(), Some(1_500_000));
        assert_eq!(vt::CMTime::INVALID.as_micros(), None);
    }
}

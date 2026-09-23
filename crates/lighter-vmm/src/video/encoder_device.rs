//! The stateful V4L2 encoder: raw frames in on OUTPUT, H.264 or HEVC out
//! on CAPTURE, encoded on VideoToolbox (`encoder.rs`).
//!
//! The `virtio-media` crate carries a stateful decoder device and no
//! encoder, so this is one, in the decoder's shape: MMAP buffers in POSIX
//! shared memory placed in the aperture by the host mapper, a session per
//! open of the node, and events for every buffer handed back. What differs
//! is the direction and the controls. An encoder is configured almost
//! entirely through controls (bitrate, GOP, profile, how parameter sets are
//! delivered), which clients set and several read back, so they are all
//! here with their menus. Output is asynchronous: a frame goes to
//! VideoToolbox on `QBUF` and its OUTPUT buffer comes straight back, since
//! the pixels are copied, while the encoded frame arrives on VideoToolbox's
//! thread, which rings the doorbell; the device's thread then calls
//! `process_events`, which fills CAPTURE buffers.
//!
//! Clients met: ffmpeg's `h264_v4l2m2m`/`hevc_v4l2m2m`, which ask for header
//! mode SEPARATE (refused; see `HEADER_MODES`) and read B_FRAMES back as 0
//! or refuse to run, and
//! GStreamer's `v4l2h264enc`/`v4l2h265enc`, which take the defaults.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use virtio_media::io::{ReadFromDescriptorChain, WriteToDescriptorChain};
use virtio_media::ioctl::{IoctlResult, VirtioMediaIoctlHandler, virtio_media_dispatch_ioctl};
use virtio_media::mmap::MmapMappingManager;
use virtio_media::protocol::{
    DequeueBufferEvent, SessionEvent, SgEntry, V4l2Event, V4l2Ioctl, VIRTIO_MEDIA_MMAP_FLAG_RW,
};
use virtio_media::v4l2r::bindings;
use virtio_media::v4l2r::ioctl::{
    BufferCapabilities, BufferField, BufferFlags, CtrlId, CtrlWhich, EventType, MemoryConsistency,
    QueryCtrlFlags, SelectionFlags, SelectionTarget, SelectionType, SubscribeEventFlags,
    V4l2Buffer, V4l2PlanesWithBacking, V4l2PlanesWithBackingMut,
};
use virtio_media::v4l2r::memory::MemoryType;
use virtio_media::v4l2r::{PixelFormat, QueueClass, QueueDirection, QueueType};
use virtio_media::{
    VirtioMediaDevice, VirtioMediaDeviceSession, VirtioMediaEventQueue, VirtioMediaHostMemoryMapper,
};

use super::decoder::Backing;
use super::encoder::{Coded, Doorbell, Params, Raw, Shared, Strides, VtEncoder};
use virtio_media::devices::video_decoder::VideoDecoderBufferBacking;

const H264: u32 = PixelFormat::from_fourcc(b"H264").to_u32();
const HEVC: u32 = PixelFormat::from_fourcc(b"HEVC").to_u32();
const NV12: u32 = PixelFormat::from_fourcc(b"NV12").to_u32();
const YU12: u32 = PixelFormat::from_fourcc(b"YU12").to_u32();
const P010: u32 = PixelFormat::from_fourcc(b"P010").to_u32();

const RAW_FORMATS: [u32; 3] = [NV12, YU12, P010];
const CODED_FORMATS: [u32; 2] = [H264, HEVC];

const DEFAULT_SIZE: (u32, u32) = (640, 480);
const MAX_DIMENSION: u32 = 8192;
/// Room for one encoded frame: never more than the raw frame in practice,
/// and a floor for small pictures whose first keyframe carries headers.
const MIN_CODED_SIZE: u32 = 1 << 20;
/// OUTPUT buffers asked for when B-frames are on: VideoToolbox was measured
/// holding up to twelve frames back at 720p with three B-frames between
/// references, and the client needs one more to fill.
const REORDER_OUTPUT_BUFFERS: i64 = 16;

// ------------------------------------------------------------ controls ---

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Bool,
    Menu(&'static [&'static str]),
    Button,
}

struct Control {
    id: u32,
    name: &'static str,
    kind: Kind,
    min: i64,
    max: i64,
    default: i64,
    read_only: bool,
}

const H264_LEVELS: &[&str] = &[
    "1", "1b", "1.1", "1.2", "1.3", "2", "2.1", "2.2", "3", "3.1", "3.2", "4", "4.1", "4.2", "5",
    "5.1", "5.2", "6.0", "6.1", "6.2",
];
const HEVC_LEVELS: &[&str] = &[
    "1", "2", "2.1", "3", "3.1", "4", "4.1", "5", "5.1", "5.2", "6", "6.1", "6.2",
];
/// Empty names are menu entries this encoder does not offer.
const H264_PROFILES: &[&str] = &["Baseline", "Constrained Baseline", "Main", "", "High"];
const HEVC_PROFILES: &[&str] = &["Main", "", "Main 10"];
const BITRATE_MODES: &[&str] = &["Variable Bitrate", "Constant Bitrate"];
/// Parameter sets always travel with the keyframe. A buffer of them alone
/// (SEPARATE, which ffmpeg asks for) comes out of ffmpeg 5.1 as a packet
/// with no picture and the first frame's timestamp, which an mp4 muxer
/// takes as a non-monotonic DTS and a decoder as a missing picture; refused,
/// ffmpeg carries on with the joined stream.
const HEADER_MODES: &[&str] = &["", "Joined With 1st Frame"];
const HEVC_MAIN_10: i64 = 2;

macro_rules! control {
    ($id:ident, $name:literal, $kind:expr, $min:expr, $max:expr, $default:expr) => {
        Control {
            id: bindings::$id,
            name: $name,
            kind: $kind,
            min: $min,
            max: $max,
            default: $default,
            read_only: false,
        }
    };
}

/// Every control, sorted by id, which is the order `NEXT` walks them in.
const CONTROLS: &[Control] = &[
    Control {
        id: bindings::V4L2_CID_MIN_BUFFERS_FOR_OUTPUT,
        name: "Min Number of Output Buffers",
        kind: Kind::Int,
        min: 1,
        max: 32,
        default: 2,
        read_only: true,
    },
    // B-frames are off unless asked for; VideoToolbox then chooses how many.
    control!(
        V4L2_CID_MPEG_VIDEO_B_FRAMES,
        "Number of B-Frames",
        Kind::Int,
        0,
        3,
        0
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_GOP_SIZE,
        "GOP Size",
        Kind::Int,
        0,
        1 << 16,
        60
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_BITRATE_MODE,
        "Video Bitrate Mode",
        Kind::Menu(BITRATE_MODES),
        0,
        1,
        0
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_BITRATE,
        "Video Bitrate",
        Kind::Int,
        1_000,
        400_000_000,
        4_000_000
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_FRAME_RC_ENABLE,
        "Frame Level Rate Control Enable",
        Kind::Bool,
        0,
        1,
        1
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_HEADER_MODE,
        "Sequence Header Mode",
        Kind::Menu(HEADER_MODES),
        1,
        1,
        1
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_REPEAT_SEQ_HEADER,
        "Repeat Sequence Header",
        Kind::Bool,
        0,
        1,
        1
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME,
        "Force Key Frame",
        Kind::Button,
        0,
        0,
        0
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_H264_MIN_QP,
        "H264 Minimum QP Value",
        Kind::Int,
        0,
        51,
        0
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_H264_MAX_QP,
        "H264 Maximum QP Value",
        Kind::Int,
        0,
        51,
        51
    ),
    // Levels are chosen by VideoToolbox from the stream (its AutoLevel);
    // the control is kept for clients that set it.
    control!(
        V4L2_CID_MPEG_VIDEO_H264_LEVEL,
        "H264 Level",
        Kind::Menu(H264_LEVELS),
        0,
        19,
        11
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_H264_PROFILE,
        "H264 Profile",
        Kind::Menu(H264_PROFILES),
        0,
        4,
        4
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_HEVC_MIN_QP,
        "HEVC Minimum QP Value",
        Kind::Int,
        0,
        51,
        0
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_HEVC_MAX_QP,
        "HEVC Maximum QP Value",
        Kind::Int,
        0,
        51,
        51
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_HEVC_PROFILE,
        "HEVC Profile",
        Kind::Menu(HEVC_PROFILES),
        0,
        2,
        0
    ),
    control!(
        V4L2_CID_MPEG_VIDEO_HEVC_LEVEL,
        "HEVC Level",
        Kind::Menu(HEVC_LEVELS),
        0,
        12,
        5
    ),
];

fn control(id: u32) -> Option<&'static Control> {
    CONTROLS.iter().find(|c| c.id == id)
}

impl Control {
    fn v4l2_type(&self) -> u32 {
        match self.kind {
            Kind::Int => bindings::v4l2_ctrl_type_V4L2_CTRL_TYPE_INTEGER,
            Kind::Bool => bindings::v4l2_ctrl_type_V4L2_CTRL_TYPE_BOOLEAN,
            Kind::Menu(_) => bindings::v4l2_ctrl_type_V4L2_CTRL_TYPE_MENU,
            Kind::Button => bindings::v4l2_ctrl_type_V4L2_CTRL_TYPE_BUTTON,
        }
    }

    /// The value as it would be stored, or `None` for a menu entry that
    /// is not offered. Integers are clamped, as the V4L2 core does.
    fn validate(&self, value: i64) -> Option<i64> {
        match self.kind {
            Kind::Menu(names) => (value >= self.min
                && value <= self.max
                && names.get(value as usize).is_some_and(|n| !n.is_empty()))
            .then_some(value),
            _ => Some(value.clamp(self.min, self.max)),
        }
    }
}

// ------------------------------------------------------------- buffers ---

struct Buffer {
    v4l2: V4l2Buffer,
    backing: Backing,
}

impl Buffer {
    fn new(queue: QueueType, index: u32, size: u32, mmap_offset: u32) -> IoctlResult<Buffer> {
        let backing = Backing::new(queue, index, &[size as usize])?;
        let mut v4l2 = V4l2Buffer::new(queue, index, MemoryType::Mmap);
        if let V4l2PlanesWithBackingMut::Mmap(mut planes) = v4l2.planes_with_backing_iter_mut()
            && let Some(mut plane) = planes.next()
        {
            plane.set_mem_offset(mmap_offset);
            *plane.length = size;
        }
        v4l2.set_flags(BufferFlags::TIMESTAMP_COPY);
        v4l2.set_field(BufferField::None);
        Ok(Buffer { v4l2, backing })
    }

    fn mem_offset(&self) -> Option<u32> {
        match self.v4l2.planes_with_backing_iter() {
            V4l2PlanesWithBacking::Mmap(mut planes) => planes.next().map(|p| p.mem_offset()),
            _ => None,
        }
    }
}

// ------------------------------------------------------------- session ---

/// What is waiting for a CAPTURE buffer.
enum Chunk {
    Data {
        bytes: Vec<u8>,
        keyframe: bool,
        timestamp: bindings::timeval,
    },
    /// The empty buffer that ends a drain.
    Last,
}

pub struct EncoderSession {
    id: u32,
    coded: Coded,
    raw: Raw,
    size: (u32, u32),
    bytesperline: u32,
    coded_sizeimage: u32,
    frame_interval: (u32, u32),
    controls: BTreeMap<u32, i64>,
    force_keyframe: bool,
    input: Vec<Buffer>,
    output: Vec<Buffer>,
    output_streaming: bool,
    capture_streaming: bool,
    /// OUTPUT buffers queued, with their sizes, waiting for both queues to
    /// stream: encoding starts only then (dev-encoder 4.5.2.5).
    waiting: VecDeque<(u32, usize)>,
    /// CAPTURE buffers queued before the queue streams.
    parked: Vec<u32>,
    /// CAPTURE buffers ready for encoded data.
    available: VecDeque<u32>,
    ready: VecDeque<Chunk>,
    shared: Arc<Shared>,
    vt: Option<VtEncoder>,
    seq: i64,
    /// The guest's timestamp for each frame still with VideoToolbox.
    stamps: HashMap<i64, bindings::timeval>,
    headers_sent: bool,
    eos_subscribed: bool,
    sequence: u32,
}

// The VideoToolbox session is used from whichever thread holds the
// transport lock, one at a time.
unsafe impl Send for EncoderSession {}

impl VirtioMediaDeviceSession for EncoderSession {
    fn poll_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        None
    }
}

impl EncoderSession {
    fn new(id: u32, bell: Arc<Doorbell>) -> EncoderSession {
        EncoderSession {
            id,
            coded: Coded::H264,
            raw: Raw::Nv12,
            size: DEFAULT_SIZE,
            bytesperline: DEFAULT_SIZE.0.next_multiple_of(16),
            coded_sizeimage: coded_size(DEFAULT_SIZE),
            frame_interval: (1, 30),
            controls: CONTROLS.iter().map(|c| (c.id, c.default)).collect(),
            force_keyframe: false,
            input: Vec::new(),
            output: Vec::new(),
            output_streaming: false,
            capture_streaming: false,
            waiting: VecDeque::new(),
            parked: Vec::new(),
            available: VecDeque::new(),
            ready: VecDeque::new(),
            shared: Shared::new(bell),
            vt: None,
            seq: 0,
            stamps: HashMap::new(),
            headers_sent: false,
            eos_subscribed: false,
            sequence: 0,
        }
    }

    fn control(&self, id: u32) -> i64 {
        self.controls.get(&id).copied().unwrap_or(0)
    }

    /// OUTPUT buffers a client must have for frames to keep flowing. A
    /// client may hold each frame's buffer until its encoded output comes
    /// back (GStreamer does), and with B-frames VideoToolbox holds frames
    /// back to look ahead, so the queue must be deeper than that.
    fn min_output_buffers(&self) -> i64 {
        if self.control(bindings::V4L2_CID_MPEG_VIDEO_B_FRAMES) > 0 {
            REORDER_OUTPUT_BUFFERS
        } else {
            2
        }
    }

    fn ten_bit(&self) -> bool {
        self.coded == Coded::Hevc
            && (self.raw == Raw::P010
                || self.control(bindings::V4L2_CID_MPEG_VIDEO_HEVC_PROFILE) == HEVC_MAIN_10)
    }

    fn params(&self) -> Params {
        let qp = match self.coded {
            Coded::H264 => (
                bindings::V4L2_CID_MPEG_VIDEO_H264_MIN_QP,
                bindings::V4L2_CID_MPEG_VIDEO_H264_MAX_QP,
            ),
            Coded::Hevc => (
                bindings::V4L2_CID_MPEG_VIDEO_HEVC_MIN_QP,
                bindings::V4L2_CID_MPEG_VIDEO_HEVC_MAX_QP,
            ),
        };
        let qp = (self.control(qp.0) as u32, self.control(qp.1) as u32);
        Params {
            coded: self.coded,
            size: self.size,
            ten_bit: self.ten_bit(),
            h264_profile: self.control(bindings::V4L2_CID_MPEG_VIDEO_H264_PROFILE),
            bitrate: self.control(bindings::V4L2_CID_MPEG_VIDEO_BITRATE) as u32,
            constant_bitrate: self.control(bindings::V4L2_CID_MPEG_VIDEO_BITRATE_MODE) == 1,
            gop: self.control(bindings::V4L2_CID_MPEG_VIDEO_GOP_SIZE) as u32,
            reorder: self.control(bindings::V4L2_CID_MPEG_VIDEO_B_FRAMES) > 0,
            frame_interval: self.frame_interval,
            qp: (qp != (0, 51)).then_some(qp),
        }
    }

    /// Ends the VideoToolbox session; the next frame starts another, with
    /// a keyframe and its parameter sets.
    fn reset(&mut self) {
        self.vt = None;
        self.stamps.clear();
        self.headers_sent = false;
    }

    fn raw_format(&self) -> bindings::v4l2_pix_format_mplane {
        let (w, h) = self.size;
        pix_mp(
            w,
            h,
            match self.raw {
                Raw::Nv12 => NV12,
                Raw::Yu12 => YU12,
                Raw::P010 => P010,
            },
            self.bytesperline,
            raw_size(self.bytesperline, h),
        )
    }

    fn coded_format(&self) -> bindings::v4l2_pix_format_mplane {
        let (w, h) = self.size;
        let fourcc = match self.coded {
            Coded::H264 => H264,
            Coded::Hevc => HEVC,
        };
        pix_mp(w, h, fourcc, 0, self.coded_sizeimage)
    }

    fn format(&self, direction: QueueDirection) -> bindings::v4l2_format {
        let queue = QueueType::from_dir_and_class(direction, QueueClass::VideoMplane);
        bindings::v4l2_format {
            type_: queue as u32,
            fmt: bindings::v4l2_format__bindgen_ty_1 {
                pix_mp: match direction {
                    QueueDirection::Output => self.raw_format(),
                    QueueDirection::Capture => self.coded_format(),
                },
            },
        }
    }

    /// Moves what VideoToolbox has finished into `ready`, deciding where
    /// the parameter sets go.
    fn collect(&mut self) {
        let repeat = self.control(bindings::V4L2_CID_MPEG_VIDEO_REPEAT_SEQ_HEADER) != 0;
        for e in self.shared.take() {
            let Some(timestamp) = self.stamps.remove(&e.seq) else {
                continue;
            };
            let mut bytes = Vec::new();
            if let Some(headers) = e.headers
                && (repeat || !self.headers_sent)
            {
                self.headers_sent = true;
                bytes = headers;
            }
            bytes.extend_from_slice(&e.data);
            self.ready.push_back(Chunk::Data {
                bytes,
                keyframe: e.keyframe,
                timestamp,
            });
        }
    }
}

fn pix_mp(
    w: u32,
    h: u32,
    fourcc: u32,
    bytesperline: u32,
    sizeimage: u32,
) -> bindings::v4l2_pix_format_mplane {
    let mut plane_fmt: [bindings::v4l2_plane_pix_format; bindings::VIDEO_MAX_PLANES as usize] =
        Default::default();
    plane_fmt[0] = bindings::v4l2_plane_pix_format {
        bytesperline,
        sizeimage,
        reserved: Default::default(),
    };
    bindings::v4l2_pix_format_mplane {
        width: w,
        height: h,
        pixelformat: fourcc,
        field: bindings::v4l2_field_V4L2_FIELD_NONE,
        colorspace: bindings::v4l2_colorspace_V4L2_COLORSPACE_DEFAULT,
        plane_fmt,
        num_planes: 1,
        ..Default::default()
    }
}

/// A 4:2:0 frame in one plane: luma rows, then half as many chroma rows of
/// the same length (NV12, P010) or twice as many at half (YU12).
fn raw_size(bytesperline: u32, h: u32) -> u32 {
    bytesperline * h * 3 / 2
}

fn coded_size((w, h): (u32, u32)) -> u32 {
    (w * h * 3 / 2).max(MIN_CODED_SIZE)
}

fn even_dimension(v: u32) -> u32 {
    (v.clamp(16, MAX_DIMENSION) + 1) & !1
}

// -------------------------------------------------------------- device ---

pub struct VideoEncoder<Q: VirtioMediaEventQueue, HM: VirtioMediaHostMemoryMapper> {
    event_queue: Q,
    host_mapper: MmapMappingManager<HM>,
    bell: Arc<Doorbell>,
}

impl<Q, HM> VideoEncoder<Q, HM>
where
    Q: VirtioMediaEventQueue,
    HM: VirtioMediaHostMemoryMapper,
{
    pub fn new(event_queue: Q, host_mapper: HM, bell: Arc<Doorbell>) -> Self {
        VideoEncoder {
            event_queue,
            host_mapper: MmapMappingManager::from(host_mapper),
            bell,
        }
    }

    /// Hands out whatever is encoded, into whatever CAPTURE buffers the
    /// guest has queued.
    fn deliver(&mut self, session: &mut EncoderSession) {
        session.collect();
        while !session.ready.is_empty() {
            let Some(index) = session.available.pop_front() else {
                return;
            };
            let Some(buffer) = session.output.get_mut(index as usize) else {
                continue;
            };
            let chunk = session.ready.pop_front().expect("checked");
            let mut flags = BufferFlags::TIMESTAMP_COPY;
            let (used, timestamp, last) = match chunk {
                Chunk::Data {
                    bytes,
                    keyframe,
                    timestamp,
                } => {
                    let dst = buffer
                        .backing
                        .planes
                        .first_mut()
                        .map(|p| p.bytes_mut())
                        .unwrap_or_default();
                    let n = bytes.len().min(dst.len());
                    dst[..n].copy_from_slice(&bytes[..n]);
                    if n < bytes.len() {
                        tracing::warn!(
                            need = bytes.len(),
                            have = dst.len(),
                            "encoded frame larger than the CAPTURE buffer"
                        );
                        flags |= BufferFlags::ERROR;
                    }
                    if keyframe {
                        flags |= BufferFlags::KEYFRAME;
                    }
                    (n as u32, timestamp, false)
                }
                Chunk::Last => {
                    flags |= BufferFlags::LAST;
                    (0, bindings::timeval::default(), true)
                }
            };
            buffer.v4l2.set_flags(flags);
            buffer.v4l2.set_timestamp(timestamp);
            buffer.v4l2.set_sequence(session.sequence);
            session.sequence += 1;
            *buffer.v4l2.get_first_plane_mut().bytesused = used;
            tracing::trace!(index, used, last, "video encode buffer out");
            self.event_queue
                .send_event(V4l2Event::DequeueBuffer(DequeueBufferEvent::new(
                    session.id,
                    buffer.v4l2.clone(),
                )));
            if last && session.eos_subscribed {
                self.event_queue
                    .send_event(V4l2Event::Event(SessionEvent::new(
                        session.id,
                        bindings::v4l2_event {
                            type_: bindings::V4L2_EVENT_EOS,
                            ..Default::default()
                        },
                    )));
            }
        }
    }

    /// Encodes the frame in OUTPUT buffer `index`, then returns the buffer.
    fn encode(&mut self, session: &mut EncoderSession, index: u32, bytes_used: usize) {
        let mut error = false;
        if bytes_used > 0 {
            if session.vt.is_none() {
                match VtEncoder::new(&session.params(), session.shared.clone()) {
                    Ok(vt) => session.vt = Some(vt),
                    Err(st) => {
                        tracing::warn!(st, params = ?session.params(), "VideoToolbox could not encode");
                        error = true;
                    }
                }
            }
            if let Some(vt) = &session.vt {
                let buffer = &session.input[index as usize];
                let src = buffer
                    .backing
                    .planes
                    .first()
                    .map(|p| p.bytes())
                    .unwrap_or_default();
                let src = &src[..bytes_used.min(src.len())];
                let seq = session.seq;
                session.seq += 1;
                let timestamp = buffer.v4l2.timestamp();
                session.stamps.insert(seq, timestamp);
                let force = std::mem::take(&mut session.force_keyframe);
                tracing::trace!(index, seq, bytes = src.len(), force, "video encode in");
                let strides = Strides::of(
                    session.raw,
                    session.bytesperline as usize,
                    session.size,
                    src.len(),
                );
                if let Err(st) = vt.encode(src, session.raw, strides, seq, force) {
                    tracing::debug!(st, "VideoToolbox rejected a frame");
                    session.stamps.remove(&seq);
                    error = true;
                }
            }
        }
        let buffer = &mut session.input[index as usize];
        buffer.v4l2.clear_flags(BufferFlags::QUEUED);
        if error {
            buffer.v4l2.add_flags(BufferFlags::ERROR);
        }
        self.event_queue
            .send_event(V4l2Event::DequeueBuffer(DequeueBufferEvent::new(
                session.id,
                buffer.v4l2.clone(),
            )));
    }

    /// Encodes whatever OUTPUT buffers wait, once both queues stream.
    fn start(&mut self, session: &mut EncoderSession) {
        if !(session.output_streaming && session.capture_streaming) {
            return;
        }
        while let Some((index, bytes_used)) = session.waiting.pop_front() {
            self.encode(session, index, bytes_used);
        }
    }

    /// Applies a control that matters to a running session at once.
    fn control_changed(&mut self, session: &mut EncoderSession, id: u32) {
        match id {
            bindings::V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME => session.force_keyframe = true,
            bindings::V4L2_CID_MPEG_VIDEO_BITRATE | bindings::V4L2_CID_MPEG_VIDEO_BITRATE_MODE => {
                let params = session.params();
                if let Some(vt) = &session.vt {
                    vt.set_rate(&params);
                }
            }
            bindings::V4L2_CID_MPEG_VIDEO_GOP_SIZE => {
                let gop = session.params().gop;
                if let Some(vt) = &session.vt {
                    vt.set_gop(gop);
                }
            }
            _ => {
                // Anything else is read when the next session starts, which
                // is now if one is running with different parameters.
                if session
                    .vt
                    .as_ref()
                    .is_some_and(|vt| *vt.params() != session.params())
                {
                    session.reset();
                }
            }
        }
    }

    fn adjust(
        &self,
        session: &EncoderSession,
        format: bindings::v4l2_format,
    ) -> IoctlResult<bindings::v4l2_format> {
        let queue = QueueType::n(format.type_).ok_or(libc::EINVAL)?;
        if queue.class() != QueueClass::VideoMplane {
            return Err(libc::EINVAL);
        }
        // SAFETY: an mplane queue's format is `pix_mp`.
        let asked = unsafe { format.fmt.pix_mp };
        let pix = match queue.direction() {
            QueueDirection::Output => {
                let fourcc = if RAW_FORMATS.contains(&{ asked.pixelformat }) {
                    asked.pixelformat
                } else {
                    NV12
                };
                let raw = raw_of(fourcc);
                let (w, h) = (even_dimension(asked.width), even_dimension(asked.height));
                let min = w * raw.sample_bytes() as u32;
                let asked_bpl = asked.plane_fmt[0].bytesperline;
                let bpl = if asked_bpl >= min && asked_bpl <= 4 * min {
                    (asked_bpl + 1) & !1
                } else {
                    // ffmpeg 5.1 copies a frame in at its own stride and
                    // never reads this one back: FFALIGN(width, 16) on
                    // arm64, where its buffer pools align to NEON's 16.
                    min.next_multiple_of(16)
                };
                pix_mp(w, h, fourcc, bpl, raw_size(bpl, h))
            }
            QueueDirection::Capture => {
                let fourcc = if CODED_FORMATS.contains(&{ asked.pixelformat }) {
                    asked.pixelformat
                } else {
                    session.coded_format().pixelformat
                };
                let size = asked.plane_fmt[0]
                    .sizeimage
                    .clamp(coded_size(session.size), 64 << 20);
                pix_mp(session.size.0, session.size.1, fourcc, 0, size)
            }
        };
        Ok(bindings::v4l2_format {
            type_: format.type_,
            fmt: bindings::v4l2_format__bindgen_ty_1 { pix_mp: pix },
        })
    }
}

fn raw_of(fourcc: u32) -> Raw {
    match fourcc {
        YU12 => Raw::Yu12,
        P010 => Raw::P010,
        _ => Raw::Nv12,
    }
}

impl<Q, HM, Reader, Writer> VirtioMediaDevice<Reader, Writer> for VideoEncoder<Q, HM>
where
    Q: VirtioMediaEventQueue,
    HM: VirtioMediaHostMemoryMapper,
    Reader: ReadFromDescriptorChain,
    Writer: WriteToDescriptorChain,
{
    type Session = EncoderSession;

    fn new_session(&mut self, session_id: u32) -> Result<EncoderSession, i32> {
        Ok(EncoderSession::new(session_id, self.bell.clone()))
    }

    fn close_session(&mut self, session: EncoderSession) {
        for buffer in session.input.iter().chain(session.output.iter()) {
            if let Some(offset) = buffer.mem_offset() {
                self.host_mapper.unregister_buffer(offset);
            }
        }
    }

    fn do_ioctl(
        &mut self,
        session: &mut EncoderSession,
        ioctl: V4l2Ioctl,
        reader: &mut Reader,
        writer: &mut Writer,
    ) -> std::io::Result<()> {
        virtio_media_dispatch_ioctl(self, session, ioctl, reader, writer)
    }

    fn do_mmap(
        &mut self,
        session: &mut EncoderSession,
        flags: u32,
        offset: u32,
    ) -> Result<(u64, u64), i32> {
        let buffer = session
            .input
            .iter()
            .chain(session.output.iter())
            .find(|b| b.mem_offset() == Some(offset))
            .ok_or(libc::EINVAL)?;
        let fd = buffer.backing.fd_for_plane(0).ok_or(libc::EINVAL)?;
        let rw = (flags & VIRTIO_MEDIA_MMAP_FLAG_RW) != 0;
        self.host_mapper
            .create_mapping(offset, fd, rw)
            .map_err(|_| libc::EINVAL)
    }

    fn do_munmap(&mut self, guest_addr: u64) -> Result<(), i32> {
        self.host_mapper
            .remove_mapping(guest_addr)
            .map(|_| ())
            .map_err(|_| libc::EINVAL)
    }

    fn process_events(&mut self, session: &mut EncoderSession) -> Result<(), i32> {
        self.deliver(session);
        Ok(())
    }
}

impl<Q, HM> VirtioMediaIoctlHandler for VideoEncoder<Q, HM>
where
    Q: VirtioMediaEventQueue,
    HM: VirtioMediaHostMemoryMapper,
{
    type Session = EncoderSession;

    fn enum_fmt(
        &mut self,
        _session: &EncoderSession,
        queue: QueueType,
        index: u32,
    ) -> IoctlResult<bindings::v4l2_fmtdesc> {
        let (pixelformat, flags) = match queue {
            QueueType::VideoOutputMplane => {
                (*RAW_FORMATS.get(index as usize).ok_or(libc::EINVAL)?, 0)
            }
            QueueType::VideoCaptureMplane => (
                *CODED_FORMATS.get(index as usize).ok_or(libc::EINVAL)?,
                bindings::V4L2_FMT_FLAG_COMPRESSED,
            ),
            _ => return Err(libc::EINVAL),
        };
        Ok(bindings::v4l2_fmtdesc {
            index,
            type_: queue as u32,
            pixelformat,
            flags,
            ..Default::default()
        })
    }

    fn enum_framesizes(
        &mut self,
        _session: &EncoderSession,
        index: u32,
        pixel_format: u32,
    ) -> IoctlResult<bindings::v4l2_frmsizeenum> {
        if index != 0
            || !(RAW_FORMATS.contains(&pixel_format) || CODED_FORMATS.contains(&pixel_format))
        {
            return Err(libc::EINVAL);
        }
        Ok(bindings::v4l2_frmsizeenum {
            index,
            pixel_format,
            type_: bindings::v4l2_frmsizetypes_V4L2_FRMSIZE_TYPE_STEPWISE,
            __bindgen_anon_1: bindings::v4l2_frmsizeenum__bindgen_ty_1 {
                stepwise: bindings::v4l2_frmsize_stepwise {
                    min_width: 16,
                    max_width: MAX_DIMENSION,
                    step_width: 2,
                    min_height: 16,
                    max_height: MAX_DIMENSION,
                    step_height: 2,
                },
            },
            ..Default::default()
        })
    }

    fn enum_frameintervals(
        &mut self,
        _session: &EncoderSession,
        index: u32,
        pixel_format: u32,
        width: u32,
        height: u32,
    ) -> IoctlResult<bindings::v4l2_frmivalenum> {
        if index != 0 || !RAW_FORMATS.contains(&pixel_format) {
            return Err(libc::EINVAL);
        }
        let fract = |numerator, denominator| bindings::v4l2_fract {
            numerator,
            denominator,
        };
        Ok(bindings::v4l2_frmivalenum {
            index,
            pixel_format,
            width,
            height,
            type_: bindings::v4l2_frmivaltypes_V4L2_FRMIVAL_TYPE_CONTINUOUS,
            __bindgen_anon_1: bindings::v4l2_frmivalenum__bindgen_ty_1 {
                stepwise: bindings::v4l2_frmival_stepwise {
                    min: fract(1, 240),
                    max: fract(1, 1),
                    step: fract(1, 1),
                },
            },
            ..Default::default()
        })
    }

    fn g_fmt(
        &mut self,
        session: &EncoderSession,
        queue: QueueType,
    ) -> IoctlResult<bindings::v4l2_format> {
        match queue {
            QueueType::VideoOutputMplane | QueueType::VideoCaptureMplane => {
                Ok(session.format(queue.direction()))
            }
            _ => Err(libc::EINVAL),
        }
    }

    fn try_fmt(
        &mut self,
        session: &EncoderSession,
        _queue: QueueType,
        format: bindings::v4l2_format,
    ) -> IoctlResult<bindings::v4l2_format> {
        self.adjust(session, format)
    }

    fn s_fmt(
        &mut self,
        session: &mut EncoderSession,
        queue: QueueType,
        format: bindings::v4l2_format,
    ) -> IoctlResult<bindings::v4l2_format> {
        let format = self.adjust(session, format)?;
        // SAFETY: `adjust` wrote `pix_mp`.
        let pix = unsafe { format.fmt.pix_mp };
        match queue.direction() {
            QueueDirection::Output => {
                let (raw, size, bpl) = (
                    raw_of(pix.pixelformat),
                    (pix.width, pix.height),
                    pix.plane_fmt[0].bytesperline,
                );
                if (raw, size, bpl) != (session.raw, session.size, session.bytesperline) {
                    session.raw = raw;
                    session.size = size;
                    session.bytesperline = bpl;
                    session.coded_sizeimage = session.coded_sizeimage.max(coded_size(size));
                    session.reset();
                }
            }
            QueueDirection::Capture => {
                let coded = if pix.pixelformat == HEVC {
                    Coded::Hevc
                } else {
                    Coded::H264
                };
                if coded != session.coded {
                    session.coded = coded;
                    session.reset();
                }
                session.coded_sizeimage = pix.plane_fmt[0].sizeimage;
            }
        }
        Ok(session.format(queue.direction()))
    }

    fn reqbufs(
        &mut self,
        session: &mut EncoderSession,
        queue: QueueType,
        memory: MemoryType,
        count: u32,
        flags: MemoryConsistency,
    ) -> IoctlResult<bindings::v4l2_requestbuffers> {
        if memory != MemoryType::Mmap {
            return Err(libc::EINVAL);
        }
        let size = match queue {
            QueueType::VideoOutputMplane => session.raw_format().plane_fmt[0].sizeimage,
            QueueType::VideoCaptureMplane => session.coded_sizeimage,
            _ => return Err(libc::EINVAL),
        };
        let buffers = if queue == QueueType::VideoOutputMplane {
            &mut session.input
        } else {
            session.available.clear();
            session.parked.clear();
            &mut session.output
        };
        // Buffers are always reallocated: a new count usually comes with a
        // new format.
        for buffer in buffers.drain(..) {
            if let Some(offset) = buffer.mem_offset() {
                self.host_mapper.unregister_buffer(offset);
            }
        }
        for i in 0..count {
            let offset = self
                .host_mapper
                .register_buffer(None, size)
                .map_err(|_| libc::ENOMEM)?;
            match Buffer::new(queue, i, size, offset) {
                Ok(b) => buffers.push(b),
                Err(e) => {
                    self.host_mapper.unregister_buffer(offset);
                    return Err(e);
                }
            }
        }
        Ok(bindings::v4l2_requestbuffers {
            count,
            type_: queue as u32,
            memory: memory as u32,
            capabilities: (BufferCapabilities::SUPPORTS_MMAP
                | BufferCapabilities::SUPPORTS_ORPHANED_BUFS)
                .bits(),
            flags: flags.bits(),
            reserved: Default::default(),
        })
    }

    fn querybuf(
        &mut self,
        session: &EncoderSession,
        queue: QueueType,
        index: u32,
    ) -> IoctlResult<V4l2Buffer> {
        let buffers = match queue {
            QueueType::VideoOutputMplane => &session.input,
            QueueType::VideoCaptureMplane => &session.output,
            _ => return Err(libc::EINVAL),
        };
        Ok(buffers
            .get(index as usize)
            .ok_or(libc::EINVAL)?
            .v4l2
            .clone())
    }

    fn qbuf(
        &mut self,
        session: &mut EncoderSession,
        buffer: V4l2Buffer,
        _guest_regions: Vec<Vec<SgEntry>>,
    ) -> IoctlResult<V4l2Buffer> {
        let index = buffer.index();
        let queue = buffer.queue();
        let buffers = match queue {
            QueueType::VideoOutputMplane => &mut session.input,
            QueueType::VideoCaptureMplane => &mut session.output,
            _ => return Err(libc::EINVAL),
        };
        let host = buffers.get_mut(index as usize).ok_or(libc::EINVAL)?;
        if buffer.memory() != MemoryType::Mmap {
            return Err(libc::EINVAL);
        }
        let plane = buffer.get_first_plane();
        let (bytesused, length) = (*plane.bytesused, *plane.length);
        host.v4l2
            .set_flags(BufferFlags::TIMESTAMP_COPY | BufferFlags::QUEUED);
        host.v4l2.set_timestamp(buffer.timestamp());
        let host_plane = host.v4l2.get_first_plane_mut();
        *host_plane.bytesused = bytesused;
        *host_plane.length = length;
        let reply = host.v4l2.clone();
        match queue.direction() {
            QueueDirection::Output => {
                session.waiting.push_back((index, bytesused as usize));
                self.start(session);
            }
            QueueDirection::Capture => {
                tracing::trace!(
                    index,
                    streaming = session.capture_streaming,
                    "video encode buffer queued"
                );
                if session.capture_streaming {
                    session.available.push_back(index);
                } else {
                    session.parked.push(index);
                }
            }
        }
        self.deliver(session);
        Ok(reply)
    }

    fn streamon(&mut self, session: &mut EncoderSession, queue: QueueType) -> IoctlResult<()> {
        match queue {
            QueueType::VideoOutputMplane => {
                if session.input.is_empty() {
                    return Err(libc::EINVAL);
                }
                session.output_streaming = true;
            }
            QueueType::VideoCaptureMplane => {
                if session.output.is_empty() {
                    return Err(libc::EINVAL);
                }
                session.capture_streaming = true;
                session.available.extend(session.parked.drain(..));
            }
            _ => return Err(libc::EINVAL),
        }
        self.start(session);
        self.deliver(session);
        Ok(())
    }

    fn streamoff(&mut self, session: &mut EncoderSession, queue: QueueType) -> IoctlResult<()> {
        let buffers = match queue {
            QueueType::VideoOutputMplane => {
                session.output_streaming = false;
                session.waiting.clear();
                &mut session.input
            }
            QueueType::VideoCaptureMplane => {
                // Stopping CAPTURE resets the encoder (dev-encoder 4.5.2.9):
                // what was in flight is dropped and the next frame starts a
                // new stream. OUTPUT buffers not yet read stay queued.
                session.capture_streaming = false;
                session.available.clear();
                session.parked.clear();
                session.ready.clear();
                session.reset();
                &mut session.output
            }
            _ => return Err(libc::EINVAL),
        };
        for b in buffers {
            b.v4l2.clear_flags(BufferFlags::QUEUED);
        }
        Ok(())
    }

    fn subscribe_event(
        &mut self,
        session: &mut EncoderSession,
        event: EventType,
        _flags: SubscribeEventFlags,
    ) -> IoctlResult<()> {
        match event {
            EventType::Eos => {
                session.eos_subscribed = true;
                Ok(())
            }
            _ => Err(libc::EINVAL),
        }
    }

    fn unsubscribe_event(
        &mut self,
        session: &mut EncoderSession,
        event: bindings::v4l2_event_subscription,
    ) -> IoctlResult<()> {
        if event.type_ == 0 || event.type_ == bindings::V4L2_EVENT_EOS {
            session.eos_subscribed = false;
            Ok(())
        } else {
            Err(libc::EINVAL)
        }
    }

    fn g_parm(
        &mut self,
        session: &EncoderSession,
        queue: QueueType,
    ) -> IoctlResult<bindings::v4l2_streamparm> {
        if queue.class() != QueueClass::VideoMplane {
            return Err(libc::EINVAL);
        }
        let (numerator, denominator) = session.frame_interval;
        let mut parm = bindings::v4l2_streamparm {
            type_: queue as u32,
            ..Default::default()
        };
        // The capture and output variants share their first three fields.
        parm.parm.output = bindings::v4l2_outputparm {
            capability: bindings::V4L2_CAP_TIMEPERFRAME,
            timeperframe: bindings::v4l2_fract {
                numerator,
                denominator,
            },
            ..Default::default()
        };
        Ok(parm)
    }

    fn s_parm(
        &mut self,
        session: &mut EncoderSession,
        parm: bindings::v4l2_streamparm,
    ) -> IoctlResult<bindings::v4l2_streamparm> {
        let queue = QueueType::n(parm.type_).ok_or(libc::EINVAL)?;
        // SAFETY: both variants start with capability, mode, timeperframe.
        let tpf = unsafe { parm.parm.output.timeperframe };
        if tpf.numerator > 0 && tpf.denominator > 0 {
            session.frame_interval = (tpf.numerator, tpf.denominator);
            self.control_changed(session, 0);
        }
        self.g_parm(session, queue)
    }

    fn g_selection(
        &mut self,
        session: &EncoderSession,
        sel_type: SelectionType,
        sel_target: SelectionTarget,
    ) -> IoctlResult<bindings::v4l2_rect> {
        match (sel_type, sel_target) {
            (
                SelectionType::Output,
                SelectionTarget::Crop | SelectionTarget::CropDefault | SelectionTarget::CropBounds,
            ) => Ok(bindings::v4l2_rect {
                left: 0,
                top: 0,
                width: session.size.0,
                height: session.size.1,
            }),
            _ => Err(libc::EINVAL),
        }
    }

    /// The whole frame is always encoded.
    fn s_selection(
        &mut self,
        session: &mut EncoderSession,
        sel_type: SelectionType,
        sel_target: SelectionTarget,
        _sel_rect: bindings::v4l2_rect,
        _sel_flags: SelectionFlags,
    ) -> IoctlResult<bindings::v4l2_rect> {
        self.g_selection(session, sel_type, sel_target)
    }

    fn query_ext_ctrl(
        &mut self,
        session: &EncoderSession,
        id: CtrlId,
        flags: QueryCtrlFlags,
    ) -> IoctlResult<bindings::v4l2_query_ext_ctrl> {
        let wanted = id.id();
        let c = if flags.contains(QueryCtrlFlags::NEXT) {
            CONTROLS.iter().find(|c| c.id > wanted)
        } else {
            control(wanted)
        }
        .ok_or(libc::EINVAL)?;
        let mut name = [0; 32];
        for (d, s) in name.iter_mut().zip(c.name.bytes()) {
            *d = s as _;
        }
        let mut ctrl_flags = 0;
        if c.read_only {
            ctrl_flags |= bindings::V4L2_CTRL_FLAG_READ_ONLY | bindings::V4L2_CTRL_FLAG_VOLATILE;
        }
        if c.kind == Kind::Button {
            ctrl_flags |=
                bindings::V4L2_CTRL_FLAG_WRITE_ONLY | bindings::V4L2_CTRL_FLAG_EXECUTE_ON_WRITE;
        }
        let _ = session;
        Ok(bindings::v4l2_query_ext_ctrl {
            id: c.id,
            type_: c.v4l2_type(),
            name,
            minimum: c.min,
            maximum: c.max,
            step: 1,
            default_value: c.default,
            flags: ctrl_flags,
            elem_size: 4,
            elems: 1,
            ..Default::default()
        })
    }

    fn queryctrl(
        &mut self,
        session: &EncoderSession,
        id: CtrlId,
        flags: QueryCtrlFlags,
    ) -> IoctlResult<bindings::v4l2_queryctrl> {
        let q = self.query_ext_ctrl(session, id, flags)?;
        Ok(bindings::v4l2_queryctrl {
            id: q.id,
            type_: q.type_,
            name: q.name.map(|c| c as u8),
            minimum: q.minimum as i32,
            maximum: q.maximum as i32,
            step: q.step as i32,
            default_value: q.default_value as i32,
            flags: q.flags,
            ..Default::default()
        })
    }

    fn querymenu(
        &mut self,
        _session: &EncoderSession,
        id: u32,
        index: u32,
    ) -> IoctlResult<bindings::v4l2_querymenu> {
        let c = control(id).ok_or(libc::EINVAL)?;
        let Kind::Menu(names) = c.kind else {
            return Err(libc::EINVAL);
        };
        let label = names
            .get(index as usize)
            .filter(|n| !n.is_empty())
            .ok_or(libc::EINVAL)?;
        let mut name = [0u8; 32];
        for (d, s) in name.iter_mut().zip(label.bytes()) {
            *d = s;
        }
        Ok(bindings::v4l2_querymenu {
            id,
            index,
            __bindgen_anon_1: bindings::v4l2_querymenu__bindgen_ty_1 { name },
            reserved: 0,
        })
    }

    fn g_ext_ctrls(
        &mut self,
        session: &EncoderSession,
        which: CtrlWhich,
        ctrls: &mut bindings::v4l2_ext_controls,
        ctrl_array: &mut Vec<bindings::v4l2_ext_control>,
        _user_regions: Vec<Vec<SgEntry>>,
    ) -> IoctlResult<()> {
        for (i, ctrl) in ctrl_array.iter_mut().enumerate() {
            let Some(c) = control(ctrl.id).filter(|c| c.kind != Kind::Button) else {
                ctrls.error_idx = i as u32;
                return Err(libc::EINVAL);
            };
            let value = if matches!(which, CtrlWhich::Default) {
                c.default
            } else if c.id == bindings::V4L2_CID_MIN_BUFFERS_FOR_OUTPUT {
                session.min_output_buffers()
            } else {
                session.control(c.id)
            };
            ctrl.__bindgen_anon_1.value = value as i32;
        }
        Ok(())
    }

    fn try_ext_ctrls(
        &mut self,
        _session: &EncoderSession,
        _which: CtrlWhich,
        ctrls: &mut bindings::v4l2_ext_controls,
        ctrl_array: &mut Vec<bindings::v4l2_ext_control>,
        _user_regions: Vec<Vec<SgEntry>>,
    ) -> IoctlResult<()> {
        for (i, ctrl) in ctrl_array.iter_mut().enumerate() {
            let c = control(ctrl.id).filter(|c| !c.read_only);
            // SAFETY: every control here is a 32-bit value.
            let value = i64::from(unsafe { ctrl.__bindgen_anon_1.value });
            let Some(v) = c.and_then(|c| c.validate(value)) else {
                ctrls.error_idx = i as u32;
                return Err(libc::EINVAL);
            };
            ctrl.__bindgen_anon_1.value = v as i32;
        }
        Ok(())
    }

    fn s_ext_ctrls(
        &mut self,
        session: &mut EncoderSession,
        which: CtrlWhich,
        ctrls: &mut bindings::v4l2_ext_controls,
        ctrl_array: &mut Vec<bindings::v4l2_ext_control>,
        user_regions: Vec<Vec<SgEntry>>,
    ) -> IoctlResult<()> {
        // All or nothing, as the V4L2 core does it.
        self.try_ext_ctrls(session, which, ctrls, ctrl_array, user_regions)?;
        for ctrl in ctrl_array.iter() {
            // SAFETY: as in `try_ext_ctrls`.
            let value = i64::from(unsafe { ctrl.__bindgen_anon_1.value });
            tracing::debug!(
                id = format_args!("{:#x}", { ctrl.id }),
                value,
                "video encoder control"
            );
            session.controls.insert(ctrl.id, value);
            self.control_changed(session, ctrl.id);
        }
        Ok(())
    }

    fn try_encoder_cmd(
        &mut self,
        _session: &EncoderSession,
        cmd: bindings::v4l2_encoder_cmd,
    ) -> IoctlResult<bindings::v4l2_encoder_cmd> {
        match cmd.cmd {
            bindings::V4L2_ENC_CMD_STOP | bindings::V4L2_ENC_CMD_START => {
                Ok(bindings::v4l2_encoder_cmd {
                    cmd: cmd.cmd,
                    flags: 0,
                    ..Default::default()
                })
            }
            _ => Err(libc::EINVAL),
        }
    }

    fn encoder_cmd(
        &mut self,
        session: &mut EncoderSession,
        cmd: bindings::v4l2_encoder_cmd,
    ) -> IoctlResult<bindings::v4l2_encoder_cmd> {
        let reply = self.try_encoder_cmd(session, cmd)?;
        if cmd.cmd == bindings::V4L2_ENC_CMD_STOP {
            // Every frame queued so far comes out, then an empty buffer
            // flagged LAST; the next frame continues the stream.
            self.start(session);
            if let Some(vt) = &session.vt {
                vt.complete();
            }
            session.collect();
            session.ready.push_back(Chunk::Last);
            tracing::debug!(ready = session.ready.len(), "video encode drain");
            self.deliver(session);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_are_sorted_for_next_and_their_defaults_are_valid() {
        for pair in CONTROLS.windows(2) {
            assert!(
                pair[0].id < pair[1].id,
                "{} before {}",
                pair[0].name,
                pair[1].name
            );
        }
        for c in CONTROLS {
            assert_eq!(c.validate(c.default), Some(c.default), "{}", c.name);
        }
    }

    #[test]
    fn menus_refuse_what_is_not_offered_and_integers_clamp() {
        let profile = control(bindings::V4L2_CID_MPEG_VIDEO_H264_PROFILE).unwrap();
        assert_eq!(profile.validate(2), Some(2));
        assert_eq!(profile.validate(3), None, "Extended is not offered");
        assert_eq!(profile.validate(5), None);
        let hevc = control(bindings::V4L2_CID_MPEG_VIDEO_HEVC_PROFILE).unwrap();
        assert_eq!(hevc.validate(1), None, "Main Still Picture is not offered");
        let bframes = control(bindings::V4L2_CID_MPEG_VIDEO_B_FRAMES).unwrap();
        assert_eq!(bframes.validate(7), Some(3));
    }

    fn session() -> EncoderSession {
        EncoderSession::new(0, Arc::new(Doorbell::default()))
    }

    fn arrive(s: &mut EncoderSession, seq: i64, keyframe: bool) {
        s.stamps.insert(
            seq,
            bindings::timeval {
                tv_sec: 0,
                tv_usec: seq,
            },
        );
        s.shared
            .out
            .lock()
            .unwrap()
            .push_back(super::super::encoder::Encoded {
                seq,
                data: vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }],
                headers: keyframe.then(|| vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68]),
                keyframe,
            });
    }

    fn sizes(s: &EncoderSession) -> Vec<usize> {
        s.ready
            .iter()
            .map(|c| match c {
                Chunk::Data { bytes, .. } => bytes.len(),
                Chunk::Last => 0,
            })
            .collect()
    }

    #[test]
    fn joined_headers_ride_every_keyframe_by_default() {
        let mut s = session();
        arrive(&mut s, 0, true);
        arrive(&mut s, 1, false);
        arrive(&mut s, 2, true);
        s.collect();
        assert_eq!(sizes(&s), vec![15, 5, 15]);
    }

    #[test]
    fn without_repeats_only_the_first_keyframe_carries_headers() {
        let mut s = session();
        s.controls
            .insert(bindings::V4L2_CID_MPEG_VIDEO_REPEAT_SEQ_HEADER, 0);
        arrive(&mut s, 0, true);
        arrive(&mut s, 1, true);
        s.collect();
        assert_eq!(sizes(&s), vec![15, 5]);
        let header_mode = control(bindings::V4L2_CID_MPEG_VIDEO_HEADER_MODE).unwrap();
        assert_eq!(header_mode.validate(0), None, "SEPARATE is refused");
    }

    #[test]
    fn ten_bits_follow_the_input_or_the_profile_and_only_for_hevc() {
        let mut s = session();
        s.raw = Raw::P010;
        assert!(!s.ten_bit(), "H.264 is eight bits");
        s.coded = Coded::Hevc;
        assert!(s.ten_bit());
        s.raw = Raw::Nv12;
        assert!(!s.ten_bit());
        s.controls
            .insert(bindings::V4L2_CID_MPEG_VIDEO_HEVC_PROFILE, HEVC_MAIN_10);
        assert!(s.ten_bit());
    }
}

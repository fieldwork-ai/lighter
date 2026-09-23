//! The slice of VideoToolbox, CoreMedia, CoreVideo and CoreFoundation the
//! decoder uses, declared by hand. All C, all in system frameworks: nothing
//! is built or bundled, and a Mac without them has no lighter either.
//!
//! Signatures follow the framework headers of macOS 15. Where a header
//! defines a struct, its layout is reproduced with `repr(C)`; where it
//! defines an opaque type, a pointer stands for it.

#![allow(non_camel_case_types, non_upper_case_globals, dead_code)]

use std::ffi::c_void;

pub type OSStatus = i32;
pub type Boolean = u8;
pub type CFIndex = isize;
pub type CFTypeRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CMFormatDescriptionRef = *const c_void;
pub type CMVideoFormatDescriptionRef = CMFormatDescriptionRef;
pub type CMBlockBufferRef = *const c_void;
pub type CMSampleBufferRef = *const c_void;
pub type CVImageBufferRef = *const c_void;
pub type CVPixelBufferRef = CVImageBufferRef;
pub type VTDecompressionSessionRef = *const c_void;
pub type VTDecodeFrameFlags = u32;
pub type VTDecodeInfoFlags = u32;
pub type CVReturn = i32;

pub const kCFNumberSInt32Type: CFIndex = 3;
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
pub type CFDataRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFBooleanRef = *const c_void;
pub type VTCompressionSessionRef = *const c_void;
pub type VTEncodeInfoFlags = u32;
pub type CVPixelBufferPoolRef = *const c_void;
pub const kCFNumberSInt64Type: CFIndex = 4;
pub const kCFNumberFloat64Type: CFIndex = 13;
/// 'avc1', 'hvc1'.
pub const kCMVideoCodecType_H264: CMVideoCodecType = 0x6176_6331;
pub const kCMVideoCodecType_HEVC: CMVideoCodecType = 0x6876_6331;
/// 'y420': three-plane 4:2:0, what a V4L2 YU12 buffer is.
pub const kCVPixelFormatType_420YpCbCr8Planar: u32 = 0x7934_3230;

pub type VTCompressionOutputCallback = Option<
    unsafe extern "C" fn(
        output_callback_refcon: *mut c_void,
        source_frame_refcon: *mut c_void,
        status: OSStatus,
        info_flags: VTEncodeInfoFlags,
        sample_buffer: CMSampleBufferRef,
    ),
>;

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`, '420v': NV12.
pub const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: u32 = 0x3432_3076;
/// `kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange`, 'x420': P010's
/// layout, ten bits in the top of each sixteen.
pub const kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange: u32 = 0x7834_3230;

pub type CMVideoCodecType = u32;
/// 'vp09'.
pub const kCMVideoCodecType_VP9: CMVideoCodecType = 0x7670_3039;

pub const kCVPixelBufferLock_ReadOnly: u64 = 1;

pub const kVTDecodeFrame_EnableAsynchronousDecompression: VTDecodeFrameFlags = 1 << 0;
pub const kVTDecodeFrame_DoNotOutputFrame: VTDecodeFrameFlags = 1 << 1;
pub const kVTDecodeFrame_1xRealTimePlayback: VTDecodeFrameFlags = 1 << 2;
pub const kVTDecodeFrame_EnableTemporalProcessing: VTDecodeFrameFlags = 1 << 3;

pub const kCMTimeFlags_Valid: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

impl CMTime {
    pub const INVALID: CMTime = CMTime {
        value: 0,
        timescale: 0,
        flags: 0,
        epoch: 0,
    };

    pub fn micros(us: i64) -> CMTime {
        CMTime {
            value: us,
            timescale: 1_000_000,
            flags: kCMTimeFlags_Valid,
            epoch: 0,
        }
    }

    /// Microseconds, for a valid time.
    pub fn as_micros(&self) -> Option<i64> {
        if self.flags & kCMTimeFlags_Valid == 0 || self.timescale <= 0 {
            return None;
        }
        Some(self.value.saturating_mul(1_000_000) / i64::from(self.timescale))
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMSampleTimingInfo {
    pub duration: CMTime,
    pub presentation_time_stamp: CMTime,
    pub decode_time_stamp: CMTime,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMVideoDimensions {
    pub width: i32,
    pub height: i32,
}

pub type VTDecompressionOutputCallback = Option<
    unsafe extern "C" fn(
        refcon: *mut c_void,
        source_frame_refcon: *mut c_void,
        status: OSStatus,
        info_flags: VTDecodeInfoFlags,
        image_buffer: CVImageBufferRef,
        presentation_time_stamp: CMTime,
        presentation_duration: CMTime,
    ),
>;

#[repr(C)]
pub struct VTDecompressionOutputCallbackRecord {
    pub callback: VTDecompressionOutputCallback,
    pub refcon: *mut c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub static kCFAllocatorDefault: CFAllocatorRef;
    pub static kCFAllocatorNull: CFAllocatorRef;
    pub static kCFTypeDictionaryKeyCallBacks: c_void;
    pub static kCFTypeDictionaryValueCallBacks: c_void;
    pub fn CFRelease(cf: CFTypeRef);
    pub static kCFBooleanTrue: CFBooleanRef;
    pub static kCFBooleanFalse: CFBooleanRef;
    pub static kCFTypeArrayCallBacks: c_void;
    pub fn CFBooleanGetValue(boolean: CFBooleanRef) -> Boolean;
    pub fn CFArrayCreate(
        allocator: CFAllocatorRef,
        values: *const *const c_void,
        num_values: CFIndex,
        callbacks: *const c_void,
    ) -> CFArrayRef;
    pub fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> *const c_void;
    pub fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    pub fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    pub fn CFDataCreate(allocator: CFAllocatorRef, bytes: *const u8, length: CFIndex) -> CFDataRef;
    pub fn CFStringCreateWithCString(
        allocator: CFAllocatorRef,
        c_str: *const std::ffi::c_char,
        encoding: u32,
    ) -> CFStringRef;
    pub fn CFNumberCreate(
        allocator: CFAllocatorRef,
        kind: CFIndex,
        value: *const c_void,
    ) -> CFNumberRef;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    pub fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    pub static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFStringRef;
    pub fn CMVideoFormatDescriptionCreate(
        allocator: CFAllocatorRef,
        codec_type: CMVideoCodecType,
        width: i32,
        height: i32,
        extensions: CFDictionaryRef,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        extensions: CFDictionaryRef,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionGetDimensions(
        desc: CMVideoFormatDescriptionRef,
    ) -> CMVideoDimensions;
    pub fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    pub static kCMSampleAttachmentKey_NotSync: CFStringRef;
    pub fn CMFormatDescriptionGetMediaSubType(desc: CMFormatDescriptionRef) -> u32;
    pub fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
    pub fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
    pub fn CMSampleBufferGetPresentationTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
    pub fn CMSampleBufferGetSampleAttachmentsArray(
        sbuf: CMSampleBufferRef,
        create: Boolean,
    ) -> CFArrayRef;
    pub fn CMBlockBufferGetDataLength(buffer: CMBlockBufferRef) -> usize;
    pub fn CMBlockBufferCopyDataBytes(
        buffer: CMBlockBufferRef,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        pointer_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_length_out: *mut i32,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        pointer_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_length_out: *mut i32,
    ) -> OSStatus;
    pub fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: CFIndex,
        num_sample_timing_entries: CFIndex,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: CFIndex,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    pub fn CVPixelBufferRetain(pb: CVPixelBufferRef) -> CVPixelBufferRef;
    pub fn CVPixelBufferRelease(pb: CVPixelBufferRef);
    pub fn CVPixelBufferLockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> CVReturn;
    pub fn CVPixelBufferUnlockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> CVReturn;
    pub fn CVPixelBufferGetWidth(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetPixelFormatType(pb: CVPixelBufferRef) -> u32;
    pub fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: CFAllocatorRef,
        pool: CVPixelBufferPoolRef,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> CVReturn;
    pub fn CVPixelBufferGetHeight(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetPlaneCount(pb: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetBaseAddressOfPlane(pb: CVPixelBufferRef, plane: usize) -> *mut c_void;
    pub fn CVPixelBufferGetBytesPerRowOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    pub fn CVPixelBufferGetWidthOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    pub fn CVPixelBufferGetHeightOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    pub fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMVideoFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    pub fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: VTDecodeFrameFlags,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut VTDecodeInfoFlags,
    ) -> OSStatus;
    pub fn VTDecompressionSessionFinishDelayedFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;
    pub fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;
    pub fn VTDecompressionSessionCanAcceptFormatDescription(
        session: VTDecompressionSessionRef,
        new_format_desc: CMFormatDescriptionRef,
    ) -> Boolean;
    pub fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
    pub fn VTRegisterSupplementalVideoDecoderIfAvailable(codec_type: CMVideoCodecType);
    pub static kVTCompressionPropertyKey_RealTime: CFStringRef;
    pub static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    pub static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    pub static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    pub static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    pub static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    pub static kVTCompressionPropertyKey_ConstantBitRate: CFStringRef;
    pub static kVTCompressionPropertyKey_MinAllowedFrameQP: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxAllowedFrameQP: CFStringRef;
    pub static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    pub static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_HEVC_Main_AutoLevel: CFStringRef;
    pub static kVTProfileLevel_HEVC_Main10_AutoLevel: CFStringRef;
    pub fn VTSessionSetProperty(
        session: *const c_void,
        key: CFStringRef,
        value: CFTypeRef,
    ) -> OSStatus;
    pub fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec_type: CMVideoCodecType,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFAllocatorRef,
        output_callback: VTCompressionOutputCallback,
        output_callback_refcon: *mut c_void,
        compression_session_out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    pub fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    pub fn VTCompressionSessionGetPixelBufferPool(
        session: VTCompressionSessionRef,
    ) -> CVPixelBufferPoolRef;
    pub fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVImageBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut VTEncodeInfoFlags,
    ) -> OSStatus;
    pub fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until_presentation_time_stamp: CMTime,
    ) -> OSStatus;
    pub fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
    pub fn VTIsHardwareDecodeSupported(codec_type: CMVideoCodecType) -> Boolean;
}

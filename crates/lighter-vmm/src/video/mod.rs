//! Video decode and encode on the Mac, for the guest's V4L2 devices
//! (`virtio::media`).

pub mod bits;
pub mod codec;
pub mod decoder;
pub mod encoder;
pub mod encoder_device;
pub mod h264;
pub mod hevc;
pub mod vp9;
pub mod vt_sys;

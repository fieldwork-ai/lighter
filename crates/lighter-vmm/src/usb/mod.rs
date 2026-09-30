//! USB devices on the Mac, served to the guest over USB/IP (0.11.0).
//!
//! `usbip` is the wire format, `server` runs every attached device's session
//! on one thread, and `iousb` is the Mac's side of a device: IOUSBHost,
//! seized from macOS's driver as the user and given back on release.

#[cfg(target_os = "macos")]
pub mod iousb;
pub mod server;
pub mod usbip;

//! A USB device on the Mac, through the IOUSBHost shim (`iousb.m`).

use std::ffi::{CStr, c_char, c_void};

use super::server::{Device, Done, Op, Sink};

#[repr(C)]
struct Raw {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawInfo {
    registry_id: u64,
    location_id: u32,
    vendor_id: u16,
    product_id: u16,
    bcd_device: u16,
    device_class: u8,
    speed: u8,
    interface_classes: u64,
    vendor: [c_char; 128],
    product: [c_char; 128],
    serial: [c_char; 128],
    driver: [c_char; 128],
    callout: [c_char; 128],
}

type DoneFn = extern "C" fn(*mut c_void, u32, i32, u32, *const u8);
type GoneFn = extern "C" fn(*mut c_void);
type ClosedFn = extern "C" fn(*mut c_void);

unsafe extern "C" {
    fn lighter_usb_list(out: *mut RawInfo, max: i32) -> i32;
    fn lighter_usb_open(
        registry_id: u64,
        ctx: *mut c_void,
        done: DoneFn,
        gone: GoneFn,
        closed: ClosedFn,
        error: *mut c_char,
        error_len: usize,
    ) -> *mut Raw;
    fn lighter_usb_control(d: *mut Raw, tag: u32, setup: *const u8, out: *const u8, out_len: u32, in_len: u32);
    fn lighter_usb_transfer(d: *mut Raw, tag: u32, endpoint: u8, out: *const u8, out_len: u32, in_len: u32);
    fn lighter_usb_set_configuration(d: *mut Raw, tag: u32, value: u8);
    fn lighter_usb_set_interface(d: *mut Raw, tag: u32, interface: u8, alternate: u8);
    fn lighter_usb_clear_halt(d: *mut Raw, tag: u32, endpoint: u8);
    fn lighter_usb_reset(d: *mut Raw, tag: u32);
    fn lighter_usb_abort(d: *mut Raw, endpoint: u8);
    fn lighter_usb_close(d: *mut Raw);
    fn lighter_usb_port_holder(path: *const c_char, pid: *mut i32, name: *mut c_char, name_len: usize) -> i32;
    fn lighter_usb_watch(ctx: *mut c_void, arrived: extern "C" fn(*mut c_void));
    fn lighter_usb_restore(registry_id: u64, error: *mut c_char, error_len: usize) -> i32;
}

/// Gives back to macOS a device a lighter that is no longer running left
/// seized and unconfigured. Nothing to do for a device no longer attached.
pub fn restore(registry_id: u64) -> Result<(), String> {
    let mut error = [0 as c_char; 256];
    // SAFETY: a message buffer of the length given.
    if unsafe { lighter_usb_restore(registry_id, error.as_mut_ptr(), error.len()) } == 0 {
        return Ok(());
    }
    // SAFETY: the shim wrote a NUL-terminated message.
    Err(unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned())
}

/// Calls `arrived` whenever a USB device appears on the Mac, for the life of
/// the process.
pub fn watch(arrived: Box<dyn Fn() + Send + Sync>) {
    extern "C" fn trampoline(ctx: *mut c_void) {
        // SAFETY: the Box leaked below, alive for the process.
        let f = unsafe { &*(ctx as *const Box<dyn Fn() + Send + Sync>) };
        f();
    }
    let ctx = Box::into_raw(Box::new(arrived)) as *mut c_void;
    // SAFETY: a callback and a context that live as long as the process.
    unsafe { lighter_usb_watch(ctx, trampoline) };
}

/// USB speeds as the guest's `vhci-hcd` wants them (`enum usb_device_speed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Low = 1,
    Full = 2,
    High = 3,
    Super = 5,
    SuperPlus = 6,
}

/// A USB device attached to the Mac.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub registry_id: u64,
    pub location_id: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub speed: Option<Speed>,
    /// Bit n: an interface of class n (n < 62); bit 62 a wireless controller
    /// (Bluetooth, class 0xe0); bit 63 vendor-specific (0xff).
    pub interface_classes: u64,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    /// The macOS driver bound to its first interface, if any.
    pub driver: String,
    /// Its serial port on the Mac (`/dev/cu.*`), if it has one.
    pub callout: Option<String>,
}

pub const CLASS_HID: u8 = 0x03;
pub const CLASS_MASS_STORAGE: u8 = 0x08;
pub const CLASS_HUB: u8 = 0x09;

impl Info {
    pub fn has_interface_class(&self, class: u8) -> bool {
        match class {
            0xe0 => self.interface_classes & (1 << 62) != 0,
            0xff => self.interface_classes & (1 << 63) != 0,
            c if c < 62 => self.interface_classes & (1 << c) != 0,
            _ => false,
        }
    }
}

fn text(raw: &[c_char; 128]) -> String {
    // SAFETY: the shim writes NUL-terminated strings within the array.
    unsafe { CStr::from_ptr(raw.as_ptr()) }.to_string_lossy().trim().to_owned()
}

/// Every USB device on the Mac.
pub fn list() -> Vec<Info> {
    let mut raw: Vec<RawInfo> = Vec::with_capacity(64);
    loop {
        let cap = raw.capacity();
        // SAFETY: the shim writes at most `cap` entries into the buffer.
        let n = unsafe { lighter_usb_list(raw.as_mut_ptr(), cap as i32) } as usize;
        if n <= cap {
            // SAFETY: the shim initialized the first n entries.
            unsafe { raw.set_len(n) };
            break;
        }
        raw = Vec::with_capacity(n + 16);
    }
    raw.iter()
        .map(|r| Info {
            registry_id: r.registry_id,
            location_id: r.location_id,
            vendor_id: r.vendor_id,
            product_id: r.product_id,
            bcd_device: r.bcd_device,
            device_class: r.device_class,
            speed: match r.speed {
                0 => Some(Speed::Low),
                1 => Some(Speed::Full),
                2 => Some(Speed::High),
                3 => Some(Speed::Super),
                4 => Some(Speed::SuperPlus),
                _ => None,
            },
            interface_classes: r.interface_classes,
            vendor: text(&r.vendor),
            product: text(&r.product),
            serial: text(&r.serial),
            driver: text(&r.driver),
            callout: Some(text(&r.callout)).filter(|c| !c.is_empty()),
        })
        .collect()
}

/// Another process holding `path` (a device's `/dev/cu.*`) open: its pid and
/// name.
pub fn port_holder(path: &str) -> Option<(i32, String)> {
    let path = std::ffi::CString::new(path).ok()?;
    let mut pid = 0i32;
    let mut name = [0 as c_char; 256];
    // SAFETY: a C string, and out-parameters of the sizes given.
    let found = unsafe { lighter_usb_port_holder(path.as_ptr(), &mut pid, name.as_mut_ptr(), name.len()) };
    if found == 0 {
        return None;
    }
    // SAFETY: proc_name wrote a NUL-terminated name within the buffer.
    let name = unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned();
    Some((pid, name))
}

/// A device seized from macOS and served to the guest.
pub struct IoUsbDevice {
    raw: *mut Raw,
}

// SAFETY: the shim serializes every operation on the device's own dispatch
// queue; the handle itself is only read.
unsafe impl Send for IoUsbDevice {}

extern "C" fn on_done(ctx: *mut c_void, tag: u32, status: i32, actual: u32, data: *const u8) {
    // SAFETY: ctx is the Box<Sink> leaked at open, freed only by on_closed,
    // which the shim calls last.
    let sink = unsafe { &*(ctx as *const Sink) };
    let data = if data.is_null() || actual == 0 {
        Vec::new()
    } else {
        // SAFETY: the shim passes `actual` readable bytes for an in transfer.
        unsafe { std::slice::from_raw_parts(data, actual as usize) }.to_vec()
    };
    sink.done(Done { tag, status, actual: actual as usize, data });
}

extern "C" fn on_gone(ctx: *mut c_void) {
    // SAFETY: as in on_done.
    let sink = unsafe { &*(ctx as *const Sink) };
    sink.gone();
}

extern "C" fn on_closed(ctx: *mut c_void) {
    // SAFETY: the Box leaked at open, returned once, after the last callback.
    drop(unsafe { Box::from_raw(ctx as *mut Sink) });
}

impl IoUsbDevice {
    /// Seizes the device and hands its completions to `sink`.
    pub fn open(registry_id: u64, sink: Sink) -> Result<IoUsbDevice, String> {
        let ctx = Box::into_raw(Box::new(sink)) as *mut c_void;
        let mut error = [0 as c_char; 256];
        // SAFETY: callbacks that honor the shim's contract, and a message buffer
        // of the length given.
        let raw = unsafe {
            lighter_usb_open(registry_id, ctx, on_done, on_gone, on_closed, error.as_mut_ptr(), error.len())
        };
        if raw.is_null() {
            // SAFETY: the shim did not keep ctx; it is ours to free.
            drop(unsafe { Box::from_raw(ctx as *mut Sink) });
            // SAFETY: the shim wrote a NUL-terminated message.
            return Err(unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned());
        }
        Ok(IoUsbDevice { raw })
    }
}

impl Device for IoUsbDevice {
    fn start(&mut self, tag: u32, op: Op) {
        // SAFETY: a live handle; buffers valid for the call (the shim copies).
        unsafe {
            match op {
                Op::Control { setup, out, in_len } => lighter_usb_control(
                    self.raw,
                    tag,
                    setup.as_ptr(),
                    out.as_ptr(),
                    out.len() as u32,
                    in_len as u32,
                ),
                Op::Transfer { endpoint, out, in_len } => {
                    lighter_usb_transfer(self.raw, tag, endpoint, out.as_ptr(), out.len() as u32, in_len as u32)
                }
                Op::SetConfiguration(value) => lighter_usb_set_configuration(self.raw, tag, value),
                Op::SetInterface { interface, alternate } => {
                    lighter_usb_set_interface(self.raw, tag, interface, alternate)
                }
                Op::ClearHalt(endpoint) => lighter_usb_clear_halt(self.raw, tag, endpoint),
                Op::Reset => lighter_usb_reset(self.raw, tag),
            }
        }
    }

    fn abort(&mut self, endpoint: u8) {
        // SAFETY: a live handle.
        unsafe { lighter_usb_abort(self.raw, endpoint) };
    }

    fn close(&mut self) {
        if self.raw.is_null() {
            return;
        }
        // SAFETY: a live handle, closed once.
        unsafe { lighter_usb_close(self.raw) };
        self.raw = std::ptr::null_mut();
    }
}

impl Drop for IoUsbDevice {
    fn drop(&mut self) {
        self.close();
    }
}

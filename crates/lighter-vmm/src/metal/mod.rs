//! ggml on the Mac's GPU, for containers: `lighter.sh/metal`.
//!
//! A container's llama.cpp (or whisper.cpp, or anything on ggml) built with
//! the RPC backend hands its tensors and graphs to a ggml RPC server; here
//! that server runs inside lighter, on the Mac's Metal backend, with ggml's
//! own Metal kernels. The weights cross once at load and the activations of
//! each token after that, which is why this reaches native speed where the
//! Vulkan device (`virtio::gpu`) stops at what MoltenVK can translate.
//!
//! Loopback only, an ephemeral port per machine, reached from the container
//! through the streams (`lighter.metal=<port>` on the kernel command line,
//! `LIGHTER_METAL` and `LLAMA_ARG_RPC` in the container). ggml's own note
//! stands: the protocol is not hardened, so it is never bound anywhere but
//! loopback.
//!
//! The protocol has a version, checked at connect: the container's ggml must
//! be the one this lighter was built with (`host/metal/build.sh` pins it and
//! `docs/gpu.md` names it).

use std::ffi::{CString, c_char, c_void};
use std::io;
use std::net::{Ipv4Addr, TcpListener};

#[repr(C)]
pub struct GgmlBackendDevice {
    _private: [u8; 0],
}

pub const GGML_BACKEND_DEVICE_TYPE_CPU: u32 = 0;
pub const GGML_BACKEND_DEVICE_TYPE_GPU: u32 = 1;

#[cfg(ggml_libs)]
unsafe extern "C" {
    fn ggml_backend_dev_by_type(kind: u32) -> *mut GgmlBackendDevice;
    fn ggml_backend_dev_name(dev: *mut GgmlBackendDevice) -> *const c_char;
    fn ggml_backend_dev_memory(dev: *mut GgmlBackendDevice, free: *mut usize, total: *mut usize);
    fn ggml_backend_rpc_start_server(
        endpoint: *const c_char,
        cache_dir: *const c_char,
        n_threads: usize,
        n_devices: usize,
        devices: *mut *mut GgmlBackendDevice,
    );
}

#[cfg(not(ggml_libs))]
mod stub {
    use super::*;
    pub unsafe fn ggml_backend_dev_by_type(_: u32) -> *mut GgmlBackendDevice {
        std::ptr::null_mut()
    }
    pub unsafe fn ggml_backend_dev_name(_: *mut GgmlBackendDevice) -> *const c_char {
        c"".as_ptr()
    }
    pub unsafe fn ggml_backend_dev_memory(
        _: *mut GgmlBackendDevice,
        free: *mut usize,
        total: *mut usize,
    ) {
        unsafe {
            *free = 0;
            *total = 0;
        }
    }
    pub unsafe fn ggml_backend_rpc_start_server(
        _: *const c_char,
        _: *const c_char,
        _: usize,
        _: usize,
        _: *mut *mut GgmlBackendDevice,
    ) {
    }
}
#[cfg(not(ggml_libs))]
use stub::*;

/// Whether ggml was linked in at all.
pub const fn linked() -> bool {
    cfg!(ggml_libs)
}

pub struct Server {
    port: u16,
}

// SAFETY: the device pointer is ggml's registry entry, valid for the process.
struct Device(*mut GgmlBackendDevice);
unsafe impl Send for Device {}

impl Server {
    /// Starts ggml's RPC server on loopback, on the Mac's GPU, from a thread.
    /// The port is chosen by binding and releasing one, since ggml takes an
    /// endpoint string rather than a socket; the window between is the same
    /// one every "pick a free port" has.
    pub fn start() -> io::Result<Server> {
        if !linked() {
            return Err(io::Error::other(
                "lighter was built without ggml (host/metal/build.sh)",
            ));
        }
        let dev = unsafe { ggml_backend_dev_by_type(GGML_BACKEND_DEVICE_TYPE_GPU) };
        if dev.is_null() {
            return Err(io::Error::other("ggml found no GPU device"));
        }
        let name = unsafe { std::ffi::CStr::from_ptr(ggml_backend_dev_name(dev)) }
            .to_string_lossy()
            .into_owned();
        let (mut free, mut total) = (0usize, 0usize);
        unsafe { ggml_backend_dev_memory(dev, &mut free, &mut total) };
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = probe.local_addr()?.port();
        drop(probe);
        let endpoint = CString::new(format!("127.0.0.1:{port}")).unwrap();
        let device = Device(dev);
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get() / 2)
            .max(1);
        crate::qos::register_accelerator_port(port);
        std::thread::Builder::new()
            .name("metal-rpc".into())
            .spawn(move || {
                // The server encodes every token's kernels on this thread;
                // at the default class macOS parks it on an efficiency core
                // while the vCPUs hold the performance cores, and a token
                // takes half again as long as it does for a native client.
                crate::qos::raise_interactive();
                let device = device;
                let mut devices = [device.0];
                // Blocks for the life of the process: ggml's accept loop.
                unsafe {
                    ggml_backend_rpc_start_server(
                        endpoint.as_ptr(),
                        std::ptr::null(),
                        threads,
                        1,
                        devices.as_mut_ptr(),
                    )
                };
                let _ = &devices as *const _ as *const c_void;
            })?;
        tracing::info!(port, device = %name, total_mib = total >> 20, "ggml rpc server on the Mac's GPU");
        Ok(Server { port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

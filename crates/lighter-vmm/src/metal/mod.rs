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
//!
//! The listener is lighter's, not ggml's. ggml's own server accepts on a
//! socket it bound, serves one client at a time, and returns from its loop
//! on the first accept error, which macOS raises for a queued client that
//! resets before it is taken (ECONNABORTED): a benchmark that was killed
//! left the device dead for the machine's life. So lighter accepts, and
//! hands each descriptor to a carried ggml entry point
//! (`host/metal/patches/0001`) on a thread of its own, with backends of its
//! own, so a resident whisper and an on-demand llama.cpp share the GPU.

use std::ffi::{CString, c_char};
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::IntoRawFd;
use std::path::Path;
use std::sync::Arc;

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
    fn ggml_backend_rpc_serve_fd(
        fd: i32,
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
    pub unsafe fn ggml_backend_rpc_serve_fd(
        _: i32,
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
#[derive(Clone, Copy)]
struct Device(*mut GgmlBackendDevice);
unsafe impl Send for Device {}

impl Server {
    /// Binds loopback on a port of the system's choosing and accepts from a
    /// thread; every client is served on a thread of its own, with ggml
    /// backends of its own.
    ///
    /// `cache` is a directory for ggml's tensor cache: a weight over 10 MiB
    /// is sent as a hash first, and one the server has seen before is read
    /// from here instead of crossing the stream again, so a model's second
    /// load costs its small tensors only. It grows with the models used
    /// and is safe to delete.
    pub fn start(cache: Option<&Path>) -> io::Result<Server> {
        if !linked() {
            return Err(io::Error::other(
                "lighter was built without ggml (host/metal/build.sh)",
            ));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        crate::qos::register_accelerator_port(port);
        let cache = match cache {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                Some(Arc::new(
                    CString::new(dir.as_os_str().as_encoded_bytes())
                        .map_err(|_| io::Error::other("the cache path holds a NUL byte"))?,
                ))
            }
            None => None,
        };
        std::thread::Builder::new()
            .name("metal-accept".into())
            .spawn(move || {
                // The Metal device is found and its library compiled here,
                // off the machine's start: the guest only needs the port,
                // and the first client waits the few milliseconds instead.
                let dev = unsafe { ggml_backend_dev_by_type(GGML_BACKEND_DEVICE_TYPE_GPU) };
                if dev.is_null() {
                    tracing::warn!(port, "ggml found no GPU device; lighter.sh/metal is absent this run");
                    return;
                }
                let name = unsafe { std::ffi::CStr::from_ptr(ggml_backend_dev_name(dev)) }
                    .to_string_lossy()
                    .into_owned();
                let (mut free, mut total) = (0usize, 0usize);
                unsafe { ggml_backend_dev_memory(dev, &mut free, &mut total) };
                tracing::info!(port, device = %name, total_mib = total >> 20, "ggml rpc server on the Mac's GPU");
                let device = Device(dev);
                let threads = std::thread::available_parallelism()
                    .map_or(4, |n| n.get() / 2)
                    .max(1);
                for accepted in listener.incoming() {
                    let stream = match accepted {
                        Ok(stream) => stream,
                        // A queued client that reset before it was taken, or
                        // a descriptor table at its limit: neither ends the
                        // device.
                        Err(e) => {
                            tracing::debug!(%e, "ggml rpc: accept");
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            continue;
                        }
                    };
                    let _ = stream.set_nodelay(true);
                    crate::sockbuf::widen(&stream);
                    let fd = stream.into_raw_fd();
                    let cache = cache.clone();
                    let spawned = std::thread::Builder::new()
                        .name("metal-rpc".into())
                        .spawn(move || {
                            // The client's kernels are encoded on this
                            // thread; at the default class macOS parks it on
                            // an efficiency core while the vCPUs hold the
                            // performance cores, and a token takes half
                            // again as long as it does for a native client.
                            crate::qos::raise_interactive();
                            let device = device;
                            let mut devices = [device.0];
                            // Takes the descriptor: ggml closes it when the
                            // client hangs up.
                            unsafe {
                                ggml_backend_rpc_serve_fd(
                                    fd,
                                    cache.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                                    threads,
                                    1,
                                    devices.as_mut_ptr(),
                                )
                            };
                        });
                    if let Err(e) = spawned {
                        tracing::warn!(%e, "ggml rpc: could not serve a client");
                        // SAFETY: ours, and no thread took it.
                        unsafe { libc::close(fd) };
                    }
                }
            })?;
        Ok(Server { port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

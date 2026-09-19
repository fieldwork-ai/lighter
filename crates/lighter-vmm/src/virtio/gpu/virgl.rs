//! virglrenderer, statically linked: the Venus renderer over MoltenVK.
//!
//! One renderer per process, initialised on the first capset query and torn
//! down with the device. Every call except the fence callbacks happens under
//! the transport lock, which is the serialisation virglrenderer requires; the
//! callbacks arrive on the renderer's own threads and touch only a queue.
//!
//! Built without the libraries (`LIGHTER_GPU_LIBS` unset and `host/out`
//! empty) this module is a stub that reports the GPU as unavailable, so the
//! rest of the VMM still compiles and the device simply refuses contexts.

use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub struct Iovec {
    pub base: *mut c_void,
    pub len: usize,
}

#[repr(C)]
pub struct BlobArgs {
    pub res_handle: u32,
    pub ctx_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub blob_id: u64,
    pub size: u64,
    pub iovecs: *const Iovec,
    pub num_iovs: u32,
}

/// `struct virgl_renderer_callbacks`, version 4.
#[repr(C)]
pub struct Callbacks {
    pub version: c_int,
    pub write_fence: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    pub create_gl_context: *const c_void,
    pub destroy_gl_context: *const c_void,
    pub make_current: *const c_void,
    pub get_drm_fd: *const c_void,
    pub write_context_fence: Option<unsafe extern "C" fn(*mut c_void, u32, u32, u64)>,
    pub get_server_fd: *const c_void,
    pub get_egl_display: *const c_void,
}

pub const THREAD_SYNC: c_int = 1 << 1;
pub const VENUS: c_int = 1 << 6;
pub const NO_VIRGL: c_int = 1 << 7;
pub const ASYNC_FENCE_CB: c_int = 1 << 8;
pub const RENDER_SERVER: c_int = 1 << 9;

#[cfg(gpu_libs)]
#[link(name = "virglrenderer", kind = "static")]
#[link(name = "virgl", kind = "static")]
#[link(name = "mesa", kind = "static")]
#[link(name = "MoltenVK", kind = "static")]
#[link(name = "Metal", kind = "framework")]
#[link(name = "Foundation", kind = "framework")]
#[link(name = "IOSurface", kind = "framework")]
#[link(name = "QuartzCore", kind = "framework")]
#[link(name = "CoreGraphics", kind = "framework")]
#[link(name = "IOKit", kind = "framework")]
#[link(name = "AppKit", kind = "framework")]
#[link(name = "objc")]
#[link(name = "c++")]
#[link(name = "clang_rt.osx", kind = "static")]
unsafe extern "C" {
    pub fn virgl_renderer_init(cookie: *mut c_void, flags: c_int, cb: *mut Callbacks) -> c_int;
    pub fn virgl_renderer_cleanup(cookie: *mut c_void);
    pub fn virgl_renderer_get_cap_set(set: u32, max_ver: *mut u32, max_size: *mut u32);
    pub fn virgl_renderer_fill_caps(set: u32, version: u32, caps: *mut c_void);
    pub fn virgl_renderer_context_create_with_flags(
        ctx_id: u32,
        flags: u32,
        nlen: u32,
        name: *const c_char,
    ) -> c_int;
    pub fn virgl_renderer_context_destroy(ctx_id: u32);
    pub fn virgl_renderer_ctx_attach_resource(ctx_id: c_int, res_handle: c_int);
    pub fn virgl_renderer_ctx_detach_resource(ctx_id: c_int, res_handle: c_int);
    pub fn virgl_renderer_resource_create_blob(args: *const BlobArgs) -> c_int;
    pub fn virgl_renderer_resource_unref(res_handle: u32);
    pub fn virgl_renderer_resource_map(res_handle: u32, map: *mut *mut c_void, size: *mut u64) -> c_int;
    pub fn virgl_renderer_resource_unmap(res_handle: u32) -> c_int;
    pub fn virgl_renderer_resource_get_map_info(res_handle: u32, map_info: *mut u32) -> c_int;
    pub fn virgl_renderer_submit_cmd(buffer: *mut c_void, ctx_id: c_int, ndw: c_int) -> c_int;
    pub fn virgl_renderer_context_create_fence(ctx_id: u32, flags: u32, ring_idx: u32, fence_id: u64) -> c_int;
    pub fn virgl_renderer_create_fence(client_fence_id: c_int, ctx_id: u32) -> c_int;
}

#[cfg(not(gpu_libs))]
mod stub {
    use super::*;
    pub unsafe fn virgl_renderer_init(_: *mut c_void, _: c_int, _: *mut Callbacks) -> c_int { -1 }
    pub unsafe fn virgl_renderer_cleanup(_: *mut c_void) {}
    pub unsafe fn virgl_renderer_get_cap_set(_: u32, v: *mut u32, s: *mut u32) { unsafe { *v = 0; *s = 0 } }
    pub unsafe fn virgl_renderer_fill_caps(_: u32, _: u32, _: *mut c_void) {}
    pub unsafe fn virgl_renderer_context_create_with_flags(_: u32, _: u32, _: u32, _: *const c_char) -> c_int { -1 }
    pub unsafe fn virgl_renderer_context_destroy(_: u32) {}
    pub unsafe fn virgl_renderer_ctx_attach_resource(_: c_int, _: c_int) {}
    pub unsafe fn virgl_renderer_ctx_detach_resource(_: c_int, _: c_int) {}
    pub unsafe fn virgl_renderer_resource_create_blob(_: *const BlobArgs) -> c_int { -1 }
    pub unsafe fn virgl_renderer_resource_unref(_: u32) {}
    pub unsafe fn virgl_renderer_resource_map(_: u32, _: *mut *mut c_void, _: *mut u64) -> c_int { -1 }
    pub unsafe fn virgl_renderer_resource_unmap(_: u32) -> c_int { -1 }
    pub unsafe fn virgl_renderer_resource_get_map_info(_: u32, _: *mut u32) -> c_int { -1 }
    pub unsafe fn virgl_renderer_submit_cmd(_: *mut c_void, _: c_int, _: c_int) -> c_int { -1 }
    pub unsafe fn virgl_renderer_context_create_fence(_: u32, _: u32, _: u32, _: u64) -> c_int { -1 }
    pub unsafe fn virgl_renderer_create_fence(_: c_int, _: u32) -> c_int { -1 }
}
#[cfg(not(gpu_libs))]
pub use stub::*;

/// Whether the renderer was linked in at all.
pub const fn linked() -> bool {
    cfg!(gpu_libs)
}

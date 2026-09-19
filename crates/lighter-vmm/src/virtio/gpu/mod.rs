//! virtio-gpu, as a render node and nothing else.
//!
//! No scanout, no cursor, no 2D: the guest sees a GPU with zero displays and
//! one capset, Venus, which is Vulkan serialised over the virtqueue. The
//! renderer is virglrenderer's Venus implementation, statically linked and
//! running MoltenVK on the host's Metal device (`virgl`). The container gets
//! `/dev/dri/renderD128`, and Mesa's Venus driver inside it does the rest.
//!
//! # Memory
//!
//! A blob the guest maps is host memory — a Metal buffer the renderer owns —
//! placed in the guest's physical address space by the hypervisor, inside the
//! aperture the layout reserved (`GuestLayout::gpu`). Nothing backs the
//! aperture until a blob is mapped there, and the mapping is removed the moment
//! the guest unmaps the blob. Those pages are the renderer's, never counted as
//! guest RAM: the balloon, virtio-mem and the reclaim passes never see them.
//! Apple silicon maps in 16 KiB pages, so the guest kernel places blobs on
//! 16 KiB boundaries (patch 0031) and the host rounds sizes to match.
//!
//! # Threads
//!
//! Every renderer call is made from `notify`, under the transport lock, which
//! is the serialisation virglrenderer wants. Fences complete on the renderer's
//! own threads: the callback records the fence and wakes a thread of the
//! device's, which takes the transport lock and finishes the waiting commands.
//! The callback itself never takes that lock — the vCPU may be holding it
//! inside a renderer call that is waiting on the very thread the callback
//! runs on.
//!
//! # Cost when unused
//!
//! The renderer is initialised the first time the guest asks for a capset,
//! which the driver does at probe, so a machine with a GPU pays the renderer's
//! initialisation at boot; the Metal device and MoltenVK instance behind it
//! are what that costs, and it is measured before this ships as a default.

pub mod virgl;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};

use crate::layout::Window;
use crate::memory::{GuestMemory, HOST_PAGE};
use crate::virtio::mmio::COMMON_FEATURES;
use crate::virtio::queue::{Descriptor, Virtqueue};
use crate::virtio::{Serviced, ShmRegion, VirtioDevice, device_type};

pub const CONTROL_QUEUE: u16 = 0;
pub const CURSOR_QUEUE: u16 = 1;

/// A fence the renderer has signalled.
#[derive(Debug, Clone, Copy)]
struct Signalled {
    ctx_id: u32,
    ring_idx: u32,
    fence_id: u64,
}

/// What the renderer's threads hand to the device's completion thread.
#[derive(Default)]
pub struct Fences {
    done: Mutex<VecDeque<Signalled>>,
    arrived: Condvar,
}

impl Fences {
    fn push(&self, f: Signalled) {
        tracing::trace!(
            ctx = f.ctx_id,
            ring = f.ring_idx,
            fence = f.fence_id,
            "gpu fence signalled"
        );
        self.done.lock().expect("gpu fences poisoned").push_back(f);
        self.arrived.notify_one();
    }

    /// Blocks until at least one fence has been signalled since the last
    /// drain. The completion thread's wait.
    pub fn wait(&self) {
        let mut done = self.done.lock().expect("gpu fences poisoned");
        while done.is_empty() {
            done = self.arrived.wait(done).expect("gpu fences poisoned");
        }
    }

    fn drain(&self) -> Vec<Signalled> {
        self.done
            .lock()
            .expect("gpu fences poisoned")
            .drain(..)
            .collect()
    }
}

unsafe extern "C" fn write_fence(cookie: *mut c_void, fence_id: u32) {
    // SAFETY: the cookie is the `Arc<Fences>` the device holds for as long as
    // the renderer is initialised.
    let fences = unsafe { &*(cookie as *const Fences) };
    fences.push(Signalled {
        ctx_id: 0,
        ring_idx: 0,
        fence_id: u64::from(fence_id),
    });
}

unsafe extern "C" fn write_context_fence(
    cookie: *mut c_void,
    ctx_id: u32,
    ring_idx: u32,
    fence_id: u64,
) {
    let fences = unsafe { &*(cookie as *const Fences) };
    fences.push(Signalled {
        ctx_id,
        ring_idx,
        fence_id,
    });
}

/// A command whose completion waits on a fence: its response is written, its
/// descriptors are held back from the used ring until the fence signals.
struct Pending {
    head: u16,
    len: u32,
    ctx_id: u32,
    ring_idx: u32,
    fence_id: u64,
}

struct Resource {
    /// Guest pages backing a guest-memory blob; the renderer keeps the
    /// pointer, so the vector lives as long as the resource.
    _iovs: Vec<virgl::Iovec>,
    /// Where in the guest a host blob is mapped, and how much.
    mapped: Option<(u64, usize)>,
}

pub struct Gpu {
    aperture: Window,
    memory: Option<Arc<GuestMemory>>,
    fences: Arc<Fences>,
    renderer: bool,
    /// The Venus capset: (max version, bytes), read once from the renderer.
    caps: Option<(u32, Vec<u8>)>,
    resources: HashMap<u32, Resource>,
    contexts: HashMap<u32, ()>,
    pending: Vec<Pending>,
    /// Kept for the renderer's lifetime: it holds the callback pointers.
    callbacks: Box<virgl::Callbacks>,
}

// SAFETY: the raw pointers in `callbacks` and `Resource::_iovs` are only ever
// used from `notify`, which runs under the transport lock.
unsafe impl Send for Gpu {}

impl Gpu {
    pub fn new(aperture: Window) -> Gpu {
        Gpu {
            aperture,
            memory: None,
            fences: Arc::new(Fences::default()),
            renderer: false,
            caps: None,
            resources: HashMap::new(),
            contexts: HashMap::new(),
            pending: Vec::new(),
            callbacks: Box::new(virgl::Callbacks {
                version: 4,
                write_fence: Some(write_fence),
                create_gl_context: std::ptr::null(),
                destroy_gl_context: std::ptr::null(),
                make_current: std::ptr::null(),
                get_drm_fd: std::ptr::null(),
                write_context_fence: Some(write_context_fence),
                get_server_fd: std::ptr::null(),
                get_egl_display: std::ptr::null(),
            }),
        }
    }

    /// The queue the renderer's threads signal fences on; the machine gives
    /// it a thread that services the control queue when they do.
    pub fn fences(&self) -> Arc<Fences> {
        self.fences.clone()
    }

    fn ensure_renderer(&mut self) -> bool {
        if self.renderer {
            return true;
        }
        if !virgl::linked() {
            return false;
        }
        let cookie = Arc::as_ptr(&self.fences) as *mut c_void;
        let flags = virgl::THREAD_SYNC
            | virgl::VENUS
            | virgl::NO_VIRGL
            | virgl::ASYNC_FENCE_CB
            | virgl::RENDER_SERVER;
        // SAFETY: the callbacks and cookie outlive the renderer (see Drop).
        let r = unsafe { virgl::virgl_renderer_init(cookie, flags, &mut *self.callbacks) };
        if r != 0 {
            tracing::warn!(
                r,
                "virglrenderer failed to initialise; the GPU is unavailable"
            );
            return false;
        }
        let (mut ver, mut size) = (0u32, 0u32);
        unsafe { virgl::virgl_renderer_get_cap_set(wire::CAPSET_VENUS, &mut ver, &mut size) };
        let mut caps = vec![0u8; size as usize];
        if size > 0 {
            unsafe {
                virgl::virgl_renderer_fill_caps(wire::CAPSET_VENUS, ver, caps.as_mut_ptr().cast())
            };
        }
        tracing::info!(
            capset_version = ver,
            capset_bytes = size,
            "gpu renderer initialised"
        );
        self.caps = Some((ver, caps));
        self.renderer = true;
        true
    }

    /// Splits a chain into what the guest wrote and where it wants the reply.
    fn split(
        mem: &GuestMemory,
        chain: impl Iterator<Item = Descriptor>,
    ) -> (Vec<u8>, Vec<(u64, u32)>) {
        let mut request = Vec::new();
        let mut reply = Vec::new();
        for desc in chain {
            if desc.is_write_only() {
                reply.push((desc.addr, desc.len));
            } else {
                let at = request.len();
                request.resize(at + desc.len as usize, 0);
                if mem.read(desc.addr, &mut request[at..]).is_err() {
                    request.truncate(at);
                }
            }
        }
        (request, reply)
    }

    fn scatter(mem: &GuestMemory, bufs: &[(u64, u32)], data: &[u8]) -> u32 {
        let mut at = 0usize;
        for &(addr, len) in bufs {
            if at >= data.len() {
                break;
            }
            let n = (len as usize).min(data.len() - at);
            if mem.write(addr, &data[at..at + n]).is_err() {
                break;
            }
            at += n;
        }
        at as u32
    }

    /// Executes one command and returns the response bytes.
    fn execute(&mut self, mem: &GuestMemory, hdr: &wire::Header, req: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(wire::HDR_LEN + 16);
        let ok = |out: &mut Vec<u8>, kind: u32| hdr.response(kind, out);
        match hdr.kind {
            wire::CMD_GET_DISPLAY_INFO => {
                ok(&mut out, wire::RESP_OK_DISPLAY_INFO);
                out.resize(wire::DISPLAY_INFO_LEN, 0);
            }
            wire::CMD_GET_CAPSET_INFO => {
                let index = wire::u32_at(req, 24);
                if index != 0 || !self.ensure_renderer() {
                    ok(&mut out, wire::RESP_ERR_INVALID_PARAMETER);
                } else {
                    let (ver, caps) = self.caps.as_ref().expect("caps after init");
                    ok(&mut out, wire::RESP_OK_CAPSET_INFO);
                    out.extend_from_slice(&wire::CAPSET_VENUS.to_le_bytes());
                    out.extend_from_slice(&ver.to_le_bytes());
                    out.extend_from_slice(&(caps.len() as u32).to_le_bytes());
                    out.extend_from_slice(&0u32.to_le_bytes());
                }
            }
            wire::CMD_GET_CAPSET => {
                let id = wire::u32_at(req, 24);
                if id != wire::CAPSET_VENUS || !self.ensure_renderer() {
                    ok(&mut out, wire::RESP_ERR_INVALID_PARAMETER);
                } else {
                    let (_, caps) = self.caps.as_ref().expect("caps after init");
                    ok(&mut out, wire::RESP_OK_CAPSET);
                    out.extend_from_slice(caps);
                }
            }
            wire::CMD_CTX_CREATE => {
                if !self.ensure_renderer() {
                    ok(&mut out, wire::RESP_ERR_UNSPEC);
                } else {
                    let nlen = wire::u32_at(req, 24).min(64);
                    let capset = wire::u32_at(req, 28) & 0xff;
                    let name = &req[32..32 + nlen as usize];
                    let flags = if capset == 0 {
                        wire::CAPSET_VENUS
                    } else {
                        capset
                    };
                    let r = unsafe {
                        virgl::virgl_renderer_context_create_with_flags(
                            hdr.ctx_id,
                            flags,
                            nlen,
                            name.as_ptr().cast(),
                        )
                    };
                    if r == 0 {
                        self.contexts.insert(hdr.ctx_id, ());
                        ok(&mut out, wire::RESP_OK_NODATA);
                    } else {
                        tracing::warn!(r, ctx = hdr.ctx_id, "gpu context create failed");
                        ok(&mut out, wire::RESP_ERR_UNSPEC);
                    }
                }
            }
            wire::CMD_CTX_DESTROY => {
                if self.contexts.remove(&hdr.ctx_id).is_some() {
                    unsafe { virgl::virgl_renderer_context_destroy(hdr.ctx_id) };
                }
                ok(&mut out, wire::RESP_OK_NODATA);
            }
            wire::CMD_CTX_ATTACH_RESOURCE | wire::CMD_CTX_DETACH_RESOURCE => {
                let res = wire::u32_at(req, 24);
                if !self.contexts.contains_key(&hdr.ctx_id) {
                    ok(&mut out, wire::RESP_ERR_INVALID_CONTEXT_ID);
                } else if !self.resources.contains_key(&res) {
                    ok(&mut out, wire::RESP_ERR_INVALID_RESOURCE_ID);
                } else {
                    if hdr.kind == wire::CMD_CTX_ATTACH_RESOURCE {
                        unsafe {
                            virgl::virgl_renderer_ctx_attach_resource(hdr.ctx_id as i32, res as i32)
                        };
                    } else {
                        unsafe {
                            virgl::virgl_renderer_ctx_detach_resource(hdr.ctx_id as i32, res as i32)
                        };
                    }
                    ok(&mut out, wire::RESP_OK_NODATA);
                }
            }
            wire::CMD_RESOURCE_CREATE_BLOB => {
                let kind = self.create_blob(mem, hdr, req);
                ok(&mut out, kind);
            }
            wire::CMD_RESOURCE_UNREF => {
                let res = wire::u32_at(req, 24);
                self.unref(mem, res);
                ok(&mut out, wire::RESP_OK_NODATA);
            }
            wire::CMD_RESOURCE_MAP_BLOB => {
                let res = wire::u32_at(req, 24);
                let offset = wire::u64_at(req, 32);
                match self.map_blob(mem, res, offset) {
                    Ok(info) => {
                        ok(&mut out, wire::RESP_OK_MAP_INFO);
                        out.extend_from_slice(&info.to_le_bytes());
                        out.extend_from_slice(&0u32.to_le_bytes());
                    }
                    Err(kind) => ok(&mut out, kind),
                }
            }
            wire::CMD_RESOURCE_UNMAP_BLOB => {
                let res = wire::u32_at(req, 24);
                ok(&mut out, self.unmap_blob(mem, res));
            }
            wire::CMD_SUBMIT_3D => {
                let size = wire::u32_at(req, 24) as usize;
                if !self.contexts.contains_key(&hdr.ctx_id) {
                    ok(&mut out, wire::RESP_ERR_INVALID_CONTEXT_ID);
                } else if req.len() < 32 + size || !size.is_multiple_of(4) {
                    ok(&mut out, wire::RESP_ERR_INVALID_PARAMETER);
                } else {
                    let mut buf = req[32..32 + size].to_vec();
                    let r = unsafe {
                        virgl::virgl_renderer_submit_cmd(
                            buf.as_mut_ptr().cast(),
                            hdr.ctx_id as i32,
                            (size / 4) as i32,
                        )
                    };
                    ok(
                        &mut out,
                        if r == 0 {
                            wire::RESP_OK_NODATA
                        } else {
                            wire::RESP_ERR_UNSPEC
                        },
                    );
                }
            }
            other => {
                tracing::debug!(
                    cmd = format_args!("{other:#x}"),
                    "gpu command not supported"
                );
                ok(&mut out, wire::RESP_ERR_UNSPEC);
            }
        }
        out
    }

    fn create_blob(&mut self, mem: &GuestMemory, hdr: &wire::Header, req: &[u8]) -> u32 {
        if req.len() < 56 || !self.ensure_renderer() {
            return wire::RESP_ERR_INVALID_PARAMETER;
        }
        let res_handle = wire::u32_at(req, 24);
        let blob_mem = wire::u32_at(req, 28);
        let blob_flags = wire::u32_at(req, 32);
        let nr_entries = wire::u32_at(req, 36) as usize;
        let blob_id = wire::u64_at(req, 40);
        let size = wire::u64_at(req, 48);
        if self.resources.contains_key(&res_handle) {
            return wire::RESP_ERR_INVALID_RESOURCE_ID;
        }
        if req.len() < 56 + nr_entries * 16 {
            return wire::RESP_ERR_INVALID_PARAMETER;
        }
        let mut iovs = Vec::with_capacity(nr_entries);
        if matches!(blob_mem, wire::BLOB_MEM_GUEST | wire::BLOB_MEM_HOST3D_GUEST) {
            for i in 0..nr_entries {
                let at = 56 + i * 16;
                let addr = wire::u64_at(req, at);
                let len = wire::u32_at(req, at + 8) as usize;
                let Ok(base) = mem.host_span(addr, len) else {
                    return wire::RESP_ERR_INVALID_PARAMETER;
                };
                iovs.push(virgl::Iovec {
                    base: base.cast(),
                    len,
                });
            }
        }
        let args = virgl::BlobArgs {
            res_handle,
            ctx_id: hdr.ctx_id,
            blob_mem,
            blob_flags,
            blob_id,
            size,
            iovecs: if iovs.is_empty() {
                std::ptr::null()
            } else {
                iovs.as_ptr()
            },
            num_iovs: iovs.len() as u32,
        };
        let r = unsafe { virgl::virgl_renderer_resource_create_blob(&args) };
        if r != 0 {
            tracing::warn!(r, res_handle, blob_mem, size, "gpu blob create failed");
            return wire::RESP_ERR_OUT_OF_MEMORY;
        }
        self.resources.insert(
            res_handle,
            Resource {
                _iovs: iovs,
                mapped: None,
            },
        );
        wire::RESP_OK_NODATA
    }

    fn map_blob(&mut self, mem: &GuestMemory, res: u32, offset: u64) -> Result<u32, u32> {
        let Some(resource) = self.resources.get_mut(&res) else {
            return Err(wire::RESP_ERR_INVALID_RESOURCE_ID);
        };
        if resource.mapped.is_some() {
            return Err(wire::RESP_ERR_INVALID_PARAMETER);
        }
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut size = 0u64;
        let r = unsafe { virgl::virgl_renderer_resource_map(res, &mut ptr, &mut size) };
        if r != 0 || ptr.is_null() {
            tracing::warn!(r, res, "gpu blob map failed in the renderer");
            return Err(wire::RESP_ERR_UNSPEC);
        }
        // The guest placed the blob on a 16 KiB boundary (patch 0031); the
        // host's size is whatever Metal gave, rounded up to the same page.
        let len = (size as usize).div_ceil(HOST_PAGE) * HOST_PAGE;
        let gpa = self.aperture.base + offset;
        let fits =
            offset.is_multiple_of(HOST_PAGE as u64)
            && offset
                .checked_add(len as u64)
                .is_some_and(|end| end <= self.aperture.size);
        if !fits {
            unsafe { virgl::virgl_renderer_resource_unmap(res) };
            tracing::warn!(res, offset, len, "gpu blob does not fit the aperture");
            return Err(wire::RESP_ERR_INVALID_PARAMETER);
        }
        // SAFETY: the renderer keeps the mapping until `resource_unmap`,
        // which happens only after this device unmaps the guest side.
        if let Err(e) = unsafe { mem.map_foreign(gpa, ptr.cast(), len) } {
            unsafe { virgl::virgl_renderer_resource_unmap(res) };
            tracing::warn!(%e, res, "gpu blob could not be mapped into the guest");
            return Err(wire::RESP_ERR_UNSPEC);
        }
        resource.mapped = Some((gpa, len));
        let mut info = 0u32;
        unsafe { virgl::virgl_renderer_resource_get_map_info(res, &mut info) };
        Ok(info)
    }

    fn unmap_blob(&mut self, mem: &GuestMemory, res: u32) -> u32 {
        let Some(resource) = self.resources.get_mut(&res) else {
            return wire::RESP_ERR_INVALID_RESOURCE_ID;
        };
        if let Some((gpa, len)) = resource.mapped.take() {
            // SAFETY: the guest asked for the unmap; its own mapping is gone.
            if let Err(e) = unsafe { mem.unmap_foreign(gpa, len) } {
                tracing::warn!(%e, res, "gpu blob unmap failed");
            }
            unsafe { virgl::virgl_renderer_resource_unmap(res) };
        }
        wire::RESP_OK_NODATA
    }

    fn unref(&mut self, mem: &GuestMemory, res: u32) {
        self.unmap_blob(mem, res);
        if self.resources.remove(&res).is_some() {
            unsafe { virgl::virgl_renderer_resource_unref(res) };
        }
    }

    /// Returns the descriptors of every command whose fence has signalled.
    fn complete_fenced(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let signalled = self.fences.drain();
        if signalled.is_empty() {
            return false;
        }
        let mut any = false;
        for f in signalled {
            let mut i = 0;
            while i < self.pending.len() {
                let p = &self.pending[i];
                if p.ctx_id == f.ctx_id && p.ring_idx == f.ring_idx && p.fence_id <= f.fence_id {
                    let p = self.pending.remove(i);
                    tracing::trace!(
                        ctx = p.ctx_id,
                        ring = p.ring_idx,
                        fence = p.fence_id,
                        "gpu fenced command completed"
                    );
                    queue.push_used(mem, p.head, p.len);
                    any = true;
                } else {
                    i += 1;
                }
            }
        }
        any
    }

    fn serve_control(&mut self, queue: &mut Virtqueue, mem: &GuestMemory) -> bool {
        let mut used = self.complete_fenced(queue, mem);
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head();
            let (req, reply) = Gpu::split(mem, chain);
            let Some(hdr) = wire::Header::parse(&req) else {
                queue.push_used(mem, head, 0);
                used = true;
                continue;
            };
            let resp = self.execute(mem, &hdr, &req);
            let len = Gpu::scatter(mem, &reply, &resp);
            let fenced = hdr.flags & wire::FLAG_FENCE != 0 && self.renderer;
            if fenced {
                let ring_idx = if hdr.flags & wire::FLAG_INFO_RING_IDX != 0 {
                    u32::from(hdr.ring_idx)
                } else {
                    0
                };
                let r = if hdr.ctx_id != 0 && self.contexts.contains_key(&hdr.ctx_id) {
                    unsafe {
                        virgl::virgl_renderer_context_create_fence(
                            hdr.ctx_id,
                            0,
                            ring_idx,
                            hdr.fence_id,
                        )
                    }
                } else {
                    unsafe { virgl::virgl_renderer_create_fence(hdr.fence_id as i32, 0) }
                };
                tracing::trace!(
                    r,
                    cmd = format_args!("{:#x}", hdr.kind),
                    ctx = hdr.ctx_id,
                    ring_idx,
                    fence = hdr.fence_id,
                    "gpu fence created"
                );
                if r == 0 {
                    self.pending.push(Pending {
                        head,
                        len,
                        ctx_id: if hdr.ctx_id != 0 && self.contexts.contains_key(&hdr.ctx_id) {
                            hdr.ctx_id
                        } else {
                            0
                        },
                        ring_idx,
                        fence_id: hdr.fence_id,
                    });
                    continue;
                }
                tracing::warn!(r, "gpu fence could not be created; completing at once");
            }
            queue.push_used(mem, head, len);
            used = true;
        }
        // A fence may have landed while the commands above were running.
        used | self.complete_fenced(queue, mem)
    }
}

impl VirtioDevice for Gpu {
    fn device_type(&self) -> u32 {
        device_type::GPU
    }

    fn name(&self) -> &'static str {
        "virtio-gpu"
    }

    fn features(&self) -> u64 {
        COMMON_FEATURES | wire::F_VIRGL | wire::F_RESOURCE_BLOB | wire::F_CONTEXT_INIT
    }

    fn queue_count(&self) -> usize {
        2
    }

    fn shm_regions(&self) -> Vec<ShmRegion> {
        vec![ShmRegion {
            id: wire::SHM_ID_HOST_VISIBLE,
            base: self.aperture.base,
            len: self.aperture.size,
        }]
    }

    fn config_read(&self, offset: u64, data: &mut [u8]) {
        // events_read, events_clear, num_scanouts, num_capsets.
        let config: [u32; 4] = [0, 0, 0, 1];
        let bytes: Vec<u8> = config.iter().flat_map(|v| v.to_le_bytes()).collect();
        for (i, b) in data.iter_mut().enumerate() {
            *b = bytes.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn activate(&mut self, mem: Arc<GuestMemory>) {
        self.memory = Some(mem);
    }

    fn notify(&mut self, queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        match queue {
            CONTROL_QUEUE => {
                let Some(q) = queues.get_mut(CONTROL_QUEUE as usize) else {
                    return Serviced::NONE;
                };
                Serviced::queue_if(CONTROL_QUEUE, self.serve_control(q, mem))
            }
            CURSOR_QUEUE => {
                let Some(q) = queues.get_mut(CURSOR_QUEUE as usize) else {
                    return Serviced::NONE;
                };
                let mut used = false;
                while let Some(head) = q.pop(mem).map(|chain| chain.head()) {
                    q.push_used(mem, head, 0);
                    used = true;
                }
                Serviced::queue_if(CURSOR_QUEUE, used)
            }
            _ => Serviced::NONE,
        }
    }

    fn reset(&mut self) {
        if let Some(mem) = self.memory.clone() {
            let ids: Vec<u32> = self.resources.keys().copied().collect();
            for res in ids {
                self.unref(&mem, res);
            }
        }
        for ctx in self.contexts.drain().map(|(id, _)| id).collect::<Vec<_>>() {
            unsafe { virgl::virgl_renderer_context_destroy(ctx) };
        }
        self.pending.clear();
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        if self.renderer {
            unsafe { virgl::virgl_renderer_cleanup(Arc::as_ptr(&self.fences) as *mut c_void) };
        }
    }
}

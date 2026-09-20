//! lighter's ONNX Runtime plugin execution provider: the Neural Engine, from
//! inside a container.
//!
//! Loaded by ONNX Runtime (`register_execution_provider_library`), it
//! advertises an NPU, claims every node the graph has that carries no
//! subgraph as one fused unit, serialises that unit back to ONNX, and sends
//! it to the host when the first run's shapes are known — batch dimensions
//! are what the Neural Engine cannot leave open. Every run forwards its inputs
//! and returns the host's outputs (`protocol` in `crates/lighter-vmm/src/ane`).
//!
//! It links no libc: one file loads in any image, Alpine or Debian, because
//! the only things it needs from the operating system are a socket, some
//! memory and the environment, which are syscalls (`sys`). Memory is a
//! linked-list heap over an anonymous mapping.
#![no_std]
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, clippy::missing_safety_doc)]

extern crate alloc;

mod pb;
mod sys;
mod ort_sys;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void, CStr};
use core::fmt::Write as _;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use ort_sys::*;
use pb::Pb;

// ---------------- runtime scaffolding ----------------

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys::exit_group(134)
}

struct Heap {
    inner: linked_list_allocator::LockedHeap,
    ready: AtomicBool,
}

/// 8 GiB of address space, committed by the kernel as it is touched.
const HEAP_LEN: usize = 8 << 30;

unsafe impl core::alloc::GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        if !self.ready.swap(true, Ordering::AcqRel) {
            let base = sys::mmap_anonymous(HEAP_LEN);
            if base.is_null() {
                sys::exit_group(133);
            }
            unsafe { self.inner.lock().init(base, HEAP_LEN) };
        }
        unsafe { self.inner.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        unsafe { self.inner.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static HEAP: Heap = Heap {
    inner: linked_list_allocator::LockedHeap::empty(),
    ready: AtomicBool::new(false),
};

static API: AtomicPtr<OrtApi> = AtomicPtr::new(ptr::null_mut());
static EP_API: AtomicPtr<OrtEpApi> = AtomicPtr::new(ptr::null_mut());
fn api() -> &'static OrtApi {
    unsafe { &*API.load(Ordering::Acquire) }
}
fn ep_api() -> &'static OrtEpApi {
    unsafe { &*EP_API.load(Ordering::Acquire) }
}

fn cstring(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

fn fail(msg: &str) -> OrtStatusPtr {
    let c = cstring(msg);
    unsafe { (api().CreateStatus.unwrap())(OrtErrorCode_ORT_EP_FAIL, c.as_ptr() as *const c_char) }
}

fn cstr(p: *const c_char) -> &'static [u8] {
    if p.is_null() {
        return b"";
    }
    unsafe { CStr::from_ptr(p) }.to_bytes()
}

macro_rules! ort_call {
    ($f:ident, $($a:expr),*) => {{
        let st = unsafe { (api().$f.unwrap())($($a),*) };
        if !st.is_null() { return st; }
    }};
}

/// A spin lock: the library has no threads of its own, but ONNX Runtime may
/// run one session from several.
struct Lock(AtomicBool);
impl Lock {
    const fn new() -> Lock {
        Lock(AtomicBool::new(false))
    }
    fn acquire(&self) {
        while self.0.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
    }
    fn release(&self) {
        self.0.store(false, Ordering::Release);
    }
}

// ---------------- the wire to the host ----------------

const KIND_LOAD: u32 = 1;
const KIND_RUN: u32 = 2;
const KIND_CLOSE: u32 = 3;
const KIND_ERR: u32 = 0xffff;

struct Link {
    fd: i64,
}

impl Link {
    /// Dials the host named by `LIGHTER_ANE` (`a.b.c.d:port`).
    fn connect() -> Result<Link, String> {
        let mut buf = [0u8; 64];
        let n = sys::env(b"LIGHTER_ANE", &mut buf).ok_or_else(|| String::from("LIGHTER_ANE is not set; run the container with --device lighter.sh/ane=all"))?;
        let text = core::str::from_utf8(&buf[..n]).map_err(|_| String::from("LIGHTER_ANE is not text"))?;
        let (host, port) = text.rsplit_once(':').ok_or_else(|| String::from("LIGHTER_ANE is not host:port"))?;
        let port: u16 = port.trim().parse().map_err(|_| String::from("LIGHTER_ANE port is not a number"))?;
        let mut addr = [0u8; 4];
        for (i, part) in host.split('.').enumerate() {
            if i >= 4 {
                return Err(String::from("LIGHTER_ANE host is not an IPv4 address"));
            }
            addr[i] = part.parse().map_err(|_| String::from("LIGHTER_ANE host is not an IPv4 address"))?;
        }
        let fd = sys::socket(sys::AF_INET, sys::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(String::from("socket() failed"));
        }
        // IPPROTO_TCP = 6, TCP_NODELAY = 1: every request is a round trip.
        sys::setsockopt_int(fd, 6, 1, 1);
        let sa = sys::SockAddrIn {
            family: sys::AF_INET as u16,
            port: port.to_be(),
            addr,
            zero: [0; 8],
        };
        if sys::connect(fd, &sa) < 0 {
            sys::close(fd);
            let mut m = String::new();
            let _ = write!(m, "cannot connect to the lighter neural engine service at {text}");
            return Err(m);
        }
        Ok(Link { fd })
    }

    fn call(&mut self, kind: u32, payload: &[u8]) -> Result<Vec<u8>, String> {
        let mut hdr = [0u8; 12];
        hdr[..4].copy_from_slice(&kind.to_le_bytes());
        hdr[4..].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        if !sys::write_all(self.fd, &hdr) || !sys::write_all(self.fd, payload) {
            return Err(String::from("the neural engine service went away (write)"));
        }
        let mut h = [0u8; 12];
        if !sys::read_exact(self.fd, &mut h) {
            return Err(String::from("the neural engine service went away (read)"));
        }
        let k = u32::from_le_bytes(h[..4].try_into().unwrap());
        let n = u64::from_le_bytes(h[4..].try_into().unwrap()) as usize;
        let mut body = vec![0u8; n];
        if !sys::read_exact(self.fd, &mut body) {
            return Err(String::from("the neural engine service went away (body)"));
        }
        if k == KIND_ERR {
            return Err(String::from_utf8_lossy(&body).into_owned());
        }
        Ok(body)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        sys::close(self.fd);
    }
}

// ---------------- graph serialisation ----------------

unsafe fn value_infos(
    count: unsafe extern "C" fn(*const OrtGraph, *mut usize) -> OrtStatusPtr,
    get: unsafe extern "C" fn(*const OrtGraph, *mut *const OrtValueInfo, usize) -> OrtStatusPtr,
    g: *const OrtGraph,
) -> Result<Vec<*const OrtValueInfo>, OrtStatusPtr> {
    let mut n = 0usize;
    let st = unsafe { count(g, &mut n) };
    if !st.is_null() {
        return Err(st);
    }
    let mut v = vec![ptr::null(); n];
    if n > 0 {
        let st = unsafe { get(g, v.as_mut_ptr(), n) };
        if !st.is_null() {
            return Err(st);
        }
    }
    Ok(v)
}

/// An input of the fused graph: name and element type; its dims come from
/// the first run.
struct Input {
    name: Vec<u8>,
    element_type: u32,
    rank: usize,
}

unsafe fn input_of(vi: *const OrtValueInfo) -> Result<Input, OrtStatusPtr> {
    let a = api();
    let mut name: *const c_char = ptr::null();
    let st = unsafe { (a.GetValueInfoName.unwrap())(vi, &mut name) };
    if !st.is_null() {
        return Err(st);
    }
    let mut ti: *const OrtTypeInfo = ptr::null();
    let st = unsafe { (a.GetValueInfoTypeInfo.unwrap())(vi, &mut ti) };
    if !st.is_null() {
        return Err(st);
    }
    let mut tsi: *const OrtTensorTypeAndShapeInfo = ptr::null();
    let st = unsafe { (a.CastTypeInfoToTensorInfo.unwrap())(ti, &mut tsi) };
    if !st.is_null() {
        return Err(st);
    }
    let (mut et, mut rank) = (0u32, 0usize);
    if !tsi.is_null() {
        unsafe {
            (a.GetTensorElementType.unwrap())(tsi, &mut et);
            (a.GetDimensionsCount.unwrap())(tsi, &mut rank);
        }
    }
    Ok(Input {
        name: cstr(name).to_vec(),
        element_type: et,
        rank,
    })
}

unsafe fn value_info_proto(vi: *const OrtValueInfo) -> Result<Pb, OrtStatusPtr> {
    let a = api();
    let mut name: *const c_char = ptr::null();
    let st = unsafe { (a.GetValueInfoName.unwrap())(vi, &mut name) };
    if !st.is_null() {
        return Err(st);
    }
    let mut ti: *const OrtTypeInfo = ptr::null();
    let st = unsafe { (a.GetValueInfoTypeInfo.unwrap())(vi, &mut ti) };
    if !st.is_null() {
        return Err(st);
    }
    let mut tsi: *const OrtTensorTypeAndShapeInfo = ptr::null();
    let st = unsafe { (a.CastTypeInfoToTensorInfo.unwrap())(ti, &mut tsi) };
    if !st.is_null() {
        return Err(st);
    }
    let mut p = Pb::new();
    p.bytes(1, cstr(name));
    if !tsi.is_null() {
        let mut et = 0u32;
        let mut nd = 0usize;
        unsafe {
            (a.GetTensorElementType.unwrap())(tsi, &mut et);
            (a.GetDimensionsCount.unwrap())(tsi, &mut nd);
        }
        let mut dims = vec![0i64; nd];
        let mut syms: Vec<*const c_char> = vec![ptr::null(); nd];
        if nd > 0 {
            unsafe {
                (a.GetDimensions.unwrap())(tsi, dims.as_mut_ptr(), nd);
                (a.GetSymbolicDimensions.unwrap())(tsi, syms.as_mut_ptr(), nd);
            }
        }
        let mut shape = Pb::new();
        for i in 0..nd {
            let mut d = Pb::new();
            if dims[i] >= 0 {
                d.int(1, dims[i]);
            } else if !syms[i].is_null() {
                d.bytes(2, cstr(syms[i]));
            }
            shape.msg(1, d);
        }
        let mut tt = Pb::new();
        tt.int(1, et as i64);
        tt.msg(2, shape);
        let mut ty = Pb::new();
        ty.msg(1, tt);
        p.msg(2, ty);
    }
    Ok(p)
}

unsafe fn tensor_proto(name: &[u8], v: *const OrtValue) -> Result<Pb, OrtStatusPtr> {
    let a = api();
    let mut tsi: *mut OrtTensorTypeAndShapeInfo = ptr::null_mut();
    let st = unsafe { (a.GetTensorTypeAndShape.unwrap())(v, &mut tsi) };
    if !st.is_null() {
        return Err(st);
    }
    let (mut et, mut nd) = (0u32, 0usize);
    unsafe {
        (a.GetTensorElementType.unwrap())(tsi, &mut et);
        (a.GetDimensionsCount.unwrap())(tsi, &mut nd);
    }
    let mut dims = vec![0i64; nd];
    if nd > 0 {
        unsafe { (a.GetDimensions.unwrap())(tsi, dims.as_mut_ptr(), nd) };
    }
    unsafe { (a.ReleaseTensorTypeAndShapeInfo.unwrap())(tsi) };
    let mut size = 0usize;
    let st = unsafe { (a.GetTensorSizeInBytes.unwrap())(v, &mut size) };
    if !st.is_null() {
        return Err(st);
    }
    let mut data: *const c_void = ptr::null();
    let st = unsafe { (a.GetTensorData.unwrap())(v, &mut data) };
    if !st.is_null() {
        return Err(st);
    }
    let mut t = Pb::new();
    t.packed_i64(1, &dims);
    t.int(2, et as i64);
    t.bytes(8, name);
    t.bytes(9, unsafe { core::slice::from_raw_parts(data as *const u8, size) });
    Ok(t)
}

unsafe fn attr_proto(attr: *const OrtOpAttr) -> Result<Pb, OrtStatusPtr> {
    let a = api();
    let mut name: *const c_char = ptr::null();
    let st = unsafe { (a.OpAttr_GetName.unwrap())(attr, &mut name) };
    if !st.is_null() {
        return Err(st);
    }
    let mut ty: OrtOpAttrType = 0;
    let st = unsafe { (a.OpAttr_GetType.unwrap())(attr, &mut ty) };
    if !st.is_null() {
        return Err(st);
    }
    let mut p = Pb::new();
    p.bytes(1, cstr(name));
    let read = |ty: OrtOpAttrType| -> Result<Vec<u8>, OrtStatusPtr> {
        let mut need = 0usize;
        let st = unsafe { (a.ReadOpAttr.unwrap())(attr, ty, ptr::null_mut(), 0, &mut need) };
        if !st.is_null() {
            unsafe { (a.ReleaseStatus.unwrap())(st) };
        }
        let mut buf = vec![0u8; need.max(1)];
        let mut out = 0usize;
        let st = unsafe { (a.ReadOpAttr.unwrap())(attr, ty, buf.as_mut_ptr() as *mut c_void, buf.len(), &mut out) };
        if !st.is_null() {
            return Err(st);
        }
        buf.truncate(out);
        Ok(buf)
    };
    match ty {
        OrtOpAttrType_ORT_OP_ATTR_INT => {
            let b = read(ty)?;
            p.int(3, i64::from_le_bytes(b[..8].try_into().unwrap()));
            p.int(20, 2)
        }
        OrtOpAttrType_ORT_OP_ATTR_INTS => {
            let b = read(ty)?;
            let v: Vec<i64> = b.chunks_exact(8).map(|c| i64::from_le_bytes(c.try_into().unwrap())).collect();
            p.packed_i64(8, &v);
            p.int(20, 7)
        }
        OrtOpAttrType_ORT_OP_ATTR_FLOAT => {
            let b = read(ty)?;
            p.f32(2, f32::from_le_bytes(b[..4].try_into().unwrap()));
            p.int(20, 1)
        }
        OrtOpAttrType_ORT_OP_ATTR_FLOATS => {
            let b = read(ty)?;
            let v: Vec<f32> = b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
            p.packed_f32(7, &v);
            p.int(20, 6)
        }
        OrtOpAttrType_ORT_OP_ATTR_STRING => {
            let mut b = read(ty)?;
            if b.last() == Some(&0) {
                b.pop();
            }
            p.bytes(4, &b);
            p.int(20, 3)
        }
        OrtOpAttrType_ORT_OP_ATTR_STRINGS => {
            let b = read(ty)?;
            for s in b.split(|c| *c == 0).filter(|s| !s.is_empty()) {
                p.bytes(9, s)
            }
            p.int(20, 8)
        }
        OrtOpAttrType_ORT_OP_ATTR_TENSOR => {
            let mut v: *mut OrtValue = ptr::null_mut();
            let st = unsafe { (a.OpAttr_GetTensorAttributeAsOrtValue.unwrap())(attr, &mut v) };
            if !st.is_null() {
                return Err(st);
            }
            let t = unsafe { tensor_proto(b"", v) }?;
            unsafe { (a.ReleaseValue.unwrap())(v) };
            p.msg(5, t);
            p.int(20, 4)
        }
        _ => return Err(fail("unsupported attribute type")),
    }
    Ok(p)
}

/// The fused graph as a ModelProto without its inputs (field 11), which are
/// added at the first run with that run's shapes, plus what those inputs are.
struct Serialized {
    model_without_inputs: Vec<u8>,
    inputs: Vec<Input>,
}

unsafe fn serialize_graph(g: *const OrtGraph) -> Result<Serialized, OrtStatusPtr> {
    let a = api();
    let mut model = Pb::new();
    let mut ir = 0i64;
    let st = unsafe { (a.Graph_GetOnnxIRVersion.unwrap())(g, &mut ir) };
    if !st.is_null() {
        return Err(st);
    }
    model.int(1, ir);
    model.bytes(2, b"lighter");
    let mut nos = 0usize;
    let st = unsafe { (a.Graph_GetNumOperatorSets.unwrap())(g, &mut nos) };
    if !st.is_null() {
        return Err(st);
    }
    let mut doms: Vec<*const c_char> = vec![ptr::null(); nos];
    let mut vers = vec![0i64; nos];
    if nos > 0 {
        let st = unsafe { (a.Graph_GetOperatorSets.unwrap())(g, doms.as_mut_ptr(), vers.as_mut_ptr(), nos) };
        if !st.is_null() {
            return Err(st);
        }
    }
    for i in 0..nos {
        let mut o = Pb::new();
        o.bytes(1, cstr(doms[i]));
        o.int(2, vers[i]);
        model.msg(8, o);
    }
    let mut graph = Pb::new();
    let mut gname: *const c_char = ptr::null();
    unsafe { (a.Graph_GetName.unwrap())(g, &mut gname) };
    graph.bytes(2, cstr(gname));
    let mut n = 0usize;
    let st = unsafe { (a.Graph_GetNumNodes.unwrap())(g, &mut n) };
    if !st.is_null() {
        return Err(st);
    }
    let mut nodes: Vec<*const OrtNode> = vec![ptr::null(); n];
    if n > 0 {
        let st = unsafe { (a.Graph_GetNodes.unwrap())(g, nodes.as_mut_ptr(), n) };
        if !st.is_null() {
            return Err(st);
        }
    }
    for &node in &nodes {
        let mut np = Pb::new();
        for (field, count, get) in [
            (1u32, a.Node_GetNumInputs.unwrap(), a.Node_GetInputs.unwrap()),
            (2u32, a.Node_GetNumOutputs.unwrap(), a.Node_GetOutputs.unwrap()),
        ] {
            let mut k = 0usize;
            unsafe { count(node, &mut k) };
            let mut vis: Vec<*const OrtValueInfo> = vec![ptr::null(); k];
            if k > 0 {
                let st = unsafe { get(node, vis.as_mut_ptr(), k) };
                if !st.is_null() {
                    return Err(st);
                }
            }
            for vi in vis {
                if vi.is_null() {
                    np.bytes(field, b"");
                } else {
                    let mut nm: *const c_char = ptr::null();
                    unsafe { (a.GetValueInfoName.unwrap())(vi, &mut nm) };
                    np.bytes(field, cstr(nm));
                }
            }
        }
        let mut s: *const c_char = ptr::null();
        unsafe { (a.Node_GetName.unwrap())(node, &mut s) };
        np.bytes(3, cstr(s));
        unsafe { (a.Node_GetOperatorType.unwrap())(node, &mut s) };
        np.bytes(4, cstr(s));
        unsafe { (a.Node_GetDomain.unwrap())(node, &mut s) };
        let d = cstr(s);
        if d != b"ai.onnx" {
            np.bytes(7, d);
        }
        let mut na = 0usize;
        unsafe { (a.Node_GetNumAttributes.unwrap())(node, &mut na) };
        let mut attrs: Vec<*const OrtOpAttr> = vec![ptr::null(); na];
        if na > 0 {
            let st = unsafe { (a.Node_GetAttributes.unwrap())(node, attrs.as_mut_ptr(), na) };
            if !st.is_null() {
                return Err(st);
            }
        }
        for at in attrs {
            np.msg(5, unsafe { attr_proto(at) }?);
        }
        graph.msg(1, np);
    }
    for vi in unsafe { value_infos(a.Graph_GetNumInitializers.unwrap(), a.Graph_GetInitializers.unwrap(), g) }? {
        let mut nm: *const c_char = ptr::null();
        unsafe { (a.GetValueInfoName.unwrap())(vi, &mut nm) };
        let mut v: *const OrtValue = ptr::null();
        let st = unsafe { (a.ValueInfo_GetInitializerValue.unwrap())(vi, &mut v) };
        if !st.is_null() {
            return Err(st);
        }
        graph.msg(5, unsafe { tensor_proto(cstr(nm), v) }?);
    }
    let mut inputs = Vec::new();
    for vi in unsafe { value_infos(a.Graph_GetNumInputs.unwrap(), a.Graph_GetInputs.unwrap(), g) }? {
        inputs.push(unsafe { input_of(vi) }?);
    }
    for vi in unsafe { value_infos(a.Graph_GetNumOutputs.unwrap(), a.Graph_GetOutputs.unwrap(), g) }? {
        graph.msg(12, unsafe { value_info_proto(vi) }?);
    }
    // The graph is field 7 of the model, and its inputs (field 11 of the
    // graph) are still to come: the model is finished at the first run, when
    // the graph bytes are re-wrapped with the inputs appended.
    Ok(Serialized {
        model_without_inputs: {
            let mut m = model;
            m.msg(7, graph);
            m.0
        },
        inputs,
    })
}

/// The finished model: the serialised graph with input value infos carrying
/// the run's concrete dims. Protobuf lets a message's fields be split across
/// several length-delimited occurrences, and a merge concatenates them, so
/// a second field-7 entry holding only the inputs is a valid graph
/// continuation.
fn finish_model(s: &Serialized, dims: &[Vec<i64>]) -> Vec<u8> {
    let mut graph_tail = Pb::new();
    for (input, d) in s.inputs.iter().zip(dims) {
        let mut vi = Pb::new();
        vi.bytes(1, &input.name);
        let mut shape = Pb::new();
        for v in d {
            let mut dim = Pb::new();
            dim.int(1, *v);
            shape.msg(1, dim);
        }
        let mut tt = Pb::new();
        tt.int(1, input.element_type as i64);
        tt.msg(2, shape);
        let mut ty = Pb::new();
        ty.msg(1, tt);
        vi.msg(2, ty);
        graph_tail.msg(11, vi);
    }
    let mut out = Pb(s.model_without_inputs.clone());
    out.msg(7, graph_tail);
    out.0
}

// ---------------- the EP objects ----------------

#[repr(C)]
struct Factory {
    base: OrtEpFactory,
    name: Vec<u8>,
    vendor: Vec<u8>,
    version: Vec<u8>,
    hw: *mut OrtHardwareDevice,
}

#[repr(C)]
struct Ep {
    base: OrtEp,
    name: Vec<u8>,
}

struct Session {
    serialized: Serialized,
    link: Option<Link>,
    id: u64,
    /// The dims the model was bound with; a run with other dims reloads.
    bound: Vec<Vec<i64>>,
}

#[repr(C)]
struct ComputeInfo {
    base: OrtNodeComputeInfo,
    lock: Lock,
    session: core::cell::UnsafeCell<Session>,
}

unsafe extern "C" fn f_get_name(this: *const OrtEpFactory) -> *const c_char {
    unsafe { (*(this as *const Factory)).name.as_ptr() as *const c_char }
}
unsafe extern "C" fn f_get_vendor(this: *const OrtEpFactory) -> *const c_char {
    unsafe { (*(this as *const Factory)).vendor.as_ptr() as *const c_char }
}
unsafe extern "C" fn f_get_vendor_id(_: *const OrtEpFactory) -> u32 {
    0x1167
}
unsafe extern "C" fn f_get_version(this: *const OrtEpFactory) -> *const c_char {
    unsafe { (*(this as *const Factory)).version.as_ptr() as *const c_char }
}

unsafe extern "C" fn f_get_supported_devices(
    this: *mut OrtEpFactory,
    _devices: *const *const OrtHardwareDevice,
    _n: usize,
    ep_devices: *mut *mut OrtEpDevice,
    max: usize,
    num: *mut usize,
) -> OrtStatusPtr {
    // What ORT found in the container is a CPU. The device advertised is the
    // Neural Engine on the other side of the stream.
    let f = unsafe { &mut *(this as *mut Factory) };
    unsafe { *num = 0 };
    if max == 0 {
        return ptr::null_mut();
    }
    let vendor = cstring("lighter");
    let mut hw: *mut OrtHardwareDevice = ptr::null_mut();
    let st = unsafe {
        (ep_api().CreateHardwareDevice.unwrap())(
            OrtHardwareDeviceType_OrtHardwareDeviceType_NPU,
            0x1167,
            0,
            vendor.as_ptr() as *const c_char,
            ptr::null(),
            &mut hw,
        )
    };
    if !st.is_null() {
        return st;
    }
    f.hw = hw;
    let mut dev: *mut OrtEpDevice = ptr::null_mut();
    let st = unsafe { (ep_api().CreateEpDevice.unwrap())(this, hw, ptr::null(), ptr::null(), &mut dev) };
    if !st.is_null() {
        return st;
    }
    unsafe {
        *ep_devices = dev;
        *num = 1;
    }
    ptr::null_mut()
}

unsafe extern "C" fn f_create_ep(
    _this: *mut OrtEpFactory,
    _d: *const *const OrtHardwareDevice,
    _m: *const *const OrtKeyValuePairs,
    _n: usize,
    _so: *const OrtSessionOptions,
    _log: *const OrtLogger,
    ep: *mut *mut OrtEp,
) -> OrtStatusPtr {
    let mut e = alloc::boxed::Box::new(Ep {
        base: unsafe { core::mem::zeroed() },
        name: cstring("LighterANE"),
    });
    e.base.ort_version_supported = ORT_API_VERSION;
    e.base.GetName = Some(ep_get_name);
    e.base.GetCapability = Some(ep_get_capability);
    e.base.Compile = Some(ep_compile);
    e.base.ReleaseNodeComputeInfos = Some(ep_release_compute_infos);
    unsafe { *ep = alloc::boxed::Box::into_raw(e) as *mut OrtEp };
    ptr::null_mut()
}
unsafe extern "C" fn f_release_ep(_this: *mut OrtEpFactory, ep: *mut OrtEp) {
    drop(unsafe { alloc::boxed::Box::from_raw(ep as *mut Ep) })
}
unsafe extern "C" fn f_create_allocator(_: *mut OrtEpFactory, _: *const OrtMemoryInfo, _: *const OrtKeyValuePairs, out: *mut *mut OrtAllocator) -> OrtStatusPtr {
    unsafe { *out = ptr::null_mut() };
    ptr::null_mut()
}
unsafe extern "C" fn f_release_allocator(_: *mut OrtEpFactory, _: *mut OrtAllocator) {}
unsafe extern "C" fn f_create_data_transfer(_: *mut OrtEpFactory, out: *mut *mut OrtDataTransferImpl) -> OrtStatusPtr {
    unsafe { *out = ptr::null_mut() };
    ptr::null_mut()
}
unsafe extern "C" fn f_is_stream_aware(_: *const OrtEpFactory) -> bool {
    false
}
unsafe extern "C" fn f_create_sync_stream(_: *mut OrtEpFactory, _: *const OrtMemoryDevice, _: *const OrtKeyValuePairs, out: *mut *mut OrtSyncStreamImpl) -> OrtStatusPtr {
    unsafe { *out = ptr::null_mut() };
    ptr::null_mut()
}
unsafe extern "C" fn f_validate_compat(_: *mut OrtEpFactory, _: *const *const OrtHardwareDevice, _: usize, _: *const c_char, out: *mut OrtCompiledModelCompatibility) -> OrtStatusPtr {
    unsafe { *out = OrtCompiledModelCompatibility_OrtCompiledModelCompatibility_EP_NOT_APPLICABLE };
    ptr::null_mut()
}

unsafe extern "C" fn ep_get_name(this: *const OrtEp) -> *const c_char {
    unsafe { (*(this as *const Ep)).name.as_ptr() as *const c_char }
}

unsafe extern "C" fn ep_get_capability(_this: *mut OrtEp, graph: *const OrtGraph, info: *mut OrtEpGraphSupportInfo) -> OrtStatusPtr {
    let a = api();
    let mut n = 0usize;
    ort_call!(Graph_GetNumNodes, graph, &mut n);
    let mut nodes: Vec<*const OrtNode> = vec![ptr::null(); n];
    if n > 0 {
        ort_call!(Graph_GetNodes, graph, nodes.as_mut_ptr(), n);
    }
    // Every node without a subgraph, as one fused unit; control flow stays
    // on the CPU provider.
    let mut take = Vec::new();
    for &nd in &nodes {
        let mut ns = 0usize;
        ort_call!(Node_GetNumSubgraphs, nd, &mut ns);
        if ns == 0 {
            take.push(nd)
        }
    }
    let _ = a;
    if !take.is_empty() {
        // The weights stay out of the fused node's inputs: a model whose
        // weights are initializers (every YOLO export) would otherwise reach
        // Compute with one input per initializer beside the real ones, and
        // the host, holding the model with its weights inside, refuses the
        // count. The serializer reads them from the fused graph's own
        // initializer list at Compile.
        let options = OrtNodeFusionOptions {
            ort_version_supported: ORT_API_VERSION,
            drop_constant_initializers: true,
        };
        let st = unsafe { (ep_api().EpGraphSupportInfo_AddNodesToFuse.unwrap())(info, take.as_ptr(), take.len(), &options) };
        if !st.is_null() {
            return st;
        }
    }
    ptr::null_mut()
}

unsafe extern "C" fn ep_compile(
    _this: *mut OrtEp,
    graphs: *mut *const OrtGraph,
    _fused: *mut *const OrtNode,
    count: usize,
    infos: *mut *mut OrtNodeComputeInfo,
    _ctx_nodes: *mut *mut OrtNode,
) -> OrtStatusPtr {
    for i in 0..count {
        let g = unsafe { *graphs.add(i) };
        let serialized = match unsafe { serialize_graph(g) } {
            Ok(s) => s,
            Err(st) => return st,
        };
        let mut ci = alloc::boxed::Box::new(ComputeInfo {
            base: unsafe { core::mem::zeroed() },
            lock: Lock::new(),
            session: core::cell::UnsafeCell::new(Session {
                serialized,
                link: None,
                id: 0,
                bound: Vec::new(),
            }),
        });
        ci.base.ort_version_supported = ORT_API_VERSION;
        ci.base.CreateState = Some(ci_create_state);
        ci.base.Compute = Some(ci_compute);
        ci.base.ReleaseState = Some(ci_release_state);
        unsafe { *infos.add(i) = alloc::boxed::Box::into_raw(ci) as *mut OrtNodeComputeInfo };
    }
    ptr::null_mut()
}

unsafe extern "C" fn ep_release_compute_infos(_this: *mut OrtEp, infos: *mut *mut OrtNodeComputeInfo, n: usize) {
    for i in 0..n {
        let ci = unsafe { alloc::boxed::Box::from_raw(*infos.add(i) as *mut ComputeInfo) };
        let s = unsafe { &mut *ci.session.get() };
        if let Some(link) = s.link.as_mut() {
            let _ = link.call(KIND_CLOSE, &s.id.to_le_bytes());
        }
        drop(ci);
    }
}
unsafe extern "C" fn ci_create_state(this: *mut OrtNodeComputeInfo, _ctx: *mut OrtNodeComputeContext, state: *mut *mut c_void) -> OrtStatusPtr {
    unsafe { *state = this as *mut c_void };
    ptr::null_mut()
}
unsafe extern "C" fn ci_release_state(_this: *mut OrtNodeComputeInfo, _state: *mut c_void) {}

fn elem_size(t: u32) -> usize {
    match t {
        1 | 6 | 12 => 4,
        2 | 3 | 9 => 1,
        4 | 5 | 10 | 16 => 2,
        7 | 11 | 13 => 8,
        _ => 0,
    }
}

/// Loads the model on the host bound to these input dims, opening the link
/// if this is the first run.
fn bind(s: &mut Session, dims: &[Vec<i64>]) -> Result<(), String> {
    if s.link.is_none() {
        s.link = Some(Link::connect()?);
    }
    let link = s.link.as_mut().unwrap();
    if s.id != 0 {
        let _ = link.call(KIND_CLOSE, &s.id.to_le_bytes());
        s.id = 0;
    }
    let model = finish_model(&s.serialized, dims);
    let reply = link.call(KIND_LOAD, &model)?;
    if reply.len() < 8 {
        return Err(String::from("short reply to load"));
    }
    s.id = u64::from_le_bytes(reply[..8].try_into().unwrap());
    s.bound = dims.to_vec();
    Ok(())
}

unsafe extern "C" fn ci_compute(_this: *mut OrtNodeComputeInfo, state: *mut c_void, ctx: *mut OrtKernelContext) -> OrtStatusPtr {
    let a = api();
    let ci = unsafe { &*(state as *const ComputeInfo) };
    let mut nin = 0usize;
    ort_call!(KernelContext_GetInputCount, ctx, &mut nin);
    let mut dims_all: Vec<Vec<i64>> = Vec::with_capacity(nin);
    let mut body = Vec::new();
    body.extend_from_slice(&(nin as u32).to_le_bytes());
    for i in 0..nin {
        let mut v: *const OrtValue = ptr::null();
        ort_call!(KernelContext_GetInput, ctx, i, &mut v);
        let mut tsi: *mut OrtTensorTypeAndShapeInfo = ptr::null_mut();
        ort_call!(GetTensorTypeAndShape, v, &mut tsi);
        let (mut et, mut nd) = (0u32, 0usize);
        ort_call!(GetTensorElementType, tsi, &mut et);
        ort_call!(GetDimensionsCount, tsi, &mut nd);
        let mut dims = vec![0i64; nd];
        if nd > 0 {
            ort_call!(GetDimensions, tsi, dims.as_mut_ptr(), nd);
        }
        unsafe { (a.ReleaseTensorTypeAndShapeInfo.unwrap())(tsi) };
        let mut size = 0usize;
        ort_call!(GetTensorSizeInBytes, v, &mut size);
        let mut data: *const c_void = ptr::null();
        ort_call!(GetTensorData, v, &mut data);
        body.extend_from_slice(&et.to_le_bytes());
        body.extend_from_slice(&(nd as u32).to_le_bytes());
        for d in &dims {
            body.extend_from_slice(&d.to_le_bytes());
        }
        body.extend_from_slice(&(size as u64).to_le_bytes());
        body.extend_from_slice(unsafe { core::slice::from_raw_parts(data as *const u8, size) });
        dims_all.push(dims);
    }
    ci.lock.acquire();
    let s = unsafe { &mut *ci.session.get() };
    let result = (|| -> Result<Vec<u8>, String> {
        if s.id == 0 || s.bound != dims_all {
            bind(s, &dims_all)?;
        }
        let mut req = Vec::with_capacity(8 + body.len());
        req.extend_from_slice(&s.id.to_le_bytes());
        req.extend_from_slice(&body);
        s.link.as_mut().unwrap().call(KIND_RUN, &req)
    })();
    ci.lock.release();
    let reply = match result {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let mut p = 0usize;
    let rd_u32 = |p: &mut usize| -> u32 {
        let v = u32::from_le_bytes(reply[*p..*p + 4].try_into().unwrap());
        *p += 4;
        v
    };
    let rd_u64 = |p: &mut usize| -> u64 {
        let v = u64::from_le_bytes(reply[*p..*p + 8].try_into().unwrap());
        *p += 8;
        v
    };
    let nout = rd_u32(&mut p) as usize;
    for i in 0..nout {
        let et = rd_u32(&mut p);
        let nd = rd_u32(&mut p) as usize;
        let mut dims = vec![0i64; nd];
        for d in dims.iter_mut() {
            *d = rd_u64(&mut p) as i64;
        }
        let size = rd_u64(&mut p) as usize;
        let mut out: *mut OrtValue = ptr::null_mut();
        ort_call!(KernelContext_GetOutput, ctx, i, dims.as_ptr(), nd, &mut out);
        let mut dst: *mut c_void = ptr::null_mut();
        ort_call!(GetTensorMutableData, out, &mut dst);
        let n: usize = dims.iter().map(|d| *d as usize).product::<usize>() * elem_size(et);
        if n != size || p + size > reply.len() {
            return fail("output size does not match its shape");
        }
        unsafe { ptr::copy_nonoverlapping(reply.as_ptr().add(p), dst as *mut u8, size) };
        p += size;
    }
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn CreateEpFactories(
    _name: *const c_char,
    base: *const OrtApiBase,
    _logger: *const OrtLogger,
    factories: *mut *mut OrtEpFactory,
    max: usize,
    num: *mut usize,
) -> OrtStatusPtr {
    let api = unsafe { ((*base).GetApi.unwrap())(ORT_API_VERSION) };
    if api.is_null() {
        unsafe { *num = 0 };
        return ptr::null_mut();
    }
    API.store(api as *mut OrtApi, Ordering::Release);
    let ep_api = unsafe { ((*api).GetEpApi.unwrap())() };
    EP_API.store(ep_api as *mut OrtEpApi, Ordering::Release);
    if max == 0 {
        unsafe { *num = 0 };
        return ptr::null_mut();
    }
    let mut f = alloc::boxed::Box::new(Factory {
        base: unsafe { core::mem::zeroed() },
        name: cstring("LighterANE"),
        vendor: cstring("lighter"),
        version: cstring("0.7.0"),
        hw: ptr::null_mut(),
    });
    f.base.ort_version_supported = ORT_API_VERSION;
    f.base.GetName = Some(f_get_name);
    f.base.GetVendor = Some(f_get_vendor);
    f.base.GetVendorId = Some(f_get_vendor_id);
    f.base.GetVersion = Some(f_get_version);
    f.base.GetSupportedDevices = Some(f_get_supported_devices);
    f.base.CreateEp = Some(f_create_ep);
    f.base.ReleaseEp = Some(f_release_ep);
    f.base.CreateAllocator = Some(f_create_allocator);
    f.base.ReleaseAllocator = Some(f_release_allocator);
    f.base.CreateDataTransfer = Some(f_create_data_transfer);
    f.base.IsStreamAware = Some(f_is_stream_aware);
    f.base.CreateSyncStreamForDevice = Some(f_create_sync_stream);
    f.base.ValidateCompiledModelCompatibilityInfo = Some(f_validate_compat);
    unsafe {
        *factories = alloc::boxed::Box::into_raw(f) as *mut OrtEpFactory;
        *num = 1;
    }
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn ReleaseEpFactory(f: *mut OrtEpFactory) -> OrtStatusPtr {
    let fb = unsafe { alloc::boxed::Box::from_raw(f as *mut Factory) };
    if !fb.hw.is_null() {
        unsafe { (ep_api().ReleaseHardwareDevice.unwrap())(fb.hw) };
    }
    ptr::null_mut()
}

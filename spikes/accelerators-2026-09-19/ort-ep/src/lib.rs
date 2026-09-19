//! Spike 4: an ONNX Runtime plugin execution provider that claims a whole graph,
//! re-serialises it to ONNX bytes, and forwards runs to a host over a unix
//! socket. Host stand-in for the spike: a Python ORT server (host.py).
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
mod ort { include!(concat!(env!("OUT_DIR"), "/ort.rs")); }
use ort::*;
use std::ffi::{c_char, c_void, CStr, CString};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::Mutex;

static mut API: *const OrtApi = ptr::null();
static mut EP_API: *const OrtEpApi = ptr::null();
fn api() -> &'static OrtApi { unsafe { &*API } }
fn ep_api() -> &'static OrtEpApi { unsafe { &*EP_API } }

macro_rules! ort_call {
    ($f:ident, $($a:expr),*) => {{
        let st = unsafe { (api().$f.unwrap())($($a),*) };
        if !st.is_null() { return st; }
    }};
}
fn fail(msg: &str) -> OrtStatusPtr {
    let c = CString::new(msg).unwrap();
    unsafe { (api().CreateStatus.unwrap())(OrtErrorCode_ORT_EP_FAIL, c.as_ptr()) }
}
fn cstr(p: *const c_char) -> String { unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() } }

// ---------------- protobuf writer for ModelProto ----------------
struct Pb(Vec<u8>);
impl Pb {
    fn new() -> Self { Pb(Vec::new()) }
    fn varint(&mut self, mut v: u64) { loop { let b = (v & 0x7f) as u8; v >>= 7; if v == 0 { self.0.push(b); break } self.0.push(b | 0x80) } }
    fn tag(&mut self, field: u32, wt: u32) { self.varint(((field << 3) | wt) as u64) }
    fn int(&mut self, field: u32, v: i64) { self.tag(field, 0); self.varint(v as u64) }
    fn bytes(&mut self, field: u32, b: &[u8]) { self.tag(field, 2); self.varint(b.len() as u64); self.0.extend_from_slice(b) }
    fn str(&mut self, field: u32, s: &str) { self.bytes(field, s.as_bytes()) }
    fn f32(&mut self, field: u32, v: f32) { self.tag(field, 5); self.0.extend_from_slice(&v.to_le_bytes()) }
    fn msg(&mut self, field: u32, m: Pb) { self.bytes(field, &m.0) }
    fn packed_i64(&mut self, field: u32, vs: &[i64]) { let mut p = Pb::new(); for v in vs { p.varint(*v as u64) } self.bytes(field, &p.0) }
    fn packed_f32(&mut self, field: u32, vs: &[f32]) { let mut p = Pb::new(); for v in vs { p.0.extend_from_slice(&v.to_le_bytes()) } self.bytes(field, &p.0) }
}

unsafe fn value_infos(f: unsafe extern "C" fn(*const OrtGraph, *mut *const OrtValueInfo, usize) -> OrtStatusPtr,
                      n_f: unsafe extern "C" fn(*const OrtGraph, *mut usize) -> OrtStatusPtr,
                      g: *const OrtGraph) -> Result<Vec<*const OrtValueInfo>, OrtStatusPtr> {
    let mut n = 0usize; let st = n_f(g, &mut n); if !st.is_null() { return Err(st) }
    let mut v = vec![ptr::null(); n]; if n > 0 { let st = f(g, v.as_mut_ptr(), n); if !st.is_null() { return Err(st) } }
    Ok(v)
}

// ValueInfoProto with tensor type and shape (symbolic dims as dim_param).
unsafe fn value_info_proto(vi: *const OrtValueInfo) -> Result<Pb, OrtStatusPtr> {
    let a = api();
    let mut name: *const c_char = ptr::null(); let st = (a.GetValueInfoName.unwrap())(vi, &mut name); if !st.is_null() { return Err(st) }
    let mut ti: *const OrtTypeInfo = ptr::null(); let st = (a.GetValueInfoTypeInfo.unwrap())(vi, &mut ti); if !st.is_null() { return Err(st) }
    let mut tsi: *const OrtTensorTypeAndShapeInfo = ptr::null(); let st = (a.CastTypeInfoToTensorInfo.unwrap())(ti, &mut tsi); if !st.is_null() { return Err(st) }
    let mut p = Pb::new(); p.str(1, &cstr(name));
    if !tsi.is_null() {
        let mut et = 0u32; (a.GetTensorElementType.unwrap())(tsi, &mut et);
        let mut nd = 0usize; (a.GetDimensionsCount.unwrap())(tsi, &mut nd);
        let mut dims = vec![0i64; nd]; if nd > 0 { (a.GetDimensions.unwrap())(tsi, dims.as_mut_ptr(), nd); }
        let mut syms: Vec<*const c_char> = vec![ptr::null(); nd]; if nd > 0 { (a.GetSymbolicDimensions.unwrap())(tsi, syms.as_mut_ptr(), nd); }
        let mut shape = Pb::new();
        for i in 0..nd { let mut d = Pb::new(); if dims[i] >= 0 { d.int(1, dims[i]) } else if !syms[i].is_null() { d.str(2, &cstr(syms[i])) } shape.msg(1, d) }
        let mut tt = Pb::new(); tt.int(1, et as i64); tt.msg(2, shape);
        let mut ty = Pb::new(); ty.msg(1, tt);
        p.msg(2, ty);
    }
    Ok(p)
}

unsafe fn tensor_proto(name: &str, v: *const OrtValue) -> Result<Pb, OrtStatusPtr> {
    let a = api();
    let mut tsi: *mut OrtTensorTypeAndShapeInfo = ptr::null_mut(); let st = (a.GetTensorTypeAndShape.unwrap())(v, &mut tsi); if !st.is_null() { return Err(st) }
    let mut et = 0u32; (a.GetTensorElementType.unwrap())(tsi, &mut et);
    let mut nd = 0usize; (a.GetDimensionsCount.unwrap())(tsi, &mut nd);
    let mut dims = vec![0i64; nd]; if nd > 0 { (a.GetDimensions.unwrap())(tsi, dims.as_mut_ptr(), nd); }
    (a.ReleaseTensorTypeAndShapeInfo.unwrap())(tsi);
    let mut size = 0usize; let st = (a.GetTensorSizeInBytes.unwrap())(v, &mut size); if !st.is_null() { return Err(st) }
    let mut data: *const c_void = ptr::null(); let st = (a.GetTensorData.unwrap())(v, &mut data); if !st.is_null() { return Err(st) }
    let mut t = Pb::new(); t.packed_i64(1, &dims); t.int(2, et as i64); t.str(8, name);
    t.bytes(9, std::slice::from_raw_parts(data as *const u8, size));
    Ok(t)
}

unsafe fn attr_proto(attr: *const OrtOpAttr) -> Result<Pb, OrtStatusPtr> {
    let a = api();
    let mut name: *const c_char = ptr::null(); let st = (a.OpAttr_GetName.unwrap())(attr, &mut name); if !st.is_null() { return Err(st) }
    let mut ty: OrtOpAttrType = 0; let st = (a.OpAttr_GetType.unwrap())(attr, &mut ty); if !st.is_null() { return Err(st) }
    let mut p = Pb::new(); p.str(1, &cstr(name));
    let read = |ty: OrtOpAttrType| -> Result<Vec<u8>, OrtStatusPtr> {
        let mut need = 0usize;
        let st = (a.ReadOpAttr.unwrap())(attr, ty, ptr::null_mut(), 0, &mut need);
        if !st.is_null() { (a.ReleaseStatus.unwrap())(st); }
        let mut buf = vec![0u8; need.max(1)]; let mut out = 0usize;
        let st = (a.ReadOpAttr.unwrap())(attr, ty, buf.as_mut_ptr() as *mut c_void, buf.len(), &mut out); if !st.is_null() { return Err(st) }
        buf.truncate(out); Ok(buf)
    };
    match ty {
        OrtOpAttrType_ORT_OP_ATTR_INT => { let b = read(ty)?; p.int(3, i64::from_le_bytes(b[..8].try_into().unwrap())); p.int(20, 2) }
        OrtOpAttrType_ORT_OP_ATTR_INTS => { let b = read(ty)?; let v: Vec<i64> = b.chunks_exact(8).map(|c| i64::from_le_bytes(c.try_into().unwrap())).collect(); p.packed_i64(8, &v); p.int(20, 7) }
        OrtOpAttrType_ORT_OP_ATTR_FLOAT => { let b = read(ty)?; p.f32(2, f32::from_le_bytes(b[..4].try_into().unwrap())); p.int(20, 1) }
        OrtOpAttrType_ORT_OP_ATTR_FLOATS => { let b = read(ty)?; let v: Vec<f32> = b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect(); p.packed_f32(7, &v); p.int(20, 6) }
        OrtOpAttrType_ORT_OP_ATTR_STRING => { let mut b = read(ty)?; if b.last() == Some(&0) { b.pop(); } p.bytes(4, &b); p.int(20, 3) }
        OrtOpAttrType_ORT_OP_ATTR_STRINGS => {
            // ReadOpAttr returns the strings NUL-separated, one NUL after each.
            let b = read(ty)?; for s in b.split(|c| *c == 0).filter(|s| !s.is_empty()) { p.bytes(9, s) } p.int(20, 8)
        }
        OrtOpAttrType_ORT_OP_ATTR_TENSOR => {
            let mut v: *mut OrtValue = ptr::null_mut(); let st = (a.OpAttr_GetTensorAttributeAsOrtValue.unwrap())(attr, &mut v); if !st.is_null() { return Err(st) }
            let t = tensor_proto("", v)?; (a.ReleaseValue.unwrap())(v); p.msg(5, t); p.int(20, 4)
        }
        _ => return Err(fail("unsupported attribute type")),
    }
    Ok(p)
}

/// The fused subgraph as a complete ONNX ModelProto.
unsafe fn serialize_graph(g: *const OrtGraph) -> Result<Vec<u8>, OrtStatusPtr> {
    let a = api();
    let mut model = Pb::new();
    let mut ir = 0i64; let st = (a.Graph_GetOnnxIRVersion.unwrap())(g, &mut ir); if !st.is_null() { return Err(st) }
    model.int(1, ir); model.str(2, "lighter");
    let mut nos = 0usize; let st = (a.Graph_GetNumOperatorSets.unwrap())(g, &mut nos); if !st.is_null() { return Err(st) }
    let mut doms: Vec<*const c_char> = vec![ptr::null(); nos]; let mut vers = vec![0i64; nos];
    if nos > 0 { let st = (a.Graph_GetOperatorSets.unwrap())(g, doms.as_mut_ptr(), vers.as_mut_ptr(), nos); if !st.is_null() { return Err(st) } }
    for i in 0..nos { let mut o = Pb::new(); o.str(1, &cstr(doms[i])); o.int(2, vers[i]); model.msg(8, o) }
    let mut graph = Pb::new();
    let mut gname: *const c_char = ptr::null(); (a.Graph_GetName.unwrap())(g, &mut gname); graph.str(2, &cstr(gname));
    let mut n = 0usize; let st = (a.Graph_GetNumNodes.unwrap())(g, &mut n); if !st.is_null() { return Err(st) }
    let mut nodes: Vec<*const OrtNode> = vec![ptr::null(); n]; if n > 0 { let st = (a.Graph_GetNodes.unwrap())(g, nodes.as_mut_ptr(), n); if !st.is_null() { return Err(st) } }
    for &node in &nodes {
        let mut np = Pb::new();
        let mut ni = 0usize; (a.Node_GetNumInputs.unwrap())(node, &mut ni);
        let mut ins: Vec<*const OrtValueInfo> = vec![ptr::null(); ni]; if ni > 0 { let st = (a.Node_GetInputs.unwrap())(node, ins.as_mut_ptr(), ni); if !st.is_null() { return Err(st) } }
        for vi in ins { if vi.is_null() { np.str(1, "") } else { let mut nm: *const c_char = ptr::null(); (a.GetValueInfoName.unwrap())(vi, &mut nm); np.str(1, &cstr(nm)) } }
        let mut no = 0usize; (a.Node_GetNumOutputs.unwrap())(node, &mut no);
        let mut outs: Vec<*const OrtValueInfo> = vec![ptr::null(); no]; if no > 0 { let st = (a.Node_GetOutputs.unwrap())(node, outs.as_mut_ptr(), no); if !st.is_null() { return Err(st) } }
        for vi in outs { if vi.is_null() { np.str(2, "") } else { let mut nm: *const c_char = ptr::null(); (a.GetValueInfoName.unwrap())(vi, &mut nm); np.str(2, &cstr(nm)) } }
        let mut s: *const c_char = ptr::null();
        (a.Node_GetName.unwrap())(node, &mut s); np.str(3, &cstr(s));
        (a.Node_GetOperatorType.unwrap())(node, &mut s); np.str(4, &cstr(s));
        (a.Node_GetDomain.unwrap())(node, &mut s); let d = cstr(s); if d != "ai.onnx" { np.str(7, &d) }
        let mut na = 0usize; (a.Node_GetNumAttributes.unwrap())(node, &mut na);
        let mut attrs: Vec<*const OrtOpAttr> = vec![ptr::null(); na]; if na > 0 { let st = (a.Node_GetAttributes.unwrap())(node, attrs.as_mut_ptr(), na); if !st.is_null() { return Err(st) } }
        for at in attrs { np.msg(5, attr_proto(at)?) }
        graph.msg(1, np);
    }
    for vi in value_infos(a.Graph_GetInitializers.unwrap(), a.Graph_GetNumInitializers.unwrap(), g)? {
        let mut nm: *const c_char = ptr::null(); (a.GetValueInfoName.unwrap())(vi, &mut nm);
        let mut v: *const OrtValue = ptr::null(); let st = (a.ValueInfo_GetInitializerValue.unwrap())(vi, &mut v); if !st.is_null() { return Err(st) }
        graph.msg(5, tensor_proto(&cstr(nm), v)?);
    }
    for vi in value_infos(a.Graph_GetInputs.unwrap(), a.Graph_GetNumInputs.unwrap(), g)? { graph.msg(11, value_info_proto(vi)?) }
    for vi in value_infos(a.Graph_GetOutputs.unwrap(), a.Graph_GetNumOutputs.unwrap(), g)? { graph.msg(12, value_info_proto(vi)?) }
    model.msg(7, graph);
    Ok(model.0)
}

// ---------------- transport (spike: unix socket to host.py) ----------------
const KIND_LOAD: u32 = 1; const KIND_RUN: u32 = 2; const KIND_ERR: u32 = 0xffff;
struct Link(UnixStream);
impl Link {
    fn connect() -> Result<Link, String> {
        let path = std::env::var("LIGHTER_ANE_SOCKET").unwrap_or_else(|_| "/tmp/lighter-ane.sock".into());
        UnixStream::connect(&path).map(Link).map_err(|e| format!("connect {path}: {e}"))
    }
    fn call(&mut self, kind: u32, payload: &[u8]) -> Result<Vec<u8>, String> {
        let mut hdr = Vec::with_capacity(12); hdr.extend_from_slice(&kind.to_le_bytes()); hdr.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        self.0.write_all(&hdr).and_then(|_| self.0.write_all(payload)).map_err(|e| e.to_string())?;
        let mut h = [0u8; 12]; self.0.read_exact(&mut h).map_err(|e| e.to_string())?;
        let k = u32::from_le_bytes(h[..4].try_into().unwrap()); let n = u64::from_le_bytes(h[4..].try_into().unwrap()) as usize;
        let mut body = vec![0u8; n]; self.0.read_exact(&mut body).map_err(|e| e.to_string())?;
        if k == KIND_ERR { return Err(String::from_utf8_lossy(&body).into_owned()) }
        Ok(body)
    }
}

struct Session { link: Mutex<Link>, id: u64 }

// ---------------- the EP objects ----------------
#[repr(C)] struct Factory { base: OrtEpFactory, name: CString, vendor: CString, version: CString, hw: *mut OrtHardwareDevice }
#[repr(C)] struct Ep { base: OrtEp, name: CString }
#[repr(C)] struct ComputeInfo { base: OrtNodeComputeInfo, session: Session }

unsafe extern "C" fn f_get_name(this: *const OrtEpFactory) -> *const c_char { (*(this as *const Factory)).name.as_ptr() }
unsafe extern "C" fn f_get_vendor(this: *const OrtEpFactory) -> *const c_char { (*(this as *const Factory)).vendor.as_ptr() }
unsafe extern "C" fn f_get_vendor_id(_: *const OrtEpFactory) -> u32 { 0x1167 }
unsafe extern "C" fn f_get_version(this: *const OrtEpFactory) -> *const c_char { (*(this as *const Factory)).version.as_ptr() }

unsafe extern "C" fn f_get_supported_devices(this: *mut OrtEpFactory, _devices: *const *const OrtHardwareDevice, _n: usize,
                                             ep_devices: *mut *mut OrtEpDevice, max: usize, num: *mut usize) -> OrtStatusPtr {
    // We ignore what ORT found (a CPU, in a guest) and advertise our own NPU:
    // the Neural Engine on the other side of the vsock.
    let f = &mut *(this as *mut Factory);
    *num = 0; if max == 0 { return ptr::null_mut() }
    let vendor = CString::new("lighter").unwrap();
    let mut hw: *mut OrtHardwareDevice = ptr::null_mut();
    let st = (ep_api().CreateHardwareDevice.unwrap())(OrtHardwareDeviceType_OrtHardwareDeviceType_NPU, 0x1167, 0, vendor.as_ptr(), ptr::null(), &mut hw);
    if !st.is_null() { return st }
    f.hw = hw;
    let mut dev: *mut OrtEpDevice = ptr::null_mut();
    let st = (ep_api().CreateEpDevice.unwrap())(this, hw, ptr::null(), ptr::null(), &mut dev);
    if !st.is_null() { return st }
    *ep_devices = dev; *num = 1;
    ptr::null_mut()
}

unsafe extern "C" fn f_create_ep(_this: *mut OrtEpFactory, _d: *const *const OrtHardwareDevice, _m: *const *const OrtKeyValuePairs, _n: usize,
                                 _so: *const OrtSessionOptions, _log: *const OrtLogger, ep: *mut *mut OrtEp) -> OrtStatusPtr {
    let mut e = Box::new(Ep { base: std::mem::zeroed(), name: CString::new("LighterANE").unwrap() });
    e.base.ort_version_supported = ORT_API_VERSION;
    e.base.GetName = Some(ep_get_name);
    e.base.GetCapability = Some(ep_get_capability);
    e.base.Compile = Some(ep_compile);
    e.base.ReleaseNodeComputeInfos = Some(ep_release_compute_infos);
    *ep = Box::into_raw(e) as *mut OrtEp;
    ptr::null_mut()
}
unsafe extern "C" fn f_release_ep(_this: *mut OrtEpFactory, ep: *mut OrtEp) { drop(Box::from_raw(ep as *mut Ep)) }
unsafe extern "C" fn f_create_allocator(_: *mut OrtEpFactory, _: *const OrtMemoryInfo, _: *const OrtKeyValuePairs, out: *mut *mut OrtAllocator) -> OrtStatusPtr { *out = ptr::null_mut(); ptr::null_mut() }
unsafe extern "C" fn f_release_allocator(_: *mut OrtEpFactory, _: *mut OrtAllocator) {}
unsafe extern "C" fn f_create_data_transfer(_: *mut OrtEpFactory, out: *mut *mut OrtDataTransferImpl) -> OrtStatusPtr { *out = ptr::null_mut(); ptr::null_mut() }
unsafe extern "C" fn f_is_stream_aware(_: *const OrtEpFactory) -> bool { false }
unsafe extern "C" fn f_create_sync_stream(_: *mut OrtEpFactory, _: *const OrtMemoryDevice, _: *const OrtKeyValuePairs, out: *mut *mut OrtSyncStreamImpl) -> OrtStatusPtr { *out = ptr::null_mut(); ptr::null_mut() }
unsafe extern "C" fn f_validate_compat(_: *mut OrtEpFactory, _: *const *const OrtHardwareDevice, _: usize, _: *const c_char, out: *mut OrtCompiledModelCompatibility) -> OrtStatusPtr { *out = OrtCompiledModelCompatibility_OrtCompiledModelCompatibility_EP_NOT_APPLICABLE; ptr::null_mut() }

unsafe extern "C" fn ep_get_name(this: *const OrtEp) -> *const c_char { (*(this as *const Ep)).name.as_ptr() }

unsafe extern "C" fn ep_get_capability(_this: *mut OrtEp, graph: *const OrtGraph, info: *mut OrtEpGraphSupportInfo) -> OrtStatusPtr {
    let a = api();
    let mut n = 0usize; ort_call!(Graph_GetNumNodes, graph, &mut n);
    let mut nodes: Vec<*const OrtNode> = vec![ptr::null(); n];
    if n > 0 { ort_call!(Graph_GetNodes, graph, nodes.as_mut_ptr(), n); }
    // Claim every node without a subgraph, as one fused unit. Control flow stays on the CPU EP.
    let mut take = Vec::new();
    for &nd in &nodes { let mut ns = 0usize; ort_call!(Node_GetNumSubgraphs, nd, &mut ns); if ns == 0 { take.push(nd) } }
    let _ = a;
    if !take.is_empty() {
        let st = (ep_api().EpGraphSupportInfo_AddNodesToFuse.unwrap())(info, take.as_ptr(), take.len(), ptr::null());
        if !st.is_null() { return st }
    }
    ptr::null_mut()
}

unsafe extern "C" fn ep_compile(_this: *mut OrtEp, graphs: *mut *const OrtGraph, _fused: *mut *const OrtNode, count: usize,
                                infos: *mut *mut OrtNodeComputeInfo, _ctx_nodes: *mut *mut OrtNode) -> OrtStatusPtr {
    for i in 0..count {
        let g = *graphs.add(i);
        let bytes = match serialize_graph(g) { Ok(b) => b, Err(st) => return st };
        let mut link = match Link::connect() { Ok(l) => l, Err(e) => return fail(&e) };
        let reply = match link.call(KIND_LOAD, &bytes) { Ok(r) => r, Err(e) => return fail(&format!("load: {e}")) };
        let id = u64::from_le_bytes(reply[..8].try_into().unwrap());
        let mut ci = Box::new(ComputeInfo { base: std::mem::zeroed(), session: Session { link: Mutex::new(link), id } });
        ci.base.ort_version_supported = ORT_API_VERSION;
        ci.base.CreateState = Some(ci_create_state);
        ci.base.Compute = Some(ci_compute);
        ci.base.ReleaseState = Some(ci_release_state);
        *infos.add(i) = Box::into_raw(ci) as *mut OrtNodeComputeInfo;
    }
    ptr::null_mut()
}
unsafe extern "C" fn ep_release_compute_infos(_this: *mut OrtEp, infos: *mut *mut OrtNodeComputeInfo, n: usize) {
    for i in 0..n { drop(Box::from_raw(*infos.add(i) as *mut ComputeInfo)) }
}
unsafe extern "C" fn ci_create_state(this: *mut OrtNodeComputeInfo, _ctx: *mut OrtNodeComputeContext, state: *mut *mut c_void) -> OrtStatusPtr { *state = this as *mut c_void; ptr::null_mut() }
unsafe extern "C" fn ci_release_state(_this: *mut OrtNodeComputeInfo, _state: *mut c_void) {}

fn elem_size(t: u32) -> usize {
    match t { 1 => 4, 2 => 1, 3 => 1, 4 => 2, 5 => 2, 6 => 4, 7 => 8, 9 => 1, 10 => 2, 11 => 8, 12 => 4, 13 => 8, 16 => 2, _ => 0 }
}

unsafe extern "C" fn ci_compute(_this: *mut OrtNodeComputeInfo, state: *mut c_void, ctx: *mut OrtKernelContext) -> OrtStatusPtr {
    let a = api();
    let ci = &*(state as *const ComputeInfo);
    let mut nin = 0usize; ort_call!(KernelContext_GetInputCount, ctx, &mut nin);
    let mut req = Vec::new(); req.extend_from_slice(&ci.session.id.to_le_bytes()); req.extend_from_slice(&(nin as u32).to_le_bytes());
    for i in 0..nin {
        let mut v: *const OrtValue = ptr::null(); ort_call!(KernelContext_GetInput, ctx, i, &mut v);
        let mut tsi: *mut OrtTensorTypeAndShapeInfo = ptr::null_mut(); ort_call!(GetTensorTypeAndShape, v, &mut tsi);
        let mut et = 0u32; ort_call!(GetTensorElementType, tsi, &mut et);
        let mut nd = 0usize; ort_call!(GetDimensionsCount, tsi, &mut nd);
        let mut dims = vec![0i64; nd]; if nd > 0 { ort_call!(GetDimensions, tsi, dims.as_mut_ptr(), nd); }
        (a.ReleaseTensorTypeAndShapeInfo.unwrap())(tsi);
        let mut size = 0usize; ort_call!(GetTensorSizeInBytes, v, &mut size);
        let mut data: *const c_void = ptr::null(); ort_call!(GetTensorData, v, &mut data);
        req.extend_from_slice(&et.to_le_bytes()); req.extend_from_slice(&(nd as u32).to_le_bytes());
        for d in &dims { req.extend_from_slice(&d.to_le_bytes()) }
        req.extend_from_slice(&(size as u64).to_le_bytes());
        req.extend_from_slice(std::slice::from_raw_parts(data as *const u8, size));
    }
    let reply = { let mut l = ci.session.link.lock().unwrap(); match l.call(KIND_RUN, &req) { Ok(r) => r, Err(e) => return fail(&format!("run: {e}")) } };
    let mut p = 0usize;
    let rd_u32 = |p: &mut usize| { let v = u32::from_le_bytes(reply[*p..*p+4].try_into().unwrap()); *p += 4; v };
    let rd_u64 = |p: &mut usize| { let v = u64::from_le_bytes(reply[*p..*p+8].try_into().unwrap()); *p += 8; v };
    let nout = rd_u32(&mut p) as usize;
    for i in 0..nout {
        let et = rd_u32(&mut p); let nd = rd_u32(&mut p) as usize;
        let mut dims = vec![0i64; nd]; for d in dims.iter_mut() { *d = rd_u64(&mut p) as i64 }
        let size = rd_u64(&mut p) as usize;
        let mut out: *mut OrtValue = ptr::null_mut(); ort_call!(KernelContext_GetOutput, ctx, i, dims.as_ptr(), nd, &mut out);
        let mut dst: *mut c_void = ptr::null_mut(); ort_call!(GetTensorMutableData, out, &mut dst);
        let n: usize = dims.iter().map(|d| *d as usize).product::<usize>() * elem_size(et);
        if n != size { return fail(&format!("output {i}: {size} bytes for {n} expected")) }
        ptr::copy_nonoverlapping(reply.as_ptr().add(p), dst as *mut u8, size); p += size;
    }
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn CreateEpFactories(_name: *const c_char, base: *const OrtApiBase, _logger: *const OrtLogger,
                                           factories: *mut *mut OrtEpFactory, max: usize, num: *mut usize) -> OrtStatusPtr {
    API = ((*base).GetApi.unwrap())(ORT_API_VERSION);
    if API.is_null() { *num = 0; return ptr::null_mut() }
    EP_API = (api().GetEpApi.unwrap())();
    if max == 0 { *num = 0; return ptr::null_mut() }
    let mut f = Box::new(Factory { base: std::mem::zeroed(), name: CString::new("LighterANE").unwrap(), vendor: CString::new("lighter").unwrap(), version: CString::new("0.1.0").unwrap(), hw: ptr::null_mut() });
    f.base.ort_version_supported = ORT_API_VERSION;
    f.base.GetName = Some(f_get_name); f.base.GetVendor = Some(f_get_vendor); f.base.GetVendorId = Some(f_get_vendor_id); f.base.GetVersion = Some(f_get_version);
    f.base.GetSupportedDevices = Some(f_get_supported_devices); f.base.CreateEp = Some(f_create_ep); f.base.ReleaseEp = Some(f_release_ep);
    f.base.CreateAllocator = Some(f_create_allocator); f.base.ReleaseAllocator = Some(f_release_allocator); f.base.CreateDataTransfer = Some(f_create_data_transfer);
    f.base.IsStreamAware = Some(f_is_stream_aware); f.base.CreateSyncStreamForDevice = Some(f_create_sync_stream); f.base.ValidateCompiledModelCompatibilityInfo = Some(f_validate_compat);
    *factories = Box::into_raw(f) as *mut OrtEpFactory; *num = 1;
    ptr::null_mut()
}
#[no_mangle]
pub unsafe extern "C" fn ReleaseEpFactory(f: *mut OrtEpFactory) -> OrtStatusPtr {
    let fb = Box::from_raw(f as *mut Factory);
    if !fb.hw.is_null() { (ep_api().ReleaseHardwareDevice.unwrap())(fb.hw) }
    ptr::null_mut()
}

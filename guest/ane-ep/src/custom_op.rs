//! The Neural Engine for an ONNX Runtime older than 1.23, which cannot load a
//! plugin execution provider but has loaded custom operator libraries since
//! long before.
//!
//! `lighter_ane_wrap` turns a model into one node, `lighter.ane:Model`, with
//! the original model as its attribute and the original's inputs and outputs
//! as its own. `RegisterCustomOps` (what `register_custom_ops_library` calls)
//! registers that op; its kernel sends the embedded model to the host, bound
//! to the first run's shapes, and every run through it, exactly as the plugin
//! provider's fused node does (`crate::compute`). The host runs the whole
//! model, so nothing is split between the container and the host: the host's
//! own CPU provider takes what CoreML will not.
//!
//! The library asks for C API 16 on this path, the oldest with fallible
//! kernels (`KernelComputeV2`) and variadic inputs, so ONNX Runtime 1.16 on
//! loads it; the struct layout it reads is a prefix of 1.23's.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use core::sync::atomic::Ordering;

use crate::ort_sys::*;
use crate::pb::{bytes_of, Pb, Reader, Value};
use crate::{api, compute, fail, Input, Lock, Serialized, Session, API};

/// The C API this path asks for. Everything reachable from it (`compute`
/// included) must be in 1.16's table, whose 266 entries end at
/// `KernelContext_GetResource`: a runtime hands back a table only as long as
/// its own, and a call past the end jumps through whatever follows it, which
/// is how 1.22 crashed on `GetTensorSizeInBytes` (1.23). A test holds this.
const API_VERSION: u32 = 16;
const API_16_ENTRIES: usize = 266;
const DOMAIN: &[u8] = b"lighter.ane\0";
const OP: &[u8] = b"Model\0";
const ATTRIBUTE: &[u8] = b"model\0";

// ---------------- the model, taken apart and wrapped ----------------

/// Why a model cannot be wrapped: `lighter_ane_wrap` returns these negated.
#[derive(Debug, PartialEq, Eq)]
#[repr(i32)]
enum Refusal {
    NotOnnx = 1,
    /// Weights kept in files beside the model, which the host cannot see.
    ExternalData = 2,
}

struct Parts<'a> {
    /// ModelProto fields other than the graph, as they were.
    model_rest: Vec<&'a [u8]>,
    /// GraphProto fields other than its inputs, as they were.
    graph_rest: Vec<&'a [u8]>,
    /// The inputs a caller feeds (initializers listed as inputs are not).
    inputs: Vec<&'a [u8]>,
    outputs: Vec<&'a [u8]>,
}

fn parts(model: &[u8]) -> Result<Parts<'_>, Refusal> {
    let mut model_rest = Vec::new();
    let mut graph = None;
    let mut read = 0;
    for f in Reader::new(model) {
        read += f.raw.len();
        match (f.number, f.value) {
            (7, Value::Bytes(g)) => graph = Some(g),
            _ => model_rest.push(f.raw),
        }
    }
    // A reader stops at the first thing that is not protobuf, so bytes left
    // over mean these were never a model.
    let graph = graph.filter(|_| read == model.len()).ok_or(Refusal::NotOnnx)?;
    let tensors: Vec<&[u8]> = Reader::new(graph)
        .filter(|f| f.number == 5)
        .filter_map(|f| match f.value {
            Value::Bytes(t) => Some(t),
            _ => None,
        })
        .collect();
    // TensorProto.data_location (14) is EXTERNAL (1).
    if tensors.iter().any(|t| Reader::new(t).any(|f| f.number == 14 && matches!(f.value, Value::Varint(1)))) {
        return Err(Refusal::ExternalData);
    }
    let initializers: Vec<&[u8]> = tensors.iter().filter_map(|t| bytes_of(t, 8)).collect();
    let (mut graph_rest, mut inputs, mut outputs) = (Vec::new(), Vec::new(), Vec::new());
    for f in Reader::new(graph) {
        match (f.number, &f.value) {
            (11, Value::Bytes(vi)) => {
                if !bytes_of(vi, 1).is_some_and(|n| initializers.contains(&n)) {
                    inputs.push(*vi);
                }
            }
            (12, Value::Bytes(vi)) => {
                outputs.push(*vi);
                graph_rest.push(f.raw);
            }
            _ => graph_rest.push(f.raw),
        }
    }
    if outputs.is_empty() {
        return Err(Refusal::NotOnnx);
    }
    Ok(Parts { model_rest, graph_rest, inputs, outputs })
}

/// A ValueInfoProto's name, element type and rank.
fn input(vi: &[u8]) -> Option<Input> {
    let tensor = bytes_of(bytes_of(vi, 2)?, 1)?;
    let element_type = Reader::new(tensor).find_map(|f| match f.value {
        Value::Varint(t) if f.number == 1 => Some(t as u32),
        _ => None,
    })?;
    let rank = bytes_of(tensor, 2).map_or(0, |shape| Reader::new(shape).filter(|f| f.number == 1).count());
    Some(Input { name: bytes_of(vi, 1)?.to_vec(), element_type, rank })
}

/// The model as the plugin provider serialises a graph: everything but the
/// inputs, which `finish_model` adds bound to a run's shapes.
fn serialized(model: &[u8]) -> Option<Serialized> {
    let p = parts(model).ok()?;
    let mut graph = Pb::new();
    for raw in &p.graph_rest {
        graph.0.extend_from_slice(raw);
    }
    let mut m = Pb::new();
    for raw in &p.model_rest {
        m.0.extend_from_slice(raw);
    }
    m.msg(7, graph);
    let inputs = p.inputs.iter().map(|vi| input(vi)).collect::<Option<Vec<_>>>()?;
    Some(Serialized { model_without_inputs: m.0, inputs })
}

/// The one-node model that stands for `model`.
fn wrap(model: &[u8]) -> Result<Vec<u8>, Refusal> {
    let p = parts(model)?;
    let mut node = Pb::new();
    for vi in &p.inputs {
        node.bytes(1, bytes_of(vi, 1).ok_or(Refusal::NotOnnx)?);
    }
    for vi in &p.outputs {
        node.bytes(2, bytes_of(vi, 1).ok_or(Refusal::NotOnnx)?);
    }
    node.bytes(3, b"lighter_ane");
    node.bytes(4, &OP[..OP.len() - 1]);
    let mut attr = Pb::new();
    attr.bytes(1, &ATTRIBUTE[..ATTRIBUTE.len() - 1]);
    attr.bytes(4, model);
    attr.int(20, 3); // AttributeProto.STRING
    node.msg(5, attr);
    node.bytes(7, &DOMAIN[..DOMAIN.len() - 1]);

    let mut graph = Pb::new();
    graph.msg(1, node);
    graph.bytes(2, b"lighter_ane");
    for vi in &p.inputs {
        graph.bytes(11, vi);
    }
    for vi in &p.outputs {
        graph.bytes(12, vi);
    }
    let mut m = Pb::new();
    for raw in &p.model_rest {
        m.0.extend_from_slice(raw);
    }
    let mut opset = Pb::new();
    opset.bytes(1, &DOMAIN[..DOMAIN.len() - 1]);
    opset.int(2, 1);
    m.msg(8, opset);
    m.msg(7, graph);
    Ok(m.0)
}

/// Wraps the ONNX model at `model` (`len` bytes) into `*out` (`*out_len`
/// bytes, freed with `lighter_ane_free`). Returns 0; -1 for bytes that are
/// not an ONNX model; -2 for a model whose weights are in external files.
#[no_mangle]
pub unsafe extern "C" fn lighter_ane_wrap(model: *const u8, len: usize, out: *mut *mut u8, out_len: *mut usize) -> c_int {
    let wrapped = match wrap(unsafe { core::slice::from_raw_parts(model, len) }) {
        Ok(w) => w,
        Err(refusal) => return -(refusal as c_int),
    };
    let wrapped = wrapped.into_boxed_slice();
    unsafe {
        *out_len = wrapped.len();
        *out = Box::into_raw(wrapped) as *mut u8;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn lighter_ane_free(p: *mut u8, len: usize) {
    drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(p, len)) });
}

// ---------------- the op ----------------

struct Kernel {
    lock: Lock,
    session: UnsafeCell<Session>,
}

unsafe extern "C" fn op_name(_: *const OrtCustomOp) -> *const c_char {
    OP.as_ptr() as *const c_char
}
unsafe extern "C" fn op_provider(_: *const OrtCustomOp) -> *const c_char {
    ptr::null()
}
unsafe extern "C" fn op_type(_: *const OrtCustomOp, _: usize) -> ONNXTensorElementDataType {
    ONNXTensorElementDataType_ONNX_TENSOR_ELEMENT_DATA_TYPE_UNDEFINED
}
unsafe extern "C" fn op_one(_: *const OrtCustomOp) -> usize {
    1
}
unsafe extern "C" fn op_variadic(_: *const OrtCustomOp, _: usize) -> OrtCustomOpInputOutputCharacteristic {
    OrtCustomOpInputOutputCharacteristic_INPUT_OUTPUT_VARIADIC
}
unsafe extern "C" fn op_memory(_: *const OrtCustomOp, _: usize) -> OrtMemType {
    OrtMemType_OrtMemTypeDefault
}
unsafe extern "C" fn op_min_arity(_: *const OrtCustomOp) -> c_int {
    1
}
unsafe extern "C" fn op_heterogeneous(_: *const OrtCustomOp) -> c_int {
    0
}

unsafe extern "C" fn op_create_kernel(_: *const OrtCustomOp, a: *const OrtApi, info: *const OrtKernelInfo, kernel: *mut *mut c_void) -> OrtStatusPtr {
    let get = unsafe { (*a).KernelInfoGetAttribute_string.unwrap() };
    let mut size = 0usize;
    let st = unsafe { get(info, ATTRIBUTE.as_ptr() as *const c_char, ptr::null_mut(), &mut size) };
    if !st.is_null() {
        return st;
    }
    let mut model = alloc::vec![0u8; size];
    let st = unsafe { get(info, ATTRIBUTE.as_ptr() as *const c_char, model.as_mut_ptr() as *mut c_char, &mut size) };
    if !st.is_null() {
        return st;
    }
    // The size counts a terminating NUL the attribute's bytes do not have.
    model.truncate(size.saturating_sub(1));
    let Some(serialized) = serialized(&model) else {
        return fail("lighter.ane:Model does not carry an ONNX model");
    };
    let k = Box::new(Kernel {
        lock: Lock::new(),
        session: UnsafeCell::new(Session { serialized, link: None, id: 0, bound: Vec::new() }),
    });
    unsafe { *kernel = Box::into_raw(k) as *mut c_void };
    ptr::null_mut()
}

unsafe extern "C" fn op_compute(kernel: *mut c_void, ctx: *mut OrtKernelContext) -> OrtStatusPtr {
    let k = unsafe { &*(kernel as *const Kernel) };
    unsafe { compute(&k.lock, &k.session, ctx) }
}

unsafe extern "C" fn op_destroy(kernel: *mut c_void) {
    drop(unsafe { Box::from_raw(kernel as *mut Kernel) });
}

/// What `SessionOptions.register_custom_ops_library` calls.
#[no_mangle]
pub unsafe extern "C" fn RegisterCustomOps(options: *mut OrtSessionOptions, base: *const OrtApiBase) -> OrtStatusPtr {
    let a = unsafe { ((*base).GetApi.unwrap())(API_VERSION) };
    if a.is_null() {
        return ptr::null_mut();
    }
    // A process that loaded the plugin provider already holds a newer table,
    // which serves everything this path calls.
    let _ = API.compare_exchange(ptr::null_mut(), a as *mut OrtApi, Ordering::AcqRel, Ordering::Acquire);
    let mut op: OrtCustomOp = unsafe { core::mem::zeroed() };
    op.version = API_VERSION;
    op.GetName = Some(op_name);
    op.GetExecutionProviderType = Some(op_provider);
    op.GetInputType = Some(op_type);
    op.GetInputTypeCount = Some(op_one);
    op.GetOutputType = Some(op_type);
    op.GetOutputTypeCount = Some(op_one);
    op.GetInputCharacteristic = Some(op_variadic);
    op.GetOutputCharacteristic = Some(op_variadic);
    op.GetInputMemoryType = Some(op_memory);
    op.GetVariadicInputMinArity = Some(op_min_arity);
    op.GetVariadicInputHomogeneity = Some(op_heterogeneous);
    op.GetVariadicOutputMinArity = Some(op_min_arity);
    op.GetVariadicOutputHomogeneity = Some(op_heterogeneous);
    op.CreateKernelV2 = Some(op_create_kernel);
    op.KernelComputeV2 = Some(op_compute);
    op.KernelDestroy = Some(op_destroy);
    // ONNX Runtime keeps pointers to the op and its domain for as long as
    // any session may use them: both live for the process.
    let op = Box::leak(Box::new(op));
    let mut domain: *mut OrtCustomOpDomain = ptr::null_mut();
    ort_call!(CreateCustomOpDomain, DOMAIN.as_ptr() as *const c_char, &mut domain);
    ort_call!(CustomOpDomain_Add, domain, op);
    ort_call!(AddCustomOpDomain, options, domain);
    ptr::null_mut()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finish_model;
    use alloc::string::String;

    fn value_info(name: &str, elem_type: i64, dims: &[i64]) -> Pb {
        let mut shape = Pb::new();
        for d in dims {
            let mut dim = Pb::new();
            dim.int(1, *d);
            shape.msg(1, dim);
        }
        let mut tensor = Pb::new();
        tensor.int(1, elem_type);
        tensor.msg(2, shape);
        let mut ty = Pb::new();
        ty.msg(1, tensor);
        let mut vi = Pb::new();
        vi.bytes(1, name.as_bytes());
        vi.msg(2, ty);
        vi
    }

    /// Two inputs (float and uint8), an initializer also listed as an input
    /// the way IR < 4 models list them, and two outputs.
    fn model(external: bool) -> Vec<u8> {
        let mut node = Pb::new();
        node.bytes(1, b"x");
        node.bytes(2, b"z");
        node.bytes(4, b"Identity");
        let mut weight = Pb::new();
        weight.packed_i64(1, &[2]);
        weight.int(2, 1);
        weight.bytes(8, b"w");
        if external {
            weight.int(14, 1);
        }
        let mut graph = Pb::new();
        graph.msg(1, node);
        graph.bytes(2, b"g");
        graph.msg(5, weight);
        graph.msg(11, value_info("x", 1, &[1, 3, 320, 320]));
        graph.msg(11, value_info("w", 1, &[2]));
        graph.msg(11, value_info("y", 2, &[4]));
        graph.msg(12, value_info("z", 1, &[1, 84, 2100]));
        graph.msg(12, value_info("n", 7, &[1]));
        let mut opset = Pb::new();
        opset.bytes(1, b"");
        opset.int(2, 17);
        let mut m = Pb::new();
        m.int(1, 8);
        m.msg(8, opset);
        m.msg(7, graph);
        m.0
    }

    fn strings(b: &[u8], number: u32) -> Vec<&[u8]> {
        Reader::new(b)
            .filter(|f| f.number == number)
            .filter_map(|f| match f.value {
                Value::Bytes(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_model_becomes_one_node_carrying_it() {
        let original = model(false);
        let wrapped = wrap(&original).unwrap();
        let graph = bytes_of(&wrapped, 7).unwrap();
        let nodes = strings(graph, 1);
        assert_eq!(nodes.len(), 1);
        let node = nodes[0];
        assert_eq!(bytes_of(node, 4), Some(&b"Model"[..]));
        assert_eq!(bytes_of(node, 7), Some(&b"lighter.ane"[..]));
        assert_eq!(strings(node, 1), [&b"x"[..], b"y"], "the initializer is not an input");
        assert_eq!(strings(node, 2), [&b"z"[..], b"n"]);
        let attr = bytes_of(node, 5).unwrap();
        assert_eq!(bytes_of(attr, 4), Some(&original[..]));
        let inputs: Vec<_> = strings(graph, 11).iter().map(|vi| bytes_of(vi, 1).unwrap()).collect();
        assert_eq!(inputs, [&b"x"[..], b"y"]);
        assert_eq!(strings(graph, 12).len(), 2);
        let domains: Vec<_> = strings(&wrapped, 8).iter().map(|o| bytes_of(o, 1).unwrap_or(b"")).collect();
        assert!(domains.contains(&&b"lighter.ane"[..]));
        let ir = Reader::new(&wrapped).find_map(|f| match f.value {
            Value::Varint(v) if f.number == 1 => Some(v),
            _ => None,
        });
        assert_eq!(ir, Some(8), "the model's ir_version is kept");
    }

    #[test]
    fn the_carried_model_binds_its_inputs_to_a_runs_dims() {
        let s = serialized(&model(false)).unwrap();
        let names: Vec<_> = s.inputs.iter().map(|i| (i.name.as_slice(), i.element_type, i.rank)).collect();
        assert_eq!(names, [(&b"x"[..], 1, 4), (&b"y"[..], 2, 1)]);
        let graph = bytes_of(&s.model_without_inputs, 7).unwrap();
        assert!(strings(graph, 11).is_empty(), "inputs come back only bound");
        let bound = finish_model(&s, &[vec![1, 3, 320, 320], vec![4]]);
        let graphs = strings(&bound, 7);
        let inputs: Vec<_> = graphs.iter().flat_map(|g| strings(g, 11)).collect();
        assert_eq!(inputs.len(), 2);
    }

    #[test]
    fn what_is_not_a_model_is_refused() {
        assert_eq!(wrap(b"").unwrap_err(), Refusal::NotOnnx);
        assert_eq!(wrap(b"not an onnx model at all").unwrap_err(), Refusal::NotOnnx);
        let mut truncated = model(false);
        truncated.truncate(truncated.len() - 3);
        assert_eq!(wrap(&truncated).unwrap_err(), Refusal::NotOnnx);
    }

    #[test]
    fn external_weights_are_refused() {
        assert_eq!(wrap(&model(true)).unwrap_err(), Refusal::ExternalData);
    }

    /// Every C API entry the custom-op path calls, read from the source, is
    /// inside 1.16's table.
    #[test]
    fn this_path_calls_only_what_api_16_has() {
        let bindings = include_str!("ort_sys.rs");
        let table = &bindings[bindings.find("pub struct OrtApi {").unwrap()..];
        let table = &table[..table.find("\n}\n").unwrap()];
        let fields: Vec<&str> = table
            .lines()
            .filter_map(|l| l.strip_prefix("    pub "))
            .filter_map(|l| l.split(':').next())
            .collect();
        let lib = include_str!("lib.rs");
        let compute = &lib[lib.find("unsafe fn compute(").unwrap()..];
        let compute = &compute[..compute.find("\n}\n").unwrap()];
        let fail = &lib[lib.find("fn fail(").unwrap()..];
        let fail = &fail[..fail.find("\n}\n").unwrap()];
        let this = include_str!("custom_op.rs");
        let this = &this[..this.find("#[cfg(test)]").unwrap()];
        let mut called = Vec::new();
        for src in [compute, fail, this] {
            for marker in ["ort_call!(", "a.", "(*a)."] {
                for (i, _) in src.match_indices(marker) {
                    let name: String = src[i + marker.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    if name.starts_with(|c: char| c.is_ascii_uppercase()) {
                        called.push(name);
                    }
                }
            }
        }
        assert!(called.iter().any(|n| n == "GetTensorMutableData"), "the scan finds calls: {called:?}");
        for name in &called {
            let index = fields.iter().position(|f| f == name).unwrap_or_else(|| panic!("{name} is not in OrtApi"));
            assert!(index < API_16_ENTRIES, "{name} is entry {} of OrtApi; ONNX Runtime 1.16 has {API_16_ENTRIES}", index + 1);
        }
    }
}

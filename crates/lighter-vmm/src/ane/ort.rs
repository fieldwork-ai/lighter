//! ONNX Runtime on the host, with its CoreML provider: what puts a model on
//! the Neural Engine.
//!
//! Statically linked when `LIGHTER_ANE_LIBS` (or `host/out/ort`) holds the
//! libraries `host/ane/build.sh` produces; otherwise every session fails
//! with a message saying so, and the guest's fallback is the CPU provider it
//! already has.
//!
//! The provider is asked for the `NeuralNetwork` model format on purpose:
//! the newer `MLProgram` never reached the Neural Engine for a convolutional
//! model in testing (E5RT refused it as unbounded whatever the shapes said),
//! while `NeuralNetwork` ran ResNet-50 in 1.76 ms where the CPU took 30.

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

use super::ort_sys as sys;
use super::protocol::{Tensor, element_size};

pub struct Runtime {
    api: &'static sys::OrtApi,
    env: *mut sys::OrtEnv,
}

// SAFETY: ORT's env and sessions are thread-safe by contract; the pointers
// are only ever used through the API.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

pub struct Session {
    api: &'static sys::OrtApi,
    session: *mut sys::OrtSession,
    inputs: Vec<CString>,
    outputs: Vec<CString>,
}

unsafe impl Send for Session {}

fn status_message(api: &sys::OrtApi, st: *mut sys::OrtStatus) -> String {
    if st.is_null() {
        return String::new();
    }
    let msg = unsafe { CStr::from_ptr((api.GetErrorMessage.unwrap())(st)) }
        .to_string_lossy()
        .into_owned();
    unsafe { (api.ReleaseStatus.unwrap())(st) };
    msg
}

macro_rules! check {
    ($api:expr, $call:expr) => {{
        let st = unsafe { $call };
        if !st.is_null() {
            return Err(status_message($api, st));
        }
    }};
}

impl Runtime {
    /// Whether the runtime was linked in at all.
    pub const fn linked() -> bool {
        cfg!(ane_libs)
    }

    pub fn new() -> Result<Runtime, String> {
        if !Runtime::linked() {
            return Err("lighter was built without ONNX Runtime (host/ane/build.sh)".into());
        }
        let base = unsafe { sys::OrtGetApiBase() };
        if base.is_null() {
            return Err("OrtGetApiBase returned null".into());
        }
        let api = unsafe { ((*base).GetApi.unwrap())(sys::ORT_API_VERSION) };
        if api.is_null() {
            return Err(format!("ONNX Runtime does not offer API version {}", sys::ORT_API_VERSION));
        }
        let api: &'static sys::OrtApi = unsafe { &*api };
        let name = CString::new("lighter").unwrap();
        let mut env: *mut sys::OrtEnv = ptr::null_mut();
        check!(api, (api.CreateEnv.unwrap())(sys::OrtLoggingLevel_ORT_LOGGING_LEVEL_WARNING, name.as_ptr(), &mut env));
        Ok(Runtime { api, env })
    }

    /// Loads a model and puts it on the Neural Engine where CoreML can, the
    /// CPU where it cannot.
    pub fn load(&self, model: &[u8]) -> Result<Session, String> {
        let api = self.api;
        let mut options: *mut sys::OrtSessionOptions = ptr::null_mut();
        check!(api, (api.CreateSessionOptions.unwrap())(&mut options));
        let keys = [CString::new("ModelFormat").unwrap(), CString::new("MLComputeUnits").unwrap()];
        let values = [CString::new("NeuralNetwork").unwrap(), CString::new("ALL").unwrap()];
        let key_ptrs: Vec<*const c_char> = keys.iter().map(|k| k.as_ptr()).collect();
        let value_ptrs: Vec<*const c_char> = values.iter().map(|v| v.as_ptr()).collect();
        let provider = CString::new("CoreML").unwrap();
        let st = unsafe {
            (api.SessionOptionsAppendExecutionProvider.unwrap())(
                options,
                provider.as_ptr(),
                key_ptrs.as_ptr(),
                value_ptrs.as_ptr(),
                keys.len(),
            )
        };
        if !st.is_null() {
            let msg = status_message(api, st);
            tracing::warn!(%msg, "CoreML provider unavailable; running on the CPU");
        }
        let mut session: *mut sys::OrtSession = ptr::null_mut();
        let created = unsafe {
            (api.CreateSessionFromArray.unwrap())(
                self.env,
                model.as_ptr().cast(),
                model.len(),
                options,
                &mut session,
            )
        };
        unsafe { (api.ReleaseSessionOptions.unwrap())(options) };
        if !created.is_null() {
            return Err(status_message(api, created));
        }
        let mut allocator: *mut sys::OrtAllocator = ptr::null_mut();
        check!(api, (api.GetAllocatorWithDefaultOptions.unwrap())(&mut allocator));
        let names = |count: unsafe extern "C" fn(*const sys::OrtSession, *mut usize) -> sys::OrtStatusPtr,
                     name: unsafe extern "C" fn(*const sys::OrtSession, usize, *mut sys::OrtAllocator, *mut *mut c_char) -> sys::OrtStatusPtr|
         -> Result<Vec<CString>, String> {
            let mut n = 0usize;
            let st = unsafe { count(session, &mut n) };
            if !st.is_null() {
                return Err(status_message(api, st));
            }
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let mut p: *mut c_char = ptr::null_mut();
                let st = unsafe { name(session, i, allocator, &mut p) };
                if !st.is_null() {
                    return Err(status_message(api, st));
                }
                out.push(unsafe { CStr::from_ptr(p) }.to_owned());
                unsafe { (api.AllocatorFree.unwrap())(allocator, p.cast()) };
            }
            Ok(out)
        };
        let inputs = names(api.SessionGetInputCount.unwrap(), api.SessionGetInputName.unwrap())?;
        let outputs = names(api.SessionGetOutputCount.unwrap(), api.SessionGetOutputName.unwrap())?;
        Ok(Session {
            api,
            session,
            inputs,
            outputs,
        })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if !self.env.is_null() {
            unsafe { (self.api.ReleaseEnv.unwrap())(self.env) };
        }
    }
}

impl Session {
    pub fn run(&self, inputs: &[Tensor]) -> Result<Vec<Tensor>, String> {
        let api = self.api;
        if inputs.len() != self.inputs.len() {
            return Err(format!("the model takes {} inputs, {} were sent", self.inputs.len(), inputs.len()));
        }
        let mut memory_info: *mut sys::OrtMemoryInfo = ptr::null_mut();
        check!(
            api,
            (api.CreateCpuMemoryInfo.unwrap())(
                sys::OrtAllocatorType_OrtArenaAllocator,
                sys::OrtMemType_OrtMemTypeDefault,
                &mut memory_info
            )
        );
        let mut values: Vec<*mut sys::OrtValue> = Vec::with_capacity(inputs.len());
        let mut result: Result<(), String> = Ok(());
        for t in inputs {
            let mut v: *mut sys::OrtValue = ptr::null_mut();
            let st = unsafe {
                (api.CreateTensorWithDataAsOrtValue.unwrap())(
                    memory_info,
                    t.bytes.as_ptr() as *mut c_void,
                    t.bytes.len(),
                    t.dims.as_ptr(),
                    t.dims.len(),
                    t.element_type,
                    &mut v,
                )
            };
            if !st.is_null() {
                result = Err(status_message(api, st));
                break;
            }
            values.push(v);
        }
        unsafe { (api.ReleaseMemoryInfo.unwrap())(memory_info) };
        let outputs = result.and_then(|_| {
            let in_names: Vec<*const c_char> = self.inputs.iter().map(|n| n.as_ptr()).collect();
            let out_names: Vec<*const c_char> = self.outputs.iter().map(|n| n.as_ptr()).collect();
            let mut outs: Vec<*mut sys::OrtValue> = vec![ptr::null_mut(); self.outputs.len()];
            let st = unsafe {
                (api.Run.unwrap())(
                    self.session,
                    ptr::null(),
                    in_names.as_ptr(),
                    values.as_ptr() as *const *const sys::OrtValue,
                    values.len(),
                    out_names.as_ptr(),
                    out_names.len(),
                    outs.as_mut_ptr(),
                )
            };
            if !st.is_null() {
                return Err(status_message(api, st));
            }
            let mut tensors = Vec::with_capacity(outs.len());
            let mut err = None;
            for v in &outs {
                match self.read(*v) {
                    Ok(t) => tensors.push(t),
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            for v in outs {
                if !v.is_null() {
                    unsafe { (api.ReleaseValue.unwrap())(v) };
                }
            }
            match err {
                Some(e) => Err(e),
                None => Ok(tensors),
            }
        });
        for v in values {
            unsafe { (api.ReleaseValue.unwrap())(v) };
        }
        outputs
    }

    fn read(&self, v: *const sys::OrtValue) -> Result<Tensor, String> {
        let api = self.api;
        let mut info: *mut sys::OrtTensorTypeAndShapeInfo = ptr::null_mut();
        check!(api, (api.GetTensorTypeAndShape.unwrap())(v, &mut info));
        let mut element_type = 0u32;
        let mut rank = 0usize;
        unsafe {
            (api.GetTensorElementType.unwrap())(info, &mut element_type);
            (api.GetDimensionsCount.unwrap())(info, &mut rank);
        }
        let mut dims = vec![0i64; rank];
        if rank > 0 {
            unsafe { (api.GetDimensions.unwrap())(info, dims.as_mut_ptr(), rank) };
        }
        unsafe { (api.ReleaseTensorTypeAndShapeInfo.unwrap())(info) };
        let size = element_size(element_type);
        if size == 0 {
            return Err(format!("output of element type {element_type} cannot be carried"));
        }
        let elements: usize = dims.iter().map(|d| (*d).max(0) as usize).product();
        let mut data: *mut c_void = ptr::null_mut();
        check!(api, (api.GetTensorMutableData.unwrap())(v as *mut sys::OrtValue, &mut data));
        let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, elements * size) }.to_vec();
        Ok(Tensor {
            element_type,
            dims,
            bytes,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.session.is_null() {
            unsafe { (self.api.ReleaseSession.unwrap())(self.session) };
        }
    }
}

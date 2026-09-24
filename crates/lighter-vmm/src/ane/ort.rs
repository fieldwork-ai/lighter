//! ONNX Runtime on the host, with its CoreML provider: what puts a model on
//! the Neural Engine.
//!
//! Statically linked when `LIGHTER_ANE_LIBS` (or `host/out/ort`) holds the
//! libraries `host/ane/build.sh` produces; otherwise every session fails
//! with a message saying so, and the guest's fallback is the CPU provider it
//! already has.
//!
//! The device is "ONNX to CoreML", and CoreML covers the Mac's GPU and CPU
//! as well as the Neural Engine, so a model is loaded three ways and the
//! first run decides: the `NeuralNetwork` format with every compute unit,
//! which is what reaches the Neural Engine (the newer `MLProgram` never did
//! for a convolutional model; E5RT refused it as unbounded whatever the
//! shapes said, while `NeuralNetwork` ran ResNet-50 in 1.76 ms where the
//! CPU took 30); `MLProgram` with the CPU and GPU, for graphs the Neural
//! Engine will not take whole; and ONNX Runtime's own CPU, the floor. Each
//! is timed on the run's real inputs after a warm-up and the fastest is
//! kept, so a model that CoreML would only partition badly still runs where
//! it is quickest. `LIGHTER_ANE_UNITS=ane|gpu|cpu` pins one instead.
//!
//! CoreML's compiled models are cached under the lighter home
//! (`coreml-cache`), so a model, or a shape of it, compiles once.

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

use super::ort_sys as sys;
use super::protocol::{Tensor, element_size};

pub struct Runtime {
    api: &'static sys::OrtApi,
    env: *mut sys::OrtEnv,
    /// CoreML's compile cache, `ModelCacheDirectory`; none when unset.
    cache: Option<CString>,
    /// `crashes` beside it: one file per (model, candidate) mid-run.
    markers: Option<std::path::PathBuf>,
}

/// One way of running a model: which format CoreML gets and which units it
/// may use, or no CoreML at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Units {
    NeuralEngine,
    Gpu,
    Cpu,
}

impl Units {
    fn label(self) -> &'static str {
        match self {
            Units::NeuralEngine => "neural engine",
            Units::Gpu => "gpu",
            Units::Cpu => "cpu",
        }
    }

    /// The candidates in the order tried: all three, or the one the
    /// environment pins.
    fn candidates() -> Vec<Units> {
        match std::env::var("LIGHTER_ANE_UNITS").as_deref() {
            Ok("ane") => vec![Units::NeuralEngine],
            Ok("gpu") => vec![Units::Gpu],
            Ok("cpu") => vec![Units::Cpu],
            _ => vec![Units::NeuralEngine, Units::Gpu, Units::Cpu],
        }
    }
}

// SAFETY: ORT's env and sessions are thread-safe by contract; the pointers
// are only ever used through the API.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

/// Where a session records the candidate it is about to run for the first
/// time, so a crash inside CoreML is remembered for the next load.
struct Markers {
    dir: std::path::PathBuf,
    hash: u64,
}

/// The model's name for the log.
pub fn model_fingerprint(model: &[u8]) -> u64 {
    fingerprint(model)
}

/// A stable name for the model. ONNX Runtime names the fused node it hands
/// the provider after a hash of the subgraph's content, `LighterANE_<n>`,
/// and the guest writes that name into the graph it sends; the bytes around
/// it vary between two loads of the same model (initializer order), so the
/// name is the key when it is there, and a hash of the bytes otherwise.
fn fingerprint(model: &[u8]) -> u64 {
    if let Some(at) = model.windows(11).position(|w| w == b"LighterANE_") {
        let digits: Vec<u8> = model[at + 11..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .copied()
            .collect();
        if let Ok(n) = std::str::from_utf8(&digits).unwrap_or("").parse::<u64>() {
            return n;
        }
    }
    fnv(model)
}

/// FNV-1a over the model bytes: a name for it, not a security property.
fn fnv(model: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in model.chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        h ^= u64::from_le_bytes(word);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^ model.len() as u64
}

fn marker_name(hash: u64, units: Units) -> String {
    format!("{hash:016x}-{}", units.label().replace(' ', "-"))
}

/// How many runs may race the candidates. A CoreML candidate can fail on one
/// input and run the next: YOLO-NAS carries its own NMS, and on a frame with
/// nothing in it (Frigate warms a detector up on zeros) the NMS leaves an
/// empty tensor its CoreML partition will not take. Failing the first race is
/// then no verdict, so a candidate that failed races again on the next runs'
/// inputs, this many races in all.
const RACES: u32 = 3;

pub struct Session {
    api: &'static sys::OrtApi,
    /// The session in use; while `pending` is not empty, the first candidate.
    session: *mut sys::OrtSession,
    units: Units,
    /// The other candidates, timed against `session` at the first run and
    /// then released.
    pending: Vec<(Units, *mut sys::OrtSession)>,
    /// Candidates that failed on a race's input, raced again at the next run.
    retry: Vec<(Units, *mut sys::OrtSession)>,
    races: u32,
    /// The CPU candidate, kept when another unit wins, for the runs the
    /// winner fails (the same empty tensor, on any frame with nothing in it).
    fallback: *mut sys::OrtSession,
    fell_back: bool,
    markers: Option<Markers>,
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

/// The session's CPU thread pools, told to sleep when they have nothing to do.
///
/// ONNX Runtime's workers spin for a while after each task so the next one
/// starts sooner, and a client that sends a frame every 100 to 200 ms never
/// lets them stop: with Frigate attached at five frames a second the helper
/// read 43 to 53% of a core, 1,449 of a three-second profile's samples in
/// `ThreadPoolTempl::WorkerLoop` and `SpinPause` against 16 in CoreML's
/// prediction (the M5, 2026-09-21). With CoreML as the provider the compute
/// is the Neural Engine's or the GPU's and the pool is waiting for work that
/// never comes, so it also gets one thread. A CPU session does its real work
/// on the pool and keeps its width; it only stops spinning between runs. The
/// price is a futex wake per inference, tens of microseconds against
/// milliseconds.
fn quiet_threads(
    api: &sys::OrtApi,
    options: *mut sys::OrtSessionOptions,
    units: Units,
) -> Result<(), String> {
    // `LIGHTER_ANE_SPIN=1` keeps ONNX Runtime's defaults, for the A/B.
    if std::env::var_os("LIGHTER_ANE_SPIN").is_some() {
        return Ok(());
    }
    for key in QUIET_THREAD_KEYS {
        let key = CString::new(*key).unwrap();
        let off = CString::new("0").unwrap();
        check!(
            api,
            (api.AddSessionConfigEntry.unwrap())(options, key.as_ptr(), off.as_ptr())
        );
    }
    if units != Units::Cpu {
        check!(api, (api.SetIntraOpNumThreads.unwrap())(options, 1));
    }
    Ok(())
}

/// `onnxruntime_session_options_config_keys.h`: "0" has a pool's workers
/// block as soon as their queue is empty.
const QUIET_THREAD_KEYS: &[&str] = &[
    "session.intra_op.allow_spinning",
    "session.inter_op.allow_spinning",
];

impl Runtime {
    /// Whether the runtime was linked in at all.
    pub const fn linked() -> bool {
        cfg!(ane_libs)
    }

    #[cfg(not(ane_libs))]
    pub fn new(_cache: Option<&std::path::Path>) -> Result<Runtime, String> {
        Err("lighter was built without ONNX Runtime (host/ane/build.sh)".into())
    }

    #[cfg(ane_libs)]
    pub fn new(cache: Option<&std::path::Path>) -> Result<Runtime, String> {
        let base = unsafe { sys::OrtGetApiBase() };
        if base.is_null() {
            return Err("OrtGetApiBase returned null".into());
        }
        let api = unsafe { ((*base).GetApi.unwrap())(sys::ORT_API_VERSION) };
        if api.is_null() {
            return Err(format!(
                "ONNX Runtime does not offer API version {}",
                sys::ORT_API_VERSION
            ));
        }
        let api: &'static sys::OrtApi = unsafe { &*api };
        let name = CString::new("lighter").unwrap();
        let mut env: *mut sys::OrtEnv = ptr::null_mut();
        check!(
            api,
            (api.CreateEnv.unwrap())(
                sys::OrtLoggingLevel_ORT_LOGGING_LEVEL_WARNING,
                name.as_ptr(),
                &mut env
            )
        );
        Ok(Runtime {
            api,
            env,
            cache: cache.and_then(|c| {
                std::fs::create_dir_all(c).ok()?;
                CString::new(c.as_os_str().as_encoded_bytes()).ok()
            }),
            markers: cache.and_then(|c| {
                let dir = c.join("crashes");
                std::fs::create_dir_all(&dir).ok()?;
                Some(dir)
            }),
        })
    }

    /// Loads a model every way it might run; the first run picks one. A
    /// candidate that crashed the process on this model before is skipped:
    /// its marker was written before the run and never removed.
    pub fn load(&self, model: &[u8]) -> Result<Session, String> {
        let api = self.api;
        let mut made: Vec<(Units, *mut sys::OrtSession)> = Vec::new();
        let mut refused = Vec::new();
        let mut last_err = String::new();
        let markers = self
            .markers
            .as_ref()
            .map(|dir| (dir.clone(), fingerprint(model)));
        for units in Units::candidates() {
            if let Some((dir, hash)) = &markers
                && dir.join(marker_name(*hash, units)).exists()
            {
                tracing::warn!(
                    units = units.label(),
                    "neural engine: candidate crashed on this model before; skipped"
                );
                last_err = format!("{} crashed on this model before", units.label());
                refused.push(format!("{}: crashed before", units.label()));
                continue;
            }
            match self.create(model, units) {
                Ok(session) => made.push((units, session)),
                Err(e) => {
                    tracing::debug!(units = units.label(), %e, "neural engine: candidate refused");
                    refused.push(format!("{}: refused", units.label()));
                    last_err = e;
                }
            }
        }
        let Some((units, session)) = made.first().copied() else {
            return Err(last_err);
        };
        let pending = made[1..].to_vec();
        // With one candidate there is no race at the first run, so the
        // placement is logged here, or a model CoreML will not take would
        // leave no line saying where it runs.
        if pending.is_empty() {
            refused.push(format!("{}: the only one", units.label()));
            tracing::info!(chosen = units.label(), candidates = %refused.join(", "), "neural engine model placed");
        }
        let markers = markers.map(|(dir, hash)| Markers { dir, hash });
        let mut allocator: *mut sys::OrtAllocator = ptr::null_mut();
        check!(
            api,
            (api.GetAllocatorWithDefaultOptions.unwrap())(&mut allocator)
        );
        let names = |count: unsafe extern "C" fn(
            *const sys::OrtSession,
            *mut usize,
        ) -> sys::OrtStatusPtr,
                     name: unsafe extern "C" fn(
            *const sys::OrtSession,
            usize,
            *mut sys::OrtAllocator,
            *mut *mut c_char,
        ) -> sys::OrtStatusPtr|
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
        let inputs = names(
            api.SessionGetInputCount.unwrap(),
            api.SessionGetInputName.unwrap(),
        )?;
        let outputs = names(
            api.SessionGetOutputCount.unwrap(),
            api.SessionGetOutputName.unwrap(),
        )?;
        Ok(Session {
            api,
            session,
            units,
            pending,
            retry: Vec::new(),
            races: 0,
            fallback: ptr::null_mut(),
            fell_back: false,
            markers,
            inputs,
            outputs,
        })
    }

    /// One session with CoreML asked for `units`, or plain ONNX Runtime.
    fn create(&self, model: &[u8], units: Units) -> Result<*mut sys::OrtSession, String> {
        let api = self.api;
        let mut options: *mut sys::OrtSessionOptions = ptr::null_mut();
        check!(api, (api.CreateSessionOptions.unwrap())(&mut options));
        if let Err(msg) = quiet_threads(api, options, units) {
            unsafe { (api.ReleaseSessionOptions.unwrap())(options) };
            return Err(msg);
        }
        if units != Units::Cpu {
            let (format, compute) = match units {
                Units::NeuralEngine => ("NeuralNetwork", "ALL"),
                _ => ("MLProgram", "CPUAndGPU"),
            };
            let mut keys = vec![
                CString::new("ModelFormat").unwrap(),
                CString::new("MLComputeUnits").unwrap(),
            ];
            let mut values = vec![
                CString::new(format).unwrap(),
                CString::new(compute).unwrap(),
            ];
            if let Some(cache) = &self.cache {
                keys.push(CString::new("ModelCacheDirectory").unwrap());
                values.push(cache.clone());
            }
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
                unsafe { (api.ReleaseSessionOptions.unwrap())(options) };
                return Err(msg);
            }
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
        Ok(session)
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
    /// Runs the model. The first run times every candidate on these inputs,
    /// after a warm-up each, and keeps the fastest.
    pub fn run(&mut self, inputs: &[Tensor]) -> Result<Vec<Tensor>, String> {
        if !self.pending.is_empty() || (!self.retry.is_empty() && self.races < RACES) {
            self.choose(inputs);
        }
        match self.run_on(self.session, inputs) {
            Err(e) if !self.fallback.is_null() => {
                if !self.fell_back {
                    self.fell_back = true;
                    tracing::info!(units = self.units.label(), error = %first_line(&e), "neural engine: a run fell back to the host's CPU");
                }
                self.run_on(self.fallback, inputs)
            }
            result => result,
        }
    }

    /// Times each candidate: one untimed run to compile and warm, then the
    /// best of three. A candidate that fails to run is dropped. Logged, so
    /// the choice is a number in the log rather than a guess.
    fn choose(&mut self, inputs: &[Tensor]) {
        let api = self.api;
        // After a race nothing won, `session` is null and is no candidate.
        let mut candidates = Vec::new();
        if !self.session.is_null() {
            candidates.push((self.units, self.session));
        }
        candidates.append(&mut self.pending);
        candidates.append(&mut self.retry);
        if !self.fallback.is_null() {
            candidates.push((Units::Cpu, self.fallback));
            self.fallback = ptr::null_mut();
        }
        self.races += 1;
        let mut best: Option<(Units, *mut sys::OrtSession, f64)> = None;
        let mut losers = Vec::new();
        let mut report = Vec::new();
        for (units, session) in candidates {
            // The marker outlives a crash: if CoreML takes the process down
            // in this run, the next load of this model skips the candidate.
            let marker = self
                .markers
                .as_ref()
                .map(|m| m.dir.join(marker_name(m.hash, units)));
            if let Some(path) = &marker {
                let _ = std::fs::write(path, b"");
            }
            let warmed = self.run_on(session, inputs);
            if let Some(path) = &marker {
                let _ = std::fs::remove_file(path);
            }
            if let Err(e) = &warmed {
                report.push(format!("{}: failed ({})", units.label(), first_line(e)));
                if self.races < RACES {
                    self.retry.push((units, session));
                } else {
                    unsafe { (api.ReleaseSession.unwrap())(session) };
                }
                continue;
            }
            let mut fastest = f64::MAX;
            for _ in 0..3 {
                let t = std::time::Instant::now();
                if self.run_on(session, inputs).is_err() {
                    break;
                }
                fastest = fastest.min(t.elapsed().as_secs_f64() * 1e3);
            }
            report.push(format!("{}: {:.2} ms", units.label(), fastest));
            match best {
                Some((_, _, ms)) if ms <= fastest => losers.push((units, session)),
                Some((previous_units, previous, _)) => {
                    losers.push((previous_units, previous));
                    best = Some((units, session, fastest));
                }
                None => best = Some((units, session, fastest)),
            }
        }
        // The CPU candidate outlives a race it lost, to answer the runs the
        // winner cannot; the rest are released.
        for (units, session) in losers {
            if units == Units::Cpu {
                self.fallback = session;
            } else {
                unsafe { (api.ReleaseSession.unwrap())(session) };
            }
        }
        match best {
            Some((units, session, _)) => {
                self.units = units;
                self.session = session;
                tracing::info!(chosen = units.label(), candidates = %report.join(", "), "neural engine model placed");
            }
            None => {
                // Every candidate failed to run; keep the first so the error
                // reaches the caller from the run itself.
                self.session = ptr::null_mut();
                tracing::warn!(candidates = %report.join(", "), "neural engine: no candidate ran");
            }
        }
    }

    fn run_on(
        &self,
        session: *mut sys::OrtSession,
        inputs: &[Tensor],
    ) -> Result<Vec<Tensor>, String> {
        let api = self.api;
        if session.is_null() {
            return Err("the model could not be run by any provider".into());
        }
        if inputs.len() != self.inputs.len() {
            return Err(format!(
                "the model takes {} inputs, {} were sent",
                self.inputs.len(),
                inputs.len()
            ));
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
                    session,
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
            return Err(format!(
                "output of element type {element_type} cannot be carried"
            ));
        }
        let elements: usize = dims.iter().map(|d| (*d).max(0) as usize).product();
        let mut data: *mut c_void = ptr::null_mut();
        check!(
            api,
            (api.GetTensorMutableData.unwrap())(v as *mut sys::OrtValue, &mut data)
        );
        let bytes =
            unsafe { std::slice::from_raw_parts(data as *const u8, elements * size) }.to_vec();
        Ok(Tensor {
            element_type,
            dims,
            bytes,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for session in [self.session, self.fallback] {
            if !session.is_null() {
                unsafe { (self.api.ReleaseSession.unwrap())(session) };
            }
        }
        for (_, session) in self.pending.drain(..).chain(self.retry.drain(..)) {
            unsafe { (self.api.ReleaseSession.unwrap())(session) };
        }
    }
}

/// The first line of an ONNX Runtime error, short enough for a log line and
/// enough to tell a missing op from an unbounded dimension.
fn first_line(e: &str) -> String {
    e.lines().next().unwrap_or("").chars().take(160).collect()
}

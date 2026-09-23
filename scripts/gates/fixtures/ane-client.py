# Runs a small CNN on the Neural Engine through lighter's library and through
# the CPU provider, and prints whether they agree. The gate reads the last line.
# ANE_ROUTE=plugin loads the library as a plugin execution provider (ONNX
# Runtime 1.23 on); ANE_ROUTE=op wraps the model into its custom op, which any
# ONNX Runtime from 1.16 loads.
import ctypes, os, sys, time, numpy as np, onnxruntime as ort
lib = os.environ.get("LIGHTER_ANE_EP", "/usr/lib/lighter/liblighter_ane_ep.so")
model = sys.argv[1]
so = ort.SessionOptions(); so.log_severity_level = 3
if os.environ.get("ANE_ROUTE", "plugin") == "op":
    dll = ctypes.CDLL(lib)
    dll.lighter_ane_wrap.argtypes = [ctypes.c_char_p, ctypes.c_size_t, ctypes.POINTER(ctypes.POINTER(ctypes.c_uint8)), ctypes.POINTER(ctypes.c_size_t)]
    dll.lighter_ane_free.argtypes = [ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t]
    raw = open(model, "rb").read()
    out, n = ctypes.POINTER(ctypes.c_uint8)(), ctypes.c_size_t()
    if dll.lighter_ane_wrap(raw, len(raw), ctypes.byref(out), ctypes.byref(n)) != 0:
        print("RESULT the library would not wrap the model"); sys.exit(1)
    wrapped = ctypes.string_at(out, n.value); dll.lighter_ane_free(out, n.value)
    so.register_custom_ops_library(lib)
    s = ort.InferenceSession(wrapped, so, providers=["CPUExecutionProvider"])
else:
    ort.register_execution_provider_library("lighter", lib)
    devs = [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"]
    if not devs:
        print("RESULT no LighterANE device"); sys.exit(1)
    so.add_provider_for_devices(devs, {})
    s = ort.InferenceSession(model, so)
inp = s.get_inputs()[0]; shape = [d if isinstance(d, int) else 1 for d in inp.shape]
x = np.random.rand(*shape).astype(np.float32)
out = s.run(None, {inp.name: x})
ref = ort.InferenceSession(model, providers=["CPUExecutionProvider"]).run(None, {inp.name: x})
diff = max(float(np.abs(a - b).max()) for a, b in zip(out, ref))
t = time.perf_counter(); n = 20
for _ in range(n): s.run(None, {inp.name: x})
ms = (time.perf_counter() - t) / n * 1000
print(f"RESULT shapes={[o.shape for o in out]} max_abs_diff={diff:.3g} ms_per_run={ms:.2f}", flush=True)
# `paced <hz> <seconds>`: a camera's cadence, a frame and then nothing, which
# is when a host that spins between requests shows. The gate reads the host's
# CPU while this runs.
if len(sys.argv) > 4 and sys.argv[2] == "paced":
    hz, seconds = float(sys.argv[3]), float(sys.argv[4])
    print("PACING", flush=True)
    end = time.perf_counter() + seconds; runs = 0; spent = 0.0
    while time.perf_counter() < end:
        t = time.perf_counter(); s.run(None, {inp.name: x}); spent += time.perf_counter() - t
        runs += 1; time.sleep(max(0.0, 1.0 / hz - (time.perf_counter() - t)))
    print(f"PACED runs={runs} ms_per_run={spent / runs * 1000:.2f}", flush=True)
sys.exit(0 if diff < 1e-2 else 2)

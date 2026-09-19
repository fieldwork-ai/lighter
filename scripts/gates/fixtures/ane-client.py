# Runs a small CNN through lighter's plugin EP and through the CPU provider,
# and prints whether they agree. The gate reads the last line.
import os, sys, time, numpy as np, onnxruntime as ort
lib = os.environ.get("LIGHTER_ANE_EP", "/usr/lib/lighter/liblighter_ane_ep.so")
model = sys.argv[1]
ort.register_execution_provider_library("lighter", lib)
devs = [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"]
if not devs:
    print("RESULT no LighterANE device"); sys.exit(1)
so = ort.SessionOptions(); so.log_severity_level = 3
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
print(f"RESULT shapes={[o.shape for o in out]} max_abs_diff={diff:.3g} ms_per_run={ms:.2f}")
sys.exit(0 if diff < 1e-2 else 2)

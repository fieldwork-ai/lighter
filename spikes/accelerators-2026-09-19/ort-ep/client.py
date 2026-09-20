import sys, time, numpy as np, onnxruntime as ort
lib, model = sys.argv[1], sys.argv[2]
ort.register_execution_provider_library("lighter", lib)
devs = [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"]
print("ep devices:", [(d.ep_name, d.ep_vendor, d.device.type) for d in devs])
so = ort.SessionOptions(); so.log_severity_level = 3
so.add_provider_for_devices(devs, {})
s = ort.InferenceSession(model, so)
inp = s.get_inputs()[0]; shape = [d if isinstance(d, int) else 1 for d in inp.shape]
x = np.random.rand(*shape).astype(np.float32)
out = s.run(None, {inp.name: x})
ref = ort.InferenceSession(model, providers=["CPUExecutionProvider"]).run(None, {inp.name: x})
print("outputs:", [o.shape for o in out], "max |diff| vs cpu:", max(float(np.abs(a-b).max()) for a, b in zip(out, ref)))
t = time.perf_counter(); n = 20
for _ in range(n): s.run(None, {inp.name: x})
print(f"{(time.perf_counter()-t)/n*1000:.2f} ms per run through the plugin EP")

import onnxruntime as ort, numpy as np, time, sys
m = sys.argv[1]
shape = [d if isinstance(d, int) else 1 for d in ort.InferenceSession(m, providers=["CPUExecutionProvider"]).get_inputs()[0].shape]
x = np.random.rand(*shape).astype(np.float32)
def bench(providers, label):
    so = ort.SessionOptions(); so.log_severity_level = 3
    t0 = time.perf_counter(); s = ort.InferenceSession(m, so, providers=providers); load = (time.perf_counter()-t0)*1000
    name = s.get_inputs()[0].name
    for _ in range(5): s.run(None, {name: x})
    t = time.perf_counter(); n = 30
    for _ in range(n): s.run(None, {name: x})
    dt = (time.perf_counter()-t)/n*1000
    print(f"{label:26s} {dt:8.2f} ms/inference   (session load {load:.0f} ms)")
bench(["CPUExecutionProvider"], "cpu")
for cu in ["CPUOnly","CPUAndGPU","CPUAndNeuralEngine","ALL"]:
    try: bench([("CoreMLExecutionProvider", {"ModelFormat":"MLProgram","MLComputeUnits":cu}), "CPUExecutionProvider"], f"coreml {cu}")
    except Exception as e: print(f"coreml {cu}: FAIL {str(e)[:160]}")

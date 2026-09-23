# One model through Frigate's own detector pipeline (input transform, the
# lighter_ane detector, Frigate's post-processing per model type), twice: on
# the container's CPU and on the Neural Engine by `route` (op = lighter's
# custom op, which any ONNX Runtime from 1.16 loads; plugin = its plugin
# execution provider, 1.23 on). Run inside a Frigate image; writes JSON.
#
#   python3 matrix.py <route> <model.onnx> <model_type> <width> <height>
#                     <input_dtype> <pixel_format> <images_dir> <out.json>
import glob
import json
import os
import sys
import time

import cv2
import numpy as np
import onnxruntime as ort

from frigate.detectors.detection_runners import ONNXModelRunner
from frigate.detectors.detector_config import ModelConfig
from frigate.detectors.device import build_detector_config, parse_device
from frigate.object_detection.base import LocalObjectDetector

route, path, model_type, width, height, dtype, pixfmt, images, out = sys.argv[1:10]
width, height = int(width), int(height)
LIB = os.environ.get("LIGHTER_ANE_EP", "/usr/lib/lighter/liblighter_ane_ep.so")
WARM = int(os.environ.get("MATRIX_RUNS", "100"))

model = ModelConfig(
    path=path,
    model_type=model_type,
    width=width,
    height=height,
    input_tensor="nchw",
    input_dtype=dtype,
    input_pixel_format=pixfmt,
    labelmap_path="/labelmap/coco-80.txt",
)
config = build_detector_config(parse_device("lighter_ane"), model)

t = time.perf_counter()
detector = LocalObjectDetector(detector_config=config)
load_ms = (time.perf_counter() - t) * 1000
api = detector.detect_api

if route == "plugin":
    ort.register_execution_provider_library("LighterANE", LIB)
    options = ort.SessionOptions()
    options.add_provider_for_devices(
        [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"], {}
    )
    api.runner = ONNXModelRunner(ort.InferenceSession(path, options), model_type)
ane_runner = api.runner
cpu_runner = ONNXModelRunner(
    ort.InferenceSession(path, providers=["CPUExecutionProvider"]), model_type
)


def frames():
    for f in sorted(glob.glob(os.path.join(images, "*.jpg"))):
        img = cv2.resize(cv2.imread(f), (width, height), interpolation=cv2.INTER_AREA)
        if pixfmt == "rgb":
            img = cv2.cvtColor(img, cv2.COLOR_BGR2RGB)
        yield os.path.basename(f), img[np.newaxis, ...]


def run(runner):
    api.runner = runner
    detections = {name: detector.detect_raw(x).tolist() for name, x in frames()}
    _, x = next(frames())
    for _ in range(5):
        detector.detect_raw(x)
    t = time.perf_counter()
    for _ in range(WARM):
        detector.detect_raw(x)
    return detections, (time.perf_counter() - t) / WARM * 1000


cpu, cpu_ms = run(cpu_runner)
ane, ane_ms = run(ane_runner)
json.dump(
    {
        "route": route,
        "ort": ort.__version__,
        "model": os.path.basename(path),
        "model_type": model_type,
        "load_ms": load_ms,
        "cpu_ms": cpu_ms,
        "ane_ms": ane_ms,
        "cpu": cpu,
        "ane": ane,
    },
    open(out, "w"),
)
print(f"{route} ORT {ort.__version__} {os.path.basename(path)}: cpu {cpu_ms:.2f} ms, ane {ane_ms:.2f} ms, load {load_ms:.0f} ms")

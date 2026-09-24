"""Frigate detector: an ONNX model on the Mac's Neural Engine through lighter.

Runs the model through the library lighter places in containers started with
`--device lighter.sh/ane=all`, as an ONNX Runtime custom op: the library wraps
the model into one node that runs it on the Neural Engine, which works with the
ONNX Runtime Frigate already ships. Everything else is Frigate's own ONNX
detector: the same model types, the same post-processing.
"""

import ctypes
import logging
import os
from typing import Literal

import onnxruntime as ort
from pydantic import ConfigDict, Field

from frigate.detectors.detection_api import DetectionApi
from frigate.detectors.detection_runners import ONNXModelRunner
from frigate.detectors.detector_config import BaseDetectorConfig, ModelTypeEnum
from frigate.detectors.plugins.onnx import ONNXDetector

logger = logging.getLogger(__name__)

DETECTOR_KEY = "lighter_ane"


class LighterANEDetectorConfig(BaseDetectorConfig):
    """ONNX models on the Mac's Neural Engine, through lighter's device."""

    model_config = ConfigDict(title="lighter Neural Engine")

    type: Literal[DETECTOR_KEY]
    library: str = Field(
        default=os.environ.get("LIGHTER_ANE_EP", "/usr/lib/lighter/liblighter_ane_ep.so"),
        title="Neural Engine library",
        description="lighter's Neural Engine library, placed in the container by the device.",
    )


class LighterANEDetector(DetectionApi):
    type_key = DETECTOR_KEY
    supported_models = getattr(ONNXDetector, "supported_models", [])

    def __init__(self, detector_config: LighterANEDetectorConfig):
        super().__init__(detector_config)
        path = detector_config.model.path
        if not os.path.exists(detector_config.library):
            raise RuntimeError(
                "no Neural Engine library: start the container with --device lighter.sh/ane=all"
            )
        lib = ctypes.CDLL(detector_config.library)
        lib.lighter_ane_wrap.argtypes = [
            ctypes.c_char_p,
            ctypes.c_size_t,
            ctypes.POINTER(ctypes.POINTER(ctypes.c_uint8)),
            ctypes.POINTER(ctypes.c_size_t),
        ]
        lib.lighter_ane_free.argtypes = [ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t]
        with open(path, "rb") as f:
            model = f.read()
        out, size = ctypes.POINTER(ctypes.c_uint8)(), ctypes.c_size_t()
        if lib.lighter_ane_wrap(model, len(model), ctypes.byref(out), ctypes.byref(size)):
            raise RuntimeError(f"{path} is not an ONNX model lighter can run")
        wrapped = ctypes.string_at(out, size.value)
        lib.lighter_ane_free(out, size.value)
        options = ort.SessionOptions()
        options.log_severity_level = 3
        options.register_custom_ops_library(detector_config.library)
        session = ort.InferenceSession(wrapped, options, providers=["CPUExecutionProvider"])
        self.runner = ONNXModelRunner(session, detector_config.model.model_type)
        self.onnx_model_type = detector_config.model.model_type
        self.onnx_model_px = detector_config.model.input_pixel_format
        self.onnx_model_shape = detector_config.model.input_tensor
        if self.onnx_model_type == ModelTypeEnum.yolox:
            self.calculate_grids_strides()
        logger.info(f"lighter_ane: {path} on the Neural Engine")
        ONNXDetector._warmup(self, detector_config)

    detect_raw = ONNXDetector.detect_raw

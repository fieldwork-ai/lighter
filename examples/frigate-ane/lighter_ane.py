"""Frigate detector: an ONNX model on the Mac's Neural Engine through lighter.

Runs the model with ONNX Runtime's plugin execution provider that lighter
ships into containers started with `--device lighter.sh/ane=all`. Everything
else is Frigate's own ONNX detector: the same model types, the same
post-processing.
"""

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
        title="Provider library",
        description="lighter's ONNX Runtime plugin provider, placed in the container by the device.",
    )


class LighterANEDetector(DetectionApi):
    type_key = DETECTOR_KEY
    supported_models = getattr(ONNXDetector, "supported_models", [])

    def __init__(self, detector_config: LighterANEDetectorConfig):
        super().__init__(detector_config)
        path = detector_config.model.path
        ort.register_execution_provider_library("lighter", detector_config.library)
        devices = [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"]
        if not devices:
            raise RuntimeError(
                "no Neural Engine device: start the container with --device lighter.sh/ane=all"
            )
        options = ort.SessionOptions()
        options.log_severity_level = 3
        options.add_provider_for_devices(devices, {})
        session = ort.InferenceSession(path, options)
        self.runner = ONNXModelRunner(session, detector_config.model.model_type)
        self.onnx_model_type = detector_config.model.model_type
        self.onnx_model_px = detector_config.model.input_pixel_format
        self.onnx_model_shape = detector_config.model.input_tensor
        if self.onnx_model_type == ModelTypeEnum.yolox:
            self.calculate_grids_strides()
        logger.info(f"lighter_ane: {path} on the Neural Engine")
        ONNXDetector._warmup(self, detector_config)

    detect_raw = ONNXDetector.detect_raw

# Verbatim from Frigate's docs (docs/data/object_detectors_models.yaml, ONNX section).
# Frigate's arguments: --rm --build-arg BACKBONE=hgnetv2 --build-arg MODEL_SIZE=n --output .
FROM python:3.11-slim AS build
RUN apt-get update && apt-get install --no-install-recommends -y git libgl1 libglib2.0-0 && rm -rf /var/lib/apt/lists/*
COPY --from=ghcr.io/astral-sh/uv:0.8.0 /uv /bin/
WORKDIR /deimv2
RUN git clone https://github.com/Intellindust-AI-Lab/DEIMv2.git .
# Install CPU-only PyTorch first to avoid pulling CUDA variant
RUN uv pip install --no-cache --system torch torchvision --index-url https://download.pytorch.org/whl/cpu
RUN uv pip install --no-cache --system -r requirements.txt
RUN uv pip install --no-cache --system onnx safetensors huggingface_hub
RUN mkdir -p output
ARG BACKBONE
ARG MODEL_SIZE
# Download from Hugging Face and convert safetensors to pth
RUN python3 -c "\
from huggingface_hub import hf_hub_download; \
from safetensors.torch import load_file; \
import torch; \
backbone = '${BACKBONE}'.replace('hgnetv2','HGNetv2').replace('dinov3','DINOv3'); \
size = '${MODEL_SIZE}'.upper(); \
st = load_file(hf_hub_download('Intellindust/DEIMv2_' + backbone + '_' + size + '_COCO', 'model.safetensors')); \
torch.save({'model': st}, 'output/deimv2.pth')"
RUN sed -i "s/data = torch.rand(2/data = torch.rand(1/" tools/deployment/export_onnx.py
# HuggingFace safetensors omits frozen constants that the model constructor initializes
RUN sed -i "s/cfg.model.load_state_dict(state)/cfg.model.load_state_dict(state, strict=False)/" tools/deployment/export_onnx.py
RUN python3 tools/deployment/export_onnx.py -c configs/deimv2/deimv2_${BACKBONE}_${MODEL_SIZE}_coco.yml -r output/deimv2.pth
FROM scratch
ARG BACKBONE
ARG MODEL_SIZE
COPY --from=build /deimv2/output/deimv2.onnx /deimv2_${BACKBONE}_${MODEL_SIZE}.onnx

# Verbatim from Frigate's docs (docs/data/object_detectors_models.yaml, ONNX section).
# Frigate's arguments: --build-arg MODEL_SIZE=s --output .
FROM python:3.11 AS build
RUN apt-get update && apt-get install --no-install-recommends -y libgl1 && rm -rf /var/lib/apt/lists/*
COPY --from=ghcr.io/astral-sh/uv:0.8.0 /uv /bin/
WORKDIR /dfine
RUN git clone https://github.com/Peterande/D-FINE.git .
RUN uv pip install --system -r requirements.txt
RUN uv pip install --system onnx onnxruntime onnxsim onnxscript
# Create output directory and download checkpoint
RUN mkdir -p output
ARG MODEL_SIZE
RUN wget https://github.com/Peterande/storage/releases/download/dfinev1.0/dfine_${MODEL_SIZE}_obj2coco.pth -O output/dfine_${MODEL_SIZE}_obj2coco.pth
# Modify line 58 of export_onnx.py to change batch size to 1
RUN sed -i '58s/data = torch.rand(.*)/data = torch.rand(1, 3, 640, 640)/' tools/deployment/export_onnx.py
RUN python3 tools/deployment/export_onnx.py -c configs/dfine/objects365/dfine_hgnetv2_${MODEL_SIZE}_obj2coco.yml -r output/dfine_${MODEL_SIZE}_obj2coco.pth
FROM scratch
ARG MODEL_SIZE
COPY --from=build /dfine/output/dfine_${MODEL_SIZE}_obj2coco.onnx /dfine-${MODEL_SIZE}.onnx

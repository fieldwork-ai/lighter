# YOLO11n as lighter's Frigate example exports it (examples/frigate-ane/README.md).
FROM python:3.11 AS build
RUN apt-get update && apt-get install --no-install-recommends -y libgl1 && rm -rf /var/lib/apt/lists/*
RUN pip install -q ultralytics onnx onnxslim
WORKDIR /out
RUN yolo export model=yolo11n.pt format=onnx imgsz=320 opset=17 simplify=True
FROM scratch
COPY --from=build /out/yolo11n.onnx /yolo11n-320.onnx

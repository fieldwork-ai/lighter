#!/usr/bin/env bash
# Builds each model the matrix runs, with Frigate's documented recipes, into
# benchmarks/.cache/ane-models/ (skipping any already there), plus the images.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$HERE/../.cache/ane-models"
mkdir -p "$OUT/images"
build() { # file, output name, build args...
	local file=$1 name=$2; shift 2
	[ -s "$OUT/$name" ] && { echo "have $name"; return; }
	echo "==> $name"
	docker buildx build "$@" --output "$OUT" -f "$HERE/$file" "$HERE" > "$OUT/$name.log" 2>&1 \
		&& [ -s "$OUT/$name" ] && echo "built $name" || echo "FAILED $name (see $name.log)"
}
# onnx-simplifier has x86-64 wheels only (and does not build here): run the
# recipe as written, as x86-64 under Rosetta, as for RF-DETR below.
build yolov9.Dockerfile yolov9-t-320.onnx --platform linux/amd64 --build-arg MODEL_SIZE=t --build-arg IMG_SIZE=320
build yolov9.Dockerfile yolov9-s-320.onnx --platform linux/amd64 --build-arg MODEL_SIZE=s --build-arg IMG_SIZE=320
build yolov9.Dockerfile yolov9-s-640.onnx --platform linux/amd64 --build-arg MODEL_SIZE=s --build-arg IMG_SIZE=640
build yolo11.Dockerfile yolo11n-320.onnx
# Its dependencies have x86-64 wheels and not arm64 ones (onnxsim builds with
# cmake): run the recipe as written, as x86-64 under Rosetta.
build rfdetr.Dockerfile rfdetr-Nano.onnx --platform linux/amd64 --build-arg MODEL_SIZE=Nano
build dfine.Dockerfile dfine-s.onnx --build-arg MODEL_SIZE=s
build deimv2.Dockerfile deimv2_hgnetv2_n.onnx --platform linux/amd64 --build-arg BACKBONE=hgnetv2 --build-arg MODEL_SIZE=n
build yolonas.Dockerfile yolo_nas_s.onnx
# YOLOX: Frigate's docs point at the official release.
[ -s "$OUT/yolox_tiny.onnx" ] || curl -fsSL -o "$OUT/yolox_tiny.onnx" https://github.com/Megvii-BaseDetection/YOLOX/releases/download/0.1.1rc0/yolox_tiny.onnx && echo "have yolox_tiny.onnx"
# Images: two with many objects (the Ultralytics samples) and frames of the camera clip.
for i in bus zidane; do
	[ -s "$OUT/images/$i.jpg" ] || curl -fsSL -o "$OUT/images/$i.jpg" "https://ultralytics.com/images/$i.jpg"
done

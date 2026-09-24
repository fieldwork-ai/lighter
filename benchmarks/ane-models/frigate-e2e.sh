#!/usr/bin/env bash
# Frigate end to end on the lighter_ane detector, one model at a time: the
# camera clip replayed at 5 fps through hardware decode, the model in Frigate's
# current `models:` form, and Frigate's own stats read after it settles. A
# model passes when the camera and detector keep up (nothing skipped), the
# model loaded on the Neural Engine, and the hardware probe lists it.
#
#   FRIGATE_IMAGE=... CLIP=/path/to/clip.mkv benchmarks/ane-models/frigate-e2e.sh
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE="$HERE/../.cache/ane-models"
IMAGE="${FRIGATE_IMAGE:?FRIGATE_IMAGE names a Frigate image with the lighter_ane detector}"
CLIP="${CLIP:?CLIP is a camera clip to replay}"
SETTLE="${SETTLE:-60}"
# The detector the model runs on: `onnx` for Frigate with ONNX Runtime 1.23+,
# which picks up the Neural Engine provider itself.
export DEVICE="${DEVICE:-lighter_ane}"

MODELS=(
	"yolov9-t-320.onnx yolo-generic 320 320 float rgb"
	"yolo11n-320.onnx yolo-generic 320 320 float rgb"
	"yolox_tiny.onnx yolox 416 416 float_denorm rgb"
	"yolo_nas_s.onnx yolonas 320 320 int bgr"
	"dfine-s.onnx dfine 640 640 float rgb"
	"rfdetr-Nano.onnx rfdetr 320 320 float rgb"
)

failed=0
for spec in "${MODELS[@]}"; do
	set -- $spec
	file=$1
	# ONLY=<regex> runs the models whose file matches it.
	[[ -z "${ONLY:-}" || $file =~ $ONLY ]] || continue
	[ -s "$CACHE/$file" ] || { echo "skip $file: not exported"; continue; }
	dir=$(mktemp -d)
	mkdir "$dir/config"
	cat > "$dir/config/config.yml" <<EOF
mqtt:
  enabled: false
cameras:
  clip:
    ffmpeg:
      hwaccel_args: -c:v h264_v4l2m2m
      inputs:
        - path: /media/frigate/clip.mkv
          input_args: -re -stream_loop -1 -fflags +genpts
          roles: [detect]
    detect:
      enabled: true
      width: 896
      height: 512
      fps: 5
    objects:
      track: [person, car, cat, dog]
models:
  - scene: all
    path: /models/$file
    model_type: $2
    width: $3
    height: $4
    input_tensor: nchw
    input_dtype: $5
    input_pixel_format: $6
    labelmap_path: /labelmap/coco-80.txt
    devices:
      - $DEVICE
version: 0.19-0
EOF
	c=$(docker create --device lighter.sh/ane=all --device lighter.sh/video=all --shm-size 256m "$IMAGE")
	docker cp "$dir/config" "$c:/config" >/dev/null
	docker start "$c" >/dev/null
	docker exec "$c" mkdir -p /models /media/frigate
	docker cp "$CACHE/$file" "$c:/models/$file" >/dev/null
	docker cp "$CLIP" "$c:/media/frigate/clip.mkv" >/dev/null
	docker restart "$c" >/dev/null
	sleep "$SETTLE"
	stats=$(docker exec "$c" curl -s http://127.0.0.1:5000/api/stats)
	probe=$(docker exec "$c" curl -s http://127.0.0.1:5000/api/hardware/probe)
	loaded=$(docker logs "$c" 2>&1 | grep -o "Loaded .* model on [A-Za-z ]*" | tail -1)
	line=$(python3 "$HERE/e2e_check.py" "$stats" "$probe" "$loaded" 2>&1)
	echo "$line  $file ($2)  [$loaded]"
	case "$line" in ok*) ;; *) failed=1; docker logs "$c" 2>&1 | grep -iE "error|traceback" | tail -5 | sed 's/^/    /' ;; esac
	docker rm -f "$c" >/dev/null
	rm -rf "$dir"
done
exit $failed

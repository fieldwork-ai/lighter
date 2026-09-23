#!/usr/bin/env bash
# The Neural Engine across every model type Frigate's lighter_ane detector
# takes, through Frigate's own pipeline, on both routes into lighter's library:
# the custom op on the ONNX Runtime Frigate ships and the plugin provider on
# 1.23. Each is checked against the container's CPU after Frigate's
# post-processing (ane-models/compare.py) and timed.
#
#   FRIGATE_IMAGE=<a Frigate image with the lighter_ane detector> benchmarks/ane-models.sh
#
# Models come from ane-models/export.sh (Frigate's documented recipes). Talks
# to whatever DOCKER_HOST names; LIGHTER_HOME is that machine's home, whose
# CoreML cache is cleared before each model so the first load is cold, and
# whose log says where the host placed each model.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE="$HERE/.cache/ane-models"
IMAGE="${FRIGATE_IMAGE:?FRIGATE_IMAGE names a Frigate image with the lighter_ane detector}"
HOME_DIR="${LIGHTER_HOME:-$HOME/.lighter}"
OUT="${ANE_MODELS_OUT:-$CACHE/results}"
mkdir -p "$OUT"

# The plugin provider needs ONNX Runtime 1.23; the image keeps what Frigate ships.
docker build -q -t lighter-ane-models-plugin - >/dev/null <<EOF || exit 1
FROM $IMAGE
RUN pip3 install --break-system-packages --root-user-action=ignore -q "onnxruntime==1.23.*"
EOF

# file | Frigate model_type | width | height | input_dtype | pixel format
MODELS=(
	"yolov9-t-320.onnx yolo-generic 320 320 float rgb"
	"yolov9-s-640.onnx yolo-generic 640 640 float rgb"
	"yolo11n-320.onnx yolo-generic 320 320 float rgb"
	"yolox_tiny.onnx yolox 416 416 float_denorm rgb"
	"yolo_nas_s.onnx yolonas 320 320 int bgr"
	"dfine-s.onnx dfine 640 640 float rgb"
	"deimv2_hgnetv2_n.onnx dfine 640 640 float rgb"
	"rfdetr-Nano.onnx rfdetr 320 320 float rgb"
)

placed() { # the host's placement lines logged since byte $1
	tail -c +"$(($1 + 1))" "$HOME_DIR/machine.log" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' |
		grep -a "neural engine model placed" | tail -1 | sed 's/.*placed //'
}

results=()
for spec in "${MODELS[@]}"; do
	set -- $spec
	file=$1
	[ -s "$CACHE/$file" ] || { echo "skip $file: not exported"; continue; }
	for route in op plugin; do
		image=$IMAGE
		[ "$route" = plugin ] && image=lighter-ane-models-plugin
		rm -rf "$HOME_DIR/coreml-cache/"* 2>/dev/null
		mark=$(stat -f %z "$HOME_DIR/machine.log" 2>/dev/null || echo 0)
		c=$(docker create --device lighter.sh/ane=all -e PYTHONPATH=/opt/frigate \
			--entrypoint python3 -w /opt/frigate "$image" \
			/m/matrix.py "$route" "/m/$file" "$2" "$3" "$4" "$5" "$6" /m/images "/m/out.json")
		docker cp "$HERE/ane-models/matrix.py" "$c:/m/matrix.py" >/dev/null 2>&1 ||
			{ docker cp "$CACHE/images" "$c:/m/" >/dev/null; docker cp "$HERE/ane-models/matrix.py" "$c:/m/matrix.py" >/dev/null; }
		docker cp "$CACHE/images" "$c:/m/" >/dev/null
		docker cp "$CACHE/$file" "$c:/m/$file" >/dev/null
		if "$HERE/../scripts/capped.sh" 900 docker start -a "$c" > "$OUT/${file%.onnx}-$route.log" 2>&1 &&
			docker cp "$c:/m/out.json" "$OUT/${file%.onnx}-$route.json" >/dev/null 2>&1; then
			echo "$(tail -1 "$OUT/${file%.onnx}-$route.log")  [host: $(placed "$mark")]"
			results+=("$OUT/${file%.onnx}-$route.json")
		else
			echo "FAIL $file $route: $(grep -aE 'Error|error' "$OUT/${file%.onnx}-$route.log" | tail -1)"
		fi
		docker rm -f "$c" >/dev/null
	done
done
echo
python3 "$HERE/ane-models/compare.py" "${results[@]}"

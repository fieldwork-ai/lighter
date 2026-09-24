#!/usr/bin/env bash
# How many cameras one Mac can run in Frigate on lighter: the camera clip
# replayed as N cameras, each decoded on the media engine (h264_v4l2m2m) and
# detected on the lighter_ane detector (YOLO11n), N raised until it fails.
#
#   FRIGATE_IMAGE=... CLIP=... MODEL=... LIGHTER_HOME=... DOCKER_HOST=... \
#     frigate-sweat.sh "<detectors>" <level> [<level> ...]
#
# A level holds when every camera averages at least 95% of its 5 fps and
# nothing is skipped. Each line: cameras, detectors, per-camera fps (min and
# mean), detections a second in all, skipped, inference ms, Frigate's CPU in
# the guest, and the host's CPU for the VM and for the Neural Engine service.
set -uo pipefail
IMAGE="${FRIGATE_IMAGE:?}"; CLIP="${CLIP:?}"; MODEL="${MODEL:?}"
HOME_DIR="${LIGHTER_HOME:?}"
SETTLE="${SETTLE:-90}"
DETECTORS=$1; shift

config() { # cameras, detectors
	echo "mqtt:"
	echo "  enabled: false"
	echo "detectors:"
	for d in $(seq 1 "$2"); do echo "  ane$d:"; echo "    type: lighter_ane"; done
	cat <<EOF
model:
  path: /models/model.onnx
  model_type: yolo-generic
  width: 320
  height: 320
  input_tensor: nchw
  input_pixel_format: rgb
  input_dtype: float
  labelmap_path: /labelmap/coco-80.txt
record:
  enabled: false
snapshots:
  enabled: false
cameras:
EOF
	for c in $(seq 1 "$1"); do
		cat <<EOF
  cam$c:
    ffmpeg:
      hwaccel_args: -c:v h264_v4l2m2m ${DECODER_ARGS:-}
      inputs:
        - path: /media/frigate/clip.mkv
          input_args: -re -stream_loop -1 -fflags +genpts
          roles: [detect]
    detect: {enabled: true, width: 896, height: 512, fps: 5}
    objects: {track: [person, car, cat, dog]}
EOF
	done
	echo "version: 0.18-0"
}

host_cpu() { # %CPU of this home's VM and of its ane-host child, over 20 s
	local vm ane
	vm=$(cat "$HOME_DIR/lighter.pid")
	ane=$(ps -Ao pid,args | awk -v h="$HOME_DIR" '$0 ~ "ane-host" && index($0, h) {print $1}' | head -1)
	top -l 3 -s 10 -stats pid,cpu -pid "$vm" -pid "${ane:-$vm}" 2>/dev/null |
		awk -v vm="$vm" -v ane="$ane" '$1==vm {v=$2} $1==ane {a=$2} END {printf "vm %s%% ane %s%%", v, a}'
}

for level in "$@"; do
	dir=$(mktemp -d); mkdir "$dir/config"; config "$level" "$DETECTORS" > "$dir/config/config.yml"
	shm=$(( 64 + level * 24 ))
	c=$(docker create --device lighter.sh/ane=all --device lighter.sh/video=all --shm-size "${shm}m" "$IMAGE")
	docker cp "$dir/config" "$c:/config" >/dev/null
	docker start "$c" >/dev/null
	docker exec "$c" mkdir -p /models /media/frigate
	docker cp "$MODEL" "$c:/models/model.onnx" >/dev/null
	docker cp "$CLIP" "$c:/media/frigate/clip.mkv" >/dev/null
	docker restart "$c" >/dev/null
	since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
	sleep "$SETTLE"
	samples=""
	for i in 1 2 3; do
		samples="$samples$(docker exec "$c" curl -s http://127.0.0.1:5000/api/stats)"$'\n'
		sleep 20
	done
	cpu=$(docker stats --no-stream --format '{{.CPUPerc}}' "$c")
	host=$(host_cpu)
	line=$(python3 - "$level" "$DETECTORS" "$cpu" "$host" <<PY
import json, sys
level, det, cpu, host = sys.argv[1:5]
snaps = [json.loads(l) for l in """$samples""".strip().splitlines() if l.strip()]
fps, skipped, detections, inference = [], 0.0, 0.0, []
for s in snaps:
    cams = s.get("cameras", {})
    fps.append([c["camera_fps"] for c in cams.values()] or [0])
    skipped += sum(c["skipped_fps"] for c in cams.values()) / len(snaps)
    detections += sum(c["detection_fps"] for c in cams.values()) / len(snaps)
    inference += [d["inference_speed"] for d in s.get("detectors", {}).values()]
per_cam = [sum(v) / len(v) for v in zip(*fps)] if fps else [0]
lo, mean = min(per_cam), sum(per_cam) / len(per_cam)
ok = len(per_cam) == int(level) and lo >= 4.75 and skipped == 0
print(f"{'ok  ' if ok else 'WALL'} cameras {level:>3} detectors {det}  fps min {lo:.2f} mean {mean:.2f}  "
      f"detections {detections:6.1f}/s  skipped {skipped:.1f}  inference {sum(inference)/max(len(inference),1):.2f} ms  "
      f"frigate {cpu}  host {host}", flush=True)
PY
)
	echo "$line"
	# Errors since the restart; the start before it ran without the clip.
	docker logs --since "$since" "$c" 2>&1 | grep -iE "error|traceback" | grep -v "go2rtc" | tail -3 | sed 's/^/    /'
	docker rm -f "$c" >/dev/null
	rm -rf "$dir"
	# One level past the first that failed, to see the wall is a wall.
	case "$line" in WALL*) walls=$(( ${walls:-0} + 1 )) ;; esac
	[ "${walls:-0}" -ge 2 ] && break
done

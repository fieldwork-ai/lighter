#!/usr/bin/env bash
# What each transcode path delivers, so "faster" cannot be "did less": runs
# transcode-h264 and transcode-hevc once on a target, keeping the result, and
# scores it against the source on the Mac with one scorer for every target
# (Homebrew's ffmpeg with libvmaf): frames, the bitrate actually delivered,
# profile, and VMAF, PSNR and SSIM. Outside run.sh because it scores rather
# than times.
#
#   benchmarks/transcode-check.sh <target> <docker context> <out.csv>
#
# native runs the cases on the Mac; any other target runs them in the
# benchmark image (LIGHTER_BENCH_TRANSCODE_IMAGE, default lighter-bench:2),
# with LIGHTER_BENCH_CASE_ARGS added to `docker run` (lighter: `--device
# lighter.sh/video=all`, as run.sh gives it).
set -euo pipefail
TARGET="$1"; CTX="${2:-}"; OUT="$3"
IMAGE="${LIGHTER_BENCH_TRANSCODE_IMAGE:-lighter-bench:2}"
CLIP="${LIGHTER_BENCH_MODEL_DIR:?}/bbb-1080p30-10s.mp4"
CASES="$(cd "$(dirname "$0")/cases" && pwd)"
SCORER="${LIGHTER_BENCH_SCORER:-/opt/homebrew/bin/ffmpeg}"
PROBE="$(dirname "$SCORER")/ffprobe"
# Under the home folder: every runtime shares it with its guest, and none
# shares the system's temporary directory.
W="$(mktemp -d "$HOME/.transcode-check.XXXXXX")"
trap 'rm -rf "$W"' EXIT
mkdir -p "$W/media" "$W/out"
cp "$CLIP" "$W/media/"
[ -s "$OUT" ] || echo "target,codec,frames,kbps,profile,vmaf,psnr_y,ssim" > "$OUT"

for codec in h264 hevc; do
	result="$W/out/$codec.mp4"
	if [ "$TARGET" = native ]; then
		WORK="$W" TRANSCODE_OUT="-y $result" sh "$CASES/transcode-$codec.sh"
	else
		# shellcheck disable=SC2086
		docker --context "$CTX" run --rm ${LIGHTER_BENCH_CASE_ARGS:-} -v "$W:/work" -v "$CASES:/cases:ro" \
			-e WORK=/work -e TRANSCODE_OUT="-y /work/out/$codec.mp4" "$IMAGE" sh "/cases/transcode-$codec.sh"
	fi
	frames="$("$PROBE" -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$result")"
	profile="$("$PROBE" -v error -select_streams v:0 -show_entries stream=profile -of csv=p=0 "$result")"
	kbps="$("$PROBE" -v error -show_entries format=bit_rate -of csv=p=0 "$result" | awk '{printf "%d", $1/1000}')"
	"$SCORER" -hide_banner -loglevel error -i "$result" -i "$W/media/$(basename "$CLIP")" -lavfi \
		"[0:v]setpts=PTS-STARTPTS[d];[1:v]setpts=PTS-STARTPTS[r];[d][r]libvmaf=feature='name=psnr|name=float_ssim':n_threads=8:log_fmt=json:log_path=$W/$codec.json" \
		-f null -
	python3 - "$W/$codec.json" "$TARGET" "$codec" "$frames" "$kbps" "$profile" >> "$OUT" <<'PY'
import json, sys
path, target, codec, frames, kbps, profile = sys.argv[1:]
m = json.load(open(path))["pooled_metrics"]
print("%s,%s,%s,%s,%s,%.2f,%.2f,%.4f" % (target, codec, frames, kbps, profile,
      m["vmaf"]["mean"], m["psnr_y"]["mean"], m["float_ssim"]["mean"]))
PY
done

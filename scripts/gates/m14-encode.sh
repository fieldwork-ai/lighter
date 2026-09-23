#!/usr/bin/env bash
# m14: a container encodes H.264 and HEVC on the Mac's media engine.
#
# Boots the Docker guest with the video devices, and in stock Debian
# containers, given `--device lighter.sh/video=all`, encodes through
# /dev/video1, the V4L2 stateful encoder, with the clients distributions
# ship: ffmpeg 5.1 (bookworm) and 7.1 (trixie), and GStreamer 1.26. The
# claims: every frame comes out, at the quality and bitrate asked for, with
# keyframes where they were asked for; the encoder takes the layouts those
# clients actually hand it (odd sizes, planar YUV, frames at their own
# strides); B-frames and HEVC Main 10 work on request; and the Mac pays a
# fraction of software's CPU for it.
#
# Synthetic sources, generated in the container: the claim is about the
# pipeline, not the content.
set -euo pipefail
if ! command -v cargo >/dev/null 2>&1; then
	. "$HOME/.cargo/env"
fi
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
KERNEL="${LIGHTER_GATE_KERNEL:-guest/out/Image}"
ROOTFS_MASTER="guest/out/rootfs.ext4"
ROOTFS="$(mktemp -t lighter-rootfs).ext4"
cp -c "$ROOTFS_MASTER" "$ROOTFS" 2>/dev/null || cp "$ROOTFS_MASTER" "$ROOTFS"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/lighter-bench"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-120}"
BOOKWORM="${LIGHTER_GATE_BOOKWORM_IMAGE:-debian:bookworm-slim}"
TRIXIE="${LIGHTER_GATE_TRIXIE_IMAGE:-debian:trixie-slim}"
# testsrc2 at 1080p and 8 Mbps encodes at 40-42 dB on both codecs; a wrong
# stride, a dropped or repeated frame, or chroma in the wrong place reads
# in the single digits or teens.
MIN_PSNR="${LIGHTER_GATE_MIN_PSNR:-36}"
# Rate control over four seconds, container overhead included.
BITRATE_TOLERANCE="${LIGHTER_GATE_BITRATE_TOLERANCE:-0.15}"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ] || ./guest/kernel/build.sh
[ -f "$ROOTFS_MASTER" ] || ./guest/rootfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m14)"
SOCKET="$RUN_DIR/docker.sock"
DATA="$RUN_DIR/data.img"
LOG="$(mktemp -t lighter-m14-log)"
VMM_PID=""
export DOCKER_HOST="unix://$SOCKET" DOCKER_CONFIG="$RUN_DIR/dockercfg"
mkdir -p "$DOCKER_CONFIG"
cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR" "$ROOTFS"
}
trap cleanup EXIT

echo
echo "==> Booting the Docker guest with the video devices"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$DATA" --disk-size-gib 16 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--docker-ports "$SOCKET" \
	--no-tty --cpus 4 --memory-mib 4096 --video \
	--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s)" \
	>"$LOG" 2>&1 &
VMM_PID=$!

waited=0
while ! grep -q "AGENT listening" "$LOG" 2>/dev/null; do
	if ! kill -0 "$VMM_PID" 2>/dev/null; then
		fail "the VMM exited during boot"; tail -20 "$LOG" | sed 's/^/    /'; exit 1
	fi
	if [ "$waited" -ge "$BOOT_TIMEOUT" ]; then
		fail "the guest agent did not come up within ${BOOT_TIMEOUT}s"; tail -20 "$LOG" | sed 's/^/    /'; exit 1
	fi
	sleep 1; waited=$((waited + 1))
done
pass "guest booted (${waited}s)"
grep -q "INIT video=.*/dev/video1" "$LOG" && pass "init published /dev/video1" || fail "no encoder node at init"

# Frames a file holds, its size in bytes, and its PSNR against the source it
# was encoded from (the same testsrc2, regenerated): shell functions for the
# containers, so each check below is one line of ffmpeg and one of judging.
HELPERS='
frames() { ffprobe -v error -count_frames -select_streams v -show_entries stream=nb_read_frames -of csv=p=0 "$1"; }
keyframes() { ffprobe -v error -select_streams v -show_entries packet=flags -of csv=p=0 "$1" | grep -c K; }
bytes() { stat -c%s "$1"; }
psnr() { ffmpeg -hide_banner -i "$1" -f lavfi -i "testsrc2=size=$2:rate=30" -frames:v "$3" -lavfi "[0:v][1:v]psnr" -f null - 2>&1 | grep -o "average:[0-9.inf]*" | cut -d: -f2; }
src() { ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=$1:rate=30" -frames:v "$2" -pix_fmt "$3" "${@:4}"; }
'
at_least() { awk -v p="${1:-0}" -v m="$2" 'BEGIN{exit !(p >= m)}'; }
within() { awk -v got="${1:-0}" -v want="$2" -v tol="$BITRATE_TOLERANCE" 'BEGIN{d = got / want - 1; if (d < 0) d = -d; exit !(d <= tol)}'; }

echo
echo "==> ffmpeg 5.1 (${BOOKWORM}, --device lighter.sh/video=all)"
bw="$(docker create --device lighter.sh/video=all "$BOOKWORM" sleep infinity)"
docker start "$bw" >/dev/null
in_bw() { docker exec "$bw" bash -c "$HELPERS$1" 2>&1; }
if ! in_bw 'export DEBIAN_FRONTEND=noninteractive; apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq ffmpeg v4l-utils >/dev/null 2>&1' >/dev/null; then
	fail "the container could not install ffmpeg (network?)"
else
	out="$(in_bw '
v4l2-ctl -d /dev/video1 --info | grep -E "Driver name|Card type"
echo "OUT $(v4l2-ctl -d /dev/video1 --list-formats-out | grep -oE "'"'"'[A-Z0-9]{4}'"'"'" | tr -d "'"'"'" | tr "\n" " ")"
echo "CAP $(v4l2-ctl -d /dev/video1 --list-formats | grep -oE "'"'"'[A-Z0-9]{4}'"'"'" | tr -d "'"'"'" | tr "\n" " ")"
echo "CTRL $(v4l2-ctl -d /dev/video1 -C number_of_b_frames,sequence_header_mode,min_number_of_output_buffers 2>&1 | tr "\n" " ")"
for c in h264 hevc; do
	src 1920x1080 120 nv12 -c:v ${c}_v4l2m2m -b:v 8M -g 30 -y /tmp/$c.mp4
	echo "ENC $c $(frames /tmp/$c.mp4) $(keyframes /tmp/$c.mp4) $(bytes /tmp/$c.mp4) $(psnr /tmp/$c.mp4 1920x1080 120) $(ffprobe -v error -show_entries stream=profile -of csv=p=0 /tmp/$c.mp4)"
done
src 1000x562 60 yuv420p -c:v h264_v4l2m2m -b:v 3M -y /tmp/odd.mp4
echo "ODD $(frames /tmp/odd.mp4) $(psnr /tmp/odd.mp4 1000x562 60)"
src 1280x720 60 nv12 -c:v h264_v4l2m2m -b:v 3M -g 1000 -force_key_frames "expr:gte(t,n_forced*0.5)" -y /tmp/fk.mp4
echo "FORCED $(frames /tmp/fk.mp4) $(keyframes /tmp/fk.mp4)"
src 1920x1080 120 yuv420p -c:v libx264 -preset veryfast -bf 2 -y /tmp/in.mkv
timeout -s KILL 60 ffmpeg -hide_banner -loglevel error -c:v h264_v4l2m2m -i /tmp/in.mkv -c:v hevc_v4l2m2m -b:v 8M -y /tmp/tx.mp4
echo "TRANSCODE $(frames /tmp/tx.mp4) $(ffprobe -v error -show_entries stream=codec_name -of csv=p=0 /tmp/tx.mp4)"
')"
	echo "$out" | sed 's/^/    /'
	grep -q "Driver name *: virtio-media" <<<"$out" && grep -q "Card type *: lighter VideoToolbox encoder" <<<"$out" \
		&& pass "/dev/video1 is the VideoToolbox encoder" || fail "/dev/video1 is not the encoder"
	[ "$(awk '/^OUT/ {$1=""; print}' <<<"$out" | xargs)" = "NV12 YU12 P010" ] && pass "it takes NV12, YU12 and P010" || fail "raw formats: $(grep ^OUT <<<"$out")"
	[ "$(awk '/^CAP/ {$1=""; print}' <<<"$out" | xargs)" = "H264 HEVC" ] && pass "it makes H264 and HEVC" || fail "coded formats: $(grep ^CAP <<<"$out")"
	ctrl="$(grep ^CTRL <<<"$out")"
	if grep -q "number_of_b_frames: 0" <<<"$ctrl" && grep -q "sequence_header_mode: 1" <<<"$ctrl" && grep -q "min_number_of_output_buffers: 2" <<<"$ctrl"; then
		pass "no B-frames and headers with the keyframe unless asked; G_CTRL answers"
	else
		fail "controls: $ctrl"
	fi
	for c in h264 hevc; do
		read -r _ _ n k b p prof <<<"$(grep "^ENC $c " <<<"$out")"
		want=$((8000000 * 4 / 8))
		if [ "${n:-0}" = 120 ] && [ "${k:-0}" = 4 ] && within "$b" "$want" && at_least "$p" "$MIN_PSNR"; then
			pass "$c: 120 frames, a keyframe every 30, $(awk -v b="$b" 'BEGIN{printf "%.2f", b * 8 / 4 / 1e6}') Mbps for 8 asked, ${p} dB, $prof"
		else
			fail "$c: frames=${n:-?} keyframes=${k:-?} bytes=${b:-?} (want ~$want) psnr=${p:-?}"
		fi
	done
	read -r _ n p <<<"$(grep ^ODD <<<"$out")"
	[ "${n:-0}" = 60 ] && at_least "$p" "$MIN_PSNR" && pass "1000x562 planar YUV at ffmpeg's own strides: ${p} dB" || fail "odd size: $(grep ^ODD <<<"$out")"
	read -r _ n k <<<"$(grep ^FORCED <<<"$out")"
	[ "${n:-0}" = 60 ] && [ "${k:-0}" = 4 ] && pass "forced keyframes land where asked (4 in 2 s)" || fail "forced keyframes: $(grep ^FORCED <<<"$out")"
	[ "$(grep ^TRANSCODE <<<"$out")" = "TRANSCODE 120 hevc" ] && pass "H.264 to HEVC entirely on the media engine: 120 frames" || fail "transcode: $(grep ^TRANSCODE <<<"$out")"

	# What the Mac pays: the whole VMM's CPU time across each encode, as
	# m13 measures decode.
	vmm_cs() { ps -o cputime= -p "$VMM_PID" | awk -F'[:.]' '{ print ($1 * 60 + $2) * 100 + $3 }'; }
	measure() {
		local before after
		before="$(vmm_cs)"
		in_bw "$1" >/dev/null
		after="$(vmm_cs)"
		echo $(( (after - before) * 10 ))
	}
	in_bw 'src 1920x1080 300 nv12 -vf "noise=alls=14:allf=t+u" -f rawvideo -y /tmp/load.yuv' >/dev/null
	raw='-f rawvideo -pix_fmt nv12 -s 1920x1080 -r 30 -i /tmp/load.yuv'
	for pair in "h264:libx264 -preset veryfast" "hevc:libx265 -preset ultrafast -x265-params log-level=error"; do
		c="${pair%%:*}"; sw="${pair#*:}"
		sw_ms="$(measure "ffmpeg -hide_banner -loglevel error $raw -c:v $sw -b:v 6M -f null -")"
		hw_ms="$(measure "timeout -s KILL 120 ffmpeg -hide_banner -loglevel error $raw -c:v ${c}_v4l2m2m -b:v 6M -f null -")"
		echo "    host CPU for 300 frames of $c: software ${sw_ms} ms, VideoToolbox ${hw_ms} ms"
		echo "ENCODE_CPU codec=$c sw=$sw_ms hw=$hw_ms frames=300" >> "$LOG"
		if [ -n "$sw_ms" ] && [ -n "$hw_ms" ] && [ "$hw_ms" -gt 0 ] && awk -v h="$hw_ms" -v s="$sw_ms" 'BEGIN{exit !(h * 2 < s)}'; then
			pass "$c: the Mac pays $(awk -v h="$hw_ms" 'BEGIN{printf "%.2f", h/300}') ms of CPU a frame on the media engine against $(awk -v s="$sw_ms" 'BEGIN{printf "%.2f", s/300}') in software"
		else
			fail "$c: hardware encode is not cheaper enough: ${hw_ms:-?} ms against ${sw_ms:-?} ms"
		fi
	done
fi
docker rm -f "$bw" >/dev/null 2>&1 || true

echo
echo "==> ffmpeg 7.1 and GStreamer (${TRIXIE})"
tx="$(docker create --device lighter.sh/video=all "$TRIXIE" sleep infinity)"
docker start "$tx" >/dev/null
in_tx() { docker exec "$tx" bash -c "$HELPERS$1" 2>&1; }
if ! in_tx 'export DEBIAN_FRONTEND=noninteractive; apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq --no-install-recommends ffmpeg gstreamer1.0-tools gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-base >/dev/null 2>&1' >/dev/null; then
	fail "the container could not install ffmpeg and GStreamer (network?)"
else
	out="$(in_tx '
# ffmpeg 7.1 drains until no CAPTURE buffer is queued; with its default
# four it lost the last frames of every stream until the device granted more.
for c in h264 hevc; do
	src 1920x1080 120 nv12 -c:v ${c}_v4l2m2m -b:v 8M -y /tmp/$c.mp4
	echo "FF71 $c $(frames /tmp/$c.mp4)"
done
gst() { timeout -s KILL 60 gst-launch-1.0 -q videotestsrc num-buffers=$1 ! "video/x-raw,format=NV12,width=1280,height=720,framerate=30/1" ! $2 ! filesink location=$3 >/dev/null 2>&1; }
gst 120 "v4l2h264enc ! video/x-h264,profile=main ! h264parse ! mp4mux" /tmp/g264.mp4
echo "GST264 $(frames /tmp/g264.mp4) $(ffprobe -v error -show_entries stream=profile -of csv=p=0 /tmp/g264.mp4)"
gst 120 "v4l2h264enc extra-controls=encode,number_of_b_frames=2 ! video/x-h264,profile=high ! h264parse" /tmp/gbf.h264
echo "GSTBF $(frames /tmp/gbf.h264) $(ffprobe -v error -show_entries stream=has_b_frames -of csv=p=0 /tmp/gbf.h264)"
gst 60 "v4l2h265enc extra-controls=encode,hevc_profile=2 ! video/x-h265,profile=main-10 ! h265parse ! mp4mux" /tmp/g10.mp4
echo "GST10 $(frames /tmp/g10.mp4) $(ffprobe -v error -show_entries stream=profile,pix_fmt -of csv=p=0 /tmp/g10.mp4)"
')"
	echo "$out" | sed 's/^/    /'
	[ "$(grep -c "^FF71 .* 120$" <<<"$out")" = 2 ] && pass "ffmpeg 7.1 gets every frame of H.264 and HEVC through its drain" || fail "ffmpeg 7.1: $(grep ^FF71 <<<"$out" | tr '\n' ' ')"
	[ "$(grep ^GST264 <<<"$out")" = "GST264 120 Main" ] && pass "GStreamer: 120 frames, the profile it negotiated" || fail "GStreamer H.264: $(grep ^GST264 <<<"$out")"
	[ "$(grep ^GSTBF <<<"$out")" = "GSTBF 120 2" ] && pass "B-frames on request, with GStreamer holding every frame's buffer" || fail "GStreamer B-frames: $(grep ^GSTBF <<<"$out")"
	[ "$(grep ^GST10 <<<"$out")" = "GST10 60 Main 10,yuv420p10le" ] && pass "HEVC Main 10 on request" || fail "GStreamer Main 10: $(grep ^GST10 <<<"$out")"
fi
docker rm -f "$tx" >/dev/null 2>&1 || true

echo
exit "$FAILED"

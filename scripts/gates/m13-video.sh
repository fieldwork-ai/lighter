#!/usr/bin/env bash
# m13: a container decodes H.264 on the Mac's media engine.
#
# Boots the Docker guest with the video decoder, and in a stock Debian
# container with Debian's own ffmpeg, given `--device lighter.sh/video=all`,
# decodes a clip through `h264_v4l2m2m` (the V4L2 stateful decoder wrapper
# every distribution ships) and through software, and compares the frames.
# The claim is that the hardware path produces the same pictures as the
# software one, all of them, in order, and costs the container a fraction
# of the CPU.
#
# The clip is made here, on the host's ffmpeg, from a synthetic source: a
# few seconds of 1080p with B-frames, so reordering is exercised, in
# Matroska so every packet carries its timestamp, as a camera's RTSP does
# (from a raw .h264 file ffmpeg sends 0 for all of them and then drops
# every frame but the first as a duplicate).
set -euo pipefail
if ! command -v cargo >/dev/null 2>&1; then
	. "$HOME/.cargo/env"
fi
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
KERNEL="${LIGHTER_GATE_KERNEL:-guest/out/Image}"
ROOTFS_MASTER="guest/out/rootfs.ext4"
ROOTFS_DIR="$(mktemp -d -t lighter-rootfs)"
ROOTFS="$ROOTFS_DIR/rootfs.ext4"
cp -c "$ROOTFS_MASTER" "$ROOTFS" 2>/dev/null || cp "$ROOTFS_MASTER" "$ROOTFS"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/lighter-bench"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-120}"
IMAGE="${LIGHTER_GATE_FFMPEG_IMAGE:-debian:bookworm-slim}"
# 40 dB is well past what two decoders of the same stream differ by (they
# differ only in rounding); a wrong frame, a dropped one or a stride slip
# reads in the teens.
MIN_PSNR="${LIGHTER_GATE_MIN_PSNR:-40}"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
command -v ffmpeg >/dev/null 2>&1 || { echo "ffmpeg is required on the host to make the clip (brew install ffmpeg)" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "python3 is required to make the PPS fixture" >&2; exit 1; }

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ] || ./guest/kernel/build.sh
[ -f "$ROOTFS_MASTER" ] || ./guest/rootfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m13)"
SOCKET="$RUN_DIR/docker.sock"
DATA="$RUN_DIR/data.img"
LOG="$(mktemp -t lighter-m13-log)"
VMM_PID=""
export DOCKER_HOST="unix://$SOCKET" DOCKER_CONFIG="$RUN_DIR/dockercfg"
mkdir -p "$DOCKER_CONFIG"
cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR" "$ROOTFS_DIR"
}
trap cleanup EXIT
trap 'exit 143' INT TERM

echo
echo "==> A clip: 1080p, 4 s, H.264 with B-frames, from the host's ffmpeg"
CLIP="$RUN_DIR/clip.mkv"
ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1920x1080:rate=30" -t 4 \
	-c:v libx264 -preset veryfast -pix_fmt yuv420p -g 30 -bf 2 -threads 1 \
	"$CLIP" 2>&1 | sed 's/^/    /' || true
[ -s "$CLIP" ] && pass "clip: $(du -k "$CLIP" | cut -f1) KiB" || { fail "no clip"; exit 1; }
# And one for the cost: ten seconds with a camera's grain and bitrate
# (6 Mbps), because a synthetic pattern decodes for next to nothing and
# would flatter whichever path is slower.
LOAD="$RUN_DIR/load.mkv"
ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1920x1080:rate=30" -t 10 \
	-vf "noise=alls=14:allf=t+u" -c:v libx264 -preset veryfast -b:v 6M -maxrate 6M -bufsize 12M \
	-pix_fmt yuv420p -g 30 -bf 2 -threads 2 "$LOAD" 2>&1 | sed 's/^/    /' || true
[ -s "$LOAD" ] && pass "load clip: $(du -k "$LOAD" | cut -f1) KiB, 300 frames" || { fail "no load clip"; exit 1; }
# And one shaped like a camera's: no B-frames, so nothing is left to reorder
# when the stream ends and the LAST buffer goes out inside the drain itself.
# A decoder that then never reports the queue readable hangs ffmpeg forever.
CAM="$RUN_DIR/cam.mkv"
ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1280x720:rate=20" -t 3 \
	-c:v libx264 -preset veryfast -pix_fmt yuv420p -g 40 -bf 0 -threads 1 "$CAM" 2>&1 | sed 's/^/    /' || true
[ -s "$CAM" ] && pass "camera-shaped clip: no B-frames, 60 frames" || { fail "no camera clip"; exit 1; }
# Raw Annex B for the STREAMOFF client, which feeds access units itself.
RACE="$RUN_DIR/race.h264"
ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=640x368:rate=20" -t 3 \
	-c:v libx264 -preset veryfast -pix_fmt yuv420p -g 40 -bf 0 -threads 1 -f h264 "$RACE" 2>&1 | sed 's/^/    /' || true
[ -s "$RACE" ] && pass "raw clip for the STREAMOFF client" || { fail "no raw clip"; exit 1; }
# Generate 180 x264 I/P pictures. Insert a PPS before picture 3 that changes
# only pic_init_qs_minus26, which these pictures do not use. Software output
# must remain identical; the VideoToolbox path must preserve all 180 frames.
PPS_RAW="$RUN_DIR/pps-original.h264"; PPS_UPDATED="$RUN_DIR/pps-updated.h264"
PPS_CLIP="$RUN_DIR/pps.mkv"
ffmpeg -nostdin -hide_banner -loglevel error -y -f lavfi -i "testsrc2=size=1920x1080:rate=30" -frames:v 180 -c:v libx264 -preset medium -threads 4 -bf 0 -g 60 -b:v 6M -f h264 "$PPS_RAW"
python3 scripts/gates/fixtures/x264-pps-variation.py "$PPS_RAW" "$PPS_UPDATED"
ffmpeg -nostdin -hide_banner -loglevel error -y -i "$PPS_RAW" -pix_fmt yuv420p -f framemd5 "$RUN_DIR/pps-original.framemd5"
ffmpeg -nostdin -hide_banner -loglevel error -y -i "$PPS_UPDATED" -pix_fmt yuv420p -f framemd5 "$RUN_DIR/pps-updated.framemd5"
cmp "$RUN_DIR/pps-original.framemd5" "$RUN_DIR/pps-updated.framemd5" ||
	{ fail "PPS fixture changes software-decoded pictures"; exit 1; }
ffmpeg -nostdin -hide_banner -loglevel error -y -f h264 -r 30 -i "$PPS_UPDATED" -c:v copy "$PPS_CLIP"
pass "x264 clip: compatible PPS update before picture 3, 180 frames"
# Switch to a second PPS ID, and separately to a second SPS/PPS pair, then
# return to the original pair without reannouncing it. Both streams must
# decode to the same pictures as their unmodified source.
IDS_RAW="$RUN_DIR/ids-original.h264"
ffmpeg -nostdin -hide_banner -loglevel error -y -f lavfi -i "testsrc2=size=640x360:rate=30" -frames:v 90 -c:v libx264 -preset medium -threads 1 -profile:v baseline -bf 0 -g 90 -b:v 2M -f h264 "$IDS_RAW"
ffmpeg -nostdin -hide_banner -loglevel error -y -f h264 -r 30 -i "$IDS_RAW" -pix_fmt yuv420p -f framemd5 "$RUN_DIR/ids-original.md5"
for mode in pps-id sps-id; do
	modified="$RUN_DIR/$mode.h264"
	python3 scripts/gates/fixtures/x264-pps-variation.py "$IDS_RAW" "$modified" "$mode"
	ffmpeg -nostdin -hide_banner -loglevel error -y -f h264 -r 30 -i "$modified" -pix_fmt yuv420p -f framemd5 "$RUN_DIR/$mode.md5"
	cmp -s "$RUN_DIR/ids-original.md5" "$RUN_DIR/$mode.md5" ||
		{ fail "$mode fixture changes software-decoded pictures"; exit 1; }
	ffmpeg -nostdin -hide_banner -loglevel error -y -f h264 -r 30 -i "$modified" -c:v copy "$RUN_DIR/$mode.mkv"
	pass "$mode clip: 90 unchanged pictures across an ID switch"
done
# Remove the VUI (and therefore the reorder bound) before picture 3 of a
# Main-profile B-frame stream. Its coded pictures and reference chain stay
# unchanged, and the same VT session must continue ordering those pictures.
ffmpeg -nostdin -hide_banner -loglevel error -y -f lavfi -i "testsrc2=size=640x360:rate=30" -frames:v 90 -c:v libx264 -preset medium -threads 1 -profile:v main -bf 2 -g 90 -x264-params "b-adapt=0:scenecut=0" -f h264 "$RUN_DIR/reorder-original.h264"
python3 scripts/gates/fixtures/x264-pps-variation.py "$RUN_DIR/reorder-original.h264" "$RUN_DIR/no-reorder-bound.h264" no-reorder-bound
# MP4 accepts the raw B-frame stream's DTS-only timing; no edit list may
# trim its leading pictures. The decode check disables frame-rate conversion.
ffmpeg -nostdin -hide_banner -loglevel error -y -r 30 -i "$RUN_DIR/no-reorder-bound.h264" -c:v copy -use_editlist 0 "$RUN_DIR/no-reorder-bound.mp4"
ffmpeg -nostdin -hide_banner -loglevel error -y -i "$RUN_DIR/reorder-original.h264" -pix_fmt yuv420p -f framemd5 "$RUN_DIR/reorder-original.md5"

# HEVC: eight bits with B-frames, ten bits, and a camera-shaped one without.
HEVC8="$RUN_DIR/hevc8.mkv"; HEVC10="$RUN_DIR/hevc10.mkv"; HEVCCAM="$RUN_DIR/hevccam.mkv"
ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1280x720:rate=30" -t 2 \
	-c:v libx265 -preset fast -x265-params "bframes=3:keyint=30:log-level=error" -pix_fmt yuv420p "$HEVC8" 2>&1 | sed 's/^/    /' || true
ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1280x720:rate=30" -t 2 \
	-c:v libx265 -preset fast -x265-params "bframes=3:keyint=30:log-level=error" -pix_fmt yuv420p10le "$HEVC10" 2>&1 | sed 's/^/    /' || true
ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1280x720:rate=20" -t 3 \
	-c:v libx265 -preset fast -x265-params "bframes=0:keyint=40:log-level=error" -pix_fmt yuv420p "$HEVCCAM" 2>&1 | sed 's/^/    /' || true
# VP9: eight bits with hidden reference frames (superframes), and profile 2.
VP98="$RUN_DIR/vp9_8.webm"; VP910="$RUN_DIR/vp9_10.webm"
ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1280x720:rate=30" -t 2 \
	-c:v libvpx-vp9 -b:v 2M -deadline good -cpu-used 4 -auto-alt-ref 1 -lag-in-frames 16 -g 30 "$VP98" 2>&1 | sed 's/^/    /' || true
ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1280x720:rate=30" -t 2 \
	-c:v libvpx-vp9 -b:v 2M -deadline good -cpu-used 4 -pix_fmt yuv420p10le -profile:v 2 -g 30 "$VP910" 2>&1 | sed 's/^/    /' || true
[ -s "$VP98" ] && [ -s "$VP910" ] && pass "VP9 clips: 8-bit with superframes, 10-bit profile 2" || { fail "no VP9 clips (ffmpeg with libvpx needed)"; exit 1; }
[ -s "$HEVC8" ] && [ -s "$HEVC10" ] && [ -s "$HEVCCAM" ] && pass "HEVC clips: 8-bit and 10-bit with B-frames, a camera-shaped one" || { fail "no HEVC clips (ffmpeg with libx265 needed)"; exit 1; }

echo
echo "==> Booting the Docker guest with the video decoder"
LIGHTER_LOG=info,lighter_vmm::video=debug "$BIN" \
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
grep -q "INIT video=/dev/video0" "$LOG" && pass "init published /dev/video0" || fail "no video node at init"

echo
echo "==> The decoder, from a container (${IMAGE}, --device lighter.sh/video=all)"
container="$(docker create --device lighter.sh/video=all "$IMAGE" sleep infinity)"
docker cp "$CLIP" "$container:/clip.mkv" >/dev/null
docker cp "$LOAD" "$container:/load.mkv" >/dev/null
docker cp "$CAM" "$container:/cam.mkv" >/dev/null
docker cp "$HEVC8" "$container:/hevc8.mkv" >/dev/null
docker cp "$HEVC10" "$container:/hevc10.mkv" >/dev/null
docker cp "$HEVCCAM" "$container:/hevccam.mkv" >/dev/null
docker cp "$VP98" "$container:/vp9_8.webm" >/dev/null
docker cp "$VP910" "$container:/vp9_10.webm" >/dev/null
docker cp "$RACE" "$container:/race.h264" >/dev/null
docker cp "$PPS_CLIP" "$container:/pps.mkv" >/dev/null
for mode in pps-id sps-id; do
	docker cp "$RUN_DIR/$mode.mkv" "$container:/$mode.mkv" >/dev/null
done
for fixture in no-reorder-bound.mp4 reorder-original.md5; do
	docker cp "$RUN_DIR/$fixture" "$container:/$fixture" >/dev/null
done
docker cp "$ROOT/scripts/gates/fixtures/v4l2-streamoff-race.py" "$container:/race.py" >/dev/null
docker start "$container" >/dev/null
in_container() { docker exec "$container" bash -c "$1" 2>&1; }
if ! in_container 'export DEBIAN_FRONTEND=noninteractive; apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq ffmpeg v4l-utils python3 >/dev/null 2>&1' >/dev/null; then
	fail "the container could not install ffmpeg (network?)"
else
	out="$(in_container '
v4l2-ctl -d /dev/video0 --info 2>&1 | grep -E "Driver name|Card type"
v4l2-ctl -d /dev/video0 --list-formats-out 2>&1 | grep -E "\[0\]"
v4l2-ctl -d /dev/video0 --list-formats 2>&1 | grep -E "\[0\]"
ffmpeg -hide_banner -loglevel error -i /clip.mkv -f rawvideo -pix_fmt yuv420p /tmp/sw.yuv
timeout -s KILL 120 ffmpeg -hide_banner -loglevel error -c:v h264_v4l2m2m -i /clip.mkv -f rawvideo -pix_fmt yuv420p /tmp/hw.yuv
ls -l /tmp/sw.yuv /tmp/hw.yuv | awk "{print \"BYTES\", \$5, \$9}"
ffmpeg -hide_banner -loglevel error -f rawvideo -pix_fmt yuv420p -s 1920x1080 -i /tmp/hw.yuv -f rawvideo -pix_fmt yuv420p -s 1920x1080 -i /tmp/sw.yuv -lavfi "psnr=stats_file=/tmp/psnr.log" -f null - 2>&1
awk "{for(i=1;i<=NF;i++) if(\$i ~ /^psnr_avg:/){split(\$i,a,\":\"); if(a[2]==\"inf\") v=99; else v=a[2]+0; if(n==0||v<min)min=v; n++}} END{printf \"PSNR frames=%d min=%.2f\\n\", n, min}" /tmp/psnr.log
rm -f /tmp/sw.yuv /tmp/hw.yuv')"
	echo "$out" | sed 's/^/    /'
	grep -q "Driver name *: virtio-media" <<<"$out" && pass "the node is virtio-media" || fail "the node is not virtio-media"
	grep -q "H264" <<<"$out" && pass "it takes H264 in" || fail "no H264 input format"
	grep -q "NV12" <<<"$out" && pass "it gives NV12 out" || fail "no NV12 output format"
	sw_bytes="$(awk '/^BYTES/ && /sw.yuv/ {print $2}' <<<"$out")"
	hw_bytes="$(awk '/^BYTES/ && /hw.yuv/ {print $2}' <<<"$out")"
	if [ -n "$hw_bytes" ] && [ "$hw_bytes" = "$sw_bytes" ] && [ "$hw_bytes" -gt 0 ]; then
		pass "the hardware path produced every frame ($((hw_bytes / 3110400)) of them)"
	else
		fail "frame count differs: hardware ${hw_bytes:-0} bytes, software ${sw_bytes:-0}"
	fi
	psnr="$(awk '/^PSNR/ {print $3}' <<<"$out" | cut -d= -f2)"
	if [ -n "$psnr" ] && awk -v p="$psnr" -v m="$MIN_PSNR" 'BEGIN{exit !(p >= m)}'; then
		pass "every frame matches software decode (worst PSNR ${psnr} dB, floor ${MIN_PSNR})"
	else
		fail "frames differ from software decode (worst PSNR ${psnr:-?} dB, floor ${MIN_PSNR})"
	fi

	cam="$(in_container 'timeout -s KILL 30 ffmpeg -hide_banner -loglevel error -c:v h264_v4l2m2m -i /cam.mkv -f framemd5 /tmp/hw.md5 >/dev/null 2>&1; echo "exit=$?"; grep -vc "^#" /tmp/hw.md5')"
	if [ "$(head -1 <<<"$cam")" = "exit=0" ] && [ "$(tail -1 <<<"$cam")" = 60 ]; then
		pass "a stream with nothing to reorder decodes to its end and exits"
	else
		fail "the camera-shaped clip did not finish: $(tr '\n' ' ' <<<"$cam")"
	fi

	pps="$(in_container '
timeout -s KILL 90 ffmpeg -nostdin -y -hide_banner -loglevel error \
    -i /pps.mkv -pix_fmt yuv420p -f framemd5 /tmp/pps-sw.md5
sw_exit=$?
timeout -s KILL 90 ffmpeg -nostdin -y -hide_banner -loglevel error \
    -c:v h264_v4l2m2m -i /pps.mkv -pix_fmt yuv420p \
    -f framemd5 /tmp/pps-hw.md5
hw_exit=$?
grep -v "^#" /tmp/pps-sw.md5 > /tmp/pps-sw.frames
grep -v "^#" /tmp/pps-hw.md5 > /tmp/pps-hw.frames
if cmp -s /tmp/pps-sw.frames /tmp/pps-hw.frames; then match=same; else match=differ; fi
echo "PPS_RESULT $sw_exit $hw_exit $(wc -l < /tmp/pps-hw.frames) $match"
')"
	echo "$pps" | sed 's/^/    /'
	if [ "$(awk '/^PPS_RESULT/ {print $2, $3, $4, $5}' <<<"$pps")" = "0 0 180 same" ]; then
		pass "H.264 PPS update: all 180 frames match software decode"
	else
		fail "H.264 PPS update lost or changed frames: $(grep '^PPS_RESULT' <<<"$pps")"
	fi
	for mode in pps-id sps-id; do
		ids="$(in_container "
mode=$mode
timeout -s KILL 90 ffmpeg -nostdin -y -hide_banner -loglevel error -i /\$mode.mkv -pix_fmt yuv420p -f framemd5 /tmp/ids-sw.md5
sw_exit=\$?
timeout -s KILL 90 ffmpeg -nostdin -y -hide_banner -loglevel error -c:v h264_v4l2m2m -i /\$mode.mkv -pix_fmt yuv420p -f framemd5 /tmp/ids-hw.md5
hw_exit=\$?
grep -v '^#' /tmp/ids-sw.md5 > /tmp/ids-sw.frames
grep -v '^#' /tmp/ids-hw.md5 > /tmp/ids-hw.frames
if cmp -s /tmp/ids-sw.frames /tmp/ids-hw.frames; then match=same; else match=differ; fi
echo IDS_RESULT \$sw_exit \$hw_exit \$(wc -l < /tmp/ids-hw.frames) \$match
")"
		echo "$ids" | sed 's/^/    /'
		if [ "$(awk '/^IDS_RESULT/ {print $2, $3, $4, $5}' <<<"$ids")" = "0 0 90 same" ]; then
			pass "H.264 $mode switch: all 90 frames match software decode"
		else
			fail "H.264 $mode switch lost or changed frames: $(grep '^IDS_RESULT' <<<"$ids")"
		fi
	done

	# Compare ordered picture hashes to the original source; the MP4's raw
	# stream timestamps must not cause frame-rate conversion or lost pictures.
	edge="$(in_container '
timeout -s KILL 60 ffmpeg -nostdin -y -hide_banner -loglevel error -i /no-reorder-bound.mp4 -vsync 0 -pix_fmt yuv420p -f framemd5 /tmp/edge-sw.md5
sw_exit=$?
timeout -s KILL 60 ffmpeg -nostdin -y -hide_banner -loglevel error -c:v h264_v4l2m2m -i /no-reorder-bound.mp4 -vsync 0 -pix_fmt yuv420p -f framemd5 /tmp/edge-hw.md5
hw_exit=$?
grep -v "^#" /tmp/edge-sw.md5 | awk -F, "{print \$NF}" > /tmp/edge-sw.frames
grep -v "^#" /tmp/edge-hw.md5 | awk -F, "{print \$NF}" > /tmp/edge-hw.frames
grep -v "^#" /reorder-original.md5 | awk -F, "{print \$NF}" > /tmp/edge-expected.frames
match=differ
if cmp -s /tmp/edge-sw.frames /tmp/edge-expected.frames && cmp -s /tmp/edge-hw.frames /tmp/edge-expected.frames; then match=same; fi
echo "EDGE_RESULT $sw_exit $hw_exit $(wc -l < /tmp/edge-hw.frames) $match"
')"
	echo "$edge" | sed 's/^/    /'
	if [ "$(awk '/^EDGE_RESULT/ {print $2, $3, $4, $5}' <<<"$edge")" = "0 0 90 same" ]; then
		pass "H.264 no-reorder-bound: all 90 frames match the source and software decode"
	else
		fail "H.264 no-reorder-bound: output differs: $(grep '^EDGE_RESULT' <<<"$edge")"
	fi

	# HEVC: every frame of the 8-bit clip identical to software decode; the
	# 10-bit one through NV12 (the top eight bits, which is what ffmpeg's
	# V4L2 wrapper takes), within rounding of software's own conversion.
	hevc="$(in_container '
cmpmd5() { grep -v "^#" $1 | awk -F, "{print \$NF}" > /tmp/a; grep -v "^#" $2 | awk -F, "{print \$NF}" > /tmp/b; echo "$(wc -l < /tmp/a) $(cmp -s /tmp/a /tmp/b && echo same || echo differ)"; }
ffmpeg -y -hide_banner -loglevel error -i /hevc8.mkv -f framemd5 -pix_fmt nv12 /tmp/sw.md5
timeout -s KILL 60 ffmpeg -y -hide_banner -loglevel error -c:v hevc_v4l2m2m -i /hevc8.mkv -f framemd5 -pix_fmt nv12 /tmp/hw.md5 >/dev/null 2>&1
echo "HEVC8 $(cmpmd5 /tmp/sw.md5 /tmp/hw.md5)"
ffmpeg -y -hide_banner -loglevel error -i /hevc10.mkv -f rawvideo -pix_fmt nv12 /tmp/sw.yuv
timeout -s KILL 60 ffmpeg -y -hide_banner -loglevel error -c:v hevc_v4l2m2m -i /hevc10.mkv -f rawvideo -pix_fmt nv12 /tmp/hw.yuv >/dev/null 2>&1
ffmpeg -hide_banner -loglevel error -f rawvideo -pix_fmt nv12 -s 1280x720 -i /tmp/hw.yuv -f rawvideo -pix_fmt nv12 -s 1280x720 -i /tmp/sw.yuv -lavfi "psnr=stats_file=/tmp/p10.log" -f null - 2>&1
awk "{for(i=1;i<=NF;i++) if(\$i ~ /^psnr_avg:/){split(\$i,a,\":\"); v=(a[2]==\"inf\")?99:a[2]+0; if(n==0||v<min)min=v; n++}} END{printf \"HEVC10 %d %.2f\n\", n, min}" /tmp/p10.log
timeout -s KILL 30 ffmpeg -hide_banner -loglevel error -c:v hevc_v4l2m2m -i /hevccam.mkv -f framemd5 /tmp/hc.md5 >/dev/null 2>&1; echo "HEVCCAM $? $(grep -vc "^#" /tmp/hc.md5)"
rm -f /tmp/sw.yuv /tmp/hw.yuv')"
	echo "$hevc" | sed 's/^/    /'
	[ "$(awk '/^HEVC8/ {print $2, $3}' <<<"$hevc")" = "60 same" ] && pass "HEVC 8-bit: every frame identical to software decode" || fail "HEVC 8-bit differs from software decode"
	h10="$(awk '/^HEVC10/ {print $3}' <<<"$hevc")"
	if [ "$(awk '/^HEVC10/ {print $2}' <<<"$hevc")" = 60 ] && awk -v p="${h10:-0}" -v m="$MIN_PSNR" 'BEGIN{exit !(p >= m)}'; then
		pass "HEVC 10-bit: 60 frames, worst PSNR ${h10} dB against software's own 10-to-8"
	else
		fail "HEVC 10-bit: $(grep HEVC10 <<<"$hevc")"
	fi
	[ "$(awk '/^HEVCCAM/ {print $2, $3}' <<<"$hevc")" = "0 60" ] && pass "HEVC with nothing to reorder decodes to its end and exits" || fail "HEVC camera-shaped clip: $(grep HEVCCAM <<<"$hevc")"

	# VP9, and AV1 refused: no Linux client drives a V4L2 AV1 decoder, so
	# the device neither lists it nor takes it.
	vp9="$(in_container '
cmpmd5() { grep -v "^#" $1 | awk -F, "{print \$NF}" > /tmp/a; grep -v "^#" $2 | awk -F, "{print \$NF}" > /tmp/b; echo "$(wc -l < /tmp/a) $(cmp -s /tmp/a /tmp/b && echo same || echo differ)"; }
ffmpeg -y -hide_banner -loglevel error -i /vp9_8.webm -f framemd5 -pix_fmt nv12 /tmp/sw.md5
timeout -s KILL 60 ffmpeg -y -hide_banner -loglevel error -c:v vp9_v4l2m2m -i /vp9_8.webm -f framemd5 -pix_fmt nv12 /tmp/hw.md5 >/dev/null 2>&1
echo "VP98 $(cmpmd5 /tmp/sw.md5 /tmp/hw.md5)"
ffmpeg -y -hide_banner -loglevel error -i /vp9_10.webm -f rawvideo -pix_fmt nv12 /tmp/sw.yuv
timeout -s KILL 60 ffmpeg -y -hide_banner -loglevel error -c:v vp9_v4l2m2m -i /vp9_10.webm -f rawvideo -pix_fmt nv12 /tmp/hw.yuv >/dev/null 2>&1
ffmpeg -hide_banner -loglevel error -f rawvideo -pix_fmt nv12 -s 1280x720 -i /tmp/hw.yuv -f rawvideo -pix_fmt nv12 -s 1280x720 -i /tmp/sw.yuv -lavfi "psnr=stats_file=/tmp/v10.log" -f null - 2>&1
awk "{for(i=1;i<=NF;i++) if(\$i ~ /^psnr_avg:/){split(\$i,a,\":\"); v=(a[2]==\"inf\")?99:a[2]+0; if(n==0||v<min)min=v; n++}} END{printf \"VP910 %d %.2f\n\", n, min}" /tmp/v10.log
echo "FORMATS $(v4l2-ctl -d /dev/video0 --list-formats-out 2>/dev/null | grep -oE "'"'"'[A-Z0-9]{4}'"'"'" | tr -d "'"'"'" | tr "\n" " ")"
echo "AV1TRY $(v4l2-ctl -d /dev/video0 --try-fmt-video-out pixelformat=AV01 2>&1 | grep -c "is invalid")"
rm -f /tmp/sw.yuv /tmp/hw.yuv')"
	echo "$vp9" | sed 's/^/    /'
	[ "$(awk '/^VP98/ {print $2, $3}' <<<"$vp9")" = "60 same" ] && pass "VP9 8-bit: every frame identical to software decode" || fail "VP9 8-bit differs from software decode"
	v10="$(awk '/^VP910/ {print $3}' <<<"$vp9")"
	if [ "$(awk '/^VP910/ {print $2}' <<<"$vp9")" = 60 ] && awk -v p="${v10:-0}" -v m="$MIN_PSNR" 'BEGIN{exit !(p >= m)}'; then
		pass "VP9 10-bit: 60 frames, worst PSNR ${v10} dB against software's own 10-to-8"
	else
		fail "VP9 10-bit: $(grep VP910 <<<"$vp9")"
	fi
	formats="$(awk '/^FORMATS/ {$1=""; print}' <<<"$vp9")"
	if grep -qw H264 <<<"$formats" && grep -qw HEVC <<<"$formats" && grep -qw VP90 <<<"$formats" && ! grep -qw AV01 <<<"$formats" && [ "$(awk '/^AV1TRY/ {print $2}' <<<"$vp9")" = 1 ]; then
		pass "it offers H264, HEVC and VP9 and refuses AV1 ($formats)"
	else
		fail "formats offered: $formats; AV1 try: $(grep AV1TRY <<<"$vp9")"
	fi

	# What the Mac pays: the whole VMM's CPU time across each decode. The
	# decoder runs inside the VMM, so the container's own CPU time would
	# hide half of the hardware path's cost and count it as the guest's.
	vmm_cs() { ps -o cputime= -p "$VMM_PID" | awk -F'[:.]' '{ print ($1 * 60 + $2) * 100 + $3 }'; }
	measure() {
		local before after
		before="$(vmm_cs)"
		in_container "$1" >/dev/null
		after="$(vmm_cs)"
		echo $(( (after - before) * 10 ))
	}
	measure 'ffmpeg -hide_banner -loglevel error -i /load.mkv -f null -' >/dev/null
	sw_ms="$(measure 'ffmpeg -hide_banner -loglevel error -i /load.mkv -f null -')"
	hw_ms="$(measure 'timeout -s KILL 120 ffmpeg -hide_banner -loglevel error -c:v h264_v4l2m2m -i /load.mkv -f null -')"
	frames=300
	echo "    host CPU for ${frames} frames: software ${sw_ms} ms, VideoToolbox ${hw_ms} ms"
	echo "DECODE_CPU sw=$sw_ms hw=$hw_ms frames=$frames" >> "$LOG"
	if [ -n "$sw_ms" ] && [ -n "$hw_ms" ] && [ "$hw_ms" -gt 0 ] && awk -v h="$hw_ms" -v s="$sw_ms" 'BEGIN{exit !(h * 2 < s)}'; then
		pass "the Mac pays $(awk -v h="$hw_ms" -v f="$frames" 'BEGIN{printf "%.2f", h/f}') ms of CPU a frame on the media engine against $(awk -v s="$sw_ms" -v f="$frames" 'BEGIN{printf "%.2f", s/f}') in software"
	else
		fail "hardware decode is not cheaper enough: ${hw_ms:-?} ms against ${sw_ms:-?} ms for ${frames} frames"
	fi

	# Sixteen clients turning CAPTURE off and on while frames complete, with
	# the vCPUs saturated. Before kernel patch 0042 a completion handled
	# after its STREAMOFF (events wait for the guest's sixteen event buffers,
	# replies do not) put a buffer on the done list twice, and the next DQBUF
	# oopsed holding the device. That takes an event backlog at the wrong
	# moment and this does not reproduce it reliably; it guards the path.
	race="$(in_container '
for i in 1 2 3 4; do timeout -s KILL 50 sh -c "while :; do :; done" & done
for i in $(seq 16); do timeout -s KILL 90 python3 /race.py /race.h264 45 640 368 > /tmp/race$i.log 2>&1 & done
wait 2>/dev/null
echo "RACE $(grep -h "^frames" /tmp/race*.log | wc -l) $(grep -h "^frames" /tmp/race*.log | awk "{c+=\$5} END {print c+0}")"')"
	clients="$(awk '/^RACE/ {print $2}' <<<"$race")"; cycles="$(awk '/^RACE/ {print $3}' <<<"$race")"
	if grep -q "Unable to handle kernel\|virtio_media_dqbuf" "$LOG"; then
		fail "the guest oopsed in the decoder driver under STREAMOFF churn"
		grep -m3 "Unable to handle kernel\|virtio_media_dqbuf" "$LOG" | sed 's/^/    /'
	elif [ "${clients:-0}" = 16 ] && [ "${cycles:-0}" -gt 0 ]; then
		pass "16 clients, ${cycles} CAPTURE STREAMOFF cycles while decoding, no oops"
	else
		fail "STREAMOFF clients did not all finish: ${race:-nothing}"
	fi
fi
docker rm -f "$container" >/dev/null 2>&1 || true
grep -qi "video stream resolution" "$LOG" && pass "the host saw the stream's resolution" || fail "the host never saw a resolution"

echo
exit "$FAILED"

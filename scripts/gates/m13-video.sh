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
ROOTFS="$(mktemp -t lighter-rootfs).ext4"
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
	rm -rf "$RUN_DIR" "$ROOTFS"
}
trap cleanup EXIT

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

echo
echo "==> Booting the Docker guest with the video decoder"
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
grep -q "INIT video=/dev/video0" "$LOG" && pass "init published /dev/video0" || fail "no video node at init"

echo
echo "==> The decoder, from a container (${IMAGE}, --device lighter.sh/video=all)"
container="$(docker create --device lighter.sh/video=all "$IMAGE" sleep infinity)"
docker cp "$CLIP" "$container:/clip.mkv" >/dev/null
docker cp "$LOAD" "$container:/load.mkv" >/dev/null
docker cp "$CAM" "$container:/cam.mkv" >/dev/null
docker start "$container" >/dev/null
in_container() { docker exec "$container" bash -c "$1" 2>&1; }
if ! in_container 'export DEBIAN_FRONTEND=noninteractive; apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq ffmpeg v4l-utils >/dev/null 2>&1' >/dev/null; then
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
fi
docker rm -f "$container" >/dev/null 2>&1 || true
grep -qi "video stream resolution" "$LOG" && pass "the host saw the stream's resolution" || fail "the host never saw a resolution"

echo
exit "$FAILED"

#!/usr/bin/env bash
# Video on the media engine, measured: decode through /dev/video0 and
# encode through /dev/video1 in a stock Debian container, against the same
# work in software in the same container, and against VideoToolbox called
# natively on the host, which is the floor.
#
# Each case runs twice: flat out, for frames a second and the Mac's CPU per
# frame, and paced at the stream's own rate (`-re`), for the share of a core
# it costs to keep up, which is what a camera or a live transcode pays. The
# Mac's CPU is the whole VMM's for the container runs (the guest and the
# devices together) and ffmpeg's own for the native ones; the VMM's idle
# cost is measured first and printed, not subtracted.
#
# Usage: benchmarks/video.sh [output file]. Needs ffmpeg on the host (with
# libx264, libx265 and libvpx) for the clips and the native runs. Clips are
# made once, under .logs/video-clips, from synthetic sources with grain, so
# neither path is flattered by a pattern that compresses to nothing.
set -euo pipefail
if ! command -v cargo >/dev/null 2>&1; then
	. "$HOME/.cargo/env"
fi
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
OUT="${1:-.logs/video-$(hostname -s)-$(date +%Y%m%d-%H%M).txt}"
KERNEL="${LIGHTER_GATE_KERNEL:-guest/out/Image}"
ROOTFS_MASTER="guest/out/rootfs.ext4"
PROFILE="${PROFILE:-release}"
BIN="target/$PROFILE/examples/lighter-bench"
CPUS="${VIDEO_CPUS:-4}"
IMAGE="${VIDEO_IMAGE:-debian:bookworm-slim}"
CLIPS="${VIDEO_CLIPS:-.logs/video-clips}"
mkdir -p "$(dirname "$OUT")" "$CLIPS"
: > "$OUT"
say() { echo "$*" | tee -a "$OUT"; }

command -v ffmpeg >/dev/null 2>&1 || { echo "ffmpeg is required on the host (brew install ffmpeg)" >&2; exit 1; }

echo "==> Clips"
grain="noise=alls=10:allf=t+u"
clip() { # name, then ffmpeg arguments
	[ -s "$CLIPS/$1" ] && return
	ffmpeg -hide_banner -loglevel error "${@:2}" -y "$CLIPS/$1"
}
clip h264-uhd30.mkv -f lavfi -i "testsrc2=size=3840x2160:rate=30,$grain" -t 10 \
	-c:v libx264 -preset veryfast -b:v 25M -g 60 -bf 2 -pix_fmt yuv420p
clip h264-fhd60.mkv -f lavfi -i "testsrc2=size=1920x1080:rate=60,$grain" -t 10 \
	-c:v libx264 -preset veryfast -b:v 12M -g 120 -bf 2 -pix_fmt yuv420p
clip hevc10-uhd30.mkv -f lavfi -i "testsrc2=size=3840x2160:rate=30,$grain" -t 10 \
	-c:v libx265 -preset ultrafast -x265-params "log-level=error:keyint=60" -b:v 20M -pix_fmt yuv420p10le
clip vp9-fhd30.webm -f lavfi -i "testsrc2=size=1920x1080:rate=30,$grain" -t 10 \
	-c:v libvpx-vp9 -deadline realtime -cpu-used 8 -b:v 6M -g 60
# Raw sources for encoding, looped: a second of 1080p and one of 4K.
clip fhd.nv12 -f lavfi -i "testsrc2=size=1920x1080:rate=30,$grain" -frames:v 30 -pix_fmt nv12 -f rawvideo
clip uhd.nv12 -f lavfi -i "testsrc2=size=3840x2160:rate=30,$grain" -frames:v 30 -pix_fmt nv12 -f rawvideo
ls -la "$CLIPS" | sed 's/^/    /'

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm 2>&1 | tail -1
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-video)"
ROOTFS="$RUN_DIR/rootfs.ext4"
cp -c "$ROOTFS_MASTER" "$ROOTFS" 2>/dev/null || cp "$ROOTFS_MASTER" "$ROOTFS"
SOCKET="$RUN_DIR/docker.sock"
LOG="$RUN_DIR/vmm.log"
VMM_PID=""
export DOCKER_HOST="unix://$SOCKET" DOCKER_CONFIG="$RUN_DIR/dockercfg"
mkdir -p "$DOCKER_CONFIG"
cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR"
}
trap cleanup EXIT

echo "==> Booting the guest ($CPUS vCPUs) with the video devices"
"$BIN" --kernel "$KERNEL" --disk "$ROOTFS" --disk "$RUN_DIR/data.img" --disk-size-gib 16 \
	--net --run-dir "$RUN_DIR" --vsock "$SOCKET:2375" --docker-ports "$SOCKET" \
	--no-tty --cpus "$CPUS" --memory-mib 4096 --video \
	--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s)" \
	>"$LOG" 2>&1 &
VMM_PID=$!
for _ in $(seq 1 120); do grep -q "AGENT listening" "$LOG" 2>/dev/null && break; sleep 1; done
grep -q "AGENT listening" "$LOG" || { echo "the guest did not come up" >&2; tail -20 "$LOG" >&2; exit 1; }

ct="$(docker create --device lighter.sh/video=all "$IMAGE" sleep infinity)"
docker start "$ct" >/dev/null
for f in "$CLIPS"/*; do docker cp "$f" "$ct:/$(basename "$f")" >/dev/null; done
docker exec "$ct" bash -c 'export DEBIAN_FRONTEND=noninteractive; apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq ffmpeg >/dev/null 2>&1; cat /*.mkv /*.webm /*.nv12 > /dev/null'

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
# The VMM's CPU time, in milliseconds.
vmm_ms() { ps -o cputime= -p "$VMM_PID" | awk -F'[:.]' '{ print (($1 * 60 + $2) * 100 + $3) * 10 }'; }
# One container run: prints "<wall s> <Mac CPU ms>".
guest() {
	local a b s e
	a="$(vmm_ms)"; s="$(now)"
	docker exec "$ct" bash -c "$1" >/dev/null 2>&1 || echo "    (failed: $1)" >&2
	e="$(now)"; b="$(vmm_ms)"
	awk -v s="$s" -v e="$e" -v c="$((b - a))" 'BEGIN{printf "%.2f %d\n", e - s, c}'
}
# One native run, through ffmpeg's own accounting.
native() {
	local s e out
	s="$(now)"
	out="$(ffmpeg -hide_banner -benchmark "$@" -f null - 2>&1 | grep -o 'utime=[0-9.]*s stime=[0-9.]*s' | tail -1)"
	e="$(now)"
	awk -v s="$s" -v e="$e" -v o="$out" 'BEGIN{split(o, a, /[=s ]+/); printf "%.2f %d\n", e - s, (a[2] + a[4]) * 1000}'
}
# case label, frames, then "<wall> <cpu>" flat out and paced.
row() {
	local label="$1" frames="$2" fw fc pw pc
	read -r fw fc <<<"$3"; read -r pw pc <<<"$4"
	awk -v l="$label" -v n="$frames" -v fw="$fw" -v fc="$fc" -v pw="$pw" -v pc="$pc" 'BEGIN{
		printf "%-34s %6.0f fps  %6.2f ms/frame   real time: %5.1f%% of a core\n", l, n / fw, fc / n, pc / pw / 10 }' | tee -a "$OUT"
}

say "# lighter video, $(sysctl -n machdep.cpu.brand_string), $(sw_vers -productVersion), $(git log --oneline -1 | cut -c1-60), $CPUS vCPUs"
a="$(vmm_ms)"; sleep 10; b="$(vmm_ms)"
say "idle VMM: $(awk -v c="$((b - a))" 'BEGIN{printf "%.1f", c / 100}')% of a core"

say ""
say "## Decode (Mac CPU per frame flat out; share of a core at the stream's rate)"
dec() { # clip frames codec
	local c="$1" n="$2" v="$3"
	row "$c software" "$n" "$(guest "ffmpeg -loglevel error -i /$c -f null -")" "$(guest "ffmpeg -loglevel error -re -i /$c -f null -")"
	row "$c V4L2" "$n" "$(guest "ffmpeg -loglevel error -c:v ${v}_v4l2m2m -i /$c -f null -")" "$(guest "ffmpeg -loglevel error -re -c:v ${v}_v4l2m2m -i /$c -f null -")"
	row "$c native VideoToolbox" "$n" "$(native -hwaccel videotoolbox -i "$CLIPS/$c")" "$(native -hwaccel videotoolbox -re -i "$CLIPS/$c")"
}
dec h264-uhd30.mkv 300 h264
dec h264-fhd60.mkv 600 h264
dec hevc10-uhd30.mkv 300 hevc
dec vp9-fhd30.webm 300 vp9

say ""
say "## Encode, 300 frames at 30 fps (software: libx264 veryfast, libx265 ultrafast)"
enc() { # raw size codec sw bitrate
	local raw="$1" size="$2" v="$3" sw="$4" br="$5" in
	in="-f rawvideo -pix_fmt nv12 -s $size -r 30 -stream_loop -1"
	row "$v $size software" 300 "$(guest "ffmpeg -loglevel error $in -i /$raw -t 10 $sw -b:v $br -f null -")" "$(guest "ffmpeg -loglevel error $in -re -i /$raw -t 10 $sw -b:v $br -f null -")"
	row "$v $size V4L2" 300 "$(guest "ffmpeg -loglevel error $in -i /$raw -t 10 -c:v ${v}_v4l2m2m -b:v $br -f null -")" "$(guest "ffmpeg -loglevel error $in -re -i /$raw -t 10 -c:v ${v}_v4l2m2m -b:v $br -f null -")"
	# shellcheck disable=SC2086
	row "$v $size native VideoToolbox" 300 "$(native $in -i "$CLIPS/$raw" -t 10 -c:v ${v}_videotoolbox -b:v "$br")" "$(native $in -re -i "$CLIPS/$raw" -t 10 -c:v ${v}_videotoolbox -b:v "$br")"
}
enc fhd.nv12 1920x1080 h264 "-c:v libx264 -preset veryfast" 8M
enc fhd.nv12 1920x1080 hevc "-c:v libx265 -preset ultrafast -x265-params log-level=error" 6M
enc uhd.nv12 3840x2160 hevc "-c:v libx265 -preset ultrafast -x265-params log-level=error" 20M

say ""
say "## Transcode: H.264 4K30 to HEVC 1080p, 300 frames"
row "software" 300 "$(guest "ffmpeg -loglevel error -i /h264-uhd30.mkv -vf scale=1920:1080 -c:v libx265 -preset ultrafast -x265-params log-level=error -b:v 6M -f null -")" "$(guest "ffmpeg -loglevel error -re -i /h264-uhd30.mkv -vf scale=1920:1080 -c:v libx265 -preset ultrafast -x265-params log-level=error -b:v 6M -f null -")"
row "V4L2 both ways" 300 "$(guest "ffmpeg -loglevel error -c:v h264_v4l2m2m -i /h264-uhd30.mkv -vf scale=1920:1080 -c:v hevc_v4l2m2m -b:v 6M -f null -")" "$(guest "ffmpeg -loglevel error -re -c:v h264_v4l2m2m -i /h264-uhd30.mkv -vf scale=1920:1080 -c:v hevc_v4l2m2m -b:v 6M -f null -")"
row "native VideoToolbox" 300 "$(native -hwaccel videotoolbox -i "$CLIPS/h264-uhd30.mkv" -vf scale=1920:1080 -c:v hevc_videotoolbox -b:v 6M)" "$(native -hwaccel videotoolbox -re -i "$CLIPS/h264-uhd30.mkv" -vf scale=1920:1080 -c:v hevc_videotoolbox -b:v 6M)"

docker rm -f "$ct" >/dev/null 2>&1 || true
echo "==> Written to $OUT"

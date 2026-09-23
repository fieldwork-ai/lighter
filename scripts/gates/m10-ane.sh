#!/usr/bin/env bash
# m10: a container runs an ONNX model on the Mac's Neural Engine.
#
# Boots the Docker guest with the Neural Engine service, then runs a small
# CNN in a stock Python container given the CDI device `lighter.sh/ane=all`:
# ONNX Runtime in the container loads lighter's plugin provider, which sends
# the graph to the host, where ONNX Runtime's CoreML provider runs it. The
# claim is that the outputs match the container's own CPU provider.
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
# 3.11: the newest Python ONNX Runtime 1.16, the oldest the custom op serves,
# has wheels for, and what Frigate runs.
IMAGE="${LIGHTER_GATE_PYTHON_IMAGE:-python:3.11-slim}"

diff_ok() { awk -v l="$1" 'BEGIN{ if (!match(l, /max_abs_diff=[^ ]+/)) exit 1; d = substr(l, RSTART + 13, RLENGTH - 13) + 0; exit !(d < 0.01) }'; }
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
if [ "$(nm "$BIN" 2>/dev/null | grep -c OrtGetApiBase)" = 0 ]; then
	fail "the VMM was built without ONNX Runtime (host/ane/build.sh)"
	exit 1
fi

RUN_DIR="$(mktemp -d -t lighter-m10)"
SOCKET="$RUN_DIR/docker.sock"
DATA="$RUN_DIR/data.img"
LOG="$(mktemp -t lighter-m10-log)"
VMM_PID=""
export DOCKER_HOST="unix://$SOCKET" DOCKER_CONFIG="$RUN_DIR/dockercfg"
mkdir -p "$DOCKER_CONFIG"
cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR" "$ROOTFS"
}
trap cleanup EXIT

echo
echo "==> Booting the Docker guest with the Neural Engine service"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$DATA" --disk-size-gib 16 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--docker-ports "$SOCKET" \
	--no-tty --cpus 4 --memory-mib 4096 --ane \
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
grep -q "neural engine service listening" "$LOG" && pass "the host service is listening" || fail "no host service"
grep -q "INIT ane=port" "$LOG" && pass "init published the device" || fail "init did not publish the device"

# Two routes into the one library. As a plugin provider it is built against
# ONNX Runtime 1.23's API, the first with plugin providers; as a custom op it
# asks for 1.16's, so it must run on 1.16, on 1.22 (what Frigate ships) and on
# the newest. ONNX Runtime before 1.19 was built against numpy 1.
for run in "plugin onnxruntime==1.23.*" "plugin onnxruntime" \
	"op onnxruntime==1.16.* numpy<2" "op onnxruntime==1.22.*" "op onnxruntime"; do
	set -- $run
	route=$1; ort=$2; numpy=${3:-numpy}
	echo
	echo "==> An ONNX model in a container (${IMAGE}, ${ort}, ${route}, --device lighter.sh/ane=all)"
	# The fixtures go in by `docker cp`: the daemon is in the guest, so a bind
	# mount of a Mac path would need a share this machine does not have.
	container="$(docker create --device lighter.sh/ane=all -e ANE_ROUTE="$route" "$IMAGE" sh -c \
		"pip install -q '$ort' '$numpy' >/dev/null 2>&1 || exit 97; python -c 'import onnxruntime; print(\"ORT\", onnxruntime.__version__)'; python /fixtures/ane-client.py /fixtures/tinycnn.onnx 2>&1 && python /fixtures/ane-client.py /fixtures/tinycnn-init.onnx 2>&1")"
	# Two models: one with its weights as Constant nodes, one with them as
	# initializers, which is what every exporter produces and what the provider
	# once handed to the fused node as inputs (a YOLO export fell back to the
	# CPU with "the model takes 1 inputs, 197 were sent").
	docker cp "$ROOT/scripts/gates/fixtures" "$container:/fixtures" >/dev/null
	if out="$(docker start -a "$container" 2>&1)"; then
		version="$(awk '/^ORT/{print $2}' <<<"$out")"
		for which in constants initializers; do
			line="$(grep RESULT <<<"$out" | if [ "$which" = constants ]; then head -1; else tail -1; fi)"
			# fp16 on the Neural Engine against fp32 on the CPU; the fixture's
			# outputs are of order 1.
			if diff_ok "$line"; then pass "$version $route $which: $line"; else fail "$version $route $which disagrees with the CPU: $line"; fi
		done
	else
		status=$?
		if [ "$status" -eq 97 ]; then
			fail "the container could not install $ort (network?)"
		else
			fail "the client failed on $ort ($status)"; tail -15 <<<"$out" | sed 's/^/    /'
		fi
	fi
	docker rm "$container" >/dev/null 2>&1 || true
done
grep -q "neural engine model loaded" "$LOG" && pass "the host loaded the model" || fail "the host never loaded a model"

echo
echo "==> What the host costs between frames (a client at 5 Hz for 24 s)"
# A camera sends a frame every 200 ms and the Neural Engine answers in
# milliseconds; the rest of the time the host has nothing to do and must
# cost nothing. ONNX Runtime's workers spin between tasks by default, which
# read 43 to 53% of a core under Frigate on 0.7.2. The whole VMM is read,
# guest included: the client is a sleep and a small copy, so what is left
# above a few percent is the host waiting loudly.
container="$(docker create --device lighter.sh/ane=all "$IMAGE" sh -c \
	'pip install -q onnxruntime numpy >/dev/null 2>&1 || exit 97; python /fixtures/ane-client.py /fixtures/tinycnn-init.onnx paced 5 24 2>&1')"
docker cp "$ROOT/scripts/gates/fixtures" "$container:/fixtures" >/dev/null
docker start "$container" >/dev/null
waited=0
until docker logs "$container" 2>&1 | grep -q '^PACING'; do
	[ "$waited" -lt 120 ] || break
	[ "$(docker inspect -f '{{.State.Running}}' "$container" 2>/dev/null)" = true ] || break
	sleep 1; waited=$((waited + 1))
done
if docker logs "$container" 2>&1 | grep -q '^PACING'; then
	sleep 3
	# top's first sample is since the process began; the mean of the next six.
	cpu="$(top -l 7 -s 2 -pid "$VMM_PID" -stats cpu 2>/dev/null | awk '/^[0-9.]+ *$/{n++; if (n>1) {s+=$1; c++}} END{if (c) printf "%.1f", s/c}')"
	docker wait "$container" >/dev/null 2>&1 || true
	paced="$(docker logs "$container" 2>&1 | grep '^PACED' | tail -1)"
	limit="${LIGHTER_GATE_ANE_IDLE_CPU:-15}"
	if [ -n "$cpu" ] && awk -v c="$cpu" -v l="$limit" 'BEGIN{exit !(c < l)}'; then
		pass "the VMM read ${cpu}% of a core between frames (under ${limit}%; $paced)"
	else
		fail "the VMM read ${cpu:-?}% of a core between frames (limit ${limit}%; $paced)"
	fi
else
	fail "the paced client never started"; docker logs "$container" 2>&1 | tail -8 | sed 's/^/    /'
fi
docker rm -f "$container" >/dev/null 2>&1 || true

echo
exit "$FAILED"

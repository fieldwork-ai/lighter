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
IMAGE="${LIGHTER_GATE_PYTHON_IMAGE:-python:3.12-slim}"

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

echo
echo "==> An ONNX model in a container (${IMAGE}, --device lighter.sh/ane=all)"
# The fixtures go in by `docker cp`: the daemon is in the guest, so a bind
# mount of a Mac path would need a share this machine does not have.
container="$(docker create --device lighter.sh/ane=all "$IMAGE" sh -c \
	'pip install -q onnxruntime numpy >/dev/null 2>&1 || exit 97; python /fixtures/ane-client.py /fixtures/tinycnn.onnx 2>&1 && python /fixtures/ane-client.py /fixtures/tinycnn-init.onnx 2>&1')"
# Two models: one with its weights as Constant nodes, one with them as
# initializers, which is what every exporter produces and what the provider
# once handed to the fused node as inputs (a YOLO export fell back to the
# CPU with "the model takes 1 inputs, 197 were sent").
docker cp "$ROOT/scripts/gates/fixtures" "$container:/fixtures" >/dev/null
if out="$(docker start -a "$container" 2>&1)"; then
	pass "constants: $(grep RESULT <<<"$out" | head -1)"
	pass "initializers: $(grep RESULT <<<"$out" | tail -1)"
else
	status=$?
	if [ "$status" -eq 97 ]; then
		fail "the container could not install onnxruntime (network?)"
	else
		fail "the client failed ($status)"; tail -15 <<<"$out" | sed 's/^/    /'
	fi
fi
docker rm "$container" >/dev/null 2>&1 || true
grep -q "neural engine model loaded" "$LOG" && pass "the host loaded the model" || fail "the host never loaded a model"

echo
exit "$FAILED"

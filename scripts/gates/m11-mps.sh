#!/usr/bin/env bash
# m11: a container's PyTorch runs on the Mac's GPU.
#
# Starts the mps host (the Mac's own torch, in the Python that has it), boots
# the Docker guest with `lighter.mps=<port>`, and in a stock Python container
# given `--device lighter.sh/mps=all` installs torch and lighter-mps, then
# trains a small model and runs a convolution on torch.device("mps").
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
IMAGE="${LIGHTER_GATE_PYTHON_IMAGE:-python:3.12-slim}"
TORCH_VERSION="${LIGHTER_TORCH_VERSION:-2.14.0}"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0
command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }

echo "==> A Python with torch and MPS on this Mac"
# Found as lighter finds it (`mps::find_python`): the configured
# `torch_python`, else the first python3 on PATH with torch. A gate that looked
# elsewhere could pass on a Mac where lighter.sh/mps is absent.
PY="${LIGHTER_GATE_TORCH_PYTHON:-}"
if [ -z "$PY" ]; then
	configured="$(python3 -c 'import json, os, sys; print(json.load(open(sys.argv[1])).get("torch_python", ""))' \
		"${LIGHTER_HOME:-$HOME/.lighter}/config.json" 2>/dev/null || true)"
	for candidate in ${configured:-$(which -a python3 2>/dev/null)}; do
		if "$candidate" -c 'import torch; assert torch.backends.mps.is_available()' >/dev/null 2>&1; then PY="$candidate"; break; fi
	done
fi
[ -n "$PY" ] || { echo "no python3 with torch and MPS: lighter config --torch-python <path>, or LIGHTER_GATE_TORCH_PYTHON" >&2; exit 1; }
pass "$PY ($("$PY" -c 'import torch; print(torch.__version__)'))"

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ] || ./guest/kernel/build.sh
[ -f "$ROOTFS_MASTER" ] || ./guest/rootfs/build.sh
echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m11)"
SOCKET="$RUN_DIR/docker.sock"
DATA="$RUN_DIR/data.img"
LOG="$(mktemp -t lighter-m11-log)"
HOST_LOG="$RUN_DIR/mps-host.log"
VMM_PID=""; HOST_PID=""
export DOCKER_HOST="unix://$SOCKET" DOCKER_CONFIG="$RUN_DIR/dockercfg"
mkdir -p "$DOCKER_CONFIG"
cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	[ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR" "$ROOTFS_DIR"
}
trap cleanup EXIT
trap 'exit 143' INT TERM

echo
echo "==> Starting the mps host"
"$PY" guest/torch-mps/host/lighter_mps_host.py --port 0 >"$HOST_LOG" 2>&1 &
HOST_PID=$!
for _ in $(seq 1 100); do grep -q "^PORT " "$HOST_LOG" 2>/dev/null && break; sleep 0.2; done
PORT="$(sed -n 's/^PORT //p' "$HOST_LOG" | head -1)"
[ -n "$PORT" ] || { fail "the mps host did not start"; cat "$HOST_LOG" | sed 's/^/    /'; exit 1; }
pass "mps host on port $PORT"

echo "==> Booting the Docker guest"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$DATA" --disk-size-gib 16 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--docker-ports "$SOCKET" \
	--no-tty --cpus 4 --memory-mib 4096 \
	--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s) lighter.mps=$PORT" \
	>"$LOG" 2>&1 &
VMM_PID=$!
waited=0
while ! grep -q "AGENT listening" "$LOG" 2>/dev/null; do
	if ! kill -0 "$VMM_PID" 2>/dev/null; then fail "the VMM exited during boot"; tail -20 "$LOG" | sed 's/^/    /'; exit 1; fi
	if [ "$waited" -ge "$BOOT_TIMEOUT" ]; then fail "the guest did not come up within ${BOOT_TIMEOUT}s"; tail -20 "$LOG" | sed 's/^/    /'; exit 1; fi
	sleep 1; waited=$((waited + 1))
done
pass "guest booted (${waited}s)"
grep -q "INIT mps=port" "$LOG" && pass "init published the device" || fail "init did not publish the device"

echo
echo "==> PyTorch in a container (${IMAGE}, --device lighter.sh/mps=all)"
docker volume create lighter-gate-pip >/dev/null 2>&1 || true
container="$(docker create --device lighter.sh/mps=all -v lighter-gate-pip:/root/.cache/pip "$IMAGE" sh -c \
	"pip install -q --index-url https://download.pytorch.org/whl/cpu torch==$TORCH_VERSION >/dev/null 2>&1 || exit 97; pip install -q --no-index lighter-mps >/dev/null 2>&1 || exit 98; python /fixtures/mps-client.py 2>&1")"
docker cp "$ROOT/scripts/gates/fixtures" "$container:/fixtures" >/dev/null
if out="$(docker start -a "$container" 2>&1)"; then
	pass "$(grep RESULT <<<"$out" | tail -1)"
else
	status=$?
	case "$status" in
		97) fail "the container could not install torch (network?)" ;;
		98) fail "the container could not install lighter-mps from the wheels" ;;
		*) fail "the client failed ($status)"; tail -15 <<<"$out" | sed 's/^/    /' ;;
	esac
fi
docker rm "$container" >/dev/null 2>&1 || true
echo
exit "$FAILED"

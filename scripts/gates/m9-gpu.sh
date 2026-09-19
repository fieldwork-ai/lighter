#!/usr/bin/env bash
# m9: a container reaches the Mac's GPU through Vulkan.
#
# Boots the Docker guest with the GPU, then runs vulkaninfo in a stock Alpine
# container given the CDI device `lighter.sh/gpu=all`. The claim is that Mesa's
# Venus driver in the container enumerates the host's GPU, which means the
# kernel's virtio-gpu driver found the device, the capset crossed, a blob was
# mapped through the aperture, and a fenced submission completed.
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
IMAGE="${LIGHTER_GATE_VULKAN_IMAGE:-alpine:edge}"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

for tool in docker; do
	command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required" >&2; exit 1; }
done

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ] || ./guest/kernel/build.sh
[ -f "$ROOTFS_MASTER" ] || ./guest/rootfs/build.sh
if [ -z "${LIGHTER_GPU_LIBS:-}" ] && [ ! -f host/out/libvirglrenderer.a ]; then
	echo "==> Building the GPU renderer"
	host/gpu/build.sh
fi

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null
if [ "$(nm "$BIN" 2>/dev/null | grep -c virgl_renderer_init)" = 0 ]; then
	fail "the VMM was built without the renderer (host/gpu/build.sh)"
	exit 1
fi

RUN_DIR="$(mktemp -d -t lighter-m9)"
SOCKET="$RUN_DIR/docker.sock"
DATA="$RUN_DIR/data.img"
LOG="$(mktemp -t lighter-m9-log)"
VMM_PID=""
export DOCKER_HOST="unix://$SOCKET" DOCKER_CONFIG="$RUN_DIR/dockercfg"
mkdir -p "$DOCKER_CONFIG"

cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR" "$ROOTFS"
}
trap cleanup EXIT

echo
echo "==> Booting the Docker guest with a GPU"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$DATA" --disk-size-gib 16 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--docker-ports "$SOCKET" \
	--no-tty --cpus 4 --memory-mib 4096 --gpu \
	--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s)" \
	>"$LOG" 2>&1 &
VMM_PID=$!

waited=0
while ! grep -q "AGENT listening" "$LOG" 2>/dev/null; do
	if ! kill -0 "$VMM_PID" 2>/dev/null; then
		fail "the VMM exited during boot"
		tail -20 "$LOG" | sed 's/^/    /'
		exit 1
	fi
	if [ "$waited" -ge "$BOOT_TIMEOUT" ]; then
		fail "the guest agent did not come up within ${BOOT_TIMEOUT}s"
		tail -20 "$LOG" | sed 's/^/    /'
		exit 1
	fi
	sleep 1
	waited=$((waited + 1))
done
pass "guest booted (${waited}s)"

grep -q "Initialized virtio_gpu" "$LOG" && pass "the guest kernel probed virtio-gpu" || fail "virtio-gpu did not probe"
grep -q "cap set 0: id 4" "$LOG" && pass "the Venus capset crossed" || fail "no Venus capset"
grep -q "INIT gpu=/dev/dri/renderD128" "$LOG" && pass "init published the render node" || fail "no render node at init"

echo
echo "==> Vulkan in a container (${IMAGE}, --device lighter.sh/gpu=all)"
if out="$(docker run --rm --device lighter.sh/gpu=all "$IMAGE" sh -c \
	'apk add -q mesa-vulkan-virtio vulkan-loader vulkan-tools >/dev/null 2>&1 || exit 97; timeout 120 vulkaninfo --summary 2>&1' 2>&1)"; then
	if grep -q "Virtio-GPU Venus" <<<"$out"; then
		pass "vulkaninfo: $(grep -m1 deviceName <<<"$out" | sed 's/.*= //')"
		pass "driver: $(grep -m1 driverInfo <<<"$out" | sed 's/.*= //')"
	else
		fail "vulkaninfo ran but found no Venus device"
		tail -20 <<<"$out" | sed 's/^/    /'
	fi
else
	status=$?
	if [ "$status" -eq 97 ]; then
		fail "the container could not install Mesa's Venus driver (network?)"
	else
		fail "vulkaninfo failed ($status)"
		tail -20 <<<"$out" | sed 's/^/    /'
	fi
fi

if grep -qi "gpu blob\|gpu context create failed\|gpu fence could not" "$LOG"; then
	fail "the device reported a problem:"
	grep -i "gpu blob\|gpu context create failed\|gpu fence could not" "$LOG" | head -5 | sed 's/^/    /'
else
	pass "the device reported no failures"
fi

echo
exit "$FAILED"

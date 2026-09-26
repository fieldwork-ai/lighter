#!/usr/bin/env bash
# m12: a container's llama.cpp runs its layers on the Mac's GPU with ggml's
# Metal kernels, through lighter.sh/metal.
#
# Boots the Docker guest with the ggml RPC server, then runs llama-bench in a
# container whose llama.cpp was built with GGML_RPC, pointed at the server the
# CDI device names. The claim is that generation is at native Metal speed: the
# gate compares against llama-bench on the Mac itself when one is at hand
# (LIGHTER_GATE_LLAMA_BENCH), and otherwise only that the RPC path ran.
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
# llama.cpp built with GGML_RPC (and GGML_VULKAN, for the comparison), from
# scripts/gates/fixtures/llama-vulkan.Dockerfile by scripts/gates/llama-image.sh
# (LIGHTER_GATE_LLAMA_IMAGE_TAR loads a given tarball instead).
IMAGE="${LIGHTER_GATE_LLAMA_IMAGE:-llama-vulkan:arm64}"
MODEL_URL="${LIGHTER_GATE_MODEL_URL:-https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf}"
MODEL="$(basename "$MODEL_URL")"
NATIVE_BENCH="${LIGHTER_GATE_LLAMA_BENCH:-}"
NATIVE_MODEL="${LIGHTER_GATE_MODEL:-}"

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
if [ "$(nm "$BIN" 2>/dev/null | grep -c ggml_backend_rpc_start_server)" = 0 ]; then
	fail "the VMM was built without ggml (host/metal/build.sh)"
	exit 1
fi

RUN_DIR="$(mktemp -d -t lighter-m12)"
SOCKET="$RUN_DIR/docker.sock"
DATA="$RUN_DIR/data.img"
LOG="$(mktemp -t lighter-m12-log)"
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
echo "==> Booting the Docker guest with the ggml RPC server"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$DATA" --disk-size-gib 16 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--docker-ports "$SOCKET" \
	--no-tty --cpus 4 --memory-mib 4096 --metal \
	--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s)" \
	>"$LOG" 2>&1 &
VMM_PID=$!
waited=0
while ! grep -q "AGENT listening" "$LOG" 2>/dev/null; do
	if ! kill -0 "$VMM_PID" 2>/dev/null; then fail "the VMM exited during boot"; tail -20 "$LOG" | sed 's/^/    /'; exit 1; fi
	if [ "$waited" -ge "$BOOT_TIMEOUT" ]; then fail "the guest did not come up within ${BOOT_TIMEOUT}s"; tail -20 "$LOG" | sed 's/^/    /'; exit 1; fi
	sleep 1; waited=$((waited + 1))
done
pass "guest booted (${waited}s)"
# The server comes up on its own thread once Metal has compiled its
# pipelines, which on the first run after a build takes longer than the
# guest takes to boot.
for _ in $(seq 1 60); do grep -q "ggml rpc server on the Mac's GPU" "$LOG" && break; sleep 1; done
grep -q "ggml rpc server on the Mac's GPU" "$LOG" && pass "$(grep -o "ggml rpc server.*" "$LOG" | head -1 | cut -c1-100)" || fail "the ggml server did not start"
grep -q "INIT metal=port" "$LOG" && pass "init published the device" || fail "init did not publish the device"

LIGHTER_GATE_LLAMA_IMAGE="$IMAGE" scripts/gates/llama-image.sh \
	|| { fail "could not build or load $IMAGE (scripts/gates/llama-image.sh)"; exit 1; }
docker volume create models >/dev/null 2>&1 || true
docker run --rm -v models:/models "$IMAGE" sh -c "[ -f /models/$MODEL ] || curl -sL -o /models/$MODEL $MODEL_URL" >/dev/null

echo
echo "==> llama-bench in a container, layers on the Mac's GPU (--device lighter.sh/metal=all)"
if out="$(docker run --rm --device lighter.sh/metal=all -v models:/models "$IMAGE" sh -c \
	'timeout 900 llama-bench -m /models/'"$MODEL"' --rpc "$LIGHTER_METAL" -ngl 99 -p 128 -n 32 -r 3 2>&1' 2>&1)"; then
	if rows="$(grep -E '^\| qwen' <<<"$out")"; then
		pp="$(awk -F'|' '/pp128/ {gsub(/ /,"",$8); print $8}' <<<"$rows" | cut -d'±' -f1)"
		tg="$(awk -F'|' '/tg32/ {gsub(/ /,"",$8); print $8}' <<<"$rows" | cut -d'±' -f1)"
		pass "prompt ${pp} t/s, generation ${tg} t/s over RPC to Metal"
		echo "$rows" | sed 's/^/    /'
	else
		fail "llama-bench produced no rows"; tail -15 <<<"$out" | sed 's/^/    /'
	fi
else
	fail "llama-bench failed"; tail -15 <<<"$out" | sed 's/^/    /'
fi

if [ -n "$NATIVE_BENCH" ] && [ -n "$NATIVE_MODEL" ] && [ -x "$NATIVE_BENCH" ]; then
	echo
	echo "==> The same model natively on the Mac"
	native="$("$NATIVE_BENCH" -m "$NATIVE_MODEL" -ngl 99 -p 128 -n 32 -r 3 2>&1 | grep -E '^\| qwen')"
	ntg="$(awk -F'|' '/tg32/ {gsub(/ /,"",$8); print $8}' <<<"$native" | cut -d'±' -f1)"
	echo "$native" | sed 's/^/    /'
	ratio="$(python3 -c "print(int(100*${tg:-0}/max(${ntg:-1},0.001)))")"
	if [ "$ratio" -ge 80 ]; then pass "generation in the container is ${ratio}% of native Metal (floor 80%)"; else fail "generation in the container is ${ratio}% of native Metal (floor 80%)"; fi
fi
echo
exit "$FAILED"

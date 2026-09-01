#!/usr/bin/env bash
# Milestone 7 gate: x86-64 containers run.
#
# The plan wanted this done with Rosetta, which would be two to three times
# faster than what is here. It is not available to us and the reason is not a
# bug we can fix — see `docs/x86-64.md`. What this checks is the capability
# rather than the mechanism: an amd64 image runs, reports the right
# architecture, and executes a real program rather than just `uname`.
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

KERNEL="guest/out/Image"
ROOTFS="guest/out/rootfs.ext4"
GVPROXY="${GVPROXY:-vendor/gvproxy}"
PROFILE="${PROFILE:-release}"
BIN="target/$PROFILE/examples/lighter-bench"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-180}"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }
FAILED=0

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
[ -x "$GVPROXY" ] || { echo "gvproxy missing; run scripts/fetch-gvproxy.sh" >&2; exit 1; }

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ] || ./guest/kernel/build.sh
[ -f "$ROOTFS" ] || ./guest/rootfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m7)"
SOCKET="$RUN_DIR/docker.sock"
LOG="$RUN_DIR/boot.log"
VMM_PID=""

cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR"
}
trap cleanup EXIT

echo
echo "==> Booting"
: > "$LOG"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$RUN_DIR/data.img" --disk-size-gib 32 \
	--net "$GVPROXY" --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--no-tty --cpus 4 --memory-mib 4096 \
	--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s)" \
	>"$LOG" 2>&1 &
VMM_PID=$!
disown "$VMM_PID" 2>/dev/null || true

waited=0
while ! grep -q "AGENT listening" "$LOG" 2>/dev/null; do
	kill -0 "$VMM_PID" 2>/dev/null || { fail "the VMM exited during boot"; tail -20 "$LOG" | sed 's/^/    /'; exit 1; }
	[ "$waited" -lt "$BOOT_TIMEOUT" ] || { fail "the guest did not come up"; exit 1; }
	sleep 1
	waited=$((waited + 1))
done
export DOCKER_HOST="unix://$SOCKET"
pass "guest booted in ${waited}s"

if grep -q "INIT binfmt=x86_64" "$LOG"; then
	pass "the guest registered an x86-64 handler"
else
	fail "binfmt_misc was not registered"
	grep -a "INIT binfmt" "$LOG" | sed 's/^/    /'
fi

echo
echo "==> Running amd64 containers"
# Pulled by digest-free tag but with an explicit platform, and *pinned to
# different local tags*. A plain `docker pull --platform linux/amd64
# alpine:3.21` replaces whatever `alpine:3.21` meant locally, so a later
# "native" check silently runs the amd64 image and reports x86_64 — which reads
# as a broken binfmt registration rather than a broken benchmark.
docker pull --quiet --platform linux/amd64 alpine:3.21 >/dev/null 2>&1 || true
docker tag alpine:3.21 lighter-test:amd64 >/dev/null 2>&1 || true
docker pull --quiet --platform linux/arm64 alpine:3.21 >/dev/null 2>&1 || true
docker tag alpine:3.21 lighter-test:arm64 >/dev/null 2>&1 || true

arch="$(docker run --rm lighter-test:amd64 uname -m 2>/dev/null || true)"
if [ "$arch" = "x86_64" ]; then
	pass "an amd64 container reports $arch"
else
	fail "an amd64 container reported '${arch:-nothing}'"
fi

# `uname` is a syscall and would work under a handler that did nothing useful.
# A real interpreter has to translate an actual program.
docker pull --quiet --platform linux/amd64 node:24-alpine >/dev/null 2>&1 || true
answer="$(docker run --rm --platform linux/amd64 node:24-alpine \
	node -e 'const os=require("os");
	         let n=0; for (let i=0;i<2e6;i++) n=(n+i*7)%1000003;
	         console.log(os.arch()+" "+n)' 2>/dev/null || true)"
case "$answer" in
"x64 "*)
	pass "an amd64 Node image ran a real program: $answer"
	;;
*)
	fail "the amd64 Node image produced '${answer:-nothing}'"
	;;
esac

# Making sure the native path did not regress while adding the emulated one.
native="$(docker run --rm lighter-test:arm64 uname -m 2>/dev/null || true)"
if [ "$native" = "aarch64" ]; then
	pass "native containers still report $native"
else
	fail "a native container reported '${native:-nothing}'"
fi

note "this is emulation, not Rosetta; see docs/x86-64.md for why"

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 7 gate passed\033[0m — x86-64 containers run.\n'
	exit 0
fi
printf '\033[31mmilestone 7 gate failed\033[0m — log at %s\n' "$LOG"
cp "$LOG" /tmp/lighter-m7.log 2>/dev/null || true
exit 1

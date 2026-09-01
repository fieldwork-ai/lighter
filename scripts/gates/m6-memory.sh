#!/usr/bin/env bash
# Milestone 6 gate: memory tracks load, and idling costs nothing.
#
# Two claims, and both are about what a virtual machine normally gets wrong.
#
#   1. A guest that used eight gigabytes for a build gives them back when the
#      build ends, without being asked and without being restarted.
#   2. A machine with nothing to do uses no CPU.
#
# The measurement is the VMM's own physical footprint, which is what macOS
# means by "Memory" and what pressure is computed from — not resident set size,
# which counts every page the guest has ever touched and so never falls.
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
BIN="target/$PROFILE/examples/boot"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-180}"
# How much the guest is made to use, and how much of it must come back.
BALLAST_MIB="${BALLAST_MIB:-3072}"
RETURN_FRACTION=70
RECLAIM_WINDOW="${RECLAIM_WINDOW:-60}"
# The idle window. The plan says ten minutes; the gate takes a shorter sample
# by default because it runs on every change, and IDLE_SECONDS raises it.
IDLE_SECONDS="${IDLE_SECONDS:-120}"
MAX_IDLE_CPU=1.0

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
cargo build $([ "$PROFILE" = release ] && echo --release) --example boot -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m6)"
SOCKET="$RUN_DIR/docker.sock"
LOG="$RUN_DIR/boot.log"
VMM_PID=""

cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$RUN_DIR"
}
trap cleanup EXIT

# The VMM reports its own footprint on an interval, because nothing outside the
# process can see the number that matters.
#
# The escape codes have to go first: `tracing` dims field names, so the log
# reads `\e[3mmib\e[0m\e[2m=\e[0m4096` and a literal `mib=` matches nothing.
footprint() {
	field mib
}

# Any field of the last footprint line.
field() {
	sed 's/\x1b\[[0-9;]*m//g' "$LOG" \
		| grep -a "FOOTPRINT" \
		| tail -1 \
		| sed -n "s/.* $1=\([0-9][0-9]*\).*/\\1/p"
}

await_footprint() {
	local waited=0
	while [ -z "$(footprint)" ]; do
		[ "$waited" -lt 30 ] || return 1
		sleep 1
		waited=$((waited + 1))
	done
}

echo
echo "==> Booting with 8 GiB of guest RAM"
: > "$LOG"
"$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$RUN_DIR/data.img" --disk-size-gib 32 \
	--net "$GVPROXY" --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--report-memory \
	--no-tty --cpus 4 --memory-mib 8192 \
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
await_footprint || { fail "the VMM never reported its footprint"; exit 1; }

BASE="$(footprint)"
pass "booted in ${waited}s, holding ${BASE} MiB"

# ------------------------------------------------------------------ memory --
echo
echo "==> Making the guest use ${BALLAST_MIB} MiB, then giving it back"
docker pull --quiet alpine:3.21 >/dev/null 2>&1 || true
# tmpfs is guest RAM and nothing else: no page cache to confuse the picture, no
# disk to write through, and it is freed the instant the container exits.
docker run --rm --tmpfs /ballast:rw,size=$((BALLAST_MIB + 256))m alpine:3.21 \
	sh -c "dd if=/dev/zero of=/ballast/x bs=1M count=$BALLAST_MIB 2>/dev/null; sync" \
	>/dev/null 2>&1 || fail "could not make the guest allocate"

PEAK=0
for _ in $(seq 1 10); do
	current="$(footprint)"
	[ "${current:-0}" -gt "$PEAK" ] && PEAK="$current"
	sleep 1
done
GREW=$((PEAK - BASE))
if [ "$GREW" -ge $((BALLAST_MIB / 2)) ]; then
	pass "footprint rose to ${PEAK} MiB (+${GREW} MiB) while the guest was using it"
else
	fail "footprint only rose ${GREW} MiB for a ${BALLAST_MIB} MiB allocation; the measurement is not seeing it"
fi

waited=0
BEST=$PEAK
while [ "$waited" -lt "$RECLAIM_WINDOW" ]; do
	sleep 5
	waited=$((waited + 5))
	current="$(footprint)"
	[ -n "$current" ] && [ "$current" -lt "$BEST" ] && BEST="$current"
	returned=$((PEAK - BEST))
	if [ "$GREW" -gt 0 ] && [ $((returned * 100 / GREW)) -ge "$RETURN_FRACTION" ]; then
		break
	fi
done
RETURNED=$((PEAK - BEST))
PERCENT=0
[ "$GREW" -gt 0 ] && PERCENT=$((RETURNED * 100 / GREW))
if [ "$PERCENT" -ge "$RETURN_FRACTION" ]; then
	pass "gave back ${RETURNED} of ${GREW} MiB (${PERCENT}%) within ${waited}s"
else
	fail "only gave back ${RETURNED} of ${GREW} MiB (${PERCENT}%) in ${RECLAIM_WINDOW}s"
	note "the guest reported $(field reported_mib) MiB of free pages over that time"
	note "zero there means the guest is not reporting; a large number means macOS kept the pages anyway"
fi

# -------------------------------------------------------------------- idle --
echo
echo "==> Watching an idle machine for ${IDLE_SECONDS}s"
# Two samples of cumulative CPU time, which is the only honest way: an instant
# percentage from `ps` is an average since the process started.
before="$(ps -o time= -p "$VMM_PID" | tr -d ' ')"
sleep "$IDLE_SECONDS"
after="$(ps -o time= -p "$VMM_PID" | tr -d ' ')"
seconds_of() { awk -F: '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; print s }' <<<"$1"; }
USED="$(awk -v a="$(seconds_of "$after")" -v b="$(seconds_of "$before")" 'BEGIN { printf "%.2f", a - b }')"
IDLE_CPU="$(awk -v u="$USED" -v w="$IDLE_SECONDS" 'BEGIN { printf "%.2f", u / w * 100 }')"
if awk -v c="$IDLE_CPU" -v m="$MAX_IDLE_CPU" 'BEGIN { exit !(c < m) }'; then
	pass "idle CPU ${IDLE_CPU}% over ${IDLE_SECONDS}s (budget ${MAX_IDLE_CPU}%)"
else
	fail "idle CPU ${IDLE_CPU}% over ${IDLE_SECONDS}s, budget ${MAX_IDLE_CPU}%"
fi
note "idle footprint $(footprint) MiB"
note "guest reported $(field reported_mib) MiB free, balloon holds $(field ballooned_mib) MiB"

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 6 memory gate passed\033[0m — the guest gives memory back, and idles at nothing.\n'
	exit 0
fi
printf '\033[31mmilestone 6 memory gate failed\033[0m — log at %s\n' "$LOG"
cp "$LOG" /tmp/lighter-m6.log 2>/dev/null || true
exit 1

#!/usr/bin/env bash
# Milestone 2 gate: the core virtio devices work, and the disk is genuinely
# dynamic.
#
# The disk half is the part nobody else demonstrates. "Sparse image" is easy to
# claim — create a large file, write nothing, observe it costs nothing. The hard
# half is giving space *back*: the guest discards blocks and the host image has
# to shrink. Proving that needs the host to measure the image between two boots
# of the same disk, which is what the three phases below do:
#
#   1. fresh image           → must cost ~nothing
#   2. boot, write 64 MiB    → host allocation must grow
#   3. boot, verify, discard → host allocation must fall back
#
# Phase 3 also proves persistence: the bytes phase 2 wrote are still there after
# a full machine restart.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

KERNEL="guest/out/Image"
INITRAMFS="guest/out/initramfs.cpio.gz"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-120}"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/boot"
IMG="${TMPDIR:-/tmp}/lighter-m2-$$.img"
LOGDIR="$ROOT/.logs"
mkdir -p "$LOGDIR"
LOG="$LOGDIR/m2-current.log"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

cleanup() {
	rm -f "$IMG"
	if [ "$FAILED" -ne 0 ]; then
		echo "  logs kept in $LOGDIR/m2-*.log"
	fi
}
trap cleanup EXIT

run_with_timeout() {
	local seconds="$1"; shift
	"$@" &
	local pid=$! waited=0
	while kill -0 "$pid" 2>/dev/null; do
		if [ "$waited" -ge "$seconds" ]; then
			kill -9 "$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
			return 124
		fi
		sleep 1
		waited=$((waited + 1))
	done
	wait "$pid"
}

# Blocks the host has actually allocated, in KiB. `du` reports allocation
# rather than logical length, which is exactly the distinction being measured.
allocated_kib() { du -k "$1" | cut -f1; }

boot() {
	local extra="$1"
	local phase="$2"
	LOG="$LOGDIR/m2-$phase.log"
	run_with_timeout "$BOOT_TIMEOUT" "$BIN" \
		--kernel "$KERNEL" \
		--initramfs "$INITRAMFS" \
		--no-tty \
		--cpus 2 \
		--disk "$IMG" \
		--disk-size-gib 4 \
		--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 $extra" \
		>"$LOG" 2>&1
}

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ]    || ./guest/kernel/build.sh
[ -f "$INITRAMFS" ] || ./guest/initramfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example boot -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

# --- phase 1: devices probe and work ---------------------------------------
echo
echo "==> Phase 1: device probe"
rm -f "$IMG"
if ! boot "lighter.selftest" probe; then
	fail "guest did not complete the selftest boot"
	tail -25 "$LOG" | sed 's/^/    /'
	exit 1
fi
pass "guest ran and exited"

fresh_kib=$(allocated_kib "$IMG")

grep -q "SELFTEST virtio id=0x0002 driver=virtio_blk"     "$LOG" && pass "virtio-blk bound"     || fail "virtio-blk did not bind"
grep -q "SELFTEST virtio id=0x0004 driver=virtio_rng"     "$LOG" && pass "virtio-rng bound"     || fail "virtio-rng did not bind"
grep -q "SELFTEST virtio id=0x0005 driver=virtio_balloon" "$LOG" && pass "virtio-balloon bound" || fail "virtio-balloon did not bind"

sectors=$(grep -o 'SELFTEST blockdev=vda size_sectors=[0-9]*' "$LOG" | cut -d= -f3)
[ "${sectors:-0}" = "8388608" ] \
	&& pass "guest sees a 4 GiB /dev/vda" \
	|| fail "guest reported ${sectors:-no} sectors, expected 8388608"

grep -q "SELFTEST blockio=ok" "$LOG" \
	&& pass "block data survives a write/read round trip" \
	|| fail "block I/O round trip failed or was corrupt"

entropy=$(grep -o 'SELFTEST entropy_avail=[0-9]*' "$LOG" | cut -d= -f2)
if [ -n "$entropy" ] && [ "$entropy" -ge 256 ]; then
	pass "entropy pool is filled ($entropy bits)"
else
	fail "entropy pool is only ${entropy:-unknown} bits"
fi

# --- phase 2: writing grows the image --------------------------------------
echo
echo "==> Phase 2: write 64 MiB in the guest"
rm -f "$IMG"
if ! boot "lighter.disktest=write" write; then
	fail "write phase did not complete"
	tail -20 "$LOG" | sed 's/^/    /'
	exit 1
fi
grep -q "DISKTEST wrote_mib=64" "$LOG" && pass "guest wrote 64 MiB to /dev/vda" || fail "guest write failed"

peak_kib=$(allocated_kib "$IMG")
if [ "$peak_kib" -ge 65536 ]; then
	pass "host image grew to ${peak_kib} KiB"
else
	fail "host image is only ${peak_kib} KiB after writing 64 MiB"
fi

# The fresh-image measurement is only meaningful next to the written one.
if [ "$fresh_kib" -lt 1024 ]; then
	pass "a freshly created 4 GiB image cost ${fresh_kib} KiB"
else
	fail "a new 4 GiB image already allocated ${fresh_kib} KiB"
fi

# --- phase 3: discard gives it back ----------------------------------------
echo
echo "==> Phase 3: verify persistence, then discard"
if ! boot "lighter.disktest=discard" discard; then
	fail "discard phase did not complete"
	tail -20 "$LOG" | sed 's/^/    /'
	exit 1
fi

grep -q "DISKTEST persisted=ok" "$LOG" \
	&& pass "data written by the previous boot was still there" \
	|| fail "data did not survive the reboot"

grep -q "DISKTEST discard=ok" "$LOG" \
	&& pass "guest issued a discard over the whole device" \
	|| fail "blkdiscard failed in the guest"

after_kib=$(allocated_kib "$IMG")
if [ "$after_kib" -lt $((peak_kib / 2)) ]; then
	pass "host image shrank from ${peak_kib} KiB to ${after_kib} KiB"
else
	fail "host image did not shrink: was ${peak_kib} KiB, now ${after_kib} KiB"
fi

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mM2 PASS\033[0m — block, entropy and balloon work; the disk grows and gives space back.\n'
else
	printf '\033[31mM2 FAIL\033[0m\n'
	exit 1
fi

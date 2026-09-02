#!/usr/bin/env bash
# Milestone 4 gate: a host directory is a directory inside the guest, and the
# two agree about what is in it.
#
# Three boots, because the three things being proved cannot share one:
#
#   1. the suite      — everything the guest can check on its own
#   2. coherence      — host and guest changing the same directory, in turns
#   3. durability     — the guest fsyncs, and the VMM is killed with SIGKILL
#
# The third is the one that cannot be faked. A server that batched writes would
# pass the first two and lose the file here.
set -euo pipefail

# cargo lives in ~/.cargo/bin, which a non-login shell does not have on PATH.
if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

KERNEL="guest/out/Image"
INITRAMFS="guest/out/initramfs.cpio.gz"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-180}"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/lighter-bench"
TAG=share
MOUNT=/mnt/share

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

# The 1 MiB the prober writes in durability mode, regenerated here so the check
# is on the bytes rather than on the length. Modulo 241 because a prime stride
# means no 4 KiB page repeats another, so a page written twice or dropped shows
# up as a mismatch rather than as identical filler.
guest_pattern() {
	perl -e 'binmode STDOUT; for my $i (0 .. 1048575) { print chr($i % 241) }'
}

# Turns the prober's own output into gate lines. Only records whose second field
# is a verdict are checks; everything else the prober prints is progress.
verdicts() {
	local name outcome rest failures
	while read -r name outcome rest; do
		case "$outcome" in
		ok) pass "$name" ;;
		FAIL) fail "$name — $rest" ;;
		esac
	# The log is a serial console, so every line ends CR LF. Without stripping
	# the CR the verdict reads as "ok\r" and matches nothing, which presents as
	# a suite that ran and reported no results at all.
	done < <(tr -d '\r' < "$LOG" | sed -n 's/^FSTEST //p')
	failures="$(tr -d '\r' < "$LOG" | sed -n 's/^FSTEST complete failures=//p' | tail -1)"
	if [ "${failures:-missing}" != 0 ]; then
		fail "the guest reported ${failures:-no} failing checks"
	fi
}

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ]    || ./guest/kernel/build.sh
[ -f "$INITRAMFS" ] || ./guest/initramfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

SHARE="$(mktemp -d -t lighter-share)"
LOG="$(mktemp -t lighter-m4)"
VMM_PID=""

cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	rm -rf "$SHARE"
	rm -rf "${RUN_DIR:-}"
	rm -f "${ROOTFS:-}"
}
trap cleanup EXIT
trap 'exit 143' INT TERM

# Boots the guest with the share attached, in one of the prober's modes.
boot() {
	local mode="$1"
	"$BIN" \
		--kernel "$KERNEL" \
		--initramfs "$INITRAMFS" \
		--no-tty \
		--cpus 4 \
		--share "$TAG:$SHARE" \
		--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 lighter.share=$TAG:$MOUNT lighter.fstest=$mode" \
		>>"$LOG" 2>&1 &
	VMM_PID=$!
	# Part three kills this with SIGKILL on purpose; without disowning it, bash
	# reports the job's death as a scary "Killed: 9" line in the middle of a
	# passing gate.
	disown "$VMM_PID" 2>/dev/null || true
}

# Waits for a line to appear in the log, or for the VMM to die.
await() {
	local pattern="$1" limit="${2:-$BOOT_TIMEOUT}" waited=0
	while ! grep -q "$pattern" "$LOG" 2>/dev/null; do
		if ! kill -0 "$VMM_PID" 2>/dev/null; then
			return 1
		fi
		if [ "$waited" -ge "$limit" ]; then
			return 2
		fi
		sleep 1
		waited=$((waited + 1))
	done
	return 0
}

# Waits for a path to appear on the host side of the share.
await_file() {
	local path="$1" limit="${2:-60}" waited=0
	while [ ! -e "$path" ]; do
		if ! kill -0 "$VMM_PID" 2>/dev/null; then
			return 1
		fi
		if [ "$waited" -ge "$limit" ]; then
			return 2
		fi
		sleep 1
		waited=$((waited + 1))
	done
	return 0
}

reap() {
	local waited=0
	while kill -0 "$VMM_PID" 2>/dev/null && [ "$waited" -lt 60 ]; do
		sleep 1
		waited=$((waited + 1))
	done
	kill -9 "$VMM_PID" 2>/dev/null || true
	VMM_PID=""
}

# ---------------------------------------------------------------- part one --
echo
echo "==> Boot 1: the guest checks the filesystem against itself"
: > "$LOG"
boot suite
if await "FSTEST complete" ; then
	verdicts
else
	fail "the guest did not finish the suite within ${BOOT_TIMEOUT}s"
	tail -30 "$LOG" | sed 's/^/    /'
fi
reap

# The suite cleans up after itself. Anything left behind is a delete that did
# not reach the host, which is a coherence failure by another name.
leftovers="$(find "$SHARE" -mindepth 1 | wc -l | tr -d ' ')"
if [ "$leftovers" -eq 0 ]; then
	pass "the guest's cleanup reached the host: nothing left in the share"
else
	fail "$leftovers entries left on the host after the guest deleted them"
	find "$SHARE" -mindepth 1 | head -5 | sed 's/^/    /'
fi

# ---------------------------------------------------------------- part two --
echo
echo "==> Boot 2: host and guest changing the same directory in turns"
: > "$LOG"
boot cross

# The guest writes first, and the host must see it without doing anything
# special — no remount, no sync, no delay beyond the write completing.
await_file "$SHARE/guest-wrote.done" 90 || true
if [ -f "$SHARE/guest-wrote.done" ] \
	&& [ "$(cat "$SHARE/guest-wrote" 2>/dev/null)" = "written inside the guest" ]; then
	pass "a file the guest wrote is on the host immediately"
	touch "$SHARE/host-saw-guest-write"
else
	fail "the host cannot see what the guest wrote"
fi

printf 'written on the host' > "$SHARE/host-wrote"
touch "$SHARE/host-wrote.done"

# The overwrite is the interesting one: the guest has already read this path, so
# a cached attribute or a retained page would show the old contents.
await "host-write-visible-to-guest" 60 || true
printf 'changed on the host' > "$SHARE/host-wrote"
touch "$SHARE/host-overwrote.done"

await "host-overwrite-is-seen" 60 || true
mv "$SHARE/host-wrote" "$SHARE/host-renamed"
touch "$SHARE/host-renamed.done"

await "host-rename-is-seen" 60 || true
rm -f "$SHARE/host-renamed"
touch "$SHARE/host-deleted.done"

mkdir -p "$SHARE/host-tree"
touch "$SHARE/host-tree/one" "$SHARE/host-tree/two" "$SHARE/host-tree/three"
touch "$SHARE/host-tree.done"

if await "FSTEST complete" 90; then
	verdicts
else
	fail "the coherence exchange did not complete"
	tail -30 "$LOG" | sed 's/^/    /'
fi
reap
rm -rf "${SHARE:?}"/*

# -------------------------------------------------------------- part three --
echo
echo "==> Boot 3: fsync, then SIGKILL the VMM mid-flight"
: > "$LOG"
boot durability
if await "FSTEST durable-synced" 120; then
	if grep -q "FSTEST durable-synced FAIL" "$LOG"; then
		fail "the guest could not sync the file at all"
	else
		# No shutdown, no sync, no chance to flush anything: SIGKILL is exactly
		# the failure this is supposed to survive.
		kill -9 "$VMM_PID" 2>/dev/null || true
		VMM_PID=""
		if [ -f "$SHARE/durable" ]; then
			size="$(wc -c < "$SHARE/durable" | tr -d ' ')"
			if [ "$size" -eq 1048576 ]; then
				# The contents matter as much as the size: a file of the right
				# length full of zeroes is what a lost write looks like.
				if cmp -s <(guest_pattern) "$SHARE/durable"; then
					pass "1 MiB survived SIGKILL of the VMM, byte for byte"
				else
					fail "the file survived at the right size with the wrong contents"
				fi
			else
				fail "the synced file is $size bytes, expected 1048576"
			fi
		else
			fail "the synced file is not on the host at all"
		fi
	fi
else
	fail "the guest never reported a successful sync"
	tail -30 "$LOG" | sed 's/^/    /'
fi
reap

# --------------------------------------------------------------- part four --
# Everything above proved the filesystem. This proves the *product*: a directory
# on the Mac, bind-mounted into a container, written from both sides.
echo
echo "==> Boot 4: a macOS directory bind-mounted into a container"
GVPROXY="${GVPROXY:-vendor/gvproxy}"
# A private clone, not the master: the master is an artifact, and any second
# machine mounting it read-write beside the first corrupts both.
ROOTFS_MASTER="guest/out/rootfs.ext4"
ROOTFS="$(mktemp -t lighter-rootfs).ext4"
cp -c "$ROOTFS_MASTER" "$ROOTFS" 2>/dev/null || cp "$ROOTFS_MASTER" "$ROOTFS"
if ! command -v docker >/dev/null 2>&1; then
	echo "  (skipped: no docker client on this machine)"
elif [ ! -x "$GVPROXY" ]; then
	echo "  (skipped: gvproxy missing; run scripts/fetch-gvproxy.sh)"
else
	[ -f "$ROOTFS" ] || ./guest/rootfs/build.sh
	rm -rf "${SHARE:?}"/*
	printf 'placed by macOS' > "$SHARE/from-host"

	RUN_DIR="$(mktemp -d -t lighter-m4d)"
	SOCKET="$RUN_DIR/docker.sock"
	: > "$LOG"
	"$BIN" \
		--kernel "$KERNEL" \
		--disk "$ROOTFS" \
		--disk "$RUN_DIR/data.img" --disk-size-gib 16 \
		--net "$GVPROXY" --run-dir "$RUN_DIR" \
		--vsock "$SOCKET:2375" \
		--share "$TAG:$SHARE" \
		--no-tty --cpus 4 --memory-mib 4096 \
		--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s) lighter.share=$TAG:$MOUNT" \
		>>"$LOG" 2>&1 &
	VMM_PID=$!
	disown "$VMM_PID" 2>/dev/null || true

	if await "AGENT listening" 180; then
		export DOCKER_HOST="unix://$SOCKET"
		grep -q "INIT share=$TAG at=$MOUNT\b" "$LOG" \
			&& pass "the share is mounted in the Docker guest" \
			|| fail "the Docker guest did not mount the share"

		# Pulled first and separately: pull progress goes to stderr, and folding
		# it into the container's own output makes a first run report the
		# progress bar as the file's contents.
		docker pull --quiet alpine:3.21 >/dev/null 2>&1 || true
		if out="$(docker run --rm -v "$MOUNT:/data" alpine:3.21 sh -c \
			'cat /data/from-host && printf " and by a container" > /data/from-container' \
			2>"$RUN_DIR/docker.err")"; then
			if [ "$out" = "placed by macOS" ]; then
				pass "a container read a file that macOS wrote"
			else
				fail "the container read: ${out:-nothing}"
			fi
			if [ "$(cat "$SHARE/from-container" 2>/dev/null)" = " and by a container" ]; then
				pass "macOS sees a file the container wrote"
			else
				fail "the container's write did not reach macOS"
			fi
		else
			fail "docker run with a bind mount failed"
			sed 's/^/    /' "$RUN_DIR/docker.err"
			tail -15 "$LOG" | sed 's/^/    /'
		fi
		unset DOCKER_HOST
	else
		fail "the Docker guest did not come up"
		tail -20 "$LOG" | sed 's/^/    /'
	fi
	kill -9 "$VMM_PID" 2>/dev/null || true
	VMM_PID=""
	rm -rf "$RUN_DIR"
fi

# ------------------------------------------------------------------- checks --
for signature in "Kernel panic" "Internal error: Oops" "Unable to handle kernel"; do
	if grep -qF "$signature" "$LOG"; then
		fail "guest reported: $signature"
		grep -F -m1 -A3 "$signature" "$LOG" | sed 's/^/    /'
	fi
done

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 4 filesystem gate passed\033[0m — a shared directory that agrees with itself.\n'
	rm -f "$LOG"
	exit 0
fi
printf '\033[31mmilestone 4 filesystem gate failed\033[0m — log at %s\n' "$LOG"
exit 1

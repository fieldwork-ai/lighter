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

KERNEL="${LIGHTER_GATE_KERNEL:-guest/out/Image}"
# A private clone, not the master: the master is an artifact, and any second
# machine mounting it read-write beside the first corrupts both.
ROOTFS_MASTER="guest/out/rootfs.ext4"
ROOTFS="$(mktemp -t lighter-rootfs).ext4"
cp -c "$ROOTFS_MASTER" "$ROOTFS" 2>/dev/null || cp "$ROOTFS_MASTER" "$ROOTFS"
PROFILE="${PROFILE:-release}"
BIN="target/$PROFILE/examples/lighter-bench"
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

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ] || ./guest/kernel/build.sh
[ -f "$ROOTFS" ] || ./guest/rootfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m6)"
SOCKET="$RUN_DIR/docker.sock"
LOG="$RUN_DIR/boot.log"
VMM_PID=""

cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	# The VMM's log outlives the run directory, because a failure names it.
	mkdir -p .logs && cp "$LOG" .logs/m6-last-boot.log 2>/dev/null || true
	rm -rf "$RUN_DIR"
	rm -f "${ROOTFS:-}"
}
trap cleanup EXIT
trap 'exit 143' INT TERM

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
# The pressure section below raises a host pressure level at a moment of
# its choosing, through the policy's test hook.
PRESSURE_FILE="$RUN_DIR/pressure"
LIGHTER_PRESSURE_TEST_FILE="$PRESSURE_FILE" "$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$RUN_DIR/data.img" --disk-size-gib 32 \
	--net --run-dir "$RUN_DIR" \
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
	note "the guest has reported $(field reported_mib) MiB cumulatively during this run"
	note "report totals include earlier and repeated pages; compare them with the footprint timeline"
fi

# -------------------------------------------------------------------- idle --
# -------------------------------------------------------------- the range --
# The guest boots with a base and a virtio-mem range (a quarter of the
# configured memory and the rest). With nothing running the range comes back
# out, page arrays and all, and that is the idle floor; a container start
# makes the guest whole again before dockerd sees the request, so what runs
# inside sees the configured size.
echo
echo "==> The range: out when nothing runs, whole for a container"
# Not to zero: the range's blocks online by the kernel's auto-movable
# policy (guest patch 0026), and a block that came up as kernel memory
# stays plugged for as long as it holds an unmovable page — a few blocks,
# variable, and plugged is not used: their free pages are reported and
# their page arrays are 1.5% of them. What must not stay is the range as
# a whole, so a residue of up to 768 MiB of the 6 GiB is the bound.
RANGE_RESIDUE_MIB=768
waited=0
while [ "$(field plugged_mib)" -gt "$RANGE_RESIDUE_MIB" ] && [ "$waited" -lt 60 ]; do
	sleep 2
	waited=$((waited + 2))
done
if [ "$(field plugged_mib)" -le "$RANGE_RESIDUE_MIB" ]; then
	pass "the range came out ${waited}s after the last container left ($(field plugged_mib) MiB of kernel-zone blocks left plugged); footprint $(footprint) MiB"
else
	fail "the range is still $(field plugged_mib) MiB plugged after ${waited}s with nothing running"
fi
SEEN_KB="$(docker run --rm alpine:3.21 awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)"
if [ "${SEEN_KB:-0}" -ge $((8192 * 1024 * 95 / 100)) ]; then
	pass "a container saw MemTotal $((SEEN_KB / 1024)) MiB of the 8192 configured"
else
	fail "a container saw MemTotal $((${SEEN_KB:-0} / 1024)) MiB; the guest was not made whole for it"
fi

# The MemTotal probe just plugged the whole range back in. Its growth hold
# and subsequent unplug are load transitions, not idle CPU. Wait for that
# observable transition to end before measuring idle, with the same bounded
# reclaim requirement as above.
waited=0
# Reports arrive every two seconds; the last one may still describe the
# pre-probe state. Require a new report before trusting its plugged size.
report_before="$(grep -ac 'FOOTPRINT' "$LOG")"
while { [ "$(grep -ac 'FOOTPRINT' "$LOG")" -le "$report_before" ] \
	|| [ "$(field plugged_mib)" -gt "$RANGE_RESIDUE_MIB" ]; } && [ "$waited" -lt 60 ]; do
	sleep 1
	waited=$((waited + 1))
done
if [ "$(grep -ac 'FOOTPRINT' "$LOG")" -le "$report_before" ] \
	|| [ "$(field plugged_mib)" -gt "$RANGE_RESIDUE_MIB" ]; then
	fail "range did not settle after the MemTotal probe"
else
	pass "range settled after the MemTotal probe (${waited}s)"
fi

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
note "guest reports total $(field reported_mib) MiB across this run; balloon currently holds $(field ballooned_mib) MiB"

# ----------------------------------------------------------------- pressure --
# At Warn the Mac asks the guest for a quarter of its RAM, at Critical a half.
# A guest with no memory to spare cannot give it, and a target past what it
# could give had its balloon driver retrying the allocation five times a
# second, each try a reclaim pass on a guest with nothing left to reclaim,
# until Docker stopped answering (a defect report of 2026-09-14: a build
# bounded at 10 GiB of a 16 GiB guest). The floor is held at what the
# balloon already has while the guest says it is short, and resumes once the
# guest has been fine for a few seconds.
echo
echo "==> A host pressure floor against a guest with no memory to spare"
# A tmpfs of most of RAM leaves the guest under an eighth available, which
# is its own line for "short"; the loop keeps it busy, and counts, so its
# progress under the floor can be read.
SHORT_MIB=7168
docker run -d --name m6-short --tmpfs /ballast:rw,size=$((SHORT_MIB + 128))m alpine:3.21 \
	sh -c "dd if=/dev/zero of=/ballast/x bs=1M count=$SHORT_MIB 2>/dev/null; i=0; while :; do i=\$((i+1)); echo \$i > /ballast/count; done" \
	>/dev/null 2>&1 || fail "could not make the guest short of memory"
sleep 20
answers() { perl -e 'alarm shift; exec @ARGV' "$1" docker ps -q >/dev/null 2>&1; }
# An exec into a guest this short takes a while: runc has to find pages for
# a process on a machine keeping a few hundred megabytes free. Docker itself
# answering is checked separately, with its own bound.
count() {
	local try c
	for try in 1 2 3; do
		c="$(perl -e 'alarm 20; exec @ARGV' docker exec m6-short cat /ballast/count 2>/dev/null || true)"
		[ -n "$c" ] && { echo "$c"; return; }
	done
}
answers 10 || fail "Docker stopped answering while the guest was merely short"
before_floor="$(field ballooned_mib)"
puffs_before="$(grep -ac 'Out of puff' "$LOG" || true)"
count_before="$(count)"
echo warn > "$PRESSURE_FILE"
sleep 20
if answers 10; then pass "Docker answers 20s into a Warn floor on a short guest"; else fail "Docker stopped answering under the floor"; fi
count_after="$(count)"
if [ "${count_after:-0}" -gt "${count_before:-0}" ]; then
	pass "the container kept running under the floor (${count_before:-?} → ${count_after:-?})"
else
	fail "the container made no progress under the floor (${count_before:-?} → ${count_after:-?})"
fi
if sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -aq "host pressure floor held"; then
	pass "the floor was held at what the balloon had"
else
	fail "the floor was not held; the policy took the quarter from a short guest"
fi
grew=$(( $(field ballooned_mib) - ${before_floor:-0} ))
[ "$grew" -le 256 ] && pass "the balloon grew ${grew} MiB under the held floor" || fail "the balloon grew ${grew} MiB under a floor that should have been held"
puffs=$(( $(grep -ac 'Out of puff' "$LOG" || true) - puffs_before ))
[ "$puffs" -le 10 ] && pass "${puffs} failed inflations while held" || fail "${puffs} failed inflations: the driver is retrying against a guest with nothing to give"
docker rm -f m6-short >/dev/null 2>&1
# With the container gone the guest is fine again and, five seconds later,
# the floor resumes: a quarter of what the guest has by then, which is the
# base and whatever of the range is still plugged, since with nothing
# running the range is on its way out. The driver may be a batch short.
waited=0
while :; do
	held="$(field ballooned_mib)"
	expected=$(( (2048 + $(field plugged_mib)) / 4 - 64 ))
	[ "${held:-0}" -ge "$expected" ] && break
	[ "$waited" -ge 90 ] && break
	sleep 5
	waited=$((waited + 5))
done
if [ "${held:-0}" -ge "$expected" ]; then
	pass "the floor resumed once the guest was fine: balloon holds ${held} MiB of a quarter of $(( 2048 + $(field plugged_mib) )) after ${waited}s"
else
	fail "the floor did not resume: balloon holds ${held:-0} MiB, a quarter of $(( 2048 + $(field plugged_mib) )) wanted, after ${waited}s"
fi
echo normal > "$PRESSURE_FILE"
sleep 5

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 6 memory gate passed\033[0m — the guest gives memory back, and idles at nothing.\n'
	exit 0
fi
printf '\033[31mmilestone 6 memory gate failed\033[0m — log at %s\n' "$LOG"
cp "$LOG" /tmp/lighter-m6.log 2>/dev/null || true
exit 1

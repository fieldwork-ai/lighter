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
ROOTFS_DIR="$(mktemp -d -t lighter-rootfs)"
ROOTFS="$ROOTFS_DIR/rootfs.ext4"
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
	rm -rf "${ROOTFS_DIR:-}"
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
# ------------------------------------------------------------------ whole --
# One zone since 0.7.2: a container sees the configured memory, always.
SEEN_KB="$(docker run --rm alpine:3.21 awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)"
if [ "${SEEN_KB:-0}" -ge $((8192 * 1024 * 95 / 100)) ]; then
	pass "a container saw MemTotal $((SEEN_KB / 1024)) MiB of the 8192 configured"
else
	fail "a container saw MemTotal $((${SEEN_KB:-0} / 1024)) MiB"
fi
sleep 10

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
# At Warn the Mac lets the balloon ramp to a quarter of the guest, at
# Critical a half. A guest with no memory to spare cannot give it, and a
# target past what it could give had its balloon driver retrying the
# allocation five times a second, each try a reclaim pass on a guest with
# nothing left to reclaim, until Docker stopped answering (a defect report
# of 2026-09-14: a build bounded at 10 GiB of a 16 GiB guest). The ramp is
# held at what the balloon already has while the guest says it is short,
# and moves again once the guest has been fine for a few seconds.
echo
echo "==> A host pressure floor against a guest with no memory to spare"
# A tmpfs of most of RAM leaves the guest under an eighth available, which
# is its own line for "short"; the loop keeps it busy, and counts, so its
# progress under the floor can be read.
# An idle container beside it, as a daily machine has.
docker run -d --name m6-keeper alpine:3.21 sleep 900 >/dev/null 2>&1 || fail "could not start the keeper"
sleep 5
SHORT_MIB=7168
docker run -d --name m6-short --tmpfs /ballast:rw,size=$((SHORT_MIB + 128))m alpine:3.21 \
	sh -c "dd if=/dev/zero of=/ballast/x bs=1M count=$SHORT_MIB 2>/dev/null; i=0; while :; do i=\$((i+1)); [ \$((i % 100000)) -eq 0 ] && echo \$i; done" \
	>/dev/null 2>&1 || fail "could not make the guest short of memory"
sleep 20
answers() { perl -e 'alarm shift; exec @ARGV' "$1" docker ps -q >/dev/null 2>&1; }
# Read through dockerd's log of the container, not an exec into it: starting
# a process in a guest this short can take longer than the check.
count() { perl -e 'alarm 10; exec @ARGV' docker logs --tail 1 m6-short 2>/dev/null | tail -1; }
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
if grep -aq "balloon ramp held" "$LOG"; then
	pass "the ramp was held at what the balloon had"
else
	fail "the ramp was not held; the policy took the quarter from a short guest"
fi
grew=$(( $(field ballooned_mib) - ${before_floor:-0} ))
[ "$grew" -le 256 ] && pass "the balloon grew ${grew} MiB under the held ramp" || fail "the balloon grew ${grew} MiB under a ramp that should have been held"
puffs=$(( $(grep -ac 'Out of puff' "$LOG" || true) - puffs_before ))
[ "$puffs" -le 10 ] && pass "${puffs} failed inflations while held" || fail "${puffs} failed inflations: the driver is retrying against a guest with nothing to give"
docker rm -f m6-short >/dev/null 2>&1
# With the container gone the guest is fine again and, five seconds later,
# the ramp climbs: a 32nd of the guest a second to a quarter of it. The
# guest gives cache, not failures. The driver may be a batch short.
puffs_before="$(grep -ac 'Out of puff' "$LOG" || true)"
waited=0
while :; do
	held="$(field ballooned_mib)"
	expected=$(( 8192 / 4 - 64 ))
	[ "${held:-0}" -ge "$expected" ] && break
	[ "$waited" -ge 60 ] && break
	sleep 5
	waited=$((waited + 5))
done
if [ "${held:-0}" -ge "$expected" ]; then
	pass "the ramp climbed once the guest was fine: balloon holds ${held} MiB of a quarter of 8192 after ${waited}s"
else
	fail "the ramp did not climb: balloon holds ${held:-0} MiB, a quarter of 8192 wanted, after ${waited}s"
fi
puffs=$(( $(grep -ac 'Out of puff' "$LOG" || true) - puffs_before ))
[ "$puffs" -le 10 ] && pass "${puffs} failed inflations on the climb: cache given, not fought for" || fail "${puffs} failed inflations on the climb"
# The level dropping is a plateau, not a cliff: what the balloon holds
# stays for the quiet rule to ease down a 256th of the guest a second,
# rather than going back to the guest to refill the cache the host then
# compresses again (the comb of 2026-09-16: eighteen Warn/Normal flips in
# half an hour, a two-gigabyte inflate-and-deflate on each).
at_warn="$(field ballooned_mib)"
guest_mib=8192
step=$(( guest_mib / 256 )); [ "$step" -lt 32 ] && step=32
echo normal > "$PRESSURE_FILE"
sleep 15
after="$(field ballooned_mib)"
# Five seconds of patience, then at most ten small steps: what a plateau
# that eases looks like fifteen seconds in. A cliff is gone at once.
floor=$(( at_warn - 10 * step - 64 ))
[ "${after:-0}" -ge "$floor" ] && pass "Normal is a plateau: ${after} of ${at_warn} MiB held 15s later (eases ${step} MiB/s at most)" || fail "Normal was a cliff: ${after:-0} of ${at_warn} MiB left after 15s, ${floor} expected"
sleep 60
later="$(field ballooned_mib)"
[ "${later:-0}" -lt "${after:-0}" ] && pass "and it eases: ${later} MiB after another 60s" || fail "it did not ease: ${later:-0} MiB after another 60s (was ${after:-0})"
# The reported workflow: the host flapping between Warn and Normal. The
# balloon must ride it out where it is, not cycle to nothing and back.
first=""
min_held=999999
puffs_before="$(grep -ac 'Out of puff' "$LOG" || true)"
for flap in 1 2 3; do
	echo warn > "$PRESSURE_FILE"
	sleep 20
	now="$(field ballooned_mib)"
	[ -z "$first" ] && first="$now"
	[ "${now:-0}" -lt "$min_held" ] && min_held="$now"
	echo normal > "$PRESSURE_FILE"
	sleep 20
	now="$(field ballooned_mib)"
	[ "${now:-0}" -lt "$min_held" ] && min_held="$now"
done
puffs=$(( $(grep -ac 'Out of puff' "$LOG" || true) - puffs_before ))
[ "$min_held" -ge $(( first / 2 )) ] && pass "three Warn/Normal flaps: balloon never below ${min_held} MiB of its first ${first}" || fail "flapping cycled the balloon down to ${min_held} MiB from ${first}"
[ "$puffs" -le 10 ] && pass "${puffs} failed inflations across the flaps" || fail "${puffs} failed inflations across the flaps"
echo normal > "$PRESSURE_FILE"
docker rm -f m6-keeper >/dev/null 2>&1
sleep 5

# ------------------------------------------------------------------ idle --
# A second boot, with the hour the agent's idle pass waits shortened to
# forty seconds by its command-line hook (`lighter.idle_age`). Its own
# boot: under the sections above the pass would have started moving their
# tmpfs ballast and cache after eighty seconds. The pass is DAMON's
# (guest `idle.rs`): every horizon it asks each unmapped page-cache page
# whether it was touched since the last pass and evicts the ones that were
# not. Three containers say what must go and what must stay: one that read
# two gigabytes once, one that re-reads its file every five seconds, and a
# sleeping process with a mapped executable.
echo
echo "==> Idle page cache: a second boot, the hour shortened to 40 s"
kill -9 "$VMM_PID" 2>/dev/null || true
wait "$VMM_PID" 2>/dev/null || true
VMM_PID=""
mkdir -p .logs && cp "$LOG" .logs/m6-last-boot-1.log 2>/dev/null || true
: > "$LOG"
# The ramp stays out of it whatever the Mac is doing. The level file alone
# was not enough: an overcommitted Mac (a daily driver with seven gigabytes
# in swap, 2026-09-21) is steered as at Warn whatever level it reports, and
# the ramp then has the guest reclaim its coldest cache ahead of each
# balloon step, which is the cold container's, before the pass reaches it.
# Right, and not what this section measures.
echo normal > "$PRESSURE_FILE"
LIGHTER_MEMORY_STEER=0 "$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$RUN_DIR/data.img" --disk-size-gib 32 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--report-memory \
	--no-tty --cpus 4 --memory-mib 8192 \
	--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s) lighter.idle_age=40" \
	>"$LOG" 2>&1 &
VMM_PID=$!
disown "$VMM_PID" 2>/dev/null || true
waited=0
while ! grep -q "AGENT listening" "$LOG" 2>/dev/null; do
	kill -0 "$VMM_PID" 2>/dev/null || { fail "the VMM exited during the second boot"; tail -20 "$LOG" | sed 's/^/    /'; exit 1; }
	[ "$waited" -lt "$BOOT_TIMEOUT" ] || { fail "the guest did not come up a second time"; exit 1; }
	sleep 1
	waited=$((waited + 1))
done
await_footprint || { fail "the VMM never reported its footprint after the second boot"; exit 1; }
T0=$(date +%s)
sleep 3
if grep -aq "AGENT idle: pass every 40 s" "$LOG"; then
	pass "the agent configured the pass at a 40 s horizon"
else
	fail "the agent did not configure the pass: $(grep -a 'AGENT idle' "$LOG" | tail -1)"
fi
# The first boot's containers may survive in dockerd's state, its VM having
# been killed rather than shut down.
docker rm -f m6-keeper m6-short >/dev/null 2>&1 || true
start() { local name="$1"; shift; local err; err="$(docker run -d --name "$name" "$@" 2>&1 >/dev/null)" || fail "could not start $name: ${err}"; }
start m6-keeper alpine:3.21 sleep 900
start m6-warm alpine:3.21 sh -c 'dd if=/dev/zero of=/warm bs=1M count=256 2>/dev/null; while :; do cat /warm >/dev/null; sleep 5; done'
start m6-cold alpine:3.21 sh -c 'dd if=/dev/zero of=/cold bs=1M count=2048 2>/dev/null; sync; sleep 900'
start m6-mapped node:24-alpine node -e 'setInterval(() => {}, 1000000)'
sleep 20
# A container's own view of its cgroup, in MiB.
cgstat() { perl -e 'alarm 10; exec @ARGV' docker exec "$1" grep -E "^$2 " /sys/fs/cgroup/memory.stat 2>/dev/null | awk '{ print int($2 / 1048576) }'; }
# The sleeping process's resident mapped file pages (its executable and
# libraries), from its own view: image layers are charged to the engine's
# cgroup, not the container's, so the cgroup's `file_mapped` says nothing.
rssfile() { perl -e 'alarm 10; exec @ARGV' docker exec "$1" grep RssFile /proc/1/status 2>/dev/null | awk '{ print int($2 / 1024) }'; }
cold0="$(cgstat m6-cold file)"
warm0="$(cgstat m6-warm file)"
mapped0="$(rssfile m6-mapped)"
reported0="$(field reported_mib)"
puffs0="$(grep -ac 'Out of puff' "$LOG" || true)"
[ "${cold0:-0}" -ge 1900 ] && pass "the cold container cached ${cold0} MiB, the warm one ${warm0} MiB, the sleeping process maps ${mapped0} MiB" \
	|| fail "the cold container cached only ${cold0:-?} MiB"
# The first pass marks every page old; the second, forty seconds later,
# evicts what stayed untouched. The agent reads the pass's stats every ten
# seconds at this horizon.
while ! grep -aq "AGENT idle: evicted" "$LOG" && [ $(( $(date +%s) - T0 )) -lt 130 ]; do sleep 2; done
evicted="$(sed -n 's/.*AGENT idle: evicted \([0-9][0-9]*\) MiB.*/\1/p' "$LOG" | tail -1)"
if [ "${evicted:-0}" -ge 1800 ]; then
	pass "the pass evicted ${evicted} MiB $(( $(date +%s) - T0 ))s after boot"
else
	fail "the pass evicted ${evicted:-nothing} within 130 s: $(grep -a 'AGENT idle' "$LOG" | tail -2 | tr '\n' ' ')"
fi
sleep 5
cold1="$(cgstat m6-cold file)"
warm1="$(cgstat m6-warm file)"
mapped1="$(rssfile m6-mapped)"
[ "${cold1:-9999}" -le 128 ] && pass "the cold container's cache is gone (${cold0} → ${cold1} MiB)" \
	|| fail "the cold container still holds ${cold1:-?} MiB of cache"
[ "${warm1:-0}" -ge 224 ] && pass "the warm container's cache stayed (${warm0} → ${warm1} MiB)" \
	|| fail "the warm container lost its cache (${warm0} → ${warm1:-?} MiB): a page read every five seconds was evicted"
[ "${mapped0:-0}" -ge 16 ] && [ "${mapped1:-0}" -ge $(( mapped0 - 4 )) ] && pass "the sleeping process's mapped pages stayed (${mapped0} → ${mapped1} MiB)" \
	|| fail "the sleeping process lost mapped pages (${mapped0:-?} → ${mapped1:-?} MiB)"
# What the pass freed comes back through free page reporting alone: the
# agent compacts, and reporting returns the runs within seconds, with no
# balloon inflated for it.
waited=0
while [ $(( $(field reported_mib) - reported0 )) -lt 1500 ] && [ "$waited" -lt 30 ]; do
	sleep 2
	waited=$((waited + 2))
done
returned=$(( $(field reported_mib) - reported0 ))
[ "$returned" -ge 1500 ] && pass "${returned} MiB reported back to the host within ${waited}s (footprint $(footprint) MiB)" \
	|| fail "only ${returned} MiB reported back within ${waited}s (footprint $(footprint) MiB)"
[ "$(field ballooned_mib)" -le 64 ] && pass "no balloon inflated for it ($(field ballooned_mib) MiB)" \
	|| fail "the balloon holds $(field ballooned_mib) MiB after the pass; the pass is reporting's, not the balloon's"
puffs=$(( $(grep -ac 'Out of puff' "$LOG" || true) - puffs0 ))
[ "$puffs" -le 10 ] && pass "${puffs} failed inflations across the pass" || fail "${puffs} failed inflations across the pass"
docker rm -f m6-warm m6-cold m6-mapped m6-keeper >/dev/null 2>&1

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mmilestone 6 memory gate passed\033[0m — the guest gives memory back, and idles at nothing.\n'
	exit 0
fi
printf '\033[31mmilestone 6 memory gate failed\033[0m — log at %s\n' "$LOG"
cp "$LOG" /tmp/lighter-m6.log 2>/dev/null || true
exit 1

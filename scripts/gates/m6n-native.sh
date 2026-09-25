#!/usr/bin/env bash
# Gate m6n: Mac-native resources (`lighter config --resources native`).
#
# A native machine is the whole Mac: every core a vCPU, and a memory ceiling
# of the Mac's own, booted on a small base with the rest a virtio-mem range
# plugged in as the guest needs it. The claims:
#
#   1. It boots on its base: nothing of the range is plugged until wanted.
#   2. A burst faster than the host can be asked (a tmpfs writer, which the
#      kernel cannot reclaim) completes: the containers' throttle sleeps it
#      at the edge while the range grows, and nothing is OOM-killed.
#   3. Kernel memory grows with it: a burst of inodes and dentries, all
#      unmovable, completes and the engine still answers.
#   4. The host's pressure still reaches it: the balloon takes memory back.
#   5. What it used goes back to the Mac, and the range shrinks, without the
#      guest's driver retrying unplugs it cannot finish.
#   6. Every core as a vCPU still idles inside the budget.
#   7. The Mac's interactive threads wait no longer behind a guest running
#      every vCPU flat out than behind the same number of native threads:
#      a container's build competes with the Mac's windows as a native
#      build does, no harder.
set -euo pipefail
if ! command -v cargo >/dev/null 2>&1; then
	# shellcheck disable=SC1091
	. "$HOME/.cargo/env"
fi
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
KERNEL="${LIGHTER_GATE_KERNEL:-guest/out/Image}"
ROOTFS_MASTER="guest/out/rootfs.ext4"
ROOTFS_DIR="$(mktemp -d -t lighter-rootfs)"
ROOTFS="$ROOTFS_DIR/rootfs.ext4"
cp -c "$ROOTFS_MASTER" "$ROOTFS" 2>/dev/null || cp "$ROOTFS_MASTER" "$ROOTFS"
PROFILE="${PROFILE:-release}"
BIN="target/$PROFILE/examples/lighter-bench"
# The command line is the CLI's (run.rs), so what is measured is what ships:
# per-cgroup pressure accounting off above all, which the throttle's stall
# trigger has to work without.
BOOT_TIMEOUT="${BOOT_TIMEOUT:-180}"
# The CLI's ceiling, the Mac's RAM less 2 GiB, capped at 16 GiB: large
# enough for the burst to need the range, and on an 8 GB M1 the 6 GiB a
# person would get (a fixed 16 GiB there grew the range into swap). The
# burst is 6000 MiB, or half the ceiling where that is less. The vCPUs are
# every core the Mac has.
MAC_MIB=$(( $(sysctl -n hw.memsize) >> 20 ))
CEILING_MIB="${CEILING_MIB:-$(( MAC_MIB - 2048 < 16384 ? MAC_MIB - 2048 : 16384 ))}"
VCPUS="$(sysctl -n hw.ncpu)"
BURST_MIB="${BURST_MIB:-$(( CEILING_MIB / 2 < 6000 ? CEILING_MIB / 2 : 6000 ))}"
IDLE_SECONDS="${IDLE_SECONDS:-60}"
MAX_IDLE_CPU=1.0
# The Mac's own interactive thread, woken every 5 ms: its 99th percentile
# lateness behind the guest's vCPUs may be at most this multiple of its
# lateness behind as many native threads, plus a millisecond.
LATE_RATIO=1.5

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
note() { printf '  \033[33m··\033[0m   %s\n' "$*"; }
FAILED=0

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
[ -f "$KERNEL" ] || ./guest/kernel/build.sh
echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example lighter-bench -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

RUN_DIR="$(mktemp -d -t lighter-m6n)"
SOCKET="$RUN_DIR/docker.sock"
LOG="$RUN_DIR/boot.log"
PRESSURE_FILE="$RUN_DIR/pressure"
VMM_PID=""
cleanup() {
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	mkdir -p .logs && cp "$LOG" .logs/m6n-last-boot.log 2>/dev/null || true
	rm -rf "$RUN_DIR" "${ROOTFS_DIR:-}"
}
trap cleanup EXIT
trap 'exit 143' INT TERM

field() {
	sed 's/\x1b\[[0-9;]*m//g' "$LOG" \
		| grep -a "FOOTPRINT" \
		| tail -1 \
		| sed -n "s/.* $1=\([0-9][0-9]*\).*/\\1/p"
}
footprint() { field mib; }
# A privileged look at the guest itself: its cgroups, its kernel log.
guest() {
	docker run --rm --privileged --pid=host --cgroupns=host -v /sys:/sys alpine:3.21 sh -c "$1; true" 2>/dev/null || true
}

echo
echo "==> Booting native: ${VCPUS} vCPUs, a ${CEILING_MIB} MiB ceiling"
echo normal > "$PRESSURE_FILE"
LIGHTER_RESOURCES=native LIGHTER_PRESSURE_TEST_FILE="$PRESSURE_FILE" "$BIN" \
	--kernel "$KERNEL" \
	--disk "$ROOTFS" \
	--disk "$RUN_DIR/data.img" --disk-size-gib 32 \
	--net --run-dir "$RUN_DIR" \
	--vsock "$SOCKET:2375" \
	--report-memory \
	--no-tty --cpus "$VCPUS" --memory-mib "$CEILING_MIB" \
	--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init reboot=t cgroup_disable=pressure swiotlb=noforce lighter.time=$(date +%s)" \
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
docker pull --quiet alpine:3.21 >/dev/null 2>&1 || true
sleep 5

# -------------------------------------------------------------------- base --
# On its base, plus at most the headroom (a gigabyte at this size): with every
# core a vCPU the kernel's per-CPU memory takes more of the base, and the
# policy tops what is left up to the headroom.
BOOT_PLUGGED="$(field plugged_mib)"
BOOT_FOOTPRINT="$(footprint)"
[ "${BOOT_PLUGGED:-99999}" -le 1024 ] \
	&& pass "booted in ${waited}s on its base: ${BOOT_PLUGGED} MiB of ${CEILING_MIB} plugged, footprint ${BOOT_FOOTPRINT} MiB" \
	|| fail "booted with ${BOOT_PLUGGED:-?} MiB of the range plugged"

# ------------------------------------------------------------------- burst --
echo
echo "==> A ${BURST_MIB} MiB tmpfs burst from the base"
events_before="$(guest 'grep "^high " /sys/fs/cgroup/docker/memory.events | cut -d" " -f2' || echo 0)"
start=$(date +%s)
if docker run --rm --tmpfs /w:size=$((BURST_MIB * 2))m alpine:3.21 \
	sh -c "dd if=/dev/zero of=/w/x bs=1M count=$BURST_MIB 2>/dev/null" >/dev/null 2>&1; then
	pass "the burst completed in $(( $(date +%s) - start ))s"
else
	fail "the burst failed"
fi
# The largest plug over the next reports: they come every two seconds, and a
# burst can finish between two of them.
PEAK_PLUGGED=0
for _ in 1 2 3 4 5 6; do
	now="$(field plugged_mib)"
	[ "${now:-0}" -gt "$PEAK_PLUGGED" ] && PEAK_PLUGGED="$now"
	sleep 1
done
state="$(guest 'cat /sys/fs/cgroup/docker/memory.events; dmesg | grep -ciE "out of memory|oom-kill"')"
oom_kills="$(awk '/^oom_kill /{print $2}' <<<"$state")"
dmesg_ooms="$(tail -1 <<<"$state")"
high_events="$(awk '/^high /{print $2}' <<<"$state")"
[ "${oom_kills:-1}" -eq 0 ] && [ "${dmesg_ooms:-1}" -eq 0 ] \
	&& pass "nothing OOM-killed; the throttle slowed it $(( ${high_events:-0} - ${events_before:-0} )) times" \
	|| fail "OOM kills: ${oom_kills:-?} in the containers, ${dmesg_ooms:-?} in the kernel log"
docker info >/dev/null 2>&1 && pass "the engine answers after the burst" || fail "the engine does not answer after the burst"
[ "${PEAK_PLUGGED:-0}" -ge $((BURST_MIB / 2)) ] \
	&& pass "the range grew to ${PEAK_PLUGGED} MiB for it" \
	|| fail "the range only grew to ${PEAK_PLUGGED:-0} MiB for a ${BURST_MIB} MiB burst"

# ------------------------------------------------------------------ kernel --
echo
echo "==> Kernel memory: 300k inodes and dentries in a tmpfs"
start=$(date +%s)
if docker run --rm --tmpfs /t:size=4g,nr_inodes=0 alpine:3.21 \
	sh -c 'cd /t && seq 1 300000 | xargs touch && ls | wc -l' 2>/dev/null | grep -q 300000; then
	pass "300000 files made in $(( $(date +%s) - start ))s"
else
	fail "the files were not all made"
fi
t0=$(date +%s)
docker ps >/dev/null 2>&1 && [ $(( $(date +%s) - t0 )) -le 5 ] \
	&& pass "the engine answers promptly after it" \
	|| fail "the engine was slow or silent after the kernel burst"
slab="$(guest 'grep -E "^(Slab|SUnreclaim):" /proc/meminfo | tr -s " " | tr "\n" " "')"
note "guest ${slab}"

# ---------------------------------------------------------------- pressure --
echo
echo "==> Host pressure: a native guest takes nothing more from the Mac at Warn"
docker run -d --name m6n-keeper alpine:3.21 sleep 900 >/dev/null 2>&1 || true
sleep 4
before_fp="$(footprint)"
echo warn > "$PRESSURE_FILE"
sleep 20
held="$(field ballooned_mib)"
after_fp="$(footprint)"
[ "${held:-0}" -gt 0 ] && [ "${after_fp:-99999}" -le $((before_fp + 64)) ] \
	&& pass "at Warn the balloon holds ${held} MiB and the footprint went ${before_fp} → ${after_fp} MiB" \
	|| fail "at Warn the balloon holds ${held:-0} MiB and the footprint went ${before_fp} → ${after_fp} MiB"
echo normal > "$PRESSURE_FILE"
docker rm -f m6n-keeper >/dev/null 2>&1 || true

# ------------------------------------------------------------ giving back --
echo
echo "==> Giving it back"
offers_before="$(grep -ac 'virtio-mem offer' "$LOG" || true)"
waited=0
while [ "$waited" -lt 90 ]; do
	sleep 5
	waited=$((waited + 5))
	[ "$(footprint)" -le $((BOOT_FOOTPRINT + 512)) ] && [ "$(field plugged_mib)" -lt "$PEAK_PLUGGED" ] && break
done
[ "$(footprint)" -le $((BOOT_FOOTPRINT + 512)) ] \
	&& pass "the footprint came back to $(footprint) MiB within ${waited}s (booted at ${BOOT_FOOTPRINT})" \
	|| fail "the footprint is $(footprint) MiB after ${waited}s (booted at ${BOOT_FOOTPRINT})"
[ "$(field plugged_mib)" -lt "$PEAK_PLUGGED" ] \
	&& pass "the range shrank ${PEAK_PLUGGED} → $(field plugged_mib) MiB" \
	|| fail "the range stayed at $(field plugged_mib) MiB"
# Which zones the plugged blocks are in: a block onlined as kernel memory can
# hold the kernel's pages and stay; one onlined movable should not.
zones="$(guest 'for b in /sys/devices/system/memory/memory*; do [ "$(cat $b/state)" = online ] && cat $b/valid_zones; done | sort | uniq -c | tr "\n" " "')"
note "online blocks by zone: ${zones}"
# The driver's retries of unplugs it cannot finish: offers a minute apart
# at most once the floor is set, not one a second.
sleep 60
offers=$(( $(grep -ac 'virtio-mem offer' "$LOG" || true) - offers_before ))
[ "$offers" -le 90 ] \
	&& pass "${offers} range offers in the $((waited + 60))s after the work" \
	|| fail "${offers} range offers in $((waited + 60))s: unplugs retried without a floor"

# -------------------------------------------------------------------- idle --
echo
echo "==> Idle with ${VCPUS} vCPUs for ${IDLE_SECONDS}s"
seconds_of() { awk -F: '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; print s }' <<<"$1"; }
before="$(ps -o time= -p "$VMM_PID" | tr -d ' ')"
sleep "$IDLE_SECONDS"
after="$(ps -o time= -p "$VMM_PID" | tr -d ' ')"
IDLE_CPU="$(awk -v a="$(seconds_of "$after")" -v b="$(seconds_of "$before")" -v w="$IDLE_SECONDS" 'BEGIN { printf "%.2f", (a - b) / w * 100 }')"
awk -v c="$IDLE_CPU" -v m="$MAX_IDLE_CPU" 'BEGIN { exit !(c < m) }' \
	&& pass "idle CPU ${IDLE_CPU}% (budget ${MAX_IDLE_CPU}%)" \
	|| fail "idle CPU ${IDLE_CPU}%, budget ${MAX_IDLE_CPU}%"

# ---------------------------------------------------------- responsiveness --
echo
echo "==> The Mac's interactive thread behind ${VCPUS} busy vCPUs, and behind ${VCPUS} native threads"
# One interactive thread, woken every 5 ms, as the Mac's windows are; with $2
# native threads spinning beside it when $2 is set.
# A file, not stdin: multiprocessing starts its children by importing the
# main module again, which a script read from stdin cannot be.
PROBE="$RUN_DIR/probe.py"
cat > "$PROBE" <<'PY'
import ctypes, multiprocessing, sys, time
def burn(sec):
    end = time.monotonic() + sec
    while time.monotonic() < end:
        pass
if __name__ == "__main__":
    seconds, native = float(sys.argv[1]), int(sys.argv[2])
    burners = [multiprocessing.Process(target=burn, args=(seconds + 3,)) for _ in range(native)]
    for b in burners:
        b.start()
    if native:
        time.sleep(2)
    ctypes.CDLL(None).pthread_set_qos_class_self_np(0x21, 0)
    late = []
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        t = time.monotonic()
        time.sleep(0.005)
        late.append((time.monotonic() - t - 0.005) * 1000)
    for b in burners:
        b.terminate()
    late.sort()
    print(f"{late[len(late) * 99 // 100]:.2f}")
PY
probe() { /usr/bin/python3 "$PROBE" "$1" "${2:-0}"; }
QUIET_P99="$(probe 10)"
NATIVE_P99="$(probe 12 "$VCPUS")"
docker run -d --name m6n-burn alpine:3.21 sh -c "for i in \$(seq 1 $VCPUS); do (while :; do :; done) & done; sleep 60" >/dev/null 2>&1
sleep 3
GUEST_P99="$(probe 12)"
docker rm -f m6n-burn >/dev/null 2>&1 || true
awk -v g="$GUEST_P99" -v n="$NATIVE_P99" -v r="$LATE_RATIO" 'BEGIN { exit !(g <= n * r + 1) }' \
	&& pass "p99 lateness ${GUEST_P99} ms behind the guest, ${NATIVE_P99} ms behind native threads (${QUIET_P99} ms quiet)" \
	|| fail "p99 lateness ${GUEST_P99} ms behind the guest against ${NATIVE_P99} ms behind native threads (${QUIET_P99} ms quiet)"

echo
[ "$FAILED" -eq 0 ] && echo "m6n: native resources hold" || echo "m6n: FAILED"
exit "$FAILED"

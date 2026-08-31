#!/usr/bin/env bash
# Milestone 1 gate: a kernel we built boots on a VMM we wrote, reaches
# userspace, and the machine it finds is the machine we described.
#
# The checks are deliberately not "did it print something". Each one pins a
# distinct subsystem that can fail silently:
#
#   cpus     — PSCI CPU_ON actually started every secondary
#   memory   — the device tree's /memory matched what we mapped
#   dt       — the kernel parsed our tree rather than falling back
#   console  — the real PL011 driver bound, not just earlycon
#   timer    — the virtual timer delivers interrupts, so sleep() returns
#   poweroff — PSCI SYSTEM_OFF reached us and stopped the machine
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

KERNEL="guest/out/Image"
INITRAMFS="guest/out/initramfs.cpio.gz"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-60}"
PROFILE="${PROFILE:-debug}"
BIN="target/$PROFILE/examples/boot"

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
FAILED=0

# macOS has no coreutils `timeout`, and a hung VMM must not hang the gate.
run_with_timeout() {
	local seconds="$1"; shift
	"$@" &
	local pid=$!
	local waited=0
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

echo "==> Building guest artifacts if missing"
[ -f "$KERNEL" ]    || ./guest/kernel/build.sh
[ -f "$INITRAMFS" ] || ./guest/initramfs/build.sh

echo "==> Building and signing the VMM"
cargo build $([ "$PROFILE" = release ] && echo --release) --example boot -p lighter-vmm
./scripts/sign.sh "$BIN" >/dev/null

# One boot per core count: 1 exercises the plain path, 4 exercises PSCI CPU_ON
# and the multi-threaded shutdown that a single core never reaches.
for cpus in 1 4; do
	echo
	echo "==> Booting with $cpus vCPU(s)"
	log="$(mktemp -t lighter-m1)"
	started=$(date +%s)
	if run_with_timeout "$BOOT_TIMEOUT" "$BIN" \
		--kernel "$KERNEL" \
		--initramfs "$INITRAMFS" \
		--no-tty \
		--cpus "$cpus" \
		--cmdline "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 lighter.selftest" \
		>"$log" 2>&1; then
		elapsed=$(( $(date +%s) - started ))
		pass "guest ran and exited (${elapsed}s wall)"
	else
		status=$?
		if [ "$status" -eq 124 ]; then
			fail "guest did not exit within ${BOOT_TIMEOUT}s"
		else
			fail "VMM exited with status $status"
		fi
		echo "--- last 20 lines ---"
		tail -20 "$log" | sed 's/^/    /'
		continue
	fi

	grep -q "SELFTEST cpus=$cpus" "$log" \
		&& pass "all $cpus core(s) online" \
		|| fail "expected $cpus online cores; got: $(grep -o 'SELFTEST cpus=[0-9]*' "$log" || echo none)"

	# 2 GiB mapped, minus what the kernel keeps for itself. Anything below
	# 1.5 GiB means the device tree and the mapping disagree.
	mem=$(grep -o 'SELFTEST memtotal_kb=[0-9]*' "$log" | cut -d= -f2)
	if [ -n "$mem" ] && [ "$mem" -gt 1500000 ]; then
		pass "guest sees $((mem / 1024)) MiB of RAM"
	else
		fail "guest saw ${mem:-no} KiB of RAM; expected >1500000"
	fi

	grep -q "SELFTEST dt=ok"            "$log" && pass "device tree parsed"        || fail "no /proc/device-tree"
	grep -q "SELFTEST console=ttyAMA0"  "$log" && pass "PL011 driver bound"        || fail "amba-pl011 did not bind"
	grep -q "SELFTEST timer=ok"         "$log" && pass "virtual timer advances"    || fail "guest clock did not advance"
	grep -q "SELFTEST done"             "$log" && pass "init completed"            || fail "init did not finish"
	grep -q "guest powered off"         "$log" && pass "PSCI SYSTEM_OFF handled"   || fail "machine did not power off cleanly"

	# A guest that panicked but still printed its selftest would otherwise pass.
	if grep -q "Kernel panic" "$log"; then
		fail "kernel panicked"
		grep -A3 "Kernel panic" "$log" | head -4 | sed 's/^/    /'
	fi

	# Signatures of a machine the guest could not make sense of. These have all
	# been seen for real and none of them stop the boot on their own: the guest
	# carries on with a broken device and dies somewhere unrelated, so the log
	# is the only place the actual cause is written down.
	#
	# "No redistributor present" is here because it happened: vCPU threads raced
	# to hv_vcpu_create, so framework ids stopped matching the thread indices we
	# were deriving MPIDR from, and roughly one SMP boot in eight came up with a
	# core whose redistributor the GIC could not find. Creation is serialized
	# now, but the check stays — it is the only outward sign of that whole class
	# of bug.
	for signature in \
		"No redistributor present" \
		"Unable to handle kernel" \
		"Internal error: Oops" \
		"BUG: " \
		"unhandled trapped system register"; do
		if grep -qF "$signature" "$log"; then
			fail "guest reported: $signature"
			grep -F -m1 -A2 "$signature" "$log" | sed 's/^/    /'
		fi
	done

	# The GIC has to have found every core's redistributor, not just core 0.
	found=$(grep -c "found redistributor" "$log" || true)
	if [ "$found" -eq "$cpus" ]; then
		pass "GIC located $cpus redistributor(s)"
	else
		fail "GIC located $found redistributor(s), expected $cpus"
	fi

	rm -f "$log"
done

echo
if [ "$FAILED" -eq 0 ]; then
	printf '\033[32mM1 PASS\033[0m — a kernel we built boots on a VMM we wrote.\n'
else
	printf '\033[31mM1 FAIL\033[0m\n'
	exit 1
fi

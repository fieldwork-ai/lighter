#!/usr/bin/env bash
# Feed synthetic case output through the full harness, without booting a VM.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d -t lighter-bench-test)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/benchmarks/cases" "$WORK/benchmarks/fixtures/npm" "$WORK/home" "$WORK/bin"
# These synthetic JavaScript cases test result handling, not file searching.
# Satisfy the native target's tool check without depending on a host ripgrep
# installation; fail if a fixture unexpectedly tries to execute it.
printf '#!/bin/sh\necho "unexpected ripgrep invocation in synthetic case" >&2\nexit 99\n' > "$WORK/bin/rg"
chmod +x "$WORK/bin/rg"
cp "$ROOT/benchmarks/run.sh" "$WORK/benchmarks/run.sh"
cp "$ROOT/benchmarks/cases/runner.js" "$WORK/benchmarks/cases/runner.js"
for file in package.json package-lock.json pnpm-lock.yaml yarn.lock; do
	: > "$WORK/benchmarks/fixtures/npm/$file"
done
: > "$WORK/benchmarks/cases/unused.sh"
cat > "$WORK/benchmarks/cases/complete.js" <<'JS'
console.log('TIME_MS 10\nTIME_MS 20\nTIME_MS 30');
JS
cat > "$WORK/benchmarks/cases/partial.js" <<'JS'
console.log('TIME_MS 10\nTIME_MS 20');
JS
cat > "$WORK/benchmarks/cases/failed.js" <<'JS'
console.log('TIME_MS 10\nTIME_MS 20\nTIME_MS 30');
console.error('case failed after reporting');
process.exit(7);
JS
cat > "$WORK/benchmarks/cases/timedout.js" <<'JS'
console.log('TIME_MS 10\nTIME_MS TIMEOUT 1s\nTIME_MS 30');
JS
cat > "$WORK/benchmarks/cases/setupfailed.setup.sh" <<'SH'
#!/bin/sh
echo 'synthetic setup failure' >&2
exit 9
SH
cat > "$WORK/benchmarks/cases/setupfailed.sh" <<'SH'
#!/bin/sh
echo 'BODY_MUST_NOT_RUN'
SH
for name in complete partial failed timedout setupfailed; do
	rc=0
	PATH="$WORK/bin:$PATH" HOME="$WORK/home" LIGHTER_BENCH_WORK="$WORK/run" bash "$WORK/benchmarks/run.sh" \
		--target native --allow-noisy --label "$name" --cases "$name" --reps 3 > "$WORK/$name.log" 2>&1 || rc=$?
	if [ "$name" = complete ]; then
		[ "$rc" -eq 0 ] || { cat "$WORK/$name.log"; exit 1; }
	else
		[ "$rc" -ne 0 ] || { echo "incorrectly accepted $name"; exit 1; }
		grep -q 'FAILED:' "$WORK/$name.log"
		[ -s "$WORK/.logs/case-$name-$name.out" ]
	fi
done
! grep -q '^BODY_MUST_NOT_RUN$' "$WORK/.logs/case-setupfailed-setupfailed.out"
# Warm-up failure must survive successful timed repetitions.
cat > "$WORK/benchmarks/cases/npm-install.js" <<'JS'
if (process.env.REPS === '1') { console.error('synthetic warmup failure'); process.exit(9); }
console.log('TIME_MS 10\nTIME_MS 20\nTIME_MS 30');
JS
rc=0
PATH="$WORK/bin:$PATH" HOME="$WORK/home" LIGHTER_BENCH_WORK="$WORK/run" bash "$WORK/benchmarks/run.sh" \
 --target native --allow-noisy --label warmfailed --cases npm-install --reps 3 > "$WORK/warmfailed.log" 2>&1 || rc=$?
[ "$rc" -ne 0 ]
grep -q 'FAILED: npm-install warm-up' "$WORK/warmfailed.log"
grep -q 'synthetic warmup failure' "$WORK/.logs/warmup-warmfailed-npm-install.out"
# Extra warm-up cases prepare caches but must not add timed observations.
for name in npm-install pnpm-install yarn-install; do
	cat > "$WORK/benchmarks/cases/$name.js" <<'JS'
for (let rep = 0; rep < Number(process.env.REPS); rep++) console.log('TIME_MS 10');
JS
done
PATH="$WORK/bin:$PATH" HOME="$WORK/home" LIGHTER_BENCH_WORK="$WORK/run" \
 BENCH_EXTRA_WARM_CASES='pnpm-install yarn-install' bash "$WORK/benchmarks/run.sh" \
 --target native --allow-noisy --label extra-warm --cases npm-install --reps 2 > "$WORK/extra-warm.log" 2>&1
for name in npm-install pnpm-install yarn-install; do
	grep -q '^TIME_MS 10$' "$WORK/.logs/warmup-extra-warm-$name.out"
done
[ "$(grep -c '^npm-install,' "$WORK/benchmarks/results/extra-warm.csv")" -eq 2 ]
! grep -qE '^(pnpm|yarn)-install,' "$WORK/benchmarks/results/extra-warm.csv"
grep -q '^extra_warm_cases=pnpm-install yarn-install$' "$WORK/benchmarks/results/extra-warm.tree"
# Memory has its own background workload and must also retain its status.
# Extract that function from the real harness, mocking only time, footprint
# and the workload so these failure paths do not need a VM or minute waits.
sed -n '/^run_memory_case() {/,/^}/p' "$ROOT/benchmarks/run.sh" > "$WORK/memory.sh"
cat >> "$WORK/memory.sh" <<'SH'
set -euo pipefail
TARGET=lighter LABEL=memory-test FAILED=0 CASE_OUT=""
RESULTS=memory.csv
: > "$RESULTS"
sleep() { :; }
runtime_footprint_mib() { echo 42; }
run_case() {
	case "$MODE" in
		complete) echo 'TIME_MS 10' ;;
		failed) echo 'TIME_MS 10'; return 7 ;;
		timedout) echo 'TIME_MS TIMEOUT 1s' ;;
	esac
}
run_memory_case
if [ "$MODE" = complete ]; then
	[ "$FAILED" = 0 ]
	[ "$(wc -l < "$RESULTS")" -eq 3 ]
else
	[ "$FAILED" = 1 ]
	[ ! -s "$RESULTS" ]
	[ -s .logs/case-memory-test-memory.out ]
fi
SH
for mode in complete failed timedout; do
	(cd "$WORK"; MODE="$mode" bash memory.sh > "memory-$mode.log" 2>&1) \
		|| { cat "$WORK/memory-$mode.log"; exit 1; }
done
# A failed HTTP client must not silently disappear from the network medians.
sed -n '/^run_net_case() {/,/^}/p' "$ROOT/benchmarks/run.sh" > "$WORK/network.sh"
cat >> "$WORK/network.sh" <<'SH'
set -euo pipefail
TARGET=lighter LABEL=network-test FAILED=0 REPS=3
RESULTS=network.csv
NET_HTTP_PORT=12345
: > "$RESULTS"
net_setup() { :; }
python3() {
	if [ "$MODE" = complete ]; then echo '100 200'; else
		echo 'ConnectionResetError: synthetic network failure' >&2; return 7
	fi
}
run_net_case net-http-latency
if [ "$MODE" = complete ]; then
	[ "$FAILED" = 0 ]
	[ "$(wc -l < "$RESULTS")" -eq 6 ]
else
	[ "$FAILED" = 1 ]
	[ ! -s "$RESULTS" ]
	grep -q 'ConnectionResetError' .logs/network-network-test-net-http-latency-1.stderr
fi
# Native has no published port; that deliberate omission remains supported.
TARGET=native FAILED=0
run_net_case net-tcp-port
[ "$FAILED" = 0 ]
SH
for mode in complete failed; do
	(cd "$WORK"; MODE="$mode" bash network.sh > "network-$mode.log" 2>&1) \
		|| { cat "$WORK/network-$mode.log"; exit 1; }
done
# A missing iperf summary must fail; a genuine measured zero remains valid.
sed -n '/^iperf_mbits() {/,/^# A client/p' "$ROOT/benchmarks/run.sh" > "$WORK/iperf.sh"
cat >> "$WORK/iperf.sh" <<'SH'
set -euo pipefail
[ "$(printf '%s' '{"end":{"sum_received":{"bits_per_second":42000000}}}' | iperf_mbits)" = 42 ]
[ "$(printf '%s' '{"end":{"sum":{"bits_per_second":0}}}' | iperf_mbits)" = 0 ]
for value in '{"end":{}}' '{"end":{"sum":{}}}' '{"error":"send failed","end":{"sum":{"bits_per_second":123}}}' '{"end":{"sum":{"bits_per_second":-1}}}' '{"end":{"sum":{"bits_per_second":NaN}}}' 'not json'; do
 [ -z "$(printf '%s' "$value" | iperf_mbits)" ]
done
SH
bash "$WORK/iperf.sh"
# A boot warm-up failure must retain Docker's error and stop the private home,
# including before the harness has assigned VMM_PID. Mock the CLI, not cleanup.
for function in cleanup boot_stop boot_start run_boot_case; do
	sed -n "/^$function() {/,/^}/p" "$ROOT/benchmarks/run.sh" >> "$WORK/boot.sh"
done
mkdir -p "$WORK/scripts"
printf '#!/bin/sh\nexit 0\n' > "$WORK/scripts/sign.sh"
cat > "$WORK/fake-lighter" <<'SH'
#!/bin/sh
case "$1" in
config) ;;
start) echo $$ > "$LIGHTER_HOME/lighter.pid"; echo 'VM diagnostic' > "$LIGHTER_HOME/machine.log" ;;
stop) rm -f "$LIGHTER_HOME/lighter.pid" ;;
esac
SH
chmod +x "$WORK/scripts/sign.sh" "$WORK/fake-lighter"
cat >> "$WORK/boot.sh" <<'SH'
set -euo pipefail
ROOT="$PWD" TARGET=lighter LABEL=boot-failure REPS=1 FAILED=0 KEEP=1
BOOT_HOME="" BOOT_START_PID="" BOOT_LOG="" BOOT_LOG_DIR=""
VMM_PID="" HELPER_PID="" RUN_DIR="" ROOTFS="" CASE_OUT=""
LIGHTER_CLI="$PWD/fake-lighter" RESULTS="$PWD/boot.csv"
: > "$RESULTS"
cargo() { :; }
net_teardown() { :; }
sleep() { :; }
now_ms() { echo 0; }
bench_memory_mib() { echo 4096; }
bench_disk_gib() { echo 128; }
boot_await_docker() {
	for _ in $(seq 1 100); do
		[ ! -f "$BOOT_HOME/machine.log" ] || return 0
		command sleep 0.01
	done
	return 1
}
dk() { echo 'synthetic boot container failure' >&2; return 125; }
trap cleanup EXIT
run_boot_case
SH
rc=0
(cd "$WORK"; bash boot.sh > boot-failure.log 2>&1) || rc=$?
[ "$rc" -eq 125 ] || { cat "$WORK/boot-failure.log"; exit 1; }
grep -q 'synthetic boot container failure' "$WORK/.logs/boot-boot-failure/container-0.log"
grep -q 'VM diagnostic' "$WORK/.logs/boot-boot-failure/machine.log"
boot_home="$(cat "$WORK/.logs/boot-boot-failure/retained-home")"
[ ! -f "$boot_home/lighter.pid" ]
[ ! -s "$WORK/boot.csv" ]
rm -rf "$boot_home"

# Runtime accounting must not include a supervisor just because its arguments
# mention allowed app paths. Feed executable-only process rows through the
# actual selector and reject use of the old argument-matching command.
sed -n '/^runtime_pids() {/,/^}/p' "$ROOT/benchmarks/run.sh" > "$WORK/pids.sh"
cat >> "$WORK/pids.sh" <<'SH'
set -euo pipefail
pgrep() { echo 'argument matching must not select runtime PIDs' >&2; return 99; }
ps() {
 [ "$*" = '-axo pid=,comm=' ]
 cat <<'ROWS'
 101 /Applications/OrbStack.app/Contents/MacOS/OrbStack
 102 /opt/homebrew/bin/limactl
 103 /System/Library/com.apple.Virtualization.VirtualMachine
 104 /Applications/Docker.app/Contents/MacOS/com.docker.backend
 105 /Applications/OrbStack.app/Contents/Frameworks/OrbStack Helper.app/Contents/MacOS/OrbStack Helper
 106 /opt/homebrew/bin/python3
 107 /usr/bin/grep
ROWS
}
TARGET=orbstack; [ "$(runtime_pids)" = '101 105 ' ]
TARGET=colima; [ "$(runtime_pids)" = '102 103 ' ]
TARGET=docker-desktop; [ "$(runtime_pids)" = '103 104 ' ]
TARGET=lighter VMM_PID=108; [ "$(runtime_pids)" = '108' ]
SH
bash "$WORK/pids.sh"
echo 'benchmark results: failures rejected; runtime accounting excludes supervisor arguments'

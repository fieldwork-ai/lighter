#!/usr/bin/env bash
# Feed synthetic case output through the full harness, without booting a VM.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d -t lighter-bench-test)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/benchmarks/cases" "$WORK/benchmarks/fixtures/npm" "$WORK/home"
cp "$ROOT/benchmarks/run.sh" "$WORK/benchmarks/run.sh"
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
for name in complete partial failed timedout; do
	rc=0
	HOME="$WORK/home" LIGHTER_BENCH_WORK="$WORK/run" bash "$WORK/benchmarks/run.sh" \
		--target native --allow-noisy --label "$name" --cases "$name" --reps 3 > "$WORK/$name.log" 2>&1 || rc=$?
	if [ "$name" = complete ]; then
		[ "$rc" -eq 0 ] || { cat "$WORK/$name.log"; exit 1; }
	else
		[ "$rc" -ne 0 ] || { echo "incorrectly accepted $name"; exit 1; }
		grep -q 'FAILED:' "$WORK/$name.log"
		[ -s "$WORK/.logs/case-$name-$name.out" ]
	fi
done
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
	[ "$FAILED" = 0 ] && [ "$(wc -l < "$RESULTS")" -eq 3 ]
else
	[ "$FAILED" = 1 ] && [ ! -s "$RESULTS" ]
	[ -s .logs/case-memory-test-memory.out ]
fi
SH
for mode in complete failed timedout; do
	(cd "$WORK"; MODE="$mode" bash memory.sh > "memory-$mode.log" 2>&1) \
		|| { cat "$WORK/memory-$mode.log"; exit 1; }
done
echo 'benchmark results: complete accepted; partial, failed and timed-out cases rejected, including the memory workload'

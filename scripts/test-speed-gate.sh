#!/usr/bin/env bash
# Drive the complete speed gate with fixed CSVs and no benchmark workload.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d -t lighter-speed-gate-test)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/scripts/gates" "$WORK/benchmarks/results"
cp "$ROOT/scripts/gates/m5-speed.sh" "$WORK/scripts/gates/"
printf '#!/bin/bash\nexit 0\n' > "$WORK/benchmarks/run.sh"
chmod +x "$WORK/benchmarks/run.sh"
: > "$WORK/benchmarks/report.py"
: > "$WORK/benchmarks/results/lighter-boot.log"
for scenario in complete npm-install ripgrep find-walk copy-tree; do
	: > "$WORK/benchmarks/results/gate-native.csv"
	: > "$WORK/benchmarks/results/gate-lighter.csv"
	for name in npm-install ripgrep find-walk copy-tree watch-latency; do
		ours=100
		[ "$name" != watch-latency ] || ours=1
		[ "$name" != "$scenario" ] || ours=100000
		printf '%s,1,1000\n' "$name" >> "$WORK/benchmarks/results/gate-native.csv"
		printf '%s,1,%s\n' "$name" "$ours" >> "$WORK/benchmarks/results/gate-lighter.csv"
	done
	rc=0
	bash "$WORK/scripts/gates/m5-speed.sh" > "$WORK/$scenario.log" 2>&1 || rc=$?
	if [ "$scenario" = complete ]; then
		[ "$rc" = 0 ] || { cat "$WORK/$scenario.log"; exit 1; }
	else
		[ "$rc" != 0 ] || { echo "speed gate accepted a failing $scenario floor"; exit 1; }
	fi
done
echo 'speed gate: passing measurements accepted; every performance floor can fail the gate'

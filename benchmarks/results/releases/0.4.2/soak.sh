#!/usr/bin/env bash
set -euo pipefail
export PATH="$HOME/.orbstack/bin:/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
cd "$HOME/lighter"
export BENCH_MEMORY_MIB=4096 BENCH_CPUS=8 BENCH_DISK_GIB=128 LIGHTER_BENCH_KEEP_OUTPUT=1
unset LIGHTER_CMDLINE_EXTRA LIGHTER_VIRTIO_MEM
HEAD_EXPECTED="${1:?exact head}"
[ "$(git rev-parse HEAD)" = "$HEAD_EXPECTED" ]
git diff --quiet HEAD
ROOT=.logs/042/soak
mkdir -p "$ROOT"
echo baseline > "$ROOT/phase"
python3 .logs/042/monitor-daemon.py "$ROOT" &
MONITOR=$!
report_backup="$(mktemp -t lighter-soak-report)"
cp benchmarks/RESULTS.md "$report_backup"
cleanup() {
  result=$?
  kill "$MONITOR" 2>/dev/null || true
  wait "$MONITOR" 2>/dev/null || true
  cp "$report_backup" benchmarks/RESULTS.md
  rm "$report_backup"
  echo "$result" > "$ROOT/exit-code"
}
trap cleanup EXIT
cargo build --release --example lighter-bench -p lighter-vmm > "$ROOT/build.log" 2>&1
scripts/sign.sh target/release/lighter target/release/examples/lighter-bench >> "$ROOT/build.log" 2>&1
{ date -u; git rev-parse HEAD; sw_vers; sysctl hw.ncpu hw.memsize; ps -Ao pid,pcpu,comm -r | sed -n '1,15p'; } > "$ROOT/environment.txt"
quiet=0
for sample in $(seq 1 90); do
  busy="$(ps -Ao pcpu,comm | awk '/mds_stores|mdworker|fseventsd|photolibraryd|mediaanalysisd/ && $1 > 1 {n++} END {print n+0}')"
  if [ "$busy" = 0 ]; then quiet=$((quiet+1)); else quiet=0; fi
  [ "$quiet" -lt 6 ] || break
  sleep 10
done
[ "$quiet" -ge 6 ] || { echo 'Host did not settle'; exit 1; }
ALL='npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf cpu-sha256 container-start watch-latency memory net-tcp-egress net-tcp-egress-r net-tcp-port net-tcp-port-r net-udp net-connect-rate net-http-latency net-dns power-idle boot'
GUEST='npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf'
AMD64='npm-install pnpm-install cpu-sha256 container-start'
for suite in 1 2 3; do
 for stage in share guest amd64; do
  args=() cases="$ALL"
  if [ "$stage" = guest ]; then args=(--where guest); cases="$GUEST"; fi
  if [ "$stage" = amd64 ]; then args=(--where guest --arch amd64); cases="$AMD64"; fi
  label="042-${HEAD_EXPECTED:0:7}-m1-$suite-$stage"
  [ ! -e "benchmarks/results/$label.csv" ]
  echo "$suite-$stage" > "$ROOT/phase"
  echo "BEGIN $label $(date -u)"
  scripts/capped.sh 2400 bash benchmarks/run.sh --target lighter --label "$label" --reps 3 --cases "$cases" ${args[@]+"${args[@]}"} > "$ROOT/$label.log" 2>&1
  cp benchmarks/results/lighter-boot.log "$ROOT/$label-boot.log"
  if grep -Ei 'rcu.*stall|soft lockup|hard LOCKUP|BUG:|kernel panic|Oops:' "$ROOT/$label-boot.log"; then exit 1; fi
  python3 - "$label" "$cases" <<'PY'
import csv, sys
from collections import Counter
from pathlib import Path
label,cases=sys.argv[1:]
rows=list(csv.reader(Path(f'benchmarks/results/{label}.csv').open()))
counts=Counter(row[0] for row in rows)
for case in cases.split():
 for name in {'memory':['memory-peak','memory-after-15s','memory-after-60s'],'power-idle':['power-cpu-ms-per-s','power-wakeups-per-s'],'boot':['boot-docker','boot-first-container','memory-idle']}.get(case,[case]):
  expected=1 if name.startswith(('memory-','power-')) else 3
  assert counts[name]==expected,(label,name,counts[name],expected)
for row in rows:
 if row[0]!='case': assert float(row[2])>=0,row
print('validated',label)
PY
  cp "$report_backup" benchmarks/RESULTS.md
  git diff --quiet HEAD
  [ "$(git rev-parse HEAD)" = "$HEAD_EXPECTED" ]
  echo "END $label $(date -u)"
 done
done
echo post-suite > "$ROOT/phase"
sleep 600
echo complete > "$ROOT/phase"
echo 'THREE SUITES AND TEN-MINUTE OBSERVATION COMPLETE'

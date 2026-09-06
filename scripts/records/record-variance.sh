#!/usr/bin/env bash
# Repeated fresh-VM storage runs. Invoke on the otherwise quiet record host.
# Results go below benchmarks/results/<label>/; existing results are preserved.
set -euo pipefail
cd "$(dirname "$0")/../.."
LABEL="${1:?usage: record-variance.sh <label> [runs]}"
RUNS="${2:-5}"
case "$LABEL" in /*|*..*) echo 'label must stay below benchmarks/results' >&2; exit 2 ;; esac
case "$RUNS" in ''|*[!0-9]*) exit 2 ;; esac
[ "$RUNS" -ge 3 ] || { echo 'need at least three fresh runs' >&2; exit 2; }
OUT="benchmarks/results/$LABEL"
[ ! -e "$OUT" ] || { echo "refusing to overwrite $OUT" >&2; exit 1; }
git diff --quiet || { echo 'tracked source is dirty' >&2; exit 1; }
mkdir -p "$OUT"
# The harness retains case output under .logs/case-<label>-<case>.out.
mkdir -p ".logs/case-$LABEL"
export BENCH_CPUS="${BENCH_CPUS:-8}" BENCH_MEMORY_MIB="${BENCH_MEMORY_MIB:-4096}" BENCH_DISK_GIB="${BENCH_DISK_GIB:-128}"
export LIGHTER_BENCH_KEEP_OUTPUT=1
unset LIGHTER_CMDLINE_EXTRA LIGHTER_VIRTIO_MEM
CASES='npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf'
{
  git rev-parse HEAD
  sw_vers
  sysctl hw.model hw.memsize hw.ncpu
  printf 'cpus=%s memory_mib=%s disk_gib=%s repetitions=3 runs=%s\n' "$BENCH_CPUS" "$BENCH_MEMORY_MIB" "$BENCH_DISK_GIB" "$RUNS"
  printf 'cases=%s\n' "$CASES"
  shasum -a 256 guest/out/Image guest/out/rootfs.ext4 guest/out/lighter-agent
} > "$OUT/environment.txt"
for run in $(seq 1 "$RUNS"); do
  quiet=0
  for sample in $(seq 1 90); do
    busy="$(ps -Ao pcpu,comm | awk '/mds_stores|mdworker|fseventsd|photolibraryd|mediaanalysisd/ && $1 > 1 {n++} END {print n+0}')"
    if [ "$busy" = 0 ]; then quiet=$((quiet+1)); else quiet=0; fi
    [ "$quiet" -lt 7 ] || break
    sleep 10
  done
  [ "$quiet" -ge 7 ] || { echo 'host did not settle; stopping' >&2; exit 1; }
  echo "BEGIN run-$run $(date -u)"
  scripts/capped.sh 1200 bash benchmarks/run.sh --target lighter --label "$LABEL/run-$run" --reps 3 --cases "$CASES" > "$OUT/run-$run.log" 2>&1
  cp benchmarks/results/lighter-boot.log "$OUT/run-$run-boot.log"
  if grep -Ei 'rcu.*stall|soft lockup|hard LOCKUP|BUG:|kernel panic|Oops:' "$OUT/run-$run-boot.log"; then exit 1; fi
  echo "END run-$run $(date -u)"
done

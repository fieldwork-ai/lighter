#!/usr/bin/env bash
set -euo pipefail
export PATH="$HOME/.orbstack/bin:/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
cd "$HOME/lighter"
BASE="$PWD/.logs/042/abba"
mkdir -p "$BASE/041" "$BASE/042"
[ "$(git rev-parse HEAD)" = d0fbd114f19b333af7b3b9732d9637a27c962d6c ]
git diff --quiet HEAD
cp target/release/examples/lighter-bench "$BASE/042/lighter-bench"
[ ! -d .logs/042/abba-old ]
git worktree add --detach .logs/042/abba-old 5f48cf03
ln -s "$PWD/target" .logs/042/abba-old/target
(
 cd .logs/042/abba-old
 cargo build --release --example lighter-bench -p lighter-vmm
)
cp target/release/examples/lighter-bench "$BASE/041/lighter-bench"
cp "$BASE/042/lighter-bench" target/release/examples/lighter-bench
scripts/sign.sh "$BASE/041/lighter-bench" "$BASE/042/lighter-bench"
tar -xzf dist/lighter-0.4.1-arm64.tar.gz -C "$BASE/041"
shasum -a 256 "$BASE/041/lighter-bench" "$BASE/042/lighter-bench" > "$BASE/binaries.sha256"
cp benchmarks/RESULTS.md "$BASE/report-backup.md"
echo baseline > "$BASE/phase"
python3 .logs/042/monitor-daemon.py "$BASE" &
MONITOR=$!
cleanup() {
 result=$?
 kill "$MONITOR" 2>/dev/null || true
 wait "$MONITOR" 2>/dev/null || true
 cp "$BASE/report-backup.md" benchmarks/RESULTS.md
 echo "$result" > "$BASE/exit-code"
}
trap cleanup EXIT
export BENCH_MEMORY_MIB=4096 BENCH_CPUS=8 BENCH_DISK_GIB=128 LIGHTER_BENCH_KEEP_OUTPUT=1
unset LIGHTER_CMDLINE_EXTRA LIGHTER_VIRTIO_MEM
quiet=0
for sample in $(seq 1 90); do
 busy="$(ps -Ao pcpu,comm | awk '/mds_stores|mdworker|fseventsd|photolibraryd|mediaanalysisd/ && $1 > 1 {n++} END {print n+0}')"
 if [ "$busy" = 0 ]; then quiet=$((quiet+1)); else quiet=0; fi
 [ "$quiet" -lt 6 ] || break
 sleep 10
done
[ "$quiet" -ge 6 ] || { echo 'Host did not settle'; exit 1; }
index=0
for version in 041 042 042 041; do
 index=$((index+1))
 export COMPARE_BIN="$BASE/$version/lighter-bench"
 if [ "$version" = 041 ]; then
  export COMPARE_GUEST_DIR="$BASE/041/lighter-0.4.1/share/lighter" COMPARE_SHA=5f48cf03
 else
  export COMPARE_GUEST_DIR="$PWD/guest/out" COMPARE_SHA=d0fbd114
 fi
 export LIGHTER_BENCH_KERNEL="$COMPARE_GUEST_DIR/Image"
 label="042-m1-abba-$index-$version"
 [ ! -e "benchmarks/results/$label.csv" ]
 echo "$index-$version" > "$BASE/phase"
 echo "BEGIN $label $(date -u)"
 scripts/capped.sh 1500 bash .logs/042/abba-run.sh --target lighter --label "$label" --reps 3 --cases 'npm-install pnpm-install yarn-install' > "$BASE/$label.log" 2>&1
 cp benchmarks/results/lighter-boot.log "$BASE/$label-boot.log"
 if grep -Ei 'rcu.*stall|soft lockup|hard LOCKUP|BUG:|kernel panic|Oops:' "$BASE/$label-boot.log"; then exit 1; fi
 python3 - "$label" <<'PY'
import csv,sys,collections,pathlib
rows=list(csv.DictReader(pathlib.Path(f'benchmarks/results/{sys.argv[1]}.csv').open()))
assert collections.Counter(r['case'] for r in rows)==dict.fromkeys(['npm-install','pnpm-install','yarn-install'],3)
assert all(float(r['ms'])>=0 for r in rows)
PY
 cp "$BASE/report-backup.md" benchmarks/RESULTS.md
 git diff --quiet HEAD
 echo "END $label $(date -u)"
 sleep 20
done
echo complete > "$BASE/phase"
echo 'PASS: M1 alternating package-install comparison complete'

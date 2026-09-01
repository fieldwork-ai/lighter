#!/usr/bin/env bash
# The fast half of a two-phase benchmark: a minute, not twenty.
#
#   benchmarks/quick.sh              # macOS and the share, small fixture
#   benchmarks/quick.sh --lighter    # just ours
#
# Phase one is for iterating. It runs the same cases as the real suite against
# a fixture a tenth the size — 128 packages and 6,246 files rather than 1,232
# and 66,213 — which is the same shape of work and a tenth of the wait. A
# change that helps shows up here, and one that does nothing shows up as
# nothing, in the time it takes to read the diff you just wrote.
#
# Phase two is `benchmarks/run.sh` on the real fixture, for a group of changes
# already believed to be good. That is where published numbers come from, and
# where an effect this cannot resolve gets settled: a tenth of the tree is a
# tenth of the page-cache pressure and a tenth of the directory sizes, and both
# matter at the margin.
#
# Two things it cannot do.
#
# It cannot arbitrate small differences — the workload cases have a standard
# deviation of a few percent whatever the fixture, so anything below that
# belongs to `benchmarks/latency.sh` (`docs/measuring.md` has the arithmetic).
#
# And it under-reports the read cases badly. `ripgrep` is 91% of native here
# and 844% on the real fixture, because sixty-two megabytes fits comfortably in
# the Mac's own page cache and nine hundred does not — so there is no advantage
# left to win. As a regression detector that is fine, since the ratio is stable
# and a change would move it. As a proxy for the published number it is
# useless, and reading it as one would be reading a cache hit as a filesystem.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CASES="${CASES:-npm-install ripgrep copy-tree}"
REPS="${REPS:-2}"
WHICH="${1:-both}"

run() {
	FIXTURE=small ./benchmarks/run.sh --target "$1" --reps "$REPS" \
		--cases "$CASES" --label "quick-$1" --allow-noisy "${@:2}" 2>&1 \
		| sed -n 's/^==> [a-z-]*: /  /p'
}

started=$SECONDS
if [ "$WHICH" != --lighter ]; then
	printf '\033[1m==> macOS\033[0m\n'
	run native
fi
printf '\033[1m==> the share\033[0m\n'
run lighter

python3 - <<'PY'
import csv, pathlib, statistics
results = pathlib.Path("benchmarks/results")


def median(target, case):
    path = results / f"quick-{target}.csv"
    if not path.exists():
        return None
    values = [int(r["ms"]) for r in csv.DictReader(path.open()) if r["case"] == case]
    return statistics.median(values) if values else None


cases = ["npm-install", "ripgrep", "copy-tree"]
print(f"\n{'case':<14}{'macOS':>10}{'share':>10}{'of native':>12}")
for case in cases:
    host, ours = median("native", case), median("lighter", case)
    if ours is None:
        continue
    ratio = f"{host / ours * 100:.0f}%" if host else "-"
    print(f"{case:<14}{host or '-':>10}{ours:>10}{ratio:>12}")
PY
printf '\n\033[32mquick benchmark: %ds\033[0m\n' "$((SECONDS - started))"

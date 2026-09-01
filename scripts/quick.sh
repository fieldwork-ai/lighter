#!/usr/bin/env bash
# The loop to run on every change. Two minutes, not forty.
#
# The gates are the ledger — they say a milestone is done, they run a package
# manager against a sixty-six-thousand-file tree, and they take the best part
# of an hour. Reaching for them after every edit is how an afternoon goes.
#
# This is what actually answers "did I break it, and did it help": the type
# checker, the unit tests, a real boot, and a few thousand syscalls timed one
# at a time. Everything here is cheap enough to run without thinking about it,
# which is the only property that matters in an inner loop.
#
#   scripts/quick.sh          # check and boot        (~15s)
#   scripts/quick.sh --lat    # and measure latency   (~30s)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
mkdir -p .logs

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
start=$SECONDS

step "Format and lint"
cargo fmt --all
cargo clippy --all-targets -- -D warnings

step "Unit tests"
cargo test --all --quiet 2>&1 | grep -vE '^$|^running|^test result: ok' || true

step "Boot"
scripts/gates/m1-boot.sh </dev/null | tail -3

if [ "${1:-}" = "--lat" ]; then
	step "Latency"
	benchmarks/latency.sh
fi

printf '\n\033[32mquick: %ds\033[0m\n' "$((SECONDS - start))"

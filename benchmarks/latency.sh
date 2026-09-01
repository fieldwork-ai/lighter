#!/usr/bin/env bash
# Per-operation latency, on the Mac and through the share, side by side.
#
#   benchmarks/latency.sh            # both, and the difference
#   benchmarks/latency.sh --lighter  # just ours
#
# This is the inner loop for anything that touches the transport or the
# filesystem server. It boots a machine, runs a few thousand of each syscall
# one at a time, and prints microseconds — about fifteen seconds, against the
# six minutes a workload case costs and the twenty a gate does. Latency is also
# what those changes actually move: a workload number is that latency times a
# request count, plus whatever concurrency hid.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

WHICH="both"
[ "${1:-}" = "--lighter" ] && WHICH=lighter
[ "${1:-}" = "--native" ] && WHICH=native

WORK="$HOME/.lighter-bench/latency"
IMAGE="lighter-bench:1"
rm -rf "$WORK"
mkdir -p "$WORK/cases"
: > "$WORK/.metadata_never_index"
cp benchmarks/cases/op-latency.js "$WORK/cases/"

native=""
if [ "$WHICH" != lighter ]; then
	native="$(DIR="$WORK" OPS="${OPS:-3000}" node "$WORK/cases/op-latency.js")"
fi

ours=""
if [ "$WHICH" != native ]; then
	cargo build --release --example boot -p lighter-vmm >/dev/null
	./scripts/sign.sh target/release/examples/boot >/dev/null
	RUN_DIR="$(mktemp -d -t lighter-latency)"
	LOG="${LIGHTER_LATENCY_LOG:-.logs/latency-boot.log}"
	mkdir -p "$(dirname "$LOG")"
	: > "$LOG"
	target/release/examples/boot \
		--kernel guest/out/Image --disk guest/out/rootfs.ext4 \
		--disk "$RUN_DIR/data.img" --disk-size-gib 16 \
		--net vendor/gvproxy --run-dir "$RUN_DIR" \
		--vsock "$RUN_DIR/docker.sock:2375" \
		--share "lat:$WORK" --no-tty --cpus 8 --memory-mib 8192 \
		--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s) lighter.share=lat:/mnt/lat ${LIGHTER_CMDLINE_EXTRA:-}" \
		>"$LOG" 2>&1 &
	VMM=$!
	trap 'kill -9 $VMM 2>/dev/null || true; rm -rf "$RUN_DIR"' EXIT
	waited=0
	until grep -q "AGENT listening" "$LOG" 2>/dev/null; do
		kill -0 "$VMM" 2>/dev/null || { echo "the VMM exited during boot" >&2; tail -20 "$LOG" >&2; exit 1; }
		[ "$waited" -lt 180 ] || { echo "lighter did not come up" >&2; tail -20 "$LOG" >&2; exit 1; }
		sleep 1; waited=$((waited + 1))
	done
	export DOCKER_HOST="unix://$RUN_DIR/docker.sock"
	docker build -q -t "$IMAGE" benchmarks >/dev/null
	ours="$(docker run --rm -v /mnt/lat:/work -e DIR=/work -e "OPS=${OPS:-3000}" "$IMAGE" \
		node /work/cases/op-latency.js)"
fi

printf '\n%-14s %10s %10s %10s\n' "operation" "macOS" "lighter" "overhead"
value() { echo "$1" | awk -v k="$2" '$1 == "US" && $2 == k { print $3 }'; }
for op in create+close stat-cached stat-missing write-4k unlink; do
	a="$(value "$native" "$op")"
	b="$(value "$ours" "$op")"
	if [ -n "$a" ] && [ -n "$b" ]; then
		printf '%-14s %9sus %9sus %9sus\n' "$op" "$a" "$b" \
			"$(awk -v a="$a" -v b="$b" 'BEGIN { printf "%+.2f", b - a }')"
	else
		printf '%-14s %9sus %9sus %10s\n' "$op" "${a:--}" "${b:--}" "-"
	fi
done
echo

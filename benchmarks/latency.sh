#!/usr/bin/env bash
# Per-operation latency, on the Mac and through the share, side by side.
#
#   benchmarks/latency.sh              # both, and the difference
#   benchmarks/latency.sh --lighter    # just ours
#   REPEAT=5 benchmarks/latency.sh     # five boots, with the spread
#
# # Read the spread before the number
#
# A single boot of this is repeatable to about a microsecond within itself and
# to five between boots: measured on `create+close`, three separate runs gave
# 36.6, 42.7 and 32.6 microseconds. Almost none of that is sampling — three
# thousand operations is already far more than enough — so taking more per boot
# buys nothing. It is boot-to-boot state, and the only way past it is more
# boots.
#
# Which is why `REPEAT` exists and why the spread is printed next to the
# median. A change smaller than the spread has not been measured, however
# confident the difference of two medians looks, and the way an afternoon
# disappears is by believing one anyway.
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

REPEAT="${REPEAT:-1}"

# One boot, one pass, printing the case's own `US <name> <micros>` lines.
measure_once() {
	local run_dir log vmm waited
	run_dir="$(mktemp -d -t lighter-latency)"
	log="${LIGHTER_LATENCY_LOG:-.logs/latency-boot.log}"
	mkdir -p "$(dirname "$log")"
	: > "$log"
	target/release/examples/boot \
		--kernel guest/out/Image --disk guest/out/rootfs.ext4 \
		--disk "$run_dir/data.img" --disk-size-gib 16 \
		--net vendor/gvproxy --run-dir "$run_dir" \
		--vsock "$run_dir/docker.sock:2375" \
		--share "lat:$WORK" --no-tty --cpus 8 --memory-mib 8192 \
		--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s) lighter.share=lat:/mnt/lat ${LIGHTER_CMDLINE_EXTRA:-}" \
		>"$log" 2>&1 &
	vmm=$!
	waited=0
	until grep -q "AGENT listening" "$log" 2>/dev/null; do
		kill -0 "$vmm" 2>/dev/null || { echo "the VMM exited during boot" >&2; tail -20 "$log" >&2; return 1; }
		[ "$waited" -lt 180 ] || { echo "lighter did not come up" >&2; tail -20 "$log" >&2; return 1; }
		sleep 1; waited=$((waited + 1))
	done
	DOCKER_HOST="unix://$run_dir/docker.sock" docker build -q -t "$IMAGE" benchmarks >/dev/null
	# `GUEST_LOCAL=1` points the case at the guest's own disk instead of the
	# share. It is not a comparison anybody ships; it is the decomposition,
	# and it settles whether a number is about the shared filesystem or about
	# the virtual machine underneath it — which are different problems with
	# different fixes, and look identical from the outside.
	local target=/work
	[ "${GUEST_LOCAL:-0}" = 1 ] && target=/tmp
	DOCKER_HOST="unix://$run_dir/docker.sock" docker run --rm -v /mnt/lat:/work \
		-e "DIR=$target" -e "OPS=${OPS:-3000}" -e "ONLY=${ONLY:-}" \
		-e "PARALLEL=${PARALLEL:-16}" -e "UV_THREADPOOL_SIZE=${PARALLEL:-16}" "$IMAGE" \
		node /work/cases/op-latency.js
	kill -9 "$vmm" 2>/dev/null || true
	wait "$vmm" 2>/dev/null || true
	rm -rf "$run_dir"
}

native=""
if [ "$WHICH" != lighter ]; then
	DIR="$WORK" OPS="${OPS:-3000}" ONLY="${ONLY:-}" PARALLEL="${PARALLEL:-16}" \
		UV_THREADPOOL_SIZE="${PARALLEL:-16}" node "$WORK/cases/op-latency.js" >/dev/null
	for _ in $(seq 1 "$REPEAT"); do
		native+="$(DIR="$WORK" OPS="${OPS:-3000}" ONLY="${ONLY:-}" PARALLEL="${PARALLEL:-16}" UV_THREADPOOL_SIZE="${PARALLEL:-16}" node "$WORK/cases/op-latency.js")"$'\n'
	done
fi

ours=""
if [ "$WHICH" != native ]; then
	cargo build --release --example boot -p lighter-vmm >/dev/null
	./scripts/sign.sh target/release/examples/boot >/dev/null
	# The first boot is a warm-up and is thrown away: it pays for a cold host
	# page cache and whatever the image build left behind, and counting it
	# puts a spread of two hundred microseconds on a number that varies by
	# five. The suite warms its package caches for the same reason.
	measure_once >/dev/null || exit 1
	for _ in $(seq 1 "$REPEAT"); do
		ours+="$(measure_once)"$'\n'
	done
fi

# Median and spread, because a difference smaller than the spread has not been
# measured. `awk` rather than anything cleverer: this runs on a stock Mac.
summarise() {
	echo "$1" | awk -v k="$2" '
		$1 == "US" && $2 == k { v[n++] = $3 }
		END {
			if (n == 0) { print "- -"; exit }
			for (i = 0; i < n; i++)
				for (j = i + 1; j < n; j++)
					if (v[j] < v[i]) { t = v[i]; v[i] = v[j]; v[j] = t }
			printf "%.2f %.2f", v[int((n - 1) / 2)], v[n - 1] - v[0]
		}'
}

if [ "$REPEAT" -gt 1 ]; then
	printf '\n%d boots each; the spread is worst-to-best across them.\n' "$REPEAT"
fi
printf '\n%-14s %10s %10s %10s %9s\n' "operation" "macOS" "lighter" "overhead" "spread"
for op in create+close create-parallel stat-cached stat-missing write-4k write-chunked unlink; do
	read -r a _ <<<"$(summarise "$native" "$op")"
	read -r b spread <<<"$(summarise "$ours" "$op")"
	if [ "$a" != "-" ] && [ "$b" != "-" ]; then
		printf '%-14s %9sus %9sus %9sus %8sus\n' "$op" "$a" "$b" \
			"$(awk -v a="$a" -v b="$b" 'BEGIN { printf "%+.2f", b - a }')" "$spread"
	elif [ "$b" != "-" ]; then
		printf '%-14s %10s %9sus %10s %8sus\n' "$op" "-" "$b" "-" "$spread"
	elif [ "$a" != "-" ]; then
		printf '%-14s %9sus %10s %10s %9s\n' "$op" "$a" "-" "-" "-"
	fi
done
echo

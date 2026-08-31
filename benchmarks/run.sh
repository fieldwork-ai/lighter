#!/usr/bin/env bash
# The reproducible benchmark suite.
#
#   benchmarks/run.sh --target native --reps 5
#   benchmarks/run.sh --target lighter --reps 5
#   benchmarks/run.sh --target orbstack --reps 5
#
# Every target runs the same case scripts against the same fixture on the same
# machine. What differs is only how the directory gets to the process: on
# `native` it is the Mac's own disk, everywhere else it is a bind mount through
# whatever that runtime uses for file sharing.
#
# # Rules this script exists to enforce
#
# **The caches are warmed, and the warming is not timed.** An npm install that
# downloads is measuring the network. Each target gets its own package cache on
# its own storage, warmed by an untimed run, so the timed run is filesystem work
# and nothing else.
#
# **The median is reported, not the mean or the best.** The best run is a
# claim about the machine being idle; the mean is dragged around by one
# scheduling hiccup.
#
# **Nothing is hand-edited.** Results go to CSV; the report is generated from
# the CSV. A number in the README that no CSV supports is a bug.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET=""
LABEL=""
REPS=3
CASES="npm-install ripgrep find-walk copy-tree watch-latency"
IMAGE="lighter-bench:1"
KEEP=0

while [ $# -gt 0 ]; do
	case "$1" in
	--target) TARGET="$2"; shift 2 ;;
	--reps)   REPS="$2"; shift 2 ;;
	--cases)  CASES="$2"; shift 2 ;;
	--keep)   KEEP=1; shift ;;
	--label)  LABEL="$2"; shift 2 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done
[ -n "$TARGET" ] || { echo "--target is required (native|lighter|colima|orbstack|docker-desktop)" >&2; exit 2; }

WORK="${LIGHTER_BENCH_WORK:-$HOME/.lighter-bench/$TARGET}"
# A label lets one target be measured twice under different settings without
# the second run overwriting the first — which is how the speed gate compares
# caching on against caching off.
RESULTS="benchmarks/results/${LABEL:-$TARGET}.csv"
VMM_PID=""
HELPER_PID=""

# Milliseconds since the epoch, from a runtime every target already needs.
# macOS `date` has no %N, and the shell has no sub-second clock at all.
now_ms() { node -e 'process.stdout.write(String(Date.now()))'; }

cleanup() {
	[ -n "$HELPER_PID" ] && kill "$HELPER_PID" 2>/dev/null || true
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	[ "$KEEP" -eq 1 ] || rm -rf "$WORK"
}
trap cleanup EXIT

# ------------------------------------------------------------------ fixture --

prepare_work() {
	rm -rf "$WORK"
	mkdir -p "$WORK/npm" "$WORK/cases"
	cp benchmarks/fixtures/npm/package.json "$WORK/npm/"
	[ -f benchmarks/fixtures/npm/package-lock.json ] \
		&& cp benchmarks/fixtures/npm/package-lock.json "$WORK/npm/"
	cp benchmarks/cases/*.sh benchmarks/cases/*.js "$WORK/cases/"
	chmod +x "$WORK/cases"/*.sh

	printf '' > "$WORK/request"
	printf '' > "$WORK/reply"
}

# ------------------------------------------------------------------ targets --
#
# Each target defines `run_case <name>`, which executes a case script with
# $WORK visible to it, and prints whatever the script printed.

setup_native() {
	command -v node >/dev/null || { echo "node is required for the native target" >&2; exit 1; }
	command -v rg   >/dev/null || { echo "ripgrep is required for the native target" >&2; exit 1; }
}

# Every target runs the same runner, which does its own timing and prints one
# `TIME_MS` line per repetition.
#
# `$2` is where the work directory appears to whatever will run it, which is not
# where it appears to us: inside a container it is `/work`. The *existence*
# check has to use our path and the emitted arguments have to use theirs, and
# conflating the two silently selects the generic runner for every case.
runner_args() {
	if [ -f "$WORK/cases/$1.js" ]; then
		echo "$2/cases/$1.js"
	else
		echo "$2/cases/runner.js $1"
	fi
}

run_case_native() {
	# shellcheck disable=SC2046
	WORK="$WORK" REPS="$REPS" node $(runner_args "$1" "$WORK")
}

# Every container target is the same command against a different daemon; only
# the context changes, which is exactly the point of comparing them.
docker_context() {
	case "$TARGET" in
	colima)         echo "colima" ;;
	orbstack)       echo "orbstack" ;;
	docker-desktop) echo "desktop-linux" ;;
	*)              echo "" ;;
	esac
}

setup_container() {
	local ctx; ctx="$(docker_context)"
	DOCKER_ARGS=()
	[ -n "$ctx" ] && DOCKER_ARGS=(--context "$ctx")
	docker "${DOCKER_ARGS[@]}" build -q -t "$IMAGE" benchmarks >/dev/null
	# The package cache lives on the runtime's own storage, not on the share:
	# putting it on the share would make every target's cache as slow as its
	# file sharing, which is a second measurement smuggled into the first.
	docker "${DOCKER_ARGS[@]}" volume create "lighter-bench-npm-$TARGET" >/dev/null
}

run_case_container() {
	local script
	script="$(runner_args "$1" /work)"
	# shellcheck disable=SC2086
	docker "${DOCKER_ARGS[@]}" run --rm \
		-v "$WORK:/work" \
		-v "lighter-bench-npm-$TARGET:/root/.npm" \
		-e WORK=/work \
		-e "REPS=$REPS" \
		"$IMAGE" node $script
}

setup_lighter() {
	KERNEL="guest/out/Image"
	ROOTFS="guest/out/rootfs.ext4"
	GVPROXY="${GVPROXY:-vendor/gvproxy}"
	BIN="target/release/examples/boot"
	[ -f "$KERNEL" ] || ./guest/kernel/build.sh
	[ -f "$ROOTFS" ] || ./guest/rootfs/build.sh
	# Release, because a debug VMM is measuring the compiler.
	cargo build --release --example boot -p lighter-vmm
	./scripts/sign.sh "$BIN" >/dev/null

	RUN_DIR="$(mktemp -d -t lighter-bench)"
	SOCKET="$RUN_DIR/docker.sock"
	# Kept outside the run directory so it survives the cleanup: it carries the
	# VMM's own log, which is where a filesystem histogram appears.
	BOOT_LOG="benchmarks/results/$TARGET-boot.log"
	# Truncated *here*, in the parent, not by the redirection below.
	#
	# The redirection happens in the forked child, which is asynchronous: the
	# loop that waits for "AGENT listening" would otherwise read the previous
	# run's log, find the line already there, and declare a machine ready that
	# has not started booting. The symptom is a docker client reporting no
	# socket, several steps later and with nothing to connect it to this.
	: > "$BOOT_LOG"
	"$BIN" \
		--kernel "$KERNEL" \
		--disk "$ROOTFS" \
		--disk "$RUN_DIR/data.img" --disk-size-gib 32 \
		--net "$GVPROXY" --run-dir "$RUN_DIR" \
		--vsock "$SOCKET:2375" \
		--share "bench:$WORK" \
		--no-tty --cpus 8 --memory-mib 8192 \
		--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s) lighter.share=bench:/mnt/bench" \
		>"$BOOT_LOG" 2>&1 &
	VMM_PID=$!
	disown "$VMM_PID" 2>/dev/null || true

	local waited=0
	while ! grep -q "AGENT listening" "$BOOT_LOG" 2>/dev/null; do
		kill -0 "$VMM_PID" 2>/dev/null || { echo "the VMM exited during boot" >&2; tail -20 "$BOOT_LOG" >&2; exit 1; }
		[ "$waited" -lt 180 ] || { echo "lighter did not come up" >&2; tail -20 "$BOOT_LOG" >&2; exit 1; }
		sleep 1; waited=$((waited + 1))
	done
	export DOCKER_HOST="unix://$SOCKET"
	DOCKER_ARGS=()
	docker build -q -t "$IMAGE" benchmarks >/dev/null
	docker volume create "lighter-bench-npm-$TARGET" >/dev/null
	# The share is mounted at a different path inside this guest than the host
	# path, so the bind mount names the guest path.
	SHARE_MOUNT="/mnt/bench"
}

run_case_lighter() {
	local script
	script="$(runner_args "$1" /work)"
	# shellcheck disable=SC2086
	docker run --rm \
		-v "$SHARE_MOUNT:/work" \
		-v "lighter-bench-npm-$TARGET:/root/.npm" \
		-e WORK=/work \
		-e "REPS=$REPS" \
		"$IMAGE" node $script
}

# ------------------------------------------------------------------- driver --

# Answers the watch-latency case from the host side, as fast as it can notice.
start_watch_helper() {
	( last=""
	  while true; do
		current="$(cat "$WORK/request" 2>/dev/null || true)"
		if [ -n "$current" ] && [ "$current" != "$last" ]; then
			printf '%s' "$current" > "$WORK/reply"
			last="$current"
		fi
	  done ) &
	HELPER_PID=$!
	# Disowned so that killing it at the end of the case is silent: bash would
	# otherwise report the job's death in the middle of a passing run.
	disown "$HELPER_PID" 2>/dev/null || true
}

prepare_work
case "$TARGET" in
native)                          setup_native;     run_case() { run_case_native "$@"; } ;;
lighter)                         setup_lighter;    run_case() { run_case_lighter "$@"; } ;;
colima|orbstack|docker-desktop)  setup_container;  run_case() { run_case_container "$@"; } ;;
*) echo "unknown target: $TARGET" >&2; exit 2 ;;
esac

mkdir -p "$(dirname "$RESULTS")"
echo "case,rep,ms" > "$RESULTS"

# The lockfile is committed, not generated: `npm ci` installs exactly what it
# says, so every target and every run installs a byte-identical tree. Generating
# it here would make the benchmark depend on whatever npm resolved that morning.
[ -f benchmarks/fixtures/npm/package-lock.json ] || {
	echo "benchmarks/fixtures/npm/package-lock.json is missing; regenerate it with" >&2
	echo "  (cd benchmarks/fixtures/npm && npm install --package-lock-only)" >&2
	exit 1
}

echo "==> $TARGET: warming caches (not timed)"
REPS=1 run_case npm-install >/dev/null 2>&1 || true

for name in $CASES; do
	printf '==> %s: %s' "$TARGET" "$name"
	[ "$name" = watch-latency ] && start_watch_helper
	rep=0
	# The case is run once and reports every repetition itself, so container
	# start-up and a cold cache are paid for once and are not in any figure.
	#
	# Output is captured rather than piped so that a case which produced no
	# measurement can say why. Silently printing a dash for a case that has
	# been failing for a week is the worst thing a benchmark harness can do.
	CASE_OUT="$(mktemp -t lighter-case)"
	run_case "$name" >"$CASE_OUT" 2>&1 || true
	while read -r ms; do
		rep=$((rep + 1))
		printf ' %s' "$ms"
		echo "$name,$rep,$ms" >> "$RESULTS"
	done < <(sed -n 's/^TIME_MS //p' "$CASE_OUT")
	if [ "$rep" -eq 0 ]; then
		printf ' no measurement:'
		printf '\n'
		tail -25 "$CASE_OUT" | sed 's/^/      /'
	fi
	rm -f "$CASE_OUT"
	printf '\n'
	if [ -n "$HELPER_PID" ]; then kill "$HELPER_PID" 2>/dev/null || true; HELPER_PID=""; fi
done

echo
echo "==> results in $RESULTS"

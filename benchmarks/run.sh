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
CASES="npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf watch-latency"
# Cases that read a package tree rather than making one. They run after the
# installs, on a tree materialized once by npm — which installer produced it
# changes what they see, and pnpm in particular builds a farm of symlinks.
TREE_CASES=" ripgrep find-walk copy-tree rm-rf "
IMAGE="lighter-bench:1"
KEEP=0
ALLOW_NOISY=0
# Where the fixture lives for a container target.
#
#   share — the Mac's own directory, bind-mounted in. What the suite is for.
#   guest — a volume on the runtime's own disk, with no host filesystem in the
#           picture at all.
#
# The second is not a comparison anybody ships, it is the decomposition: it
# separates what the virtual machine costs from what sharing a filesystem with
# it costs, and without that split every number is a sum of two things and
# tuning aims at whichever one you guessed.
WHERE="share"

while [ $# -gt 0 ]; do
	case "$1" in
	--target) TARGET="$2"; shift 2 ;;
	--reps)   REPS="$2"; shift 2 ;;
	--cases)  CASES="$2"; shift 2 ;;
	--keep)   KEEP=1; shift ;;
	--label)  LABEL="$2"; shift 2 ;;
	--allow-noisy) ALLOW_NOISY=1; shift ;;
	--where)  WHERE="$2"; shift 2 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done
[ -n "$TARGET" ] || { echo "--target is required (native|lighter|colima|orbstack|docker-desktop)" >&2; exit 2; }

WORK="${LIGHTER_BENCH_WORK:-$HOME/.lighter-bench/$TARGET}"
# A label lets one target be measured twice under different settings without
# the second run overwriting the first — which is how the speed gate compares
# caching on against caching off.
RESULTS="benchmarks/results/${LABEL:-$TARGET}.csv"
[ "$WHERE" = share ] || [ "$WHERE" = guest ] || { echo "--where wants share or guest, got $WHERE" >&2; exit 2; }
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

# --------------------------------------------------------------- exclusivity --

# Refuses to measure while another virtual machine is running.
#
# Not fussiness. A second hypervisor on the same laptop competes for the same
# cores and the same page cache, and it does not compete evenly: the native
# target and the guest target feel it differently, so the *ratio* between them
# moves. These numbers are the whole claim, and a claim that depends on what
# else the machine happened to be doing is not one.
#
# The obvious irony is not lost: for now, the thing most likely to be running
# is the container runtime that builds lighter's own guest. `scripts/dogfood.sh`
# is how that stops being true.
check_exclusive() {
	local noisy=()
	# The runtime being measured is not noise — it is the measurement. Every
	# check below skips itself when it is the target, which is the difference
	# between a guard and a refusal to ever benchmark anything.
	#
	# Colima and Lima both run their VM through the same Apple XPC service, so
	# the process to look for is the hypervisor rather than the CLI.
	if [ "$TARGET" != colima ] && pgrep -f "limactl hostagent" >/dev/null 2>&1; then
		noisy+=("Colima or Lima (limactl hostagent)")
	fi
	if [ "$TARGET" != docker-desktop ] && pgrep -x "com.docker.backend" >/dev/null 2>&1; then
		noisy+=("Docker Desktop")
	fi
	if [ "$TARGET" != orbstack ] \
		&& { pgrep -x "OrbStack Helper" >/dev/null 2>&1 || pgrep -x "xbin" >/dev/null 2>&1; }; then
		noisy+=("OrbStack")
	fi
	# Ours too. The `lighter` target starts a machine of its own, and a second
	# one already running competes with it for exactly the resources being
	# measured — which is the same objection as for anybody else's. It is not
	# skipped for the `lighter` target: this looks for a *daemon* machine that
	# `lighter start` left running, and the suite starts its own.
	if [ -f "$HOME/.lighter/lighter.pid" ] \
		&& kill -0 "$(cat "$HOME/.lighter/lighter.pid" 2>/dev/null)" 2>/dev/null; then
		noisy+=("a lighter machine (stop it with \`lighter stop\`)")
	fi
	[ "${#noisy[@]}" -eq 0 ] && return 0

	echo "==> Another virtual machine is running:" >&2
	printf '      %s
' "${noisy[@]}" >&2
	if [ "$ALLOW_NOISY" -eq 1 ]; then
		echo "    --allow-noisy given; the numbers will not be comparable." >&2
		return 0
	fi
	cat >&2 <<-EOF

	    Stop it and run again, or pass --allow-noisy to measure anyway.
	    A second hypervisor moves the ratio between the native and guest
	    figures, which is the only thing this suite reports.
	EOF
	exit 1
}

check_exclusive

# ------------------------------------------------------------------ fixture --

prepare_work() {
	rm -rf "$WORK"
	mkdir -p "$WORK/npm" "$WORK/cases"
	# Spotlight indexes anything under $HOME, and a package tree is sixty
	# thousand files it very much wants to read, thumbnail and index. Left
	# alone it costs more CPU than the benchmark does and lands in the middle
	# of the measurement. This marker is how macOS is told not to.
	: > "$WORK/.metadata_never_index"
	# Every lockfile, so each package manager installs the identical tree —
	# twice, because one of them does not survive the others.
	#
	# `npm ci` rewrites a `yarn.lock` it finds beside a `package-lock.json`,
	# into npm's own dialect: registry.npmjs.org URLs and no `#sha` fragment.
	# It is a perfectly good file and yarn 1.x cannot parse a word of it, so
	# after one npm run the yarn case fails with a syntax error at line 7784 of
	# a lockfile that is pristine in the repository. That took a while to stop
	# looking like a filesystem returning the wrong bytes.
	#
	# So `fixture/` is the pristine copy and `npm/` is the working one, and
	# every install case restores what it needs before it runs. A benchmark
	# whose cases can destroy each other's inputs measures the order they ran
	# in.
	mkdir -p "$WORK/fixture"
	cp benchmarks/fixtures/npm/package.json \
		benchmarks/fixtures/npm/package-lock.json \
		benchmarks/fixtures/npm/pnpm-lock.yaml \
		benchmarks/fixtures/npm/yarn.lock \
		"$WORK/fixture/"
	cp "$WORK"/fixture/* "$WORK/npm/"
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

# What gets mounted at /work inside a container.
#
# On `share` it is the host directory, which is the measurement. On `guest` it
# is a named volume seeded from that directory once, so the case scripts see an
# identical tree with no host filesystem underneath it.
work_mount() {
	if [ "$WHERE" = guest ]; then
		echo "lighter-bench-work-$TARGET"
	else
		echo "$SHARE_MOUNT"
	fi
}

# Copies the fixture onto the runtime's own disk, for `--where guest`.
#
# Not timed, and deliberately done with the same `docker cp` for every target:
# the seeding is setup, and a target that seeded faster would be flattering
# itself with work the cases are not measuring.
seed_guest_volume() {
	[ "$WHERE" = guest ] || return 0
	local volume; volume="lighter-bench-work-$TARGET"
	# Bash 3.2 — which is the one macOS ships — treats an empty array under
	# `set -u` as unset, so an unadorned "${DOCKER_ARGS[@]}" aborts the run for
	# every target that needs no context flag.
	local dk=(docker); [ "${#DOCKER_ARGS[@]}" -eq 0 ] || dk=(docker "${DOCKER_ARGS[@]}")
	"${dk[@]}" volume rm -f "$volume" >/dev/null 2>&1 || true
	"${dk[@]}" volume create "$volume" >/dev/null
	local id
	id="$("${dk[@]}" create -v "$volume:/work" "$IMAGE" true)"
	"${dk[@]}" cp "$WORK/." "$id:/work"
	"${dk[@]}" rm "$id" >/dev/null
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
	SHARE_MOUNT="$WORK"
	DOCKER_ARGS=()
	[ -n "$ctx" ] && DOCKER_ARGS=(--context "$ctx")
	docker "${DOCKER_ARGS[@]}" build -q -t "$IMAGE" benchmarks >/dev/null
	# The package cache lives on the runtime's own storage, not on the share:
	# putting it on the share would make every target's cache as slow as its
	# file sharing, which is a second measurement smuggled into the first.
	for cache in npm pnpm yarn; do
		docker "${DOCKER_ARGS[@]}" volume create "lighter-bench-$cache-$TARGET" >/dev/null
	done
	seed_guest_volume
}

run_case_container() {
	local script
	script="$(runner_args "$1" /work)"
	# shellcheck disable=SC2086
	docker "${DOCKER_ARGS[@]}" run --rm \
		-v "$(work_mount)":/work \
		-v "lighter-bench-npm-$TARGET:/root/.npm" \
		-v "lighter-bench-pnpm-$TARGET:/root/.local/share/pnpm/store" \
		-v "lighter-bench-yarn-$TARGET:/usr/local/share/.cache/yarn" \
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
		--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init lighter.time=$(date +%s) lighter.share=bench:/mnt/bench ${LIGHTER_CMDLINE_EXTRA:-}" \
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
	for cache in npm pnpm yarn; do
		docker volume create "lighter-bench-$cache-$TARGET" >/dev/null
	done
	# The share is mounted at a different path inside this guest than the host
	# path, so the bind mount names the guest path.
	SHARE_MOUNT="/mnt/bench"
	seed_guest_volume
}

run_case_lighter() {
	local script
	script="$(runner_args "$1" /work)"
	# shellcheck disable=SC2086
	docker run --rm \
		-v "$(work_mount)":/work \
		-v "lighter-bench-npm-$TARGET:/root/.npm" \
		-v "lighter-bench-pnpm-$TARGET:/root/.local/share/pnpm/store" \
		-v "lighter-bench-yarn-$TARGET:/usr/local/share/.cache/yarn" \
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
# Each package manager populates its own cache once. An install that downloads
# is measuring the network, and the three do not share a cache.
for warm in npm-install pnpm-install yarn-install; do
	case " $CASES " in
	*" $warm "*) REPS=1 run_case "$warm" >/dev/null 2>&1 || true ;;
	esac
done
# `rm-rf` and `copy-tree` need a tree to work on, whichever installs ran.
case " $CASES " in
*rm-rf*|*copy-tree*|*ripgrep*|*find-walk*)
	[ -d "$WORK/npm/node_modules" ] || REPS=1 run_case npm-install >/dev/null 2>&1 || true
	;;
esac

materialized=0
for name in $CASES; do
	# A tree the previous case deleted is a case that measures nothing and
	# says so in milliseconds. This is not timed.
	case "$TREE_CASES" in
	*" $name "*)
		if [ "$materialized" -eq 0 ] || [ ! -d "$WORK/npm/node_modules" ]; then
			printf '==> %s: materializing the package tree\n' "$TARGET"
			REPS=1 run_case npm-install >/dev/null 2>&1 || true
			materialized=1
		fi
		;;
	esac

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
		# Both ends, not just the tail. A package manager prints the reason
		# first and a Node stack trace last, and a tail alone shows only the
		# stack — which says a command failed and never says why.
		head -20 "$CASE_OUT" | sed 's/^/      /'
		[ "$(wc -l < "$CASE_OUT")" -le 40 ] || {
			printf '      ...\n'
			tail -20 "$CASE_OUT" | sed 's/^/      /'
		}
	elif [ "$name" != watch-latency ]; then
		# A case that finishes instantly finished because there was nothing
		# there. Saying so is the difference between a bug and a headline.
		fastest="$(awk -F, -v w="$name" '$1 == w { print $3 }' "$RESULTS" | sort -n | head -1)"
		if [ "${fastest:-1}" -lt 5 ]; then
			printf '  <- implausible; the fixture was probably missing'
		fi
	fi
	rm -f "$CASE_OUT"
	printf '\n'
	if [ -n "$HELPER_PID" ]; then kill "$HELPER_PID" 2>/dev/null || true; HELPER_PID=""; fi
done

echo
echo "==> results in $RESULTS"

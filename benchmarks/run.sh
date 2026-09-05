#!/usr/bin/env bash
# The reproducible benchmark suite.
#
#   benchmarks/run.sh --target native --reps 3
#   benchmarks/run.sh --target lighter --reps 3
#   benchmarks/run.sh --target orbstack --reps 3
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
CASES="npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf cpu-sha256 container-start watch-latency memory net-tcp-egress net-tcp-egress-r net-tcp-port net-tcp-port-r net-udp net-connect-rate net-http-latency net-dns power-idle boot"
# Cases that read a package tree rather than making one. They run after the
# installs, on a tree materialized once by npm — which installer produced it
# changes what they see, and pnpm in particular builds a farm of symlinks.
TREE_CASES=" ripgrep find-walk copy-tree rm-rf "
IMAGE="lighter-bench:1"
# What the guest is given. Defaults suit the machine this was written on;
# `BENCH_MEMORY_MIB` and `BENCH_CPUS` are how it runs somewhere smaller.
#
# The memory is a ceiling rather than an allocation — the guest reports free
# pages back and the host reclaims them — so the number to set is what the
# workload's page cache wants, not what the machine can spare.
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
# Which architecture the container is. `amd64` runs the same cases in the
# x86-64 build of the image, `--platform linux/amd64`, which on Apple silicon
# means under Rosetta (or whatever the runtime substitutes for it): the
# translator's cost on a real install, beside the native number.
ARCH="arm64"
# Which fixture to install.
#
#   npm    1,232 packages, 66,213 files, 908 MB — what the published table uses
#   small  128 packages, 6,246 files, 62 MB — the same shape at a tenth of it
#
# The small one exists because iteration speed is a property of the harness. An
# install of the big fixture is fifteen seconds, so comparing two settings at
# two repetitions is five minutes of waiting for a question that is usually
# answered in the first ten seconds. Same file shape — thousands of small
# JavaScript files — so the operation mix is the same and only the wait is not.
#
# It is not for published numbers. A tenth of the tree is a tenth of the page
# cache pressure and a tenth of the directory sizes, and those matter at the
# margin; the table is measured on the real one.
FIXTURE="${FIXTURE:-npm}"

while [ $# -gt 0 ]; do
	case "$1" in
	--target) TARGET="$2"; shift 2 ;;
	--reps)   REPS="$2"; shift 2 ;;
	--cases)  CASES="$2"; shift 2 ;;
	--keep)   KEEP=1; shift ;;
	--label)  LABEL="$2"; shift 2 ;;
	--allow-noisy) ALLOW_NOISY=1; shift ;;
	--where)  WHERE="$2"; shift 2 ;;
	--arch)   ARCH="$2"; shift 2 ;;
	--fixture) FIXTURE="$2"; shift 2 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done
[ -n "$TARGET" ] || { echo "--target is required (native|lighter|colima|orbstack|docker-desktop)" >&2; exit 2; }

WORK="${LIGHTER_BENCH_WORK:-$HOME/.lighter-bench/$TARGET}"
# A label lets one target be measured twice under different settings without
# the second run overwriting the first — which is how the speed gate compares
# caching on against caching off.
RESULTS="benchmarks/results/${LABEL:-$TARGET}.csv"
# A run on the runtime's own disk is its own result set, not a rerun of the
# share: the report reads `<target>-guest.csv` for that section, and an
# unlabelled guest run used to overwrite the share's numbers.
[ "$WHERE" != guest ] || [ -n "$LABEL" ] || RESULTS="benchmarks/results/$TARGET-guest.csv"
# The fixture is part of what a number means, so a run on the small one does
# not overwrite the published CSV unless it was asked to by name.
[ "$FIXTURE" = npm ] || [ -n "$LABEL" ] || RESULTS="benchmarks/results/$TARGET-$FIXTURE.csv"
[ "$WHERE" = share ] || [ "$WHERE" = guest ] || { echo "--where wants share or guest, got $WHERE" >&2; exit 2; }
[ "$ARCH" = arm64 ] || [ "$ARCH" = amd64 ] || { echo "--arch wants arm64 or amd64, got $ARCH" >&2; exit 2; }
# An amd64 run is its own result set too, `<target>-amd64.csv`, whichever
# disk it ran on; the report's x86-64 table reads it.
PLATFORM=()
CACHE_SUFFIX=""
if [ "$ARCH" = amd64 ]; then
	PLATFORM=(--platform linux/amd64)
	CACHE_SUFFIX="-amd64"
	[ -n "$LABEL" ] || RESULTS="benchmarks/results/$TARGET-amd64.csv"
fi
VMM_PID=""
HELPER_PID=""
ROOTFS=""
RUN_DIR=""
CASE_OUT=""

# Milliseconds since the epoch, from a runtime every target already needs.
# macOS `date` has no %N, and the shell has no sub-second clock at all.
now_ms() { node -e 'process.stdout.write(String(Date.now()))'; }

# Everything the run made under $TMPDIR goes too: the rootfs clone and the
# run directory, whose data disk is a sparse file that has grown to whatever
# the guest wrote. A run that was killed leaves them otherwise, and nine
# hundred such leftovers once filled the disk of an unsupervised machine.
cleanup() {
	net_teardown 2>/dev/null || true
	[ -n "$HELPER_PID" ] && kill "$HELPER_PID" 2>/dev/null || true
	[ -n "$VMM_PID" ] && kill -9 "$VMM_PID" 2>/dev/null || true
	[ "$KEEP" -eq 1 ] || rm -rf "$WORK"
	[ -z "$RUN_DIR" ] || rm -rf "$RUN_DIR"
	[ -z "$ROOTFS" ] || rm -f "$ROOTFS"
	[ -z "$CASE_OUT" ] || rm -f "$CASE_OUT"
}
trap cleanup EXIT
# An EXIT trap does not run for a fatal signal; the cap kills with TERM.
trap 'exit 143' INT TERM

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
	local from="benchmarks/fixtures/$FIXTURE"
	[ -d "$from" ] || { echo "no such fixture: $FIXTURE" >&2; exit 2; }
	cp "$from/package.json" "$from/package-lock.json" \
		"$from/pnpm-lock.yaml" "$from/yarn.lock" "$WORK/fixture/"
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
	WORK="$WORK" REPS="$REPS" CASE_TIMEOUT_S="${CASE_TIMEOUT_S:-300}" node $(runner_args "$1" "$WORK")
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
	# The x86-64 build of the image under its own tag: a plain build would
	# replace the native one, and the next native run would measure Rosetta.
	[ "$ARCH" = arm64 ] || IMAGE="$IMAGE-$ARCH"
	docker "${DOCKER_ARGS[@]}" build -q ${PLATFORM[@]+"${PLATFORM[@]}"} -t "$IMAGE" benchmarks >/dev/null
	# The package cache lives on the runtime's own storage, not on the share:
	# putting it on the share would make every target's cache as slow as its
	# file sharing, which is a second measurement smuggled into the first.
	# One set per architecture: npm's cache holds prebuilt binaries by arch.
	for cache in npm pnpm yarn; do
		docker "${DOCKER_ARGS[@]}" volume create "lighter-bench-$cache-$TARGET$CACHE_SUFFIX" >/dev/null
	done
	seed_guest_volume
}

run_case_container() {
	local script
	script="$(runner_args "$1" /work)"
	# shellcheck disable=SC2086
	docker "${DOCKER_ARGS[@]}" run --rm ${PLATFORM[@]+"${PLATFORM[@]}"} \
		-v "$(work_mount)":/work \
		-v "lighter-bench-npm-$TARGET$CACHE_SUFFIX:/root/.npm" \
		-v "lighter-bench-pnpm-$TARGET$CACHE_SUFFIX:/root/.local/share/pnpm/store" \
		-v "lighter-bench-yarn-$TARGET$CACHE_SUFFIX:/usr/local/share/.cache/yarn" \
		-e WORK=/work \
		-e "REPS=$REPS" \
		-e "CASE_TIMEOUT_S=${CASE_TIMEOUT_S:-300}" \
		"$IMAGE" node $script
}

# The guest's memory, matched to OrbStack's on this machine when it is
# installed: a copy of a tree that fits under one guest's writeback threshold
# and not the other's is a comparison of memory sizes, not of runtimes.
# `BENCH_MEMORY_MIB` overrides; without OrbStack the default is 8 GiB.
# The guest's disk is sized as the product sizes it: what the Mac has free,
# never under 64 GiB. It is sparse, so the number is a ceiling, but btrfs
# reads it: with 32 GiB a copy of a package tree ticketed its metadata
# reservations and was flushed file by file mid-copy, 1.6 s where 64 GiB
# copied in 1.1 and OrbStack's 320 GB guest disk in 1.05.
bench_disk_gib() {
	if [ -n "${BENCH_DISK_GIB:-}" ]; then echo "$BENCH_DISK_GIB"; return; fi
	local gib; gib="$(df -g "$HOME" | awk 'NR == 2 { print $4 }')"
	[ -n "$gib" ] && [ "$gib" -gt 64 ] 2>/dev/null && echo "$gib" || echo 64
}

# The guest's idle poll window, as the product sets it (`config::idle_poll_ns`):
# 50 µs where the vCPUs fill the Mac's cores, 200 where cores are left for
# the share's server. Last on the command line wins, so LIGHTER_CMDLINE_EXTRA
# can still override it.
bench_idle_poll_ns() {
	local cores; cores="$(sysctl -n hw.ncpu)"
	if [ "${BENCH_CPUS:-8}" -ge "$cores" ]; then echo 50000; else echo 200000; fi
}

bench_memory_mib() {
	if [ -n "${BENCH_MEMORY_MIB:-}" ]; then echo "$BENCH_MEMORY_MIB"; return; fi
	# OrbStack's configured limit, which is what its guest boots with. Asking
	# it (`orb config show`) starts it when it is not running — a second VM
	# under our measurement — so ask only when it is up, and otherwise apply
	# its default, which it stores nowhere: half the machine, capped at
	# 16 GiB (4 GiB on an 8 GB M1, 16 on a 48 GB M5). A probe that needed
	# its guest running silently fell back to 8 GiB against its 16 for a
	# whole set of results.
	local mib=""
	if pgrep -q -f "OrbStack Helper" 2>/dev/null; then
		mib="$(orb config show 2>/dev/null | awk '/^memory_mib:/ { print $2 }')"
	fi
	if [ -z "$mib" ] || ! [ "$mib" -gt 0 ] 2>/dev/null; then
		mib=$(( $(sysctl -n hw.memsize) / 1048576 / 2 ))
		[ "$mib" -le 16384 ] || mib=16384
	fi
	echo "$mib"
}

setup_lighter() {
	# The kernel the CLI boots by default (`config::kernel_hz`): 250 Hz.
	# `LIGHTER_BENCH_KERNEL=guest/out/Image-hz1000` measures the other.
	KERNEL="${LIGHTER_BENCH_KERNEL:-guest/out/Image}"
	BIN="target/release/examples/lighter-bench"
	# Rosetta, when the Mac has it, the way `lighter start` attaches it: the
	# guest's amd64 path is Rosetta or a message, and `--arch amd64` needs
	# the former. Nothing changes for arm64 cases.
	ROSETTA_DIR=""
	[ -x /Library/Apple/usr/libexec/oah/RosettaLinux/rosetta ] && ROSETTA_DIR=/Library/Apple/usr/libexec/oah/RosettaLinux
	[ -f "$KERNEL" ] || ./guest/kernel/build.sh
	# A private clone, not the master: the master is an artifact, and any
	# second machine mounting it read-write beside the first — a daily
	# driver, another gate — corrupts both. clonefile makes the copy free.
	ROOTFS_MASTER="guest/out/rootfs.ext4"
	[ -f "$ROOTFS_MASTER" ] || ./guest/rootfs/build.sh
	ROOTFS="$(mktemp -t lighter-rootfs).ext4"
	cp -c "$ROOTFS_MASTER" "$ROOTFS" 2>/dev/null || cp "$ROOTFS_MASTER" "$ROOTFS"
	# Release, because a debug VMM is measuring the compiler.
	cargo build --release --example lighter-bench -p lighter-vmm
	./scripts/sign.sh "$BIN" >/dev/null

	# Let fseventsd finish with any VMM that died on this share moments
	# ago — the previous run's, typically — before a new stream opens on
	# it. Its final writes otherwise arrive in the new boot as host changes,
	# withdrawing entries the guest was just given, for the first seconds.
	sleep "${LIGHTER_BENCH_SETTLE_S:-2}"
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
		--disk "$RUN_DIR/data.img" --disk-size-gib "$(bench_disk_gib)" \
		--net --run-dir "$RUN_DIR" \
		--vsock "$SOCKET:2375" \
		--vsock "$RUN_DIR/control.sock:2376" \
		--docker-ports "$SOCKET" \
		--share "bench:$WORK" \
		${LIGHTER_BENCH_DEV_AGENT:+--share "dev:$(dirname "$LIGHTER_BENCH_DEV_AGENT")"} \
		${ROSETTA_DIR:+--share "rosetta:$ROSETTA_DIR"} \
		--no-tty --cpus "${BENCH_CPUS:-8}" --memory-mib "$(bench_memory_mib)" \
		--cmdline "console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init idle.poll_ns=$(bench_idle_poll_ns) lighter.time=$(date +%s) lighter.share=bench:/mnt/bench ${LIGHTER_BENCH_DEV_AGENT:+lighter.share=dev:/mnt/dev lighter.devagent=/mnt/dev/$(basename "$LIGHTER_BENCH_DEV_AGENT")}${ROSETTA_DIR:+ lighter.rosetta} ${LIGHTER_CMDLINE_EXTRA:-}" \
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
	[ "$ARCH" = arm64 ] || IMAGE="$IMAGE-$ARCH"
	docker build -q ${PLATFORM[@]+"${PLATFORM[@]}"} -t "$IMAGE" benchmarks >/dev/null
	for cache in npm pnpm yarn; do
		docker volume create "lighter-bench-$cache-$TARGET$CACHE_SUFFIX" >/dev/null
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
	docker run --rm ${PLATFORM[@]+"${PLATFORM[@]}"} \
		-v "$(work_mount)":/work \
		-v "lighter-bench-npm-$TARGET$CACHE_SUFFIX:/root/.npm" \
		-v "lighter-bench-pnpm-$TARGET$CACHE_SUFFIX:/root/.local/share/pnpm/store" \
		-v "lighter-bench-yarn-$TARGET$CACHE_SUFFIX:/usr/local/share/.cache/yarn" \
		-e WORK=/work \
		-e "REPS=$REPS" \
		-e "CASE_TIMEOUT_S=${CASE_TIMEOUT_S:-300}" \
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
[ -f "benchmarks/fixtures/$FIXTURE/package-lock.json" ] || {
	echo "benchmarks/fixtures/$FIXTURE/package-lock.json is missing; regenerate it with" >&2
	echo "  (cd benchmarks/fixtures/$FIXTURE && npm install --package-lock-only)" >&2
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

# What the runtime costs the Mac, as Activity Monitor accounts it: the
# physical footprint of the runtime's own processes, in MiB, summed. Four
# readings: settled before an install, the peak through one (sampled every
# second), and fifteen and sixty seconds after it ends with nothing running
# — the last two being what a runtime gives back on its own. The footprint
# of a Hypervisor.framework guest reads high (a 2 GiB guest that had touched
# all its memory read 3.5 GB), and the same accounting applies to every
# runtime here, so the figures compare with each other and with what a
# user sees, not with the guest's size.
# The runtime's processes: every one that exists because the runtime is up,
# which for lighter is the one VMM process.
runtime_pids() {
	case "$TARGET" in
	lighter)        echo "$VMM_PID" ;;
	orbstack)       pgrep -f 'OrbStack' | tr '\n' ' ' ;;
	colima)         pgrep -f 'limactl|lima-driver|com.apple.Virtualization.VirtualMachine|virtiofsd' | tr '\n' ' ' ;;
	# Its VM is Virtualization.framework's own XPC service, not a docker
	# process: without it the reading was the app's 300 MiB and never the
	# guest's gigabytes.
	docker-desktop) pgrep -f 'com.docker|com.apple.Virtualization.VirtualMachine' | tr '\n' ' ' ;;
	esac
}

runtime_footprint_mib() {
	local pids; pids="$(runtime_pids)"
	local total=0 pid mb
	for pid in $pids; do
		local raw; raw="$(footprint "$pid" 2>&1)"
		[ -z "${LIGHTER_BENCH_DEBUG_FOOTPRINT:-}" ] || echo "FOOTPRINT pid=$pid $(echo "$raw" | grep -E 'phys_footprint|rror|annot|ailed' | head -2 | tr '\n' ' ')" >&2
		# `footprint` switches to GB at ten gigabytes; a guest that has just
		# run the storage cases holds its whole RAM as cache and reads there.
		mb="$(echo "$raw" | sed -n 's/.*phys_footprint: *\([0-9.]*\) \([MG]\)B.*/\1 \2/p' | head -1 | awk '{v=$1; if ($2=="G") v=v*1024; printf "%d", v}')"
		total=$(( total + ${mb:-0} ))
	done
	echo "$total"
}

run_memory_case() {
	[ "$TARGET" != native ] || return 0
	printf '==> %s: memory' "$TARGET"
	# What the runtime holds at rest is the boot case's reading, a minute
	# after a cold start; this case is what an install does to it. A reading
	# taken here would be five seconds after the warm-up's three installs,
	# before the guest has trimmed anything, and was published as "settled"
	# for a while.
	sleep 5
	local peak after15 after60 now
	REPS=1 run_case npm-install >/dev/null 2>&1 &
	local install=$!
	peak="$(runtime_footprint_mib)"
	while kill -0 "$install" 2>/dev/null; do
		now="$(runtime_footprint_mib)"
		[ "$now" -le "$peak" ] || peak="$now"
		sleep 1
	done
	sleep 15; after15="$(runtime_footprint_mib)"
	sleep 45; after60="$(runtime_footprint_mib)"
	printf ' peak=%s after15s=%s after60s=%s (MiB)\n' "$peak" "$after15" "$after60"
	echo "memory-peak,1,$peak" >> "$RESULTS"
	echo "memory-after-15s,1,$after15" >> "$RESULTS"
	echo "memory-after-60s,1,$after60" >> "$RESULTS"
}


# ---------------------------------------------------------------- network --
#
# Four paths, from the container's point of view, plus what surrounds them:
#   net-tcp-egress    container -> the Mac's LAN address     (Mbit/s)
#   net-tcp-egress-r  the Mac -> container, same connection  (Mbit/s)
#   net-tcp-port      the Mac -> a published port            (Mbit/s)
#   net-tcp-port-r    the container -> the Mac, same port    (Mbit/s)
#   net-udp           container -> the Mac, UDP, unthrottled (Mbit/s)
#   net-connect-rate  TCP connects/s from the Mac to a published port
#   net-http-latency  µs per GET on a kept-alive connection to a published
#                     port, the median (net-http-p99 is recorded beside it)
#   net-dns           µs per lookup of a real name from inside a container
#
# `native` is the Mac talking to itself over loopback for the transfer and
# request cases, which is the ceiling every runtime's published port is
# measured against; the egress direction has no native meaning.
# docker with the target's context, if it has one. bash 3.2 reads an empty
# array as unbound under `set -u`, and the lighter target's is empty.
dk() { docker ${DOCKER_ARGS[@]+"${DOCKER_ARGS[@]}"} "$@"; }
NET_CASES=" net-tcp-egress net-tcp-egress-r net-tcp-port net-tcp-port-r net-udp net-connect-rate net-http-latency net-dns "
NET_HOST_PORT="${NET_HOST_PORT:-5399}"
NET_PUB_PORT="${NET_PUB_PORT:-5398}"
NET_HTTP_PORT="${NET_HTTP_PORT:-5397}"
NET_LAN_IP=""
NET_READY=0
NET_HTTP_PID=""
net_setup() {
	[ "$NET_READY" -eq 0 ] || return 0
	command -v iperf3 >/dev/null || { echo "iperf3 is required on the host for the network cases (brew install iperf3)" >&2; return 1; }
	NET_LAN_IP="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || echo 127.0.0.1)"
	pkill -f "iperf3 -s -D -p $NET_HOST_PORT" 2>/dev/null || true
	iperf3 -s -D -p "$NET_HOST_PORT" --logfile /dev/null
	if [ "$TARGET" = native ]; then
		node -e 'require("http").createServer((q, r) => r.end("ok")).listen(process.argv[1], "127.0.0.1")' "$NET_HTTP_PORT" &
		NET_HTTP_PID=$!
	else
		dk rm -f "lighter-bench-net-$TARGET" "lighter-bench-http-$TARGET" >/dev/null 2>&1 || true
		dk run -d --name "lighter-bench-net-$TARGET" -p "$NET_PUB_PORT:5201" "$IMAGE" iperf3 -s >/dev/null
		dk run -d --name "lighter-bench-http-$TARGET" -p "$NET_HTTP_PORT:8080" "$IMAGE" \
			node -e 'require("http").createServer((q, r) => r.end("ok")).listen(8080)' >/dev/null
	fi
	# The published ports take a moment to be reachable on every runtime.
	local i; for i in $(seq 1 50); do
		python3 -c "import socket,sys; s=socket.create_connection(('127.0.0.1', int(sys.argv[1])), 0.2); s.close()" "$NET_HTTP_PORT" 2>/dev/null && break
		sleep 0.2
	done
	NET_READY=1
}
net_teardown() {
	[ "$NET_READY" -eq 1 ] || return 0
	pkill -f "iperf3 -s -D -p $NET_HOST_PORT" 2>/dev/null || true
	[ -z "$NET_HTTP_PID" ] || kill "$NET_HTTP_PID" 2>/dev/null || true
	[ "$TARGET" = native ] || dk rm -f "lighter-bench-net-$TARGET" "lighter-bench-http-$TARGET" >/dev/null 2>&1 || true
	NET_READY=0
}
# iperf3's JSON, reduced to the receiver's Mbit/s (the sender's for UDP,
# where the receiver's figure is after loss, and loss is its own number).
iperf_mbits() { python3 -c 'import json,sys
d=json.load(sys.stdin); e=d["end"]
bps=(e.get("sum_received") or e.get("sum") or {}).get("bits_per_second", 0)
print(int(bps/1e6))' 2>/dev/null || echo ""; }
# A client in the container for the egress paths; on the Mac for the rest.
net_client() { dk run --rm "$IMAGE" "$@"; }
run_net_case() {
	local name="$1" rep value out
	# A measurement that fails is a dash in the table, not the end of the
	# run: the script is `set -e` and a client that could not connect would
	# otherwise take every case after it down silently.
	set +e
	net_setup || { set -e; return 1; }
	printf '==> %s: %s' "$TARGET" "$name"
	for rep in $(seq 1 "$REPS"); do
		value=""
		case "$name" in
		net-tcp-egress)
			[ "$TARGET" = native ] && out="$(iperf3 -c 127.0.0.1 -p "$NET_HOST_PORT" -t 3 -J 2>/dev/null)" \
				|| out="$(net_client iperf3 -c "$NET_LAN_IP" -p "$NET_HOST_PORT" -t 3 -J 2>/dev/null)"
			value="$(echo "$out" | iperf_mbits)" ;;
		net-tcp-egress-r)
			[ "$TARGET" = native ] && out="$(iperf3 -c 127.0.0.1 -p "$NET_HOST_PORT" -t 3 -R -J 2>/dev/null)" \
				|| out="$(net_client iperf3 -c "$NET_LAN_IP" -p "$NET_HOST_PORT" -t 3 -R -J 2>/dev/null)"
			value="$(echo "$out" | iperf_mbits)" ;;
		net-tcp-port)
			[ "$TARGET" = native ] || { out="$(iperf3 -c 127.0.0.1 -p "$NET_PUB_PORT" -t 3 -J 2>/dev/null)"; value="$(echo "$out" | iperf_mbits)"; } ;;
		net-tcp-port-r)
			[ "$TARGET" = native ] || { out="$(iperf3 -c 127.0.0.1 -p "$NET_PUB_PORT" -t 3 -R -J 2>/dev/null)"; value="$(echo "$out" | iperf_mbits)"; } ;;
		net-udp)
			[ "$TARGET" = native ] && out="$(iperf3 -u -b 0 -c 127.0.0.1 -p "$NET_HOST_PORT" -t 3 -J 2>/dev/null)" \
				|| out="$(net_client iperf3 -u -b 0 -c "$NET_LAN_IP" -p "$NET_HOST_PORT" -t 3 -J 2>/dev/null)"
			value="$(echo "$out" | iperf_mbits)" ;;
		net-connect-rate)
			value="$(python3 -c 'import socket,sys,time
port=int(sys.argv[1]); n=1000; t=time.perf_counter()
for _ in range(n):
    s=socket.create_connection(("127.0.0.1", port)); s.close()
print(int(n/(time.perf_counter()-t)))' "$NET_HTTP_PORT" 2>/dev/null)" ;;
		net-http-latency)
			# Two rows from one run: the median, and the tail beside it.
			out="$(python3 -c 'import http.client,sys,time
port=int(sys.argv[1]); c=http.client.HTTPConnection("127.0.0.1", port); ts=[]
for _ in range(2000):
    t=time.perf_counter(); c.request("GET","/"); c.getresponse().read(); ts.append(time.perf_counter()-t)
ts.sort(); print(int(ts[len(ts)//2]*1e6), int(ts[int(len(ts)*0.99)]*1e6))' "$NET_HTTP_PORT" 2>/dev/null)"
			value="${out%% *}"
			[ -z "$out" ] || echo "net-http-p99,$rep,${out##* }" >> "$RESULTS" ;;
		net-dns)
			local script='const dns=require("dns").promises;(async()=>{const ts=[];for(let i=0;i<200;i++){const t=process.hrtime.bigint();await dns.resolve4("example.com");ts.push(Number(process.hrtime.bigint()-t)/1000)}ts.sort((a,b)=>a-b);console.log(Math.round(ts[100]))})()'
			[ "$TARGET" = native ] && value="$(node -e "$script" 2>/dev/null)" || value="$(net_client node -e "$script" 2>/dev/null)" ;;
		esac
		if [ -n "$value" ]; then
			printf ' %s' "$value"
			echo "$name,$rep,$value" >> "$RESULTS"
		else
			printf ' —'
		fi
	done
	printf '\n'
	set -e
}

# ------------------------------------------------------------------ power --
#
# What the runtime costs the Mac while doing nothing: after a minute of quiet,
# a minute of samples over the runtime's processes. CPU as milliseconds per
# second (1% of a core = 10) and wakeups per second — the interrupt wakeups
# powermetrics counts, and the ones that pull the package out of idle, which
# are the battery's. powermetrics needs root; a machine that lets `sudo -n`
# run it (a sudoers line for that one binary) gets it, and any other gets
# `top`'s idle-wakeup and energy-impact columns instead, which need nothing.
run_power_case() {
	[ "$TARGET" != native ] || return 0
	printf '==> %s: power-idle (a minute of quiet, then a minute of samples)' "$TARGET"
	sleep 60
	local pids pid
	pids="$(runtime_pids)"
	[ -n "$(echo "$pids" | tr -d ' ')" ] || { printf ' no processes\n'; return 0; }
	local cpu wakeups pkg energy
	if sudo -n powermetrics --samplers tasks -i 1000 -n 1 >/dev/null 2>&1; then
		# Twelve five-second samples; the Name column can hold spaces, so the
		# numbers are taken from the right.
		read -r cpu wakeups pkg < <(sudo -n powermetrics --samplers tasks -i 5000 -n 12 2>/dev/null | python3 -c '
import sys
want=set(sys.argv[1].split()); samples=0; cpu=0.0; intr=0.0; pkg=0.0
for line in sys.stdin:
    if line.startswith("*** Sampled"): samples+=1; continue
    f=line.split()
    if len(f) < 7: continue
    try:
        pid=f[-7]; c=float(f[-6]); i=float(f[-2]); p=float(f[-1])
    except ValueError: continue
    if pid in want: cpu+=c; intr+=i; pkg+=p
if samples == 0: sys.exit(1)
print("%d %d %d" % (round(cpu/samples), round(intr/samples), round(pkg/samples)))' "$pids")
		[ -n "${cpu:-}" ] || { printf ' no samples\n'; return 0; }
		printf ' cpu=%s ms/s wakeups=%s/s pkg-idle-wakeups=%s/s (powermetrics)\n' "$cpu" "$wakeups" "$pkg"
		echo "power-cpu-ms-per-s,1,$cpu" >> "$RESULTS"
		echo "power-wakeups-per-s,1,$wakeups" >> "$RESULTS"
		echo "power-pkg-idle-wakeups-per-s,1,$pkg" >> "$RESULTS"
		return 0
	fi
	local args=""
	for pid in $pids; do args="$args -pid $pid"; done
	# shellcheck disable=SC2086
	read -r cpu wakeups energy < <(top -l 13 -s 5 -stats pid,cpu,idlew,power $args 2>/dev/null | python3 -c '
import re,sys
samples=[]; cur=None
for line in sys.stdin:
    if line.startswith("PID"):
        if cur is not None: samples.append(cur)
        cur={"cpu":0.0,"idlew":0,"power":0.0}
        continue
    m=re.match(r"\s*(\d+)\s+([\d.]+)\s+(\d+)\s+([\d.]+)", line)
    if m and cur is not None:
        cur["cpu"]+=float(m.group(2)); cur["idlew"]+=int(m.group(3)); cur["power"]+=float(m.group(4))
if cur is not None: samples.append(cur)
samples=samples[1:]   # the first sample is since process start
if len(samples) < 2: sys.exit(1)
cpu=sum(s["cpu"] for s in samples)/len(samples)
wakeups=(samples[-1]["idlew"]-samples[0]["idlew"])/(5*(len(samples)-1))
power=sum(s["power"] for s in samples)/len(samples)
print("%d %d %d" % (round(cpu*10), round(wakeups), round(power*10)))')
	[ -n "${cpu:-}" ] || { printf ' no samples\n'; return 0; }
	printf ' cpu=%s ms/s wakeups=%s/s energy=%s (x0.1, top)\n' "$cpu" "$wakeups" "$energy"
	echo "power-cpu-ms-per-s,1,$cpu" >> "$RESULTS"
	echo "power-wakeups-per-s,1,$wakeups" >> "$RESULTS"
	echo "power-energy-x10,1,$energy" >> "$RESULTS"
}

materialized=0
# ------------------------------------------------------------------ boot ----

# From a cold stop until usable, the way a person starts each runtime:
# `lighter start`, `orb start`, `colima start`, opening Docker Desktop. Two
# readings a repetition: `docker version` answering, then the first container
# having run — an image already present, so no pull is in it — after one
# untimed round that pays for the pull and, for lighter, a first data disk.
# Last in the list, because it stops the runtime the other cases were using.
# For lighter it is the real command on a home of its own, not the benchmark
# machine, so the CLI's own work (doctor, the rootfs clone, waiting for the
# engine) is inside the number as the others' is inside theirs.
BOOT_HOME=""
LIGHTER_CLI="target/release/lighter"
boot_stop() {
	case "$TARGET" in
	lighter) LIGHTER_HOME="$BOOT_HOME" "$LIGHTER_CLI" stop >/dev/null 2>&1 || true ;;
	orbstack) orb stop >/dev/null 2>&1 || true ;;
	colima) colima stop >/dev/null 2>&1 || true ;;
	docker-desktop)
		# It answers to either name depending on the version installed.
		osascript -e 'quit app "Docker"' >/dev/null 2>&1 || true
		osascript -e 'quit app "Docker Desktop"' >/dev/null 2>&1 || true
		# Quit takes the window and the VM down at once, but the backend
		# processes (com.docker.backend, com.docker.build, docker-agent)
		# stay for two to three minutes, and an `open` while any of them
		# is alive is swallowed: nothing starts, and when they finally
		# exit the app is simply down. Measured on 4.89: 190 s from Quit to
		# the last process gone. So: a short grace for the orderly exit,
		# then the rest of the tree is ended, and the start is timed from
		# a Mac with no Docker process on it.
		local waited=0
		while pgrep -f '/Docker.app/' >/dev/null 2>&1 && [ "$waited" -lt 20 ]; do
			sleep 1; waited=$((waited + 1))
		done
		if pgrep -f '/Docker.app/' >/dev/null 2>&1; then
			pkill -f '/Docker.app/' 2>/dev/null || true
			sleep 3
			pkill -9 -f '/Docker.app/' 2>/dev/null || true
		fi
		while pgrep -f '/Docker.app/' >/dev/null 2>&1; do sleep 1; done
		sleep 5
		;;
	esac
}
boot_start() {
	case "$TARGET" in
	lighter) LIGHTER_HOME="$BOOT_HOME" LIGHTER_GUEST_DIR="${LIGHTER_GUEST_DIR:-guest/out}" "$LIGHTER_CLI" start >/dev/null 2>&1 & ;;
	orbstack) orb start >/dev/null 2>&1 & ;;
	colima) colima start >/dev/null 2>&1 & ;;
	docker-desktop) open -a Docker 2>/dev/null || open -a "Docker Desktop" 2>/dev/null; sleep 0.1 & ;;
	esac
	START_PID=$!
}
# Waits until `docker version` answers, or gives up after five minutes (Docker
# Desktop takes a minute on a good day).
boot_await_docker() {
	local t0="$1"
	while ! dk version >/dev/null 2>&1; do
		[ $(( $(now_ms) - t0 )) -lt 300000 ] || return 1
		sleep 0.05
	done
}
run_boot_case() {
	[ "$TARGET" != native ] || return 0
	if [ "$TARGET" = lighter ]; then
		cargo build --release -p lighter-cli >/dev/null 2>&1
		./scripts/sign.sh "$LIGHTER_CLI" >/dev/null
		BOOT_HOME="$(mktemp -d -t lighter-boot-home)"
		# The benchmark machine goes first: two of our machines on one Mac is
		# not the measurement.
		if [ -n "$VMM_PID" ]; then
			kill "$VMM_PID" 2>/dev/null; wait "$VMM_PID" 2>/dev/null || true; VMM_PID=""
		fi
		export DOCKER_HOST="unix://$BOOT_HOME/docker.sock"
		DOCKER_ARGS=()
	fi
	printf '==> %s: boot (a cold stop, then a start, %s times; untimed round first)' "$TARGET" "$REPS"
	local rep t0 t1 t2 START_PID
	for rep in $(seq 0 "$REPS"); do
		boot_stop
		sleep 2
		t0=$(now_ms)
		boot_start
		if ! boot_await_docker "$t0"; then
			printf ' no measurement: docker did not answer within five minutes'
			echo "boot-docker,$rep,timeout" >> "$RESULTS"
			break
		fi
		t1=$(now_ms)
		dk run --rm alpine:3.21 true >/dev/null 2>&1
		t2=$(now_ms)
		wait "$START_PID" 2>/dev/null || true
		[ "$rep" -gt 0 ] || continue
		printf ' %s/%s' "$((t1 - t0))" "$((t2 - t0))"
		echo "boot-docker,$rep,$((t1 - t0))" >> "$RESULTS"
		echo "boot-first-container,$rep,$((t2 - t0))" >> "$RESULTS"
	done
	printf '\n'
	# At rest: one more cold start with nothing run on it, and a minute to
	# settle. This is the memory table's first row; the memory case itself
	# comes after the warm-up's three installs and cannot read it.
	boot_stop
	sleep 2
	t0=$(now_ms)
	boot_start
	if boot_await_docker "$t0"; then
		wait "$START_PID" 2>/dev/null || true
		sleep 60
		[ "$TARGET" != lighter ] || VMM_PID="$(cat "$BOOT_HOME/lighter.pid" 2>/dev/null)"
		local idle
		idle="$(runtime_footprint_mib)"
		[ "$TARGET" != lighter ] || VMM_PID=""
		echo "==> $TARGET: memory idle=$idle MiB, a minute after a cold start"
		echo "memory-idle,1,$idle" >> "$RESULTS"
	fi
	if [ "$TARGET" = lighter ]; then
		boot_stop
		rm -rf "$BOOT_HOME"
	fi
}

# A container's whole life on a running machine: `docker run --rm alpine
# true`, timed from the host, the way a test suite or a compose stack pays it
# per container. Under `--arch amd64` the container is the x86-64 image, so
# the row is also what the translator adds to a start. The image is pulled
# and run once untimed first.
run_container_start_case() {
	[ "$TARGET" != native ] || return 0
	printf '==> %s: container-start' "$TARGET"
	dk pull --quiet ${PLATFORM[@]+"${PLATFORM[@]}"} alpine:3.21 >/dev/null 2>&1 || true
	dk run --rm ${PLATFORM[@]+"${PLATFORM[@]}"} alpine:3.21 true >/dev/null 2>&1 || true
	local rep t0 t1
	for rep in $(seq 1 "$REPS"); do
		t0=$(now_ms)
		if dk run --rm ${PLATFORM[@]+"${PLATFORM[@]}"} alpine:3.21 true >/dev/null 2>&1; then
			t1=$(now_ms)
			printf ' %s' "$((t1 - t0))"
			echo "container-start,$rep,$((t1 - t0))" >> "$RESULTS"
		else
			printf ' failed'
			echo "container-start,$rep,timeout" >> "$RESULTS"
		fi
	done
	printf '\n'
}

for name in $CASES; do
	if [ "$name" = memory ]; then
		run_memory_case
		continue
	fi
	case "$NET_CASES" in *" $name "*) run_net_case "$name"; continue ;; esac
	if [ "$name" = power-idle ]; then
		net_teardown
		run_power_case
		continue
	fi
	if [ "$name" = boot ]; then
		net_teardown
		run_boot_case
		continue
	fi
	if [ "$name" = container-start ]; then
		run_container_start_case
		continue
	fi
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
	# A host-side cap on top of the runner's own: the runner cannot help
	# when `docker run` itself never returns — a container that has exited
	# but whose exit never reached the client sat here for seventy minutes
	# with the in-container limit long expired. Every repetition's limit
	# plus a minute of grace, then the client is killed and the case is
	# recorded as timed out.
	cap=$(( ${CASE_TIMEOUT_S:-300} * REPS + 60 ))
	run_case "$name" >"$CASE_OUT" 2>&1 &
	case_pid=$!
	# The watchdog detaches from our stdout before it sleeps. Killing the
	# subshell orphans its `sleep`, and an orphan that still holds the
	# pipe keeps whoever is reading us waiting out the whole cap — which
	# is how a one-minute quick gate came to take eleven.
	( exec </dev/null >/dev/null 2>&1; sleep "$cap"; kill "$case_pid" 2>/dev/null && echo "TIME_MS TIMEOUT host-watchdog ${cap}s" >>"$CASE_OUT" ) &
	watchdog_pid=$!
	# A case still running well past what it should take is sampled where
	# it stands, so a stall that happens once in a session leaves a profile
	# behind rather than a timeout and nothing to look at. Only our own VMM
	# is ours to sample; the other targets are left alone.
	sampler_pid=""
	if [ -n "$VMM_PID" ] && [ "${LIGHTER_BENCH_SAMPLE_AFTER_S:-150}" -gt 0 ]; then
		( exec </dev/null >/dev/null 2>&1; sleep "${LIGHTER_BENCH_SAMPLE_AFTER_S:-150}"; kill -0 "$case_pid" 2>/dev/null && { mkdir -p .logs; sample "$VMM_PID" 8 -mayDie -file ".logs/stall-$TARGET-$name.txt"; } ) &
		sampler_pid=$!
	fi
	wait "$case_pid" 2>/dev/null || true
	pkill -P "$watchdog_pid" 2>/dev/null || true
	kill "$watchdog_pid" 2>/dev/null; wait "$watchdog_pid" 2>/dev/null || true
	if [ -n "$sampler_pid" ]; then
		pkill -P "$sampler_pid" 2>/dev/null || true
		kill "$sampler_pid" 2>/dev/null; wait "$sampler_pid" 2>/dev/null || true
	fi
	while read -r ms; do
		rep=$((rep + 1))
		printf ' %s' "$ms"
		# A timeout is recorded as such rather than as a number or a gap: a
		# missing row reads as "not run", and a huge number would be averaged
		# into a median as if it were a measurement. The report skips it and
		# says so.
		echo "$name,$rep,$ms" >> "$RESULTS"
	done < <(sed -n 's/^TIME_MS //p' "$CASE_OUT" | sed 's/^TIMEOUT .*/timeout/')
	# The children's CPU time beside their wall time, where the runner can
	# report it. Not a column: a diagnostic for reading a gap, not a result.
	cpu="$(sed -n 's/^CPU_MS //p' "$CASE_OUT" | tr '\n' ' ')"
	[ -z "$cpu" ] || printf '  (cpu ms: %s)' "${cpu% }"
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
		fastest="$(awk -F, -v w="$name" '$1 == w && $3 ~ /^[0-9]+$/ { print $3 }' "$RESULTS" | sort -n | head -1)"
		if [ "${fastest:-1}" -lt 5 ]; then
			printf '  <- implausible; the fixture was probably missing'
		fi
	fi
	# The case's whole output, for a run that produced fewer measurements
	# than repetitions: a repetition that failed is otherwise invisible.
	[ -z "${LIGHTER_BENCH_KEEP_OUTPUT:-}" ] || cp "$CASE_OUT" ".logs/case-$TARGET-$name.out" 2>/dev/null || true
	rm -f "$CASE_OUT"
	printf '\n'
	if [ -n "$HELPER_PID" ]; then kill "$HELPER_PID" 2>/dev/null || true; HELPER_PID=""; fi
done
net_teardown

echo
echo "==> results in $RESULTS"

#!/usr/bin/env bash
# The cross-runtime record in one command: every target sized alike, run one
# at a time with the rest stopped, the Mac settled before each case, and the
# results gathered with a summary. What made a record fair used to live in a
# script on each machine, and the mistakes were made there: a stop issued
# before a wait killed a running stage, and a stage ran unsized.
#
#   benchmarks/compare.sh --machine m1 [options]
#
#   --targets "..."   default: native lighter orbstack docker-desktop colima podman apple-container
#   --stages "..."    any of share guest media (default: all three)
#   --gpu             also llm-gpu.sh: Metal on the Mac and through lighter, Vulkan through lighter and Podman
#   --quality         also transcode-check.sh: the Mac, lighter, and one software runtime
#   --pause-containers  for --gpu/--quality, stop the containers running on the
#                     installed lighter and start them again after; without it a
#                     busy engine is a refusal
#   --reps N          default 3
#   --out DIR         default benchmarks/results/machines/<machine>/compare-<date>
#
# Sized by BENCH_CPUS (default 8) and BENCH_MEMORY_MIB (default 4096, whole
# GiB for Colima); run.sh reads every guest's size from inside it and refuses
# a mismatch. Needs LIGHTER_BENCH_IMAGE_DIR (image-dir.sh), LIGHTER_BENCH_MODEL_DIR
# (the model and the clip), and for the lighter target LIGHTER_BENCH_BIN and
# LIGHTER_BENCH_GUEST_DIR. The Mac's tools, the image's versions built for the
# Mac, come from scripts/records/prepare-benchmark-tools.sh and are checked.
#
# It stops only what it starts. A lighter machine already running (a daily
# driver) is a refusal, not something to stop; so is a container already
# running on an engine it needs, for --gpu and --quality through the installed
# lighter. Podman's machine (BENCH_PODMAN_MACHINE, default bench) must exist.
set -uo pipefail
cd "$(dirname "$0")/.."
MACHINE=""; TARGETS="native lighter orbstack docker-desktop colima podman apple-container"
STAGES="share guest media"; GPU=0; QUALITY=0; PAUSE=0; REPS=3; OUT=""
while [ $# -gt 0 ]; do
	case "$1" in
	--machine) MACHINE="$2"; shift 2 ;;
	--targets) TARGETS="$2"; shift 2 ;;
	--stages) STAGES="$2"; shift 2 ;;
	--gpu) GPU=1; shift ;;
	--quality) QUALITY=1; shift ;;
	--pause-containers) PAUSE=1; shift ;;
	--reps) REPS="$2"; shift 2 ;;
	--out) OUT="$2"; shift 2 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done
[ -n "$MACHINE" ] || { echo "--machine is required (m1, m5, ...)" >&2; exit 2; }
export BENCH_CPUS="${BENCH_CPUS:-8}" BENCH_MEMORY_MIB="${BENCH_MEMORY_MIB:-4096}"
export LIGHTER_BENCH_SETTLE=1
: "${LIGHTER_BENCH_IMAGE_DIR:?}" "${LIGHTER_BENCH_MODEL_DIR:?}"
OUT="${OUT:-benchmarks/results/machines/$MACHINE/compare-$(date +%F)}"
mkdir -p "$OUT" .logs
PODMAN="${BENCH_PODMAN:-/opt/podman/bin/podman}"
PODMAN_MACHINE="${BENCH_PODMAN_MACHINE:-bench}"
NATIVE_TOOLS="$PWD/.logs/050/tools/native"
GUEST_CASES="npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf disk-seq-write"
MEDIA_CASES="cpu-zstd-1 cpu-zstd-8 transcode-h264 transcode-hevc llm"
DD_SETTINGS="$HOME/Library/Group Containers/group.com.docker/settings-store.json"
log() { echo "==> $* $(date +%H:%M)"; }

lighter_daemon_running() { [ -f "$HOME/.lighter/lighter.pid" ] && kill -0 "$(cat "$HOME/.lighter/lighter.pid")" 2>/dev/null; }
if lighter_daemon_running; then
	echo "A lighter machine is running (a daily driver?). compare.sh does not stop what it did not start: stop it first." >&2
	exit 1
fi
case " $TARGETS " in *" native "*) ;; *) [ "$GPU$QUALITY" = 00 ] || { echo "--gpu and --quality need the native target" >&2; exit 2; } ;; esac
bash scripts/records/prepare-benchmark-tools.sh > .logs/prepare-benchmark-tools.log 2>&1 \
	|| { echo "prepare-benchmark-tools.sh failed; see .logs/prepare-benchmark-tools.log" >&2; exit 1; }
export BENCH_TOOLS_PATH="$NATIVE_TOOLS/bin" BENCH_REQUIRE_PINNED_TOOLS=1

# Every runtime down, and waited for: the stops return before the VMs are
# gone, and run.sh's guard would see them. OrbStack starts itself when its
# socket is touched, so the CLI's context is parked on nothing first.
all_off() {
	docker context use default >/dev/null 2>&1
	pkill -x socktainer 2>/dev/null; container system stop >/dev/null 2>&1
	"$PODMAN" machine stop "$PODMAN_MACHINE" >/dev/null 2>&1
	orb stop >/dev/null 2>&1; colima stop >/dev/null 2>&1
	docker desktop stop --timeout 120 >/dev/null 2>&1; osascript -e 'quit app "Docker Desktop"' >/dev/null 2>&1
	local i
	for i in $(seq 1 120); do
		pgrep -x "OrbStack Helper" >/dev/null && orb stop >/dev/null 2>&1
		pgrep -x "OrbStack Helper" >/dev/null || pgrep -x xbin >/dev/null || pgrep -f "limactl hostagent" >/dev/null \
			|| pgrep -x com.docker.backend >/dev/null || pgrep -f "$(dirname "$PODMAN")/krunkit" >/dev/null \
			|| pgrep -f container-runtime-linux >/dev/null || return 0
		sleep 1
	done
	echo "a runtime did not shut down:" >&2
	pgrep -fl "OrbStack Helper|xbin|limactl hostagent|com.docker.backend|krunkit|container-runtime-linux" | cut -c1-80 >&2
	exit 1
}

# Sizes set where each runtime keeps them, before any starts. `orb config set`
# starts OrbStack, hence the stop after it.
configure() {
	case " $TARGETS " in *" orbstack "*)
		orb config set cpu "$BENCH_CPUS" >/dev/null && orb config set memory_mib "$BENCH_MEMORY_MIB" >/dev/null; orb stop >/dev/null 2>&1 ;; esac
	case " $TARGETS " in *" docker-desktop "*)
		python3 - "$DD_SETTINGS" "$BENCH_CPUS" "$BENCH_MEMORY_MIB" <<-'PY'
		import json, sys
		path, cpus, mib = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
		settings = json.load(open(path)); settings["Cpus"] = cpus; settings["MemoryMiB"] = mib
		json.dump(settings, open(path, "w"), indent=2)
		PY
		;; esac
	case " $TARGETS " in *" podman "*)
		"$PODMAN" machine set --cpus "$BENCH_CPUS" --memory "$BENCH_MEMORY_MIB" "$PODMAN_MACHINE" >/dev/null ;; esac
	case " $TARGETS " in *" colima "*)
		[ $((BENCH_MEMORY_MIB % 1024)) -eq 0 ] || { echo "Colima takes whole GiB; BENCH_MEMORY_MIB=$BENCH_MEMORY_MIB" >&2; exit 2; } ;; esac
}

start_target() {
	case "$1" in
	native|lighter) ;;
	orbstack) orb start >/dev/null 2>&1 ;;
	docker-desktop) open -a Docker; local i; for i in $(seq 1 90); do docker --context desktop-linux info >/dev/null 2>&1 && break; sleep 2; done ;;
	colima) colima start --cpu "$BENCH_CPUS" --memory $((BENCH_MEMORY_MIB / 1024)) --vm-type vz --mount-type virtiofs >/dev/null 2>&1 ;;
	podman) "$PODMAN" machine start "$PODMAN_MACHINE" >/dev/null 2>&1 ;;
	apple-container) container system start >/dev/null 2>&1
		pgrep -x socktainer >/dev/null || { nohup socktainer --no-docker-context >/dev/null 2>&1 </dev/null & }; sleep 5 ;;
	*) echo "unknown target: $1" >&2; exit 2 ;;
	esac
}

# The target's environment for run.sh: lighter gets the media engine.
target_env() {
	case "$1" in
	lighter) echo "LIGHTER_BENCH_VMM_ARGS=--video LIGHTER_BENCH_CASE_ARGS=--device=lighter.sh/video=all" ;;
	esac
}

run_stage() { # target, stage
	local target="$1" stage="$2" label cases where=""
	case "$stage" in
	share) label="$MACHINE-$target"; cases="" ;;
	guest) [ "$target" != native ] || return 0; label="$MACHINE-$target-guest"; cases="$GUEST_CASES"; where="--where guest" ;;
	media) label="$MACHINE-$target-media"; cases="$MEDIA_CASES" ;;
	esac
	log "STAGE $target $stage"
	# shellcheck disable=SC2046,SC2086
	env $(target_env "$target") scripts/capped.sh 7200 ./benchmarks/run.sh --target "$target" --reps "$REPS" \
		${cases:+--cases "$cases"} $where --label "$label" > "$OUT/$label.log" 2>&1
	echo "$target-$stage=$?"
	cp "benchmarks/results/$label.csv" "benchmarks/results/$label.tree" "$OUT/" 2>/dev/null
}

all_off
configure
for target in $TARGETS; do
	all_off
	start_target "$target"
	for stage in $STAGES; do run_stage "$target" "$stage"; done
done
all_off

# The extras run through the installed lighter (the release, with Metal and
# the media engine), which the bench VMM may be built without. They refuse an
# engine with containers running rather than stop anybody's.
extras_on_lighter() {
	lighter start --timeout 120 >/dev/null 2>&1 || { echo "the installed lighter did not start" >&2; return 1; }
	local paused=""
	if [ "$(docker --context lighter ps -q | wc -l | tr -d ' ')" -gt 0 ]; then
		if [ "$PAUSE" = 0 ]; then
			echo "containers are running on the installed lighter; stop them, or pass --pause-containers:" >&2
			docker --context lighter ps --format '    {{.Names}}' >&2
			lighter stop >/dev/null 2>&1; return 1
		fi
		paused="$(docker --context lighter ps -q)"
		# shellcheck disable=SC2086
		docker --context lighter stop $paused >/dev/null
	fi
	docker --context lighter image inspect lighter-bench:2 >/dev/null 2>&1 \
		|| docker --context lighter load -i "$LIGHTER_BENCH_IMAGE_DIR/arm64.tar" >/dev/null
	sleep 20
	[ "$QUALITY" = 0 ] || { log "STAGE lighter quality"; LIGHTER_BENCH_CASE_ARGS="--device lighter.sh/video=all" \
		benchmarks/transcode-check.sh lighter lighter "$OUT/$MACHINE-transcode-quality.csv" > "$OUT/quality-lighter.log" 2>&1; echo "lighter-quality=$?"; }
	[ "$GPU" = 0 ] || { log "STAGE lighter gpu"; benchmarks/llm-gpu.sh lighter lighter "$OUT/$MACHINE-llm-gpu.csv" > "$OUT/gpu-lighter.log" 2>&1; echo "lighter-gpu=$?"; }
	# Podman's Vulkan row runs the same image; it goes across from here.
	if [ "$GPU" = 1 ]; then
		docker --context lighter save "${LIGHTER_BENCH_LLAMA_IMAGE:-llama-vulkan:arm64}" -o "$HOME/.lighter-bench-llama.tar" 2>/dev/null
	fi
	# shellcheck disable=SC2086
	[ -z "$paused" ] || docker --context lighter start $paused >/dev/null
	lighter stop >/dev/null 2>&1
}
if [ "$GPU$QUALITY" != 00 ]; then
	rm -f "$OUT/$MACHINE-transcode-quality.csv" "$OUT/$MACHINE-llm-gpu.csv"
	log "STAGE native extras"
	[ "$QUALITY" = 0 ] || { benchmarks/transcode-check.sh native "" "$OUT/$MACHINE-transcode-quality.csv" > "$OUT/quality-native.log" 2>&1; echo "native-quality=$?"; }
	[ "$GPU" = 0 ] || { LIGHTER_BENCH_NATIVE_LLAMA="$NATIVE_TOOLS/metal/bin/llama-bench" benchmarks/llm-gpu.sh native "" "$OUT/$MACHINE-llm-gpu.csv" > "$OUT/gpu-native.log" 2>&1; echo "native-gpu=$?"; }
	extras_on_lighter
	case " $TARGETS " in *" podman "*)
		start_target podman
		[ "$QUALITY" = 0 ] || { log "STAGE podman quality"; benchmarks/transcode-check.sh podman podman-bench "$OUT/$MACHINE-transcode-quality.csv" > "$OUT/quality-podman.log" 2>&1; echo "podman-quality=$?"; }
		if [ "$GPU" = 1 ]; then
			log "STAGE podman gpu"
			[ ! -f "$HOME/.lighter-bench-llama.tar" ] || docker --context podman-bench load -i "$HOME/.lighter-bench-llama.tar" >/dev/null
			rm -f "$HOME/.lighter-bench-llama.tar"
			benchmarks/llm-gpu.sh podman podman-bench "$OUT/$MACHINE-llm-gpu.csv" > "$OUT/gpu-podman.log" 2>&1; echo "podman-gpu=$?"
		fi
		all_off ;;
	esac
fi

log "DONE"
python3 benchmarks/compare-summary.py "$OUT" "$MACHINE"

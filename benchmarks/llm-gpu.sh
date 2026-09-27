#!/usr/bin/env bash
# The llm case on a GPU where a runtime has one: llama-bench's own tokens a
# second, prompt (512) and generation (128), Qwen2.5 0.5B Q4_K_M, three
# repetitions, every layer offloaded. Outside run.sh because the image (the
# gate's llama-vulkan: llama.cpp with Vulkan and the RPC backend) has no node
# for the runner, and because the number that matters is llama-bench's own.
#
#   benchmarks/llm-gpu.sh <target> <docker context> <out.csv>
#
# native: Metal, from LIGHTER_BENCH_NATIVE_LLAMA (default: llama-bench on
# PATH); build it from the commit the image and lighter's Metal server use, or
# the row compares builds rather than runtimes. lighter: Metal through
# lighter.sh/metal (ggml RPC to the Mac's GPU) and Vulkan through
# lighter.sh/gpu. podman: Vulkan through krunkit's virtio-gpu (/dev/dri).
# Every container target also runs the same image on the CPU, 8 threads, for
# reference, which is all the rest have.
set -euo pipefail
TARGET="$1"; CTX="${2:-}"; OUT="$3"
IMAGE="${LIGHTER_BENCH_LLAMA_IMAGE:-llama-vulkan:arm64}"
MODEL_DIR="${LIGHTER_BENCH_MODEL_DIR:?}"
MODEL=qwen2.5-0.5b-instruct-q4_k_m.gguf
ARGS=(-p 512 -n 128 -r 3 -o csv)
row() { # label, then llama-bench's csv on stdin
	python3 -c '
import csv,sys
label,target=sys.argv[1],sys.argv[2]
for r in csv.DictReader(sys.stdin):
    test="pp512" if r["n_prompt"]!="0" else "tg128"
    print("%s,%s,%s,%.1f" % (target, label, test, float(r["avg_ts"])))' "$1" "$TARGET" >> "$OUT"
}
dk() { docker --context "$CTX" "$@"; }
mount_model() { echo "-v $MODEL_DIR:/models:ro"; }
case "$TARGET" in
native)
	NATIVE="${LIGHTER_BENCH_NATIVE_LLAMA:-llama-bench}"
	echo "native llama.cpp: $("$NATIVE" --version 2>&1 | grep -m1 version)" >&2
	"$NATIVE" -m "$MODEL_DIR/$MODEL" -ngl 99 "${ARGS[@]}" | row metal ;;
*)
	# Another container on the engine is load in the guest the rows would not
	# show; a test stack left running once cost llama.cpp 19%.
	if [ "$(dk ps -q | wc -l | tr -d ' ')" -gt 0 ]; then
		echo "containers are running on $CTX; stop them first:" >&2
		dk ps --format '    {{.Names}}' >&2
		exit 1
	fi
	LIGHTER_GATE_LLAMA_IMAGE="$IMAGE" "$(dirname "$0")/../scripts/gates/llama-image.sh" --context "$CTX"
	echo "image llama.cpp: $(dk run --rm "$IMAGE" llama-bench --version 2>&1 | grep -m1 version)" >&2
	# shellcheck disable=SC2046
	dk run --rm $(mount_model) "$IMAGE" llama-bench -m /models/$MODEL -ngl 0 -t 8 "${ARGS[@]}" | row cpu
	case "$TARGET" in
	lighter)
		dk run --rm --device lighter.sh/metal=all $(mount_model) "$IMAGE" sh -c \
			'llama-bench -m /models/'$MODEL' --rpc "$LIGHTER_METAL" -ngl 99 -p 512 -n 128 -r 3 -o csv' | row metal
		dk run --rm --device lighter.sh/gpu=all $(mount_model) "$IMAGE" \
			llama-bench -m /models/$MODEL -ngl 99 "${ARGS[@]}" | row vulkan ;;
	podman)
		dk run --rm --device /dev/dri $(mount_model) "$IMAGE" \
			llama-bench -m /models/$MODEL -ngl 99 "${ARGS[@]}" | row vulkan ;;
	esac ;;
esac

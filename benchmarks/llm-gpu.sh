#!/usr/bin/env bash
# The llm case on a GPU where a runtime has one: llama-bench's own tokens a
# second, prompt (512) and generation (128), Qwen2.5 0.5B Q4_K_M, three
# repetitions, every layer offloaded. Outside run.sh because the image (the
# gate's llama-vulkan: llama.cpp with Vulkan and the RPC backend) has no node
# for the runner, and because the number that matters is llama-bench's own.
#
#   benchmarks/llm-gpu.sh <target> <docker context> <out.csv>
#
# native: Metal (Homebrew's llama-bench). lighter: Metal through
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
    print(f"{target},{label},{test},{float(r[\"avg_ts\"]):.1f}")' "$1" "$TARGET" >> "$OUT"
}
dk() { docker --context "$CTX" "$@"; }
mount_model() { echo "-v $MODEL_DIR:/models:ro"; }
case "$TARGET" in
native)
	llama-bench -m "$MODEL_DIR/$MODEL" -ngl 99 "${ARGS[@]}" | row metal ;;
*)
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

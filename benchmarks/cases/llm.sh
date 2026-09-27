#!/bin/sh
# A small language model on the CPU: Qwen2.5 0.5B (Q4_K_M) read from the
# shared folder, 512 tokens of prompt and 128 generated, on 8 threads, with
# llama.cpp's own benchmark, which also prints tokens a second. The wall time
# includes loading the model, a 491 MB file the first repetition reads cold.
set -eu
m="$WORK/models/qwen2.5-0.5b-instruct-q4_k_m.gguf"
llama-bench -m "$m" -p 512 -n 128 -r 1 -t 8 -ngl 0 -o md | grep -E "pp512|tg128"
